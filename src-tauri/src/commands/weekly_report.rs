//! Weekly Intelligence Report Commands
//!
//! Provides Tauri commands for generating, storing, and retrieving weekly
//! intelligence reports. Reports are stored in the .loom/reports/ directory
//! as JSON files for persistence and easy export.
//!
//! Part of Phase 5 (Loom Intelligence) - Issue #1111

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tauri::command;

// ============================================================================
// Types
// ============================================================================

/// Report schedule configuration
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReportSchedule {
    /// Day of week (0=Sunday, 6=Saturday)
    pub day_of_week: i32,
    /// Hour of day (0-23)
    pub hour_of_day: i32,
    /// Timezone offset in minutes from UTC
    pub timezone_offset: i32,
    /// Whether automatic report generation is enabled
    pub enabled: bool,
}

impl Default for ReportSchedule {
    fn default() -> Self {
        Self {
            day_of_week: 1, // Monday
            hour_of_day: 9, // 9 AM
            timezone_offset: 0,
            enabled: true,
        }
    }
}

/// Report history entry (lightweight summary)
#[derive(Debug, Serialize, Deserialize)]
pub struct ReportHistoryEntry {
    pub id: String,
    pub generated_at: String,
    pub week_start: String,
    pub week_end: String,
    pub summary_features: i32,
    pub summary_cost: f64,
    pub status: String,
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Get the reports directory path, creating it if needed
fn get_reports_dir(workspace_path: &str) -> Result<std::path::PathBuf, String> {
    let reports_dir = Path::new(workspace_path).join(".loom").join("reports");

    if !reports_dir.exists() {
        fs::create_dir_all(&reports_dir)
            .map_err(|e| format!("Failed to create reports directory: {e}"))?;
    }

    Ok(reports_dir)
}

/// Get the schedule file path
fn get_schedule_path(workspace_path: &str) -> std::path::PathBuf {
    Path::new(workspace_path)
        .join(".loom")
        .join("report_schedule.json")
}

/// Get the activity database connection
fn get_db_connection(workspace_path: &str) -> Result<Connection, String> {
    let db_path = Path::new(workspace_path).join(".loom").join("activity.db");

    Connection::open(&db_path).map_err(|e| format!("Failed to open activity database: {e}"))
}

// ============================================================================
// Report Storage Commands
// ============================================================================

/// Save a weekly report to the reports directory
#[command]
pub fn save_weekly_report(workspace_path: String, report_json: String) -> Result<(), String> {
    // Parse the report to get the ID
    let report: serde_json::Value = serde_json::from_str(&report_json)
        .map_err(|e| format!("Failed to parse report JSON: {e}"))?;

    let report_id = report
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("Report missing 'id' field")?;

    let reports_dir = get_reports_dir(&workspace_path)?;
    let report_path = reports_dir.join(format!("{report_id}.json"));

    fs::write(&report_path, &report_json)
        .map_err(|e| format!("Failed to write report file: {e}"))?;

    Ok(())
}

/// Get a specific report by ID
#[command]
pub fn get_weekly_report(
    workspace_path: String,
    report_id: String,
) -> Result<Option<String>, String> {
    let reports_dir = get_reports_dir(&workspace_path)?;
    let report_path = reports_dir.join(format!("{report_id}.json"));

    if !report_path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(&report_path).map_err(|e| format!("Failed to read report file: {e}"))?;

    Ok(Some(content))
}

/// Get the latest weekly report
#[command]
pub fn get_latest_weekly_report(workspace_path: String) -> Result<Option<String>, String> {
    let reports_dir = get_reports_dir(&workspace_path)?;

    // Find the most recently modified report file
    let entries =
        fs::read_dir(&reports_dir).map_err(|e| format!("Failed to read reports directory: {e}"))?;

    let mut latest: Option<(std::fs::Metadata, std::path::PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            if let Ok(metadata) = entry.metadata() {
                if let Ok(modified) = metadata.modified() {
                    if latest.is_none()
                        || latest
                            .as_ref()
                            .map(|(m, _)| m.modified().map(|t| t < modified).unwrap_or(true))
                            .unwrap_or(true)
                    {
                        latest = Some((metadata, path));
                    }
                }
            }
        }
    }

    match latest {
        Some((_, path)) => {
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read report file: {e}"))?;
            Ok(Some(content))
        }
        None => Ok(None),
    }
}

/// Get report history (last N reports)
#[command]
pub fn get_weekly_report_history(
    workspace_path: String,
    limit: i32,
) -> Result<Vec<ReportHistoryEntry>, String> {
    let reports_dir = get_reports_dir(&workspace_path)?;

    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();

    for entry in fs::read_dir(&reports_dir)
        .map_err(|e| format!("Failed to read reports directory: {e}"))?
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            if let Ok(metadata) = entry.metadata() {
                if let Ok(modified) = metadata.modified() {
                    entries.push((modified, path));
                }
            }
        }
    }

    // Sort by modification time (newest first)
    entries.sort_by(|a, b| b.0.cmp(&a.0));

    // Take the requested limit
    let entries: Vec<_> = entries.into_iter().take(limit as usize).collect();

    let mut history = Vec::new();

    for (_, path) in entries {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(report) = serde_json::from_str::<serde_json::Value>(&content) {
                let id = report
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let generated_at = report
                    .get("generated_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let week_start = report
                    .get("week_start")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let week_end = report
                    .get("week_end")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let status = report
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("generated")
                    .to_string();

                // Extract summary data
                let summary_features = report
                    .get("summary")
                    .and_then(|s| s.get("features_completed"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32;

                let summary_cost = report
                    .get("summary")
                    .and_then(|s| s.get("total_cost"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                history.push(ReportHistoryEntry {
                    id,
                    generated_at,
                    week_start,
                    week_end,
                    summary_features,
                    summary_cost,
                    status,
                });
            }
        }
    }

    Ok(history)
}

/// Mark a report as viewed
#[command]
pub fn mark_weekly_report_viewed(workspace_path: String, report_id: String) -> Result<(), String> {
    let reports_dir = get_reports_dir(&workspace_path)?;
    let report_path = reports_dir.join(format!("{report_id}.json"));

    if !report_path.exists() {
        return Err(format!("Report {report_id} not found"));
    }

    let content =
        fs::read_to_string(&report_path).map_err(|e| format!("Failed to read report file: {e}"))?;

    let mut report: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse report JSON: {e}"))?;

    // Update status to viewed
    if let Some(obj) = report.as_object_mut() {
        obj.insert("status".to_string(), serde_json::json!("viewed"));
    }

    let updated_content = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("Failed to serialize report: {e}"))?;

    fs::write(&report_path, updated_content)
        .map_err(|e| format!("Failed to write report file: {e}"))?;

    Ok(())
}

// ============================================================================
// Schedule Commands
// ============================================================================

/// Get the current report schedule
#[command]
pub fn get_weekly_report_schedule(workspace_path: String) -> Result<ReportSchedule, String> {
    let schedule_path = get_schedule_path(&workspace_path);

    if !schedule_path.exists() {
        return Ok(ReportSchedule::default());
    }

    let content = fs::read_to_string(&schedule_path)
        .map_err(|e| format!("Failed to read schedule file: {e}"))?;

    serde_json::from_str(&content).map_err(|e| format!("Failed to parse schedule JSON: {e}"))
}

/// Save the report schedule
#[command]
pub fn save_weekly_report_schedule(
    workspace_path: String,
    schedule_json: String,
) -> Result<(), String> {
    let schedule_path = get_schedule_path(&workspace_path);

    // Validate the JSON
    let _schedule: ReportSchedule =
        serde_json::from_str(&schedule_json).map_err(|e| format!("Invalid schedule JSON: {e}"))?;

    fs::write(&schedule_path, &schedule_json)
        .map_err(|e| format!("Failed to write schedule file: {e}"))?;

    Ok(())
}

// ============================================================================
// Metrics Commands for Reports
// ============================================================================

/// Get metrics for the previous week (for week-over-week comparison)
#[command]
pub fn get_previous_week_metrics(workspace_path: String) -> Result<PreviousWeekMetrics, String> {
    let conn = get_db_connection(&workspace_path)?;

    // Calculate date range for previous week
    let now = chrono::Utc::now();
    let prev_week_end = now - chrono::Duration::days(7);
    let prev_week_start = prev_week_end - chrono::Duration::days(7);

    let start_str = prev_week_start.format("%Y-%m-%d").to_string();
    let end_str = prev_week_end.format("%Y-%m-%d").to_string();

    // Query activity data for previous week
    let prompt_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_activity
             WHERE timestamp >= ?1 AND timestamp < ?2",
            params![start_str, end_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let issues_closed: i32 = conn
        .query_row(
            "SELECT COUNT(DISTINCT issue_number) FROM agent_activity
             WHERE timestamp >= ?1 AND timestamp < ?2
             AND outcome = 'success'
             AND issue_number IS NOT NULL",
            params![start_str, end_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let prs_created: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM github_events
             WHERE event_type = 'pr_merged'
             AND event_time >= ?1 AND event_time < ?2",
            params![start_str, end_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Get token usage
    let total_tokens: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(t.total_tokens), 0) FROM token_usage t
             JOIN agent_activity a ON t.activity_id = a.id
             WHERE a.timestamp >= ?1 AND a.timestamp < ?2",
            params![start_str, end_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Calculate cost (approximate: $0.00001 per token average)
    let total_cost = (total_tokens as f64) * 0.00001;

    // Calculate success rate
    let total_outcomes: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_activity
             WHERE timestamp >= ?1 AND timestamp < ?2
             AND outcome != 'no_work'",
            params![start_str, end_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let successful_outcomes: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_activity
             WHERE timestamp >= ?1 AND timestamp < ?2
             AND outcome = 'success'",
            params![start_str, end_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let success_rate = if total_outcomes > 0 {
        (successful_outcomes as f64) / (total_outcomes as f64)
    } else {
        0.0
    };

    Ok(PreviousWeekMetrics {
        prompt_count,
        total_tokens,
        total_cost,
        success_rate,
        prs_created,
        issues_closed,
    })
}

/// Previous week metrics structure
#[derive(Debug, Serialize, Deserialize)]
pub struct PreviousWeekMetrics {
    pub prompt_count: i32,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub success_rate: f64,
    pub prs_created: i32,
    pub issues_closed: i32,
}

/// Get count of PRs stuck in review beyond threshold
#[command]
pub fn get_stuck_pr_count(workspace_path: String, hours_threshold: i32) -> Result<i32, String> {
    let conn = get_db_connection(&workspace_path)?;

    let threshold_time = chrono::Utc::now() - chrono::Duration::hours(i64::from(hours_threshold));
    let threshold_str = threshold_time.format("%Y-%m-%dT%H:%M:%S").to_string();

    // Count PRs that were opened but not merged/closed within threshold
    // This is a simplified check - real implementation would use GitHub API
    let count: i32 = conn
        .query_row(
            "SELECT COUNT(DISTINCT pg.pr_number) FROM prompt_github pg
             LEFT JOIN github_events ge ON ge.pr_number = pg.pr_number AND ge.event_type = 'pr_merged'
             WHERE pg.pr_number IS NOT NULL
             AND pg.event_time < ?1
             AND ge.id IS NULL",
            params![threshold_str],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(count)
}
