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
    /// GitHub repository owner (e.g., "loomhq")
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
struct MetricsState {
    /// Last PR sync timestamp (ISO 8601)
    last_pr_sync: Option<String>,
    /// Last issue sync timestamp (ISO 8601)
    last_issue_sync: Option<String>,
    /// Last commit sync timestamp (ISO 8601)
    last_commit_sync: Option<String>,
}

impl MetricsState {
    fn load_from_workspace(workspace_path: &str) -> Result<Self> {
        let state_path = PathBuf::from(workspace_path)
            .join(".loom")
            .join("metrics_state.json");

        if !state_path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&state_path)
            .context("Failed to read metrics state file")?;

        serde_json::from_str(&contents)
            .context("Failed to parse metrics state JSON")
    }

    fn save_to_workspace(&self, workspace_path: &str) -> Result<()> {
        let state_path = PathBuf::from(workspace_path)
            .join(".loom")
            .join("metrics_state.json");

        let contents = serde_json::to_string_pretty(self)
            .context("Failed to serialize metrics state")?;

        fs::write(&state_path, contents)
            .context("Failed to write metrics state file")?;

        Ok(())
    }
}

/// GitHub PR data from gh CLI
#[derive(Debug, Deserialize)]
struct GitHubPR {
    number: i64,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
    author: Author,
}

/// GitHub Issue data from gh CLI
#[derive(Debug, Deserialize)]
struct GitHubIssue {
    number: i64,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    author: Author,
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
/// Returns None if disabled, Some(interval_secs) if enabled
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

    let remote_url = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 in git remote URL")?;

    // Parse owner/repo from various URL formats:
    // - https://github.com/owner/repo.git
    // - git@github.com:owner/repo.git
    let re = regex::Regex::new(r"github\.com[:/]([^/]+)/([^/.]+)")
        .context("Failed to compile regex")?;

    let caps = re.captures(&remote_url)
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
///     repo_owner: "loomhq".to_string(),
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

    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(config.interval_secs));

            match collect_and_store_events(&config) {
                Ok(event_count) => {
                    log::info!("📊 Collected {event_count} GitHub events");
                }
                Err(e) => {
                    log::error!("❌ Metrics collection failed: {e}");
                }
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

    let conn = Connection::open(&config.db_path)
        .context("Failed to open activity database")?;

    let mut total_events = 0;

    // Collect PRs
    match collect_pr_events(config, &conn, state.last_pr_sync.as_deref()) {
        Ok(count) => {
            total_events += count;
            state.last_pr_sync = Some(chrono::Utc::now().to_rfc3339());
        }
        Err(e) => {
            log::error!("Failed to collect PR events: {e}");
        }
    }

    // Collect issues
    match collect_issue_events(config, &conn, state.last_issue_sync.as_deref()) {
        Ok(count) => {
            total_events += count;
            state.last_issue_sync = Some(chrono::Utc::now().to_rfc3339());
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
    match Command::new("gh")
        .args(["api", "rate_limit"])
        .output()
    {
        Ok(output) if output.status.success() => {
            match serde_json::from_slice::<RateLimitResponse>(&output.stdout) {
                Ok(rate_info) => {
                    let remaining = rate_info.resources.core.remaining;
                    if remaining < 100 {
                        log::warn!("⚠️  GitHub API rate limit low: {} remaining", remaining);
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
    // Create longer-lived string values
    let repo_string = format!("{}/{}", config.repo_owner, config.repo_name);
    let search_query = since
        .and_then(|since_time| {
            chrono::DateTime::parse_from_rfc3339(since_time)
                .ok()
                .map(|dt| format!("merged:>{}", dt.format("%Y-%m-%d")))
        });

    let mut args = vec![
        "pr",
        "list",
        "--repo",
        &repo_string,
        "--state",
        "merged",
        "--limit",
        "100",
        "--json",
        "number,mergedAt,author",
    ];

    // Add date filter if we have a search query
    if let Some(ref query) = search_query {
        args.push("--search");
        args.push(query);
    }

    let output = Command::new("gh")
        .args(&args)
        .output()
        .context("Failed to execute gh pr list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh pr list failed: {}", stderr));
    }

    let prs: Vec<GitHubPR> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse PR JSON")?;

    let mut count = 0;
    for pr in prs {
        if let Some(merged_at) = pr.merged_at {
            if insert_github_event(
                conn,
                "pr_merged",
                &merged_at,
                Some(pr.number),
                None,
                None,
                &pr.author.login,
            )? {
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Collect issue close events
fn collect_issue_events(
    config: &MetricsConfig,
    conn: &Connection,
    since: Option<&str>,
) -> Result<usize> {
    // Create longer-lived string values
    let repo_string = format!("{}/{}", config.repo_owner, config.repo_name);
    let search_query = since
        .and_then(|since_time| {
            chrono::DateTime::parse_from_rfc3339(since_time)
                .ok()
                .map(|dt| format!("closed:>{}", dt.format("%Y-%m-%d")))
        });

    let mut args = vec![
        "issue",
        "list",
        "--repo",
        &repo_string,
        "--state",
        "closed",
        "--limit",
        "100",
        "--json",
        "number,closedAt,author",
    ];

    // Add date filter if we have a search query
    if let Some(ref query) = search_query {
        args.push("--search");
        args.push(query);
    }

    let output = Command::new("gh")
        .args(&args)
        .output()
        .context("Failed to execute gh issue list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh issue list failed: {}", stderr));
    }

    let issues: Vec<GitHubIssue> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse issue JSON")?;

    let mut count = 0;
    for issue in issues {
        if let Some(closed_at) = issue.closed_at {
            if insert_github_event(
                conn,
                "issue_closed",
                &closed_at,
                None,
                Some(issue.number),
                None,
                &issue.author.login,
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
        log::debug!("Event already recorded, skipping: {} at {}", event_type, event_time);
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

    log::debug!("Inserted {} event: PR#{:?} Issue#{:?} at {}",
               event_type, pr_number, issue_number, event_time);

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
            log::info!("📊 Metrics collection disabled: {}", e);
            None
        }
    }
}
