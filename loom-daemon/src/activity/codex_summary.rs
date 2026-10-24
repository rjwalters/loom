//! `session.summary` for Codex sessions (Issue #11862).
//!
//! The Claude pass ([`super::transcript_ingest`]) summarizes Claude Code
//! transcripts; a Codex run left no session record at all. This module adapts a
//! Codex rollout (`$CODEX_HOME/sessions/**/rollout-*.jsonl`) into the same
//! [`ParsedTranscript`] shape, so [`super::session_summary`] builds the one wire
//! record for both runtimes (a separate Codex-only record type would duplicate
//! the schema).
//!
//! # Wire safety
//!
//! The adapter reads timestamps, ids, the model name, token counters and tool
//! *names* — never a prompt, a tool's arguments or its output. Files are opened
//! only through [`crate::codex_usage::RolloutPath::read_capped`], whose guard
//! refuses everything but `sessions/**/rollout-*.jsonl`.
//!
//! # Memory discipline
//!
//! The same as the Claude pass: each rollout is `stat`ed first, an unchanged
//! one (ledger size+mtime) is skipped without being opened, one over
//! [`IngestOptions::max_transcript_bytes`] is skipped and counted in
//! `skipped_oversize`, and only then is one file read, under that same byte
//! ceiling. At most one rollout is held in memory at a time.
//!
//! # What stays absent
//!
//! A rollout with no `session_meta.cwd`, no usable timestamp or no token
//! usage emits no summary: a record fabricated from nothing would read as a
//! zero-length, zero-token session. The record's `tool_errors` is not
//! derivable from a rollout without reading tool output, so it is left at its
//! schema default rather than guessed.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::db::ActivityDb;
use super::transcript_ingest::{
    emit_session_records, lookup_ledger, upsert_ledger, IngestOptions, IngestStats,
};
use super::transcript_parse::{ParsedTranscript, UsageBucket};
use crate::codex_usage::{fold_rollout, parse_instant, string_field, SessionFilter};

/// Runtime label stamped on Codex records (#8664's `loom.runtime` vocabulary).
const RUNTIME: &str = "codex";

/// Ledger-key prefix keeping Codex rollouts apart from Claude's
/// projects-relative transcript keys.
const LEDGER_PREFIX: &str = "codex:";

/// Adapt one rollout's lines into a [`ParsedTranscript`], or `None` when the
/// rollout cannot honestly be summarized (see the module doc).
#[must_use]
pub fn parse_rollout(lines: &[String]) -> Option<ParsedTranscript> {
    let mut parsed = ParsedTranscript::default();
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(ts) = string_field(&value, "timestamp").and_then(|t| parse_instant(&t)) {
            parsed.first_timestamp = Some(parsed.first_timestamp.map_or(ts, |f| f.min(ts)));
            parsed.last_timestamp = Some(parsed.last_timestamp.map_or(ts, |l| l.max(ts)));
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let kind = payload.get("type").and_then(serde_json::Value::as_str);
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("session_meta") => {
                parsed.cwd = string_field(payload, "cwd");
                parsed.session_id =
                    string_field(payload, "session_id").or_else(|| string_field(payload, "id"));
                parsed.branch = payload.get("git").and_then(|g| string_field(g, "branch"));
            }
            Some("event_msg") if kind == Some("user_message") => parsed.turns += 1,
            Some("response_item") => {
                let tool = match kind {
                    Some("function_call" | "custom_tool_call") => string_field(payload, "name"),
                    Some("local_shell_call") => Some("local_shell".to_string()),
                    Some("web_search_call") => Some("web_search".to_string()),
                    _ => None,
                };
                if let Some(tool) = tool {
                    *parsed.tool_calls.entry(tool.clone()).or_insert(0) += 1;
                    parsed.tool_call_order.push(tool);
                }
            }
            _ => {}
        }
    }

    let cwd = parsed.cwd.clone()?;
    parsed.first_timestamp?;
    // Token rows reuse `codex_usage`'s delta folding, so the summary's counters
    // equal what the usage path attributes. The cwd filter is the rollout's own,
    // so it always matches; the rows are the whole session.
    parsed.buckets = fold_rollout(lines, &SessionFilter::directories(&[cwd.into()]), None)
        .into_iter()
        .map(|row| UsageBucket {
            model: row.model,
            day: row.created_at.format("%Y-%m-%d").to_string(),
            timestamp: row.created_at,
            tokens_input: row.input,
            tokens_output: row.output,
            tokens_cache_read: row.cache_read,
            tokens_cache_write: row.cache_write,
            messages: 0,
        })
        .collect();
    parsed.has_usage().then_some(parsed)
}

/// Summarize every changed Codex rollout in the pass window, once per file
/// version (the same size+mtime ledger the Claude pass uses). Codex rollouts
/// count into the pass's shared `skipped_by_window` / `skipped_unchanged` /
/// `skipped_oversize` counters.
///
/// # Errors
///
/// Propagates ledger SQLite failures.
pub fn ingest_codex(db: &ActivityDb, opts: &IngestOptions, stats: &mut IngestStats) -> Result<()> {
    if opts.summary_sink.is_none() && opts.analysis_sink.is_none() {
        return Ok(());
    }
    let window = opts.since.map(|since| (since, Utc::now()));
    for rollout in crate::codex_usage::rollout_paths(opts.codex_home_dir.as_deref(), window) {
        let path = rollout.path();
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let modified: DateTime<Utc> = meta.modified().map_or_else(|_| Utc::now(), Into::into);
        if opts.since.is_some_and(|since| modified < since) {
            stats.skipped_by_window += 1;
            continue;
        }
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        let mtime = modified.timestamp();
        let key = ledger_key(path);
        let previous = lookup_ledger(db, &key)?;
        if !opts.force
            && previous
                .as_ref()
                .is_some_and(|p| p.file_size == size && p.file_mtime == mtime)
        {
            stats.skipped_unchanged += 1;
            continue;
        }
        if meta.len() > opts.max_transcript_bytes {
            log::warn!(
                "transcript-ingest: skipping oversized Codex rollout {} ({} bytes)",
                path.display(),
                meta.len()
            );
            stats.skipped_oversize += 1;
            continue;
        }
        // Bounded by the same ceiling, so a rollout that grew past it since
        // the stat above is still not loaded whole.
        let Some(lines) = rollout.read_capped(opts.max_transcript_bytes) else {
            log::warn!(
                "transcript-ingest: skipping Codex rollout {} (unreadable or over {} bytes)",
                path.display(),
                opts.max_transcript_bytes
            );
            stats.skipped_oversize += 1;
            continue;
        };
        let parsed = parse_rollout(&lines);
        drop(lines);
        let session = match parsed {
            Some(parsed) => {
                emit_session_records(opts, stats, path, &parsed, RUNTIME);
                parsed.session_id.unwrap_or_else(|| key.clone())
            }
            // Remembered anyway, like the Claude pass's usage-free
            // transcripts, so an unsummarizable rollout is not re-read every
            // pass; it is re-read once it changes.
            None => key.clone(),
        };
        upsert_ledger(&db.conn, &key, &session, None, size, mtime, 0)?;
    }
    Ok(())
}

fn ledger_key(path: &Path) -> String {
    format!(
        "{LEDGER_PREFIX}{}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
