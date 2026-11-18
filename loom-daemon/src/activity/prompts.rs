//! Prompt tracking: forge event correlation.
//!
//! Extracted from `db.rs` — provides recording of prompt-level tracking data
//! including forge event correlation.

use anyhow::Result;
use rusqlite::{params, Connection};

use super::models::PromptForgeEvent;

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
