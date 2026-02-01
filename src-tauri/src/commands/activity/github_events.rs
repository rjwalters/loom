use super::db::open_activity_db;
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Log a GitHub event (PR created, issue closed, etc.)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_github_event(
    workspace_path: String,
    event_type: String,
    pr_number: Option<i32>,
    issue_number: Option<i32>,
    commit_sha: Option<String>,
    author: Option<String>,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO github_events (event_type, event_time, pr_number, issue_number, commit_sha, author)
         VALUES (?1, datetime('now'), ?2, ?3, ?4, ?5)",
        params![event_type, pr_number, issue_number, commit_sha, author],
    )
    .map_err(|e| format!("Failed to log GitHub event: {e}"))?;

    Ok(())
}

// ============================================================================
// Prompt-GitHub Correlation (Phase 2: Correlation & Context)
// ============================================================================

/// GitHub event types for prompt correlation
/// These map to specific GitHub CLI operations detected in terminal output
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PromptGitHubEventType {
    /// Issue was claimed (label changed to loom:building)
    IssueClaimed,
    /// New PR was created
    PrCreated,
    /// PR was merged
    PrMerged,
    /// PR was closed without merge
    PrClosed,
    /// Label was added to issue or PR
    LabelAdded,
    /// Label was removed from issue or PR
    LabelRemoved,
    /// Issue was closed
    IssueClosed,
    /// Issue was reopened
    IssueReopened,
    /// PR review was submitted
    PrReviewed,
    /// PR changes were requested
    PrChangesRequested,
    /// PR was approved
    PrApproved,
}

impl std::fmt::Display for PromptGitHubEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::IssueClaimed => "issue_claimed",
            Self::PrCreated => "pr_created",
            Self::PrMerged => "pr_merged",
            Self::PrClosed => "pr_closed",
            Self::LabelAdded => "label_added",
            Self::LabelRemoved => "label_removed",
            Self::IssueClosed => "issue_closed",
            Self::IssueReopened => "issue_reopened",
            Self::PrReviewed => "pr_reviewed",
            Self::PrChangesRequested => "pr_changes_requested",
            Self::PrApproved => "pr_approved",
        };
        write!(f, "{s}")
    }
}

/// Entry for prompt-GitHub correlation
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptGitHubEntry {
    pub id: Option<i64>,
    pub activity_id: i64,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub label_before: Option<String>,
    pub label_after: Option<String>,
    pub event_type: String,
    pub event_time: String,
}

/// Log a prompt-GitHub correlation entry
#[allow(dead_code)]
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_prompt_github(
    workspace_path: String,
    activity_id: i64,
    event_type: String,
    issue_number: Option<i32>,
    pr_number: Option<i32>,
    label_before: Option<String>,
    label_after: Option<String>,
) -> Result<i64, String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO prompt_github (
            activity_id, issue_number, pr_number, label_before, label_after, event_type, event_time
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
        params![
            activity_id,
            issue_number,
            pr_number,
            label_before,
            label_after,
            event_type
        ],
    )
    .map_err(|e| format!("Failed to log prompt-GitHub correlation: {e}"))?;

    Ok(conn.last_insert_rowid())
}

/// Query prompt-GitHub correlations for a specific issue
#[allow(dead_code)]
#[tauri::command]
pub fn get_prompts_for_issue(
    workspace_path: &str,
    issue_number: i32,
) -> Result<Vec<PromptGitHubEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, activity_id, issue_number, pr_number, label_before, label_after, event_type, event_time
             FROM prompt_github
             WHERE issue_number = ?1
             ORDER BY event_time ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map([issue_number], |row| {
            Ok(PromptGitHubEntry {
                id: row.get(0)?,
                activity_id: row.get(1)?,
                issue_number: row.get(2)?,
                pr_number: row.get(3)?,
                label_before: row.get(4)?,
                label_after: row.get(5)?,
                event_type: row.get(6)?,
                event_time: row.get(7)?,
            })
        })
        .map_err(|e| format!("Failed to query prompt-GitHub entries: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect entries: {e}"))?;

    Ok(entries)
}

/// Query prompt-GitHub correlations for a specific PR
#[allow(dead_code)]
#[tauri::command]
pub fn get_prompts_for_pr(
    workspace_path: &str,
    pr_number: i32,
) -> Result<Vec<PromptGitHubEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, activity_id, issue_number, pr_number, label_before, label_after, event_type, event_time
             FROM prompt_github
             WHERE pr_number = ?1
             ORDER BY event_time ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map([pr_number], |row| {
            Ok(PromptGitHubEntry {
                id: row.get(0)?,
                activity_id: row.get(1)?,
                issue_number: row.get(2)?,
                pr_number: row.get(3)?,
                label_before: row.get(4)?,
                label_after: row.get(5)?,
                event_type: row.get(6)?,
                event_time: row.get(7)?,
            })
        })
        .map_err(|e| format!("Failed to query prompt-GitHub entries: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect entries: {e}"))?;

    Ok(entries)
}
