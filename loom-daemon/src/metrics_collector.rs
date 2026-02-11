//! Background GitHub metrics collector
//!
//! This module provides a background thread that periodically collects GitHub events
//! (PRs merged, issues closed, commits) and stores them in the activity database for
//! timeline visualization and correlation with agent activity.
//!
//! Metrics collection runs every 15 minutes by default. You can customize the interval:
//! ```bash
//! LOOM_METRICS_INTERVAL=300 pnpm daemon:preview  # Check every 5 minutes
//! ```
//!
//! To disable metrics collection:
//! ```bash
//! LOOM_GITHUB_METRICS=0 pnpm daemon:preview  # Disabled
//! ```

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Configuration for metrics collection
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    /// Path to workspace directory
    pub workspace_path: String,
    /// GitHub repository owner (e.g., "rjwalters")
    pub repo_owner: String,
    /// GitHub repository name (e.g., "loom")
    pub repo_name: String,
    /// Collection interval in seconds
    pub interval_secs: u64,
    /// Path to activity database
    pub db_path: String,
}

/// State tracking for incremental syncing
#[derive(Debug, Default, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
struct MetricsState {
    /// Last PR sync timestamp (ISO 8601)
    pr_sync: Option<String>,
    /// Last issue sync timestamp (ISO 8601)
    issue_sync: Option<String>,
    /// Last commit sync timestamp (ISO 8601)
    commit_sync: Option<String>,
}

impl MetricsState {
    fn load_from_workspace(workspace_path: &str) -> Result<Self> {
        let state_path = PathBuf::from(workspace_path)
            .join(".loom")
            .join("metrics_state.json");

        if !state_path.exists() {
            return Ok(Self::default());
        }

        let contents =
            fs::read_to_string(&state_path).context("Failed to read metrics state file")?;

        serde_json::from_str(&contents).context("Failed to parse metrics state JSON")
    }

    fn save_to_workspace(&self, workspace_path: &str) -> Result<()> {
        let state_path = PathBuf::from(workspace_path)
            .join(".loom")
            .join("metrics_state.json");

        let contents =
            serde_json::to_string_pretty(self).context("Failed to serialize metrics state")?;

        fs::write(&state_path, contents).context("Failed to write metrics state file")?;

        Ok(())
    }
}

/// Unified GitHub event item for deserialization
/// Uses serde aliases to handle both mergedAt and closedAt fields
#[derive(Debug, Deserialize)]
struct GitHubEventItem {
    number: i64,
    /// Event timestamp - handles both mergedAt (PRs) and closedAt (issues)
    #[serde(alias = "mergedAt", alias = "closedAt")]
    event_time: Option<String>,
    author: Author,
}

/// Configuration for collecting different types of GitHub events
struct EventCollectionConfig {
    /// Resource type for gh CLI subcommand ("pr" or "issue")
    resource_type: &'static str,
    /// State filter for gh CLI ("merged" or "closed")
    state: &'static str,
    /// Query prefix for date filtering ("merged:>" or "closed:>")
    query_prefix: &'static str,
    /// JSON fields to request from gh CLI
    json_fields: &'static str,
    /// Event type string for database storage
    event_type: &'static str,
    /// Whether this is a PR event (true) or issue event (false)
    is_pr: bool,
}

/// GitHub author data
#[derive(Debug, Deserialize)]
struct Author {
    login: String,
}

/// GitHub API rate limit response
#[derive(Debug, Deserialize)]
struct RateLimitResponse {
    resources: RateLimitResources,
}

#[derive(Debug, Deserialize)]
struct RateLimitResources {
    core: RateLimitInfo,
}

#[derive(Debug, Deserialize)]
struct RateLimitInfo {
    remaining: i32,
}

/// Check if GitHub CLI is installed
fn check_gh_cli_installed() -> bool {
    Command::new("which")
        .arg("gh")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Check environment variable to see if metrics collection is enabled
/// Returns None if disabled, `Some(interval_secs)` if enabled
pub fn check_env_enabled() -> Option<u64> {
    match std::env::var("LOOM_GITHUB_METRICS") {
        Ok(val) if val == "0" => {
            log::info!("📊 GitHub metrics collection disabled (LOOM_GITHUB_METRICS=0)");
            None
        }
        _ => {
            // Default: 15 minutes (900 seconds)
            let interval = std::env::var("LOOM_METRICS_INTERVAL")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(900);

            Some(interval)
        }
    }
}

/// Validate workspace is a GitHub repository and extract owner/repo
fn validate_github_workspace(workspace_path: &str) -> Result<(String, String)> {
    let output = Command::new("git")
        .args(["-C", workspace_path, "remote", "get-url", "origin"])
        .output()
        .context("Failed to get git remote URL")?;

    if !output.status.success() {
        return Err(anyhow!("Not a git repository"));
    }

    let remote_url = String::from_utf8(output.stdout).context("Invalid UTF-8 in git remote URL")?;

    // Parse owner/repo from various URL formats:
    // - https://github.com/owner/repo.git
    // - git@github.com:owner/repo.git
    let re =
        regex::Regex::new(r"github\.com[:/]([^/]+)/([^/.]+)").context("Failed to compile regex")?;

    let caps = re
        .captures(&remote_url)
        .ok_or_else(|| anyhow!("Not a GitHub repository (no github.com in remote URL)"))?;

    Ok((caps[1].to_string(), caps[2].to_string()))
}

/// Start background metrics collector thread
///
/// # Arguments
/// * `config` - Configuration for metrics collection
///
/// # Returns
/// A `JoinHandle` for the background thread
///
/// # Example
/// ```
/// use loom_daemon::metrics_collector;
///
/// let config = metrics_collector::MetricsConfig {
///     workspace_path: "/path/to/workspace".to_string(),
///     repo_owner: "rjwalters".to_string(),
///     repo_name: "loom".to_string(),
///     interval_secs: 900,
///     db_path: "/path/to/activity.db".to_string(),
/// };
///
/// let _metrics_handle = metrics_collector::start_metrics_collector(config);
/// ```
pub fn start_metrics_collector(config: MetricsConfig) -> JoinHandle<()> {
    log::info!(
        "📊 Starting GitHub metrics collector for {}/{} (checking every {} seconds)",
        config.repo_owner,
        config.repo_name,
        config.interval_secs
    );

    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(config.interval_secs));

        match collect_and_store_events(&config) {
            Ok(event_count) => {
                log::info!("📊 Collected {event_count} GitHub events");
            }
            Err(e) => {
                log::error!("❌ Metrics collection failed: {e}");
            }
        }
    })
}

/// Collect GitHub events and store them in the database
fn collect_and_store_events(config: &MetricsConfig) -> Result<usize> {
    // Check rate limit before proceeding
    if !check_rate_limit() {
        log::warn!("⚠️  Skipping metrics collection due to low GitHub API rate limit");
        return Ok(0);
    }

    // Load previous sync state
    let mut state = MetricsState::load_from_workspace(&config.workspace_path)?;

    let conn = Connection::open(&config.db_path).context("Failed to open activity database")?;

    let mut total_events = 0;

    // Collect PRs
    match collect_pr_events(config, &conn, state.pr_sync.as_deref()) {
        Ok(count) => {
            total_events += count;
            state.pr_sync = Some(chrono::Utc::now().to_rfc3339());
        }
        Err(e) => {
            log::error!("Failed to collect PR events: {e}");
        }
    }

    // Collect issues
    match collect_issue_events(config, &conn, state.issue_sync.as_deref()) {
        Ok(count) => {
            total_events += count;
            state.issue_sync = Some(chrono::Utc::now().to_rfc3339());
        }
        Err(e) => {
            log::error!("Failed to collect issue events: {e}");
        }
    }

    // Save updated state
    state.save_to_workspace(&config.workspace_path)?;

    Ok(total_events)
}

/// Check GitHub API rate limit
fn check_rate_limit() -> bool {
    match Command::new("gh").args(["api", "rate_limit"]).output() {
        Ok(output) if output.status.success() => {
            match serde_json::from_slice::<RateLimitResponse>(&output.stdout) {
                Ok(rate_info) => {
                    let remaining = rate_info.resources.core.remaining;
                    if remaining < 100 {
                        log::warn!("⚠️  GitHub API rate limit low: {remaining} remaining");
                        return false;
                    }
                    true
                }
                Err(e) => {
                    log::warn!("Failed to parse rate limit response: {e}");
                    true // Assume OK if we can't parse
                }
            }
        }
        Ok(_) | Err(_) => {
            log::warn!("Failed to check rate limit, proceeding anyway");
            true // Assume OK if command fails
        }
    }
}

/// Collect PR merge events
fn collect_pr_events(
    config: &MetricsConfig,
    conn: &Connection,
    since: Option<&str>,
) -> Result<usize> {
    static PR_CONFIG: EventCollectionConfig = EventCollectionConfig {
        resource_type: "pr",
        state: "merged",
        query_prefix: "merged:>",
        json_fields: "number,mergedAt,author",
        event_type: "pr_merged",
        is_pr: true,
    };
    collect_github_events(config, conn, since, &PR_CONFIG)
}

/// Collect issue close events
fn collect_issue_events(
    config: &MetricsConfig,
    conn: &Connection,
    since: Option<&str>,
) -> Result<usize> {
    static ISSUE_CONFIG: EventCollectionConfig = EventCollectionConfig {
        resource_type: "issue",
        state: "closed",
        query_prefix: "closed:>",
        json_fields: "number,closedAt,author",
        event_type: "issue_closed",
        is_pr: false,
    };
    collect_github_events(config, conn, since, &ISSUE_CONFIG)
}

/// Generic GitHub event collection function
fn collect_github_events(
    metrics_config: &MetricsConfig,
    conn: &Connection,
    since: Option<&str>,
    event_config: &EventCollectionConfig,
) -> Result<usize> {
    let repo_string = format!("{}/{}", metrics_config.repo_owner, metrics_config.repo_name);
    let search_query = since.and_then(|since_time| {
        chrono::DateTime::parse_from_rfc3339(since_time)
            .ok()
            .map(|dt| format!("{}{}", event_config.query_prefix, dt.format("%Y-%m-%d")))
    });

    let mut args = vec![
        event_config.resource_type,
        "list",
        "--repo",
        &repo_string,
        "--state",
        event_config.state,
        "--limit",
        "100",
        "--json",
        event_config.json_fields,
    ];

    if let Some(ref query) = search_query {
        args.push("--search");
        args.push(query);
    }

    let output = Command::new("gh")
        .args(&args)
        .output()
        .with_context(|| format!("Failed to execute gh {} list", event_config.resource_type))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh {} list failed: {stderr}", event_config.resource_type));
    }

    let items: Vec<GitHubEventItem> = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("Failed to parse {} JSON", event_config.resource_type))?;

    let mut count = 0;
    for item in items {
        if let Some(event_time) = item.event_time {
            let (pr_number, issue_number) = if event_config.is_pr {
                (Some(item.number), None)
            } else {
                (None, Some(item.number))
            };

            if insert_github_event(
                conn,
                event_config.event_type,
                &event_time,
                pr_number,
                issue_number,
                None,
                &item.author.login,
            )? {
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Insert a GitHub event into the database, checking for duplicates
/// Returns true if a new event was inserted, false if it already existed
#[allow(clippy::too_many_arguments)]
fn insert_github_event(
    conn: &Connection,
    event_type: &str,
    event_time: &str,
    pr_number: Option<i64>,
    issue_number: Option<i64>,
    commit_sha: Option<&str>,
    author: &str,
) -> Result<bool> {
    // Check if event already exists
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM github_events
         WHERE event_type = ?1 AND event_time = ?2
         AND pr_number IS ?3 AND issue_number IS ?4",
        params![event_type, event_time, pr_number, issue_number],
        |row| row.get(0),
    )?;

    if exists {
        log::debug!("Event already recorded, skipping: {event_type} at {event_time}");
        return Ok(false);
    }

    // Try to correlate with agent activity
    let activity_id = correlate_with_activity(conn, pr_number, issue_number, event_time)?;

    // Insert the event
    conn.execute(
        "INSERT INTO github_events (activity_id, event_type, event_time, pr_number, issue_number, commit_sha, author)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![activity_id, event_type, event_time, pr_number, issue_number, commit_sha, author],
    )?;

    log::debug!(
        "Inserted {event_type} event: PR#{pr_number:?} Issue#{issue_number:?} at {event_time}"
    );

    Ok(true)
}

/// Attempt to correlate a GitHub event with agent activity
fn correlate_with_activity(
    conn: &Connection,
    _pr_number: Option<i64>,
    issue_number: Option<i64>,
    event_time: &str,
) -> Result<Option<i64>> {
    // Strategy 1: Match by issue number
    if let Some(issue_num) = issue_number {
        let activity_id = conn
            .query_row(
                "SELECT id FROM agent_activity
                 WHERE issue_number = ?1
                 ORDER BY ABS(julianday(timestamp) - julianday(?2))
                 LIMIT 1",
                params![issue_num, event_time],
                |row| row.get(0),
            )
            .optional()?;

        if activity_id.is_some() {
            return Ok(activity_id);
        }
    }

    // Strategy 2: Match by PR number (find linked issue in activity)
    // For now, skip this as it requires parsing PR bodies for issue links

    // Strategy 3: Match by timestamp proximity (within 1 hour)
    let activity_id = conn
        .query_row(
            "SELECT id FROM agent_activity
             WHERE ABS(julianday(timestamp) - julianday(?1)) < (1.0/24.0)
             AND outcome = 'success'
             ORDER BY ABS(julianday(timestamp) - julianday(?1))
             LIMIT 1",
            params![event_time],
            |row| row.get(0),
        )
        .optional()?;

    Ok(activity_id)
}

/// Initialize metrics collector if enabled and workspace is valid
pub fn try_init_metrics_collector(
    workspace_path: Option<&str>,
    db_path: &str,
) -> Option<JoinHandle<()>> {
    // Check if enabled via environment variable
    let interval = check_env_enabled()?;

    // Check if workspace is provided
    let workspace_path = workspace_path?;

    // Check if GitHub CLI is installed
    if !check_gh_cli_installed() {
        log::warn!("⚠️  GitHub CLI (gh) not found, metrics collection disabled");
        log::warn!("   Install with: brew install gh");
        return None;
    }

    // Validate workspace is a GitHub repository
    match validate_github_workspace(workspace_path) {
        Ok((repo_owner, repo_name)) => {
            let config = MetricsConfig {
                workspace_path: workspace_path.to_string(),
                repo_owner,
                repo_name,
                interval_secs: interval,
                db_path: db_path.to_string(),
            };

            let handle = start_metrics_collector(config);
            log::info!("✅ GitHub metrics collection enabled (interval: {}min)", interval / 60);
            Some(handle)
        }
        Err(e) => {
            log::info!("📊 Metrics collection disabled: {e}");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    // ===== check_env_enabled tests =====

    #[test]
    #[serial]
    fn test_check_env_enabled_default() {
        std::env::remove_var("LOOM_GITHUB_METRICS");
        std::env::remove_var("LOOM_METRICS_INTERVAL");
        // Default interval is 900 seconds (15 minutes)
        assert_eq!(check_env_enabled(), Some(900));
    }

    #[test]
    #[serial]
    fn test_check_env_enabled_disabled() {
        std::env::set_var("LOOM_GITHUB_METRICS", "0");
        std::env::remove_var("LOOM_METRICS_INTERVAL");
        assert_eq!(check_env_enabled(), None);
        std::env::remove_var("LOOM_GITHUB_METRICS");
    }

    #[test]
    #[serial]
    fn test_check_env_enabled_custom_interval() {
        std::env::remove_var("LOOM_GITHUB_METRICS");
        std::env::set_var("LOOM_METRICS_INTERVAL", "300");
        assert_eq!(check_env_enabled(), Some(300));
        std::env::remove_var("LOOM_METRICS_INTERVAL");
    }

    #[test]
    #[serial]
    fn test_check_env_enabled_invalid_interval_uses_default() {
        std::env::remove_var("LOOM_GITHUB_METRICS");
        std::env::set_var("LOOM_METRICS_INTERVAL", "not_a_number");
        assert_eq!(check_env_enabled(), Some(900));
        std::env::remove_var("LOOM_METRICS_INTERVAL");
    }

    // ===== MetricsState persistence tests =====

    #[test]
    fn test_metrics_state_default() {
        let state = MetricsState::default();
        assert!(state.pr_sync.is_none());
        assert!(state.issue_sync.is_none());
        assert!(state.commit_sync.is_none());
    }

    #[test]
    fn test_metrics_state_load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let state = MetricsState::load_from_workspace(dir.path().to_str().unwrap()).unwrap();
        assert!(state.pr_sync.is_none());
        assert!(state.issue_sync.is_none());
    }

    #[test]
    fn test_metrics_state_save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_str().unwrap();

        // Create .loom directory
        fs::create_dir_all(dir.path().join(".loom")).unwrap();

        let state = MetricsState {
            pr_sync: Some("2026-01-01T00:00:00Z".to_string()),
            issue_sync: Some("2026-01-02T00:00:00Z".to_string()),
            commit_sync: None,
        };

        state.save_to_workspace(workspace).unwrap();

        let loaded = MetricsState::load_from_workspace(workspace).unwrap();
        assert_eq!(loaded.pr_sync, Some("2026-01-01T00:00:00Z".to_string()));
        assert_eq!(loaded.issue_sync, Some("2026-01-02T00:00:00Z".to_string()));
        assert!(loaded.commit_sync.is_none());
    }

    #[test]
    fn test_metrics_state_load_invalid_json() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("metrics_state.json"), "not json").unwrap();

        let result = MetricsState::load_from_workspace(dir.path().to_str().unwrap());
        assert!(result.is_err());
    }

    // ===== insert_github_event tests =====

    fn create_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r"
            CREATE TABLE github_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                activity_id INTEGER,
                event_type TEXT NOT NULL,
                event_time TEXT NOT NULL,
                pr_number INTEGER,
                issue_number INTEGER,
                commit_sha TEXT,
                author TEXT
            );

            CREATE TABLE agent_activity (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_number INTEGER,
                timestamp DATETIME,
                outcome TEXT
            );
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_insert_github_event_new_event() {
        let conn = create_test_db();
        let inserted = insert_github_event(
            &conn,
            "pr_merged",
            "2026-01-15T10:00:00Z",
            Some(42),
            None,
            None,
            "testuser",
        )
        .unwrap();
        assert!(inserted, "New event should be inserted");
    }

    #[test]
    fn test_insert_github_event_duplicate_detection() {
        let conn = create_test_db();

        // Insert once
        insert_github_event(
            &conn,
            "pr_merged",
            "2026-01-15T10:00:00Z",
            Some(42),
            None,
            None,
            "testuser",
        )
        .unwrap();

        // Insert duplicate
        let inserted = insert_github_event(
            &conn,
            "pr_merged",
            "2026-01-15T10:00:00Z",
            Some(42),
            None,
            None,
            "testuser",
        )
        .unwrap();
        assert!(!inserted, "Duplicate event should not be inserted");
    }

    #[test]
    fn test_insert_github_event_issue_event() {
        let conn = create_test_db();
        let inserted = insert_github_event(
            &conn,
            "issue_closed",
            "2026-01-15T12:00:00Z",
            None,
            Some(100),
            None,
            "contributor",
        )
        .unwrap();
        assert!(inserted);
    }

    #[test]
    fn test_insert_github_event_correlates_with_activity() {
        let conn = create_test_db();

        // Insert an activity record to correlate with
        conn.execute(
            "INSERT INTO agent_activity (issue_number, timestamp, outcome) VALUES (?1, ?2, ?3)",
            params![100, "2026-01-15T12:00:00Z", "success"],
        )
        .unwrap();

        // Insert event for same issue
        let inserted = insert_github_event(
            &conn,
            "issue_closed",
            "2026-01-15T12:05:00Z",
            None,
            Some(100),
            None,
            "testuser",
        )
        .unwrap();
        assert!(inserted);

        // Verify correlation was set
        let activity_id: Option<i64> = conn
            .query_row(
                "SELECT activity_id FROM github_events WHERE issue_number = 100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(activity_id.is_some());
    }
}
