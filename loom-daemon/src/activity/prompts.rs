//! Prompt tracking: git changes and GitHub event correlation.
//!
//! Extracted from `db.rs` — provides recording and querying of prompt-level
//! tracking data including git changes per prompt and GitHub event correlation.

use anyhow::Result;
use rusqlite::{params, Connection};

use super::models::{PromptChanges, PromptGitHubEvent, PromptGitHubEventType};

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

/// Get prompt changes for a specific input.
pub(super) fn get_prompt_changes(
    conn: &Connection,
    input_id: i64,
) -> Result<Option<PromptChanges>> {
    let result = conn.query_row(
        r"
        SELECT id, input_id, before_commit, after_commit, files_changed,
               lines_added, lines_removed, tests_added, tests_modified
        FROM prompt_changes
        WHERE input_id = ?1
        ",
        params![input_id],
        |row| {
            Ok(PromptChanges {
                id: Some(row.get(0)?),
                input_id: row.get(1)?,
                before_commit: row.get(2)?,
                after_commit: row.get(3)?,
                files_changed: row.get(4)?,
                lines_added: row.get(5)?,
                lines_removed: row.get(6)?,
                tests_added: row.get(7)?,
                tests_modified: row.get(8)?,
            })
        },
    );

    match result {
        Ok(changes) => Ok(Some(changes)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Get total lines changed across all prompts for a terminal.
pub(super) fn get_terminal_changes_summary(
    conn: &Connection,
    terminal_id: &str,
) -> Result<(i64, i64, i64)> {
    // Returns (total_files_changed, total_lines_added, total_lines_removed)
    let result = conn.query_row(
        r"
        SELECT
            COALESCE(SUM(pc.files_changed), 0),
            COALESCE(SUM(pc.lines_added), 0),
            COALESCE(SUM(pc.lines_removed), 0)
        FROM prompt_changes pc
        JOIN agent_inputs ai ON pc.input_id = ai.id
        WHERE ai.terminal_id = ?1
        ",
        params![terminal_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    Ok(result)
}

// ========================================================================
// Prompt GitHub Events
// ========================================================================

/// Record a prompt-GitHub correlation event.
///
/// Links a prompt (agent input) with a GitHub action it triggered,
/// such as creating an issue, opening a PR, or changing labels.
pub(super) fn record_prompt_github_event(
    conn: &Connection,
    event: &PromptGitHubEvent,
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

/// Get prompt-GitHub events for a specific input.
pub(super) fn get_prompt_github_events(
    conn: &Connection,
    input_id: i64,
) -> Result<Vec<PromptGitHubEvent>> {
    let mut stmt = conn.prepare(
        r"
        SELECT id, input_id, issue_number, pr_number, label_before, label_after, event_type
        FROM prompt_github
        WHERE input_id = ?1
        ORDER BY id
        ",
    )?;

    let events = stmt.query_map(params![input_id], |row| {
        let id: i64 = row.get(0)?;
        let input_id: Option<i64> = row.get(1)?;
        let issue_number: Option<i32> = row.get(2)?;
        let pr_number: Option<i32> = row.get(3)?;
        let label_before_json: Option<String> = row.get(4)?;
        let label_after_json: Option<String> = row.get(5)?;
        let event_type_str: String = row.get(6)?;

        let label_before = label_before_json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

        let label_after = label_after_json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

        let event_type = PromptGitHubEventType::from_str(&event_type_str).ok_or_else(|| {
            rusqlite::Error::ToSqlConversionFailure(
                format!("Invalid event_type: {event_type_str}").into(),
            )
        })?;

        Ok(PromptGitHubEvent {
            id: Some(id),
            input_id,
            issue_number,
            pr_number,
            label_before,
            label_after,
            event_type,
        })
    })?;

    events.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}
