//! Build the `session.summary` telemetry record from one parsed transcript
//! (Issue #8757, G3 of epic #8714).
//!
//! Pure derivation only — this module reads a
//! [`ParsedTranscript`](super::transcript_parse::ParsedTranscript) plus the
//! transcript's path (for the parent/subagent split) and produces a
//! [`SessionSummaryRecord`]. The ingest pass
//! ([`super::transcript_ingest`]) owns *when* it is emitted (once per
//! transcript that contributed `resource_usage` rows, on each pass that
//! re-reads a changed file) and *where* it goes (the observability
//! [`SessionSummarySink`](crate::observability::session_summary::SessionSummarySink),
//! when one is configured).
//!
//! # Wire safety
//!
//! Every field here is sourced from the parse's counts, ids and names —
//! none of which carry message text, tool arguments or tool output (see
//! `scan_session_shape`'s doc). The unit tests pin that by fixture: a
//! transcript containing a prompt, raw tool output, a key-shaped string and
//! an email address produces a record whose JSON contains none of them.

use std::path::Path;

use crate::activity::transcript_parse::ParsedTranscript;
use crate::telemetry::{RepoVisibility, SessionSummaryRecord, ToolCallCount};

/// Runtime label this pass stamps. It reads Claude Code transcripts only;
/// #8664's per-runtime tails are the future producers for other runtimes.
const RUNTIME: &str = "claude";

/// Derive one transcript's `session.summary` record.
///
/// Session identity follows the same parent/subagent split
/// `transcript_ingest::session_identifier` uses: a `subagents/` file gets
/// its own id (its records' `sessionId` when that is a distinct id, else
/// the file stem) plus the enclosing session's uuid as
/// `parent_session_id`; a parent-session transcript gets its records'
/// `sessionId` (file-stem fallback) and no parent.
#[must_use]
pub fn build_session_summary(path: &Path, parsed: &ParsedTranscript) -> SessionSummaryRecord {
    let is_subagent = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "subagents");
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `<projects>/<slug>/<uuid>/subagents/<agent>.jsonl` — the enclosing
    // session's uuid is the directory above `subagents/`.
    let parent_uuid = is_subagent
        .then(|| {
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
        })
        .flatten();

    let (session_id, parent_session_id) = if is_subagent {
        // A subagent transcript's records may restate the parent's own
        // sessionId; only a distinct id is the subagent's own, otherwise
        // the file stem is the stable identity (the same composite
        // `session_identifier` builds for the ledger).
        let parent = parent_uuid.clone().unwrap_or_default();
        let own = parsed
            .session_id
            .clone()
            .filter(|s| !s.is_empty() && *s != parent)
            .unwrap_or(stem);
        (own, parent_uuid)
    } else {
        (
            parsed
                .session_id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or(stem),
            None,
        )
    };

    let mut models: Vec<String> = parsed.buckets.iter().map(|b| b.model.clone()).collect();
    models.sort();
    models.dedup();

    let sum = |f: fn(&crate::activity::transcript_parse::UsageBucket) -> i64| {
        parsed.buckets.iter().map(f).fold(0i64, i64::saturating_add)
    };

    SessionSummaryRecord {
        // `unknown` matches `cost_by_role`'s convention for an
        // unattributable row — honest rather than a guess.
        repo: parsed.repo.clone().unwrap_or_else(|| "unknown".to_string()),
        // Fail-closed default; see the record's field doc.
        visibility: RepoVisibility::Private,
        session_id,
        parent_session_id,
        runtime: RUNTIME.to_string(),
        role: parsed.role.clone(),
        issue: parsed.issue.and_then(|i| u32::try_from(i).ok()),
        pr_number: None,
        models,
        tokens_input: sum(|b| b.tokens_input),
        tokens_output: sum(|b| b.tokens_output),
        tokens_cache_read: sum(|b| b.tokens_cache_read),
        tokens_cache_write: sum(|b| b.tokens_cache_write),
        wall_ms: parsed
            .first_timestamp
            .zip(parsed.last_timestamp)
            .map_or(0, |(first, last)| (last - first).num_milliseconds().max(0)),
        turns: parsed.turns,
        tool_calls: parsed
            .tool_calls
            .iter()
            .map(|(tool, count)| ToolCallCount {
                tool: tool.clone(),
                count: *count,
            })
            .collect(),
        tool_errors: parsed.tool_errors,
        outcome: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::activity::transcript_parse::parse_transcript;
    use chrono::TimeZone as _;
    use std::io::Write as _;

    const WORKSPACE: &str = "/home/ubuntu/GitHub/loom";

    fn fallback() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).unwrap()
    }

    fn write_transcript(dir: &Path, name: &str, lines: &[String]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        path
    }

    fn user_turn_line(text: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "gitBranch": "main",
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    fn assistant_line(id: &str, model: &str, ts: &str, input: i64, output: i64) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "gitBranch": "main",
            "message": {
                "model": model,
                "id": id,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": 100,
                    "cache_creation_input_tokens": 10,
                },
            },
        })
        .to_string()
    }

    /// An assistant record whose content carries one `tool_use` block — the
    /// shape a real transcript writes between a user turn and the tool's
    /// result.
    fn assistant_tool_line(id: &str, tool: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "gitBranch": "main",
            "message": {
                "model": "claude-sonnet-5",
                "id": id,
                "content": [
                    {"type": "text", "text": "SECRET_ASSISTANT_TEXT"},
                    {"type": "tool_use", "name": tool, "input": {"command": "SECRET_TOOL_ARGUMENT"}},
                ],
                "usage": {"input_tokens": 1, "output_tokens": 2},
            },
        })
        .to_string()
    }

    fn tool_result_line(ts: &str, is_error: bool) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "gitBranch": "main",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": is_error,
                 "content": "SECRET_TOOL_OUTPUT"},
            ]},
        })
        .to_string()
    }

    #[test]
    fn a_parent_transcript_summarizes_its_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line(
                    "<command-name>/loom:sweep</command-name>\n<command-args>8757</command-args>",
                    "2026-09-23T04:00:00Z",
                ),
                assistant_tool_line("msg_1", "Bash", "2026-09-23T04:01:00Z"),
                tool_result_line("2026-09-23T04:01:30Z", false),
                assistant_tool_line("msg_2", "Bash", "2026-09-23T04:02:00Z"),
                tool_result_line("2026-09-23T04:02:30Z", true),
                assistant_line("msg_3", "claude-opus-5", "2026-09-23T04:03:00Z", 10, 20),
                // A streamed repeat of msg_3: same id, restated blocks —
                // must not add tokens or tool calls.
                assistant_line("msg_3", "claude-opus-5", "2026-09-23T04:03:01Z", 10, 20),
            ],
        );

        let parsed = parse_transcript(&path, fallback());
        let record = build_session_summary(&path, &parsed);

        assert_eq!(record.session_id, "uuid-a");
        assert_eq!(record.parent_session_id, None);
        assert_eq!(record.runtime, "claude");
        assert_eq!(record.role.as_deref(), Some("sweep"));
        assert_eq!(record.issue, Some(8757));
        assert_eq!(record.repo, "loom");
        assert_eq!(record.visibility, RepoVisibility::Private);
        assert_eq!(record.models, vec!["claude-opus-5", "claude-sonnet-5"]);
        // msg_1 (1/2) + msg_2 (1/2) + msg_3 (10/20, the streamed repeat
        // folded) — cache fields only on msg_3.
        assert_eq!(record.tokens_input, 12);
        assert_eq!(record.tokens_output, 24);
        assert_eq!(record.tokens_cache_read, 100);
        assert_eq!(record.tokens_cache_write, 10);
        // 04:00:00Z → 04:03:01Z = 181_000 ms (the streamed repeat's own
        // timestamp extends the span, which is honest: the session was
        // still running then).
        assert_eq!(record.wall_ms, 181_000);
        assert_eq!(record.turns, 1, "the slash-command prompt is the one human turn");
        assert_eq!(
            record.tool_calls,
            vec![ToolCallCount {
                tool: "Bash".to_string(),
                count: 2
            },],
            "two Bash tool_use blocks across distinct message ids"
        );
        assert_eq!(record.tool_errors, 1);
        assert_eq!(record.outcome, None);
        // Optional unknowns stay absent, never fabricated.
        assert_eq!(record.pr_number, None);
    }

    #[test]
    fn a_subagent_transcript_carries_its_parent_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let subagents = tmp.path().join("uuid-a").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        // These records restate the parent's own sessionId — only the stem
        // is a distinct subagent identity, so session_id falls back to it.
        let lines = vec![
            user_turn_line("You are the Loom Builder for this repository.", "2026-09-23T04:00:00Z"),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-23T04:05:00Z", 30, 40),
        ];
        let path = write_transcript(&subagents, "agent-1.jsonl", &lines);

        let parsed = parse_transcript(&path, fallback());
        let record = build_session_summary(&path, &parsed);

        assert_eq!(record.session_id, "agent-1");
        assert_eq!(record.parent_session_id.as_deref(), Some("uuid-a"));
        assert_eq!(record.role.as_deref(), Some("builder"));
        assert_eq!(record.wall_ms, 300_000);
    }

    #[test]
    fn a_subagent_transcript_with_its_own_session_id_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let subagents = tmp.path().join("uuid-a").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let mut lines = vec![serde_json::json!({
            "type": "user", "timestamp": "2026-09-23T04:00:00Z",
            "sessionId": "sub-session-9", "cwd": WORKSPACE,
            "message": {"role": "user", "content": "You are the Loom Judge."},
        })
        .to_string()];
        lines.push(assistant_line("msg_1", "claude-sonnet-5", "2026-09-23T04:01:00Z", 1, 1));
        let path = write_transcript(&subagents, "agent-2.jsonl", &lines);

        let parsed = parse_transcript(&path, fallback());
        let record = build_session_summary(&path, &parsed);

        assert_eq!(record.session_id, "sub-session-9");
        assert_eq!(record.parent_session_id.as_deref(), Some("uuid-a"));
    }

    /// Issue #8757's redaction criterion: a transcript containing a prompt,
    /// raw tool output, a key-shaped string and an email produces a record
    /// whose JSON contains none of the four.
    #[test]
    fn no_prompt_tool_output_key_or_email_ever_reaches_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        let secret_key = "sk-ant-api03-SECRETKEYVALUE000000000";
        let email = "agent-secret@example.com";
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line(
                    &format!(
                        "Investigate {email} using credential {secret_key} and read ~/notes.txt"
                    ),
                    "2026-09-23T04:00:00Z",
                ),
                assistant_tool_line("msg_1", "Read", "2026-09-23T04:01:00Z"),
                tool_result_line("2026-09-23T04:01:30Z", false),
                assistant_line("msg_2", "claude-sonnet-5", "2026-09-23T04:02:00Z", 5, 5),
            ],
        );

        let parsed = parse_transcript(&path, fallback());
        let record = build_session_summary(&path, &parsed);

        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("SECRET"), "raw secret material leaked: {json}");
        assert!(!json.contains(email), "email leaked: {json}");
        assert!(!json.contains("notes.txt"), "prompt path leaked: {json}");
        // The record still carries the real shape.
        assert_eq!(record.tool_calls.len(), 1);
        assert_eq!(record.tool_errors, 0);
    }
}
