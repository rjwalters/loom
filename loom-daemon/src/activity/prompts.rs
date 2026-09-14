//! Prompt tracking: git changes and forge event correlation.
//!
//! Extracted from `db.rs` — provides recording of prompt-level tracking data
//! including git changes per prompt and forge event correlation.

use anyhow::Result;
use rusqlite::{params, Connection};

use super::models::{PromptChanges, PromptForgeEvent};

// ========================================================================
// Prompt Changes (Git)
// ========================================================================

/// Record git changes associated with a prompt.
pub(super) fn record_prompt_changes(conn: &Connection, changes: &PromptChanges) -> Result<i64> {
    conn.execute(
        r"
        INSERT INTO prompt_changes (
            input_id, before_commit, after_commit, files_changed,
            lines_added, lines_removed, tests_added, tests_modified
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            changes.input_id,
            &changes.before_commit,
            &changes.after_commit,
            changes.files_changed,
            changes.lines_added,
            changes.lines_removed,
            changes.tests_added,
            changes.tests_modified,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

// ========================================================================
// Prompt GitHub Events
// ========================================================================

/// Record a prompt-GitHub correlation event.
///
/// Links a prompt (agent input) with a GitHub action it triggered,
/// such as creating an issue, opening a PR, or changing labels.
pub(super) fn record_prompt_forge_event(
    conn: &Connection,
    event: &PromptForgeEvent,
) -> Result<i64> {
    let label_before_json = event
        .label_before
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let label_after_json = event
        .label_after
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    conn.execute(
        r"
        INSERT INTO prompt_github (input_id, issue_number, pr_number, label_before, label_after, event_type)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![
            event.input_id,
            event.issue_number,
            event.pr_number,
            label_before_json,
            label_after_json,
            event.event_type.as_str(),
        ],
    )?;

    Ok(conn.last_insert_rowid())
}
