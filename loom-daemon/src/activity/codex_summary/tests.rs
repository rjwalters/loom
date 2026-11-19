use super::*;
use crate::observability::queue::DurableQueue;
use crate::observability::session_summary::SessionSummarySink;
use std::io::Write as _;

const SECRET: &str = "sk-codex-PLANTED-SECRET-0000";
const CWD: &str = "/w/.loom/worktrees/issue-11862";

fn line(ts: &str, kind: &str, payload: serde_json::Value) -> String {
    serde_json::json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
}

fn rollout() -> Vec<String> {
    let usage = |input: i64, output: i64| {
        line(
            "2026-09-21T02:10:00.000Z",
            "event_msg",
            serde_json::json!({"type": "token_count", "info": {"total_token_usage": {
                "input_tokens": input, "cached_input_tokens": 100,
                "output_tokens": output, "reasoning_output_tokens": 5, "total_tokens": input + output,
            }}}),
        )
    };
    vec![
        line(
            "2026-09-21T02:06:31.000Z",
            "session_meta",
            serde_json::json!({
                "id": "sess-1", "cwd": CWD, "timestamp": "2026-09-21T02:06:31Z",
                "git": {"branch": "feature/issue-11862"},
                "base_instructions": {"text": SECRET},
            }),
        ),
        line(
            "2026-09-21T02:06:32.000Z",
            "turn_context",
            serde_json::json!({"cwd": CWD, "model": "gpt-5.6-sol"}),
        ),
        line(
            "2026-09-21T02:06:33.000Z",
            "event_msg",
            serde_json::json!({"type": "user_message", "message": format!("use {SECRET}")}),
        ),
        line(
            "2026-09-21T02:07:00.000Z",
            "response_item",
            serde_json::json!({"type": "function_call", "name": "shell", "call_id": "c1",
                "arguments": format!("{{\"cmd\":\"echo {SECRET}\"}}")}),
        ),
        line(
            "2026-09-21T02:07:01.000Z",
            "response_item",
            serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": SECRET}),
        ),
        line(
            "2026-09-21T02:08:00.000Z",
            "response_item",
            serde_json::json!({"type": "function_call", "name": "shell", "call_id": "c2", "arguments": "{}"}),
        ),
        line(
            "2026-09-21T02:09:00.000Z",
            "response_item",
            serde_json::json!({"type": "custom_tool_call", "name": "apply_patch", "input": SECRET}),
        ),
        usage(1100, 40),
        usage(1100, 40),
        "{ this final line is truncated".to_string(),
    ]
}

#[test]
fn a_rollout_becomes_a_codex_summary_with_duration_and_tool_counts() {
    let parsed = parse_rollout(&rollout()).unwrap();
    let context = crate::activity::session_context::SessionContext::derive(&parsed);
    let record = crate::activity::session_summary::build_session_summary_for(
        RUNTIME,
        Path::new("rollout-2026-09-21T02-06-31-sess-1.jsonl"),
        &parsed,
        &context,
    );
    assert_eq!(record.runtime, "codex");
    assert_eq!(record.session_id, "sess-1");
    assert_eq!(record.issue, Some(11862));
    assert_eq!(record.models, vec!["gpt-5.6-sol"]);
    assert_eq!(record.turns, 1);
    // 02:06:31 -> 02:10:00
    assert_eq!(record.wall_ms, 209_000);
    let calls: Vec<(&str, u64)> = record
        .tool_calls
        .iter()
        .map(|c| (c.tool.as_str(), c.count))
        .collect();
    assert_eq!(calls, vec![("apply_patch", 1), ("shell", 2)]);
    // input 1100 - cached 100; the duplicated token_count adds nothing.
    assert_eq!(record.tokens_input, 1000);
    assert_eq!(record.tokens_cache_read, 100);
    assert_eq!(record.tokens_output, 40);
}

#[test]
fn no_prompt_tool_argument_or_output_reaches_the_record() {
    let parsed = parse_rollout(&rollout()).unwrap();
    let context = crate::activity::session_context::SessionContext::derive(&parsed);
    let record = crate::activity::session_summary::build_session_summary_for(
        RUNTIME,
        Path::new("rollout-x.jsonl"),
        &parsed,
        &context,
    );
    let json = serde_json::to_string(&record).unwrap();
    assert!(!json.contains("PLANTED-SECRET"), "secret leaked: {json}");
    assert!(!json.contains("echo"), "tool argument leaked: {json}");
}

#[test]
fn unsummarizable_rollouts_yield_nothing_rather_than_zeros() {
    assert!(parse_rollout(&[]).is_none(), "empty rollout");
    // Meta and tools but no token usage: no fabricated zero-token session.
    let no_usage: Vec<String> = rollout().into_iter().take(7).collect();
    assert!(parse_rollout(&no_usage).is_none());
    // No session_meta: no cwd, so nothing to attribute.
    let no_meta: Vec<String> = rollout().into_iter().skip(1).collect();
    assert!(parse_rollout(&no_meta).is_none());
}

#[test]
fn ingest_emits_once_per_rollout_version_and_reads_nothing_outside_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".codex");
    let day = home.join("sessions/2026/09/21");
    std::fs::create_dir_all(&day).unwrap();
    // Credential-bearing siblings, poisoned: none may be read or leak.
    std::fs::write(home.join("auth.json"), SECRET).unwrap();
    std::fs::write(home.join("history.jsonl"), SECRET).unwrap();
    let mut file =
        std::fs::File::create(day.join("rollout-2026-09-21T02-06-31-sess-1.jsonl")).unwrap();
    for l in rollout() {
        writeln!(file, "{l}").unwrap();
    }
    drop(file);

    let db = ActivityDb::new(tmp.path().join("activity.db")).unwrap();
    let queue_path = tmp.path().join("queue.jsonl");
    let sink = SessionSummarySink::new(
        std::sync::Arc::new(DurableQueue::open(queue_path.clone(), 100)),
        "host-test",
    );
    let opts = IngestOptions {
        summary_sink: Some(sink),
        include_codex: true,
        codex_home_dir: Some(tmp.path().to_path_buf()),
        ..IngestOptions::default()
    };
    let mut stats = IngestStats::default();
    ingest_codex(&db, &opts, &mut stats).unwrap();
    assert_eq!(stats.session_summaries, 1);
    let envelopes = DurableQueue::open(queue_path, 100).peek_batch(50);
    assert_eq!(envelopes.len(), 1);
    let crate::telemetry::TelemetryRecord::SessionSummary(sent) = &envelopes[0].record else {
        panic!("expected a session.summary");
    };
    assert_eq!(sent.runtime, "codex");
    assert!(!serde_json::to_string(sent)
        .unwrap()
        .contains("PLANTED-SECRET"));

    // Unchanged file: the ledger suppresses a duplicate emission.
    ingest_codex(&db, &opts, &mut stats).unwrap();
    assert_eq!(stats.session_summaries, 1);
}

/// A Codex home under `tmp` with one fixture rollout; returns its path.
fn write_fixture_rollout(tmp: &Path) -> std::path::PathBuf {
    let day = tmp.join(".codex/sessions/2026/09/21");
    std::fs::create_dir_all(&day).unwrap();
    let path = day.join("rollout-2026-09-21T02-06-31-sess-1.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    for l in rollout() {
        writeln!(file, "{l}").unwrap();
    }
    path
}

fn summary_opts(tmp: &Path) -> IngestOptions {
    let sink = SessionSummarySink::new(
        std::sync::Arc::new(DurableQueue::open(tmp.join("queue.jsonl"), 100)),
        "host-test",
    );
    IngestOptions {
        summary_sink: Some(sink),
        include_codex: true,
        codex_home_dir: Some(tmp.to_path_buf()),
        ..IngestOptions::default()
    }
}

#[test]
fn an_unchanged_rollout_is_skipped_without_being_read() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_fixture_rollout(tmp.path());
    let db = ActivityDb::new(tmp.path().join("activity.db")).unwrap();
    let opts = summary_opts(tmp.path());

    let mut first = IngestStats::default();
    ingest_codex(&db, &opts, &mut first).unwrap();
    assert_eq!(first.session_summaries, 1);
    assert_eq!(first.skipped_unchanged, 0);

    // Unreadable from here on: a pass that opened it would fail to read it
    // and count it oversize/unreadable. Size and mtime are untouched, so the
    // ledger must skip it on the stat alone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let mut second = IngestStats::default();
    ingest_codex(&db, &opts, &mut second).unwrap();
    assert_eq!(second.skipped_unchanged, 1, "{second:?}");
    assert_eq!(second.skipped_oversize, 0, "never opened: {second:?}");
    assert_eq!(second.session_summaries, 0);
}

#[test]
fn an_unsummarizable_rollout_is_ledgered_and_not_reread() {
    let tmp = tempfile::tempdir().unwrap();
    let day = tmp.path().join(".codex/sessions/2026/09/21");
    std::fs::create_dir_all(&day).unwrap();
    // Meta and tools, no token usage: no summary, but no re-read either.
    let mut file = std::fs::File::create(day.join("rollout-2026-09-21T02-06-31-x.jsonl")).unwrap();
    for l in rollout().into_iter().take(7) {
        writeln!(file, "{l}").unwrap();
    }
    drop(file);
    let db = ActivityDb::new(tmp.path().join("activity.db")).unwrap();
    let opts = summary_opts(tmp.path());
    let mut stats = IngestStats::default();
    ingest_codex(&db, &opts, &mut stats).unwrap();
    assert_eq!(stats.session_summaries, 0);
    ingest_codex(&db, &opts, &mut stats).unwrap();
    assert_eq!(stats.skipped_unchanged, 1, "{stats:?}");
}

#[test]
fn an_oversized_rollout_is_skipped_and_counted_not_loaded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_fixture_rollout(tmp.path());
    let len = std::fs::metadata(&path).unwrap().len();
    let db = ActivityDb::new(tmp.path().join("activity.db")).unwrap();
    let opts = IngestOptions {
        max_transcript_bytes: len - 1,
        ..summary_opts(tmp.path())
    };
    let mut stats = IngestStats::default();
    ingest_codex(&db, &opts, &mut stats).unwrap();
    assert_eq!(stats.skipped_oversize, 1, "{stats:?}");
    assert_eq!(stats.session_summaries, 0);
    // Not ledgered: raising the ceiling later lets it through.
    let mut stats = IngestStats::default();
    ingest_codex(&db, &summary_opts(tmp.path()), &mut stats).unwrap();
    assert_eq!(stats.session_summaries, 1, "{stats:?}");
}

#[test]
fn the_capped_reader_refuses_a_file_over_the_ceiling() {
    // The read itself is bounded too, for a rollout that grows past the
    // ceiling between the stat and the read.
    let tmp = tempfile::tempdir().unwrap();
    let path = write_fixture_rollout(tmp.path());
    let len = std::fs::metadata(&path).unwrap().len();
    let rollouts = crate::codex_usage::rollout_paths(Some(tmp.path()), None);
    assert_eq!(rollouts.len(), 1);
    assert!(rollouts[0].read_capped(len - 1).is_none());
    assert_eq!(rollouts[0].read_capped(len).unwrap().len(), rollout().len());
}
