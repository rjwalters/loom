//! Issue #14166: a step-dispatched Codex run reaches a `loom.runtime.usage`
//! span with `loom.runtime` set.
//!
//! Drives the real pieces the sweep terminal path chains — the entry's own
//! step log, [`crate::usage_source::sweep_usage_runtime_from_log`], the Codex
//! rollout reader behind [`resolve`], and the usage-span writer — over a
//! fixture `$CODEX_HOME`, so a regression in any hop fails here and not only
//! in a reader unit test.
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use serial_test::serial;

use super::*;
use crate::observability::runtime_usage;
use crate::telemetry::trace::store::TraceStore;

const ISSUE: u32 = 14166;
const SWEEP_ID: &str = "step-builder-14166-1";

/// One fixture rollout in the live Codex shape, attributed to the issue's
/// worktree and timestamped inside the test window.
fn write_rollout(home: &Path, cwd: &Path, model: &str) {
    let created_at = Utc::now() - Duration::minutes(10);
    let created = created_at.to_rfc3339();
    let usage = serde_json::json!({
        "input_tokens": 1_000, "cached_input_tokens": 400,
        "output_tokens": 100, "reasoning_output_tokens": 40, "total_tokens": 1_100,
    });
    let lines = [
        serde_json::json!({"timestamp": created, "type": "session_meta", "payload": {
            "session_id": "s1", "id": "s1", "timestamp": created,
            "cwd": cwd, "originator": "codex_exec", "model_provider": "openai"}}),
        serde_json::json!({"timestamp": created, "type": "turn_context",
            "payload": {"cwd": cwd, "model": model}}),
        serde_json::json!({"timestamp": created, "type": "event_msg", "payload": {
            "type": "token_count", "info": {
                "total_token_usage": usage, "last_token_usage": usage}}}),
    ];
    let dir = home
        .join("sessions")
        .join(created_at.format("%Y/%m/%d").to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let mut file = std::fs::File::create(dir.join("rollout-s1.jsonl")).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn step_log(root: &Path) -> PathBuf {
    let log = root
        .join(".loom")
        .join("logs")
        .join(format!("step-builder-{ISSUE}.log"));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "# LOOM_RUNTIME_RESOLVED runtime=codex\n").unwrap();
    log
}

#[test]
#[serial(codex_home_env)]
fn a_step_dispatched_codex_run_yields_a_codex_usage_span() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    let home = tmp.path().join(".codex");
    let worktree = root
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{ISSUE}"));
    std::fs::create_dir_all(&worktree).unwrap();
    write_rollout(&home, &worktree, "gpt-unpriced-fixture");
    let log = step_log(&root);

    // The measured cause: the per-issue lookup never sees a step log.
    assert_eq!(crate::usage_source::sweep_usage_runtime(None, &root, ISSUE), None);
    let runtime = crate::usage_source::sweep_usage_runtime_from_log(None, Some(&log), &root, ISSUE);
    assert_eq!(runtime.as_deref(), Some("codex"));

    std::env::set_var(crate::codex_usage::CODEX_HOME_ENV, &home);
    let window = window(Some(Utc::now() - Duration::hours(1)), 3_600);
    let usage = resolve(runtime.as_deref(), &root, ISSUE, window, None);
    std::env::remove_var(crate::codex_usage::CODEX_HOME_ENV);
    assert_eq!(usage.status, TokensStatus::Measured, "{usage:?}");
    let rows = usage.tokens_by_model.clone().expect("codex rows read");
    assert_eq!(rows.len(), 1, "{rows:?}");

    TraceStore::new(&root)
        .load_or_create(&root, SWEEP_ID)
        .unwrap();
    let spans = runtime_usage::journal_usage(
        &root,
        SWEEP_ID,
        window.unwrap(),
        Some(&rows),
        runtime.as_deref(),
    )
    .unwrap();
    let [span] = spans.as_slice() else {
        panic!("one model, one usage span: {spans:?}");
    };
    assert_eq!(span.name.as_str(), "loom.runtime.usage");
    let attr = |key: &str| span.attributes.get(key).map(String::as_str);
    assert_eq!(attr("loom.runtime"), Some("codex"));
    assert_eq!(attr("loom.sweep_id"), Some(SWEEP_ID));
    assert_eq!(attr("loom.tokens.input"), Some("600"));
    assert_eq!(attr("loom.tokens.cache_read"), Some("400"));
    assert_eq!(attr("loom.tokens.output"), Some("100"));
    // The fixture rollout carries no `cache_write_input_tokens`: absent, not `0`.
    for key in [
        "loom.tokens.cache_write",
        "loom.tokens.cache_write_5m",
        "loom.tokens.cache_write_1h",
        "gen_ai.usage.cache_creation_input_tokens",
    ] {
        assert_eq!(attr(key), None, "{key}");
    }
    // No launch span declared a class: `unknown`, never a guessed one, and a
    // model off the rate card gets no dollar estimate at all.
    assert_eq!(attr("llm.billing"), Some("unknown"));
    assert_eq!(attr("loom.cost.usd_estimate"), None);
}

#[test]
#[serial(codex_home_env)]
fn a_codex_run_with_no_rollout_yields_no_span() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    let home = tmp.path().join(".codex");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let log = step_log(&root);
    let runtime = crate::usage_source::sweep_usage_runtime_from_log(None, Some(&log), &root, ISSUE);

    std::env::set_var(crate::codex_usage::CODEX_HOME_ENV, &home);
    let usage = resolve(
        runtime.as_deref(),
        &root,
        ISSUE,
        window(Some(Utc::now() - Duration::hours(1)), 3_600),
        None,
    );
    std::env::remove_var(crate::codex_usage::CODEX_HOME_ENV);
    // Unknown is not zero: nothing attributable means no rows, so no span.
    assert!(usage.tokens_by_model.is_none(), "{usage:?}");
    TraceStore::new(&root)
        .load_or_create(&root, SWEEP_ID)
        .unwrap();
    let spans = runtime_usage::journal_usage(
        &root,
        SWEEP_ID,
        (Utc::now() - Duration::hours(1), Utc::now()),
        usage.tokens_by_model.as_deref(),
        runtime.as_deref(),
    )
    .unwrap();
    assert!(spans.is_empty(), "{spans:?}");
}

/// One session row in the live `opencode.db` shape, attributed to `directory`
/// and created inside the test window.
fn write_opencode_db(path: &Path, directory: &Path, model: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
             id TEXT PRIMARY KEY, directory TEXT NOT NULL, model TEXT,
             tokens_input INTEGER DEFAULT 0 NOT NULL,
             tokens_output INTEGER DEFAULT 0 NOT NULL,
             tokens_reasoning INTEGER DEFAULT 0 NOT NULL,
             tokens_cache_read INTEGER DEFAULT 0 NOT NULL,
             tokens_cache_write INTEGER DEFAULT 0 NOT NULL,
             time_created INTEGER NOT NULL);",
    )
    .unwrap();
    let created_ms = (Utc::now() - Duration::minutes(10)).timestamp_millis();
    let model_json =
        serde_json::json!({"id": model, "providerID": "fixture", "variant": "default"}).to_string();
    conn.execute(
        "INSERT INTO session (id, directory, model, tokens_input, tokens_output, \
         tokens_reasoning, tokens_cache_read, tokens_cache_write, time_created) \
         VALUES ('ses_1', ?1, ?2, 700, 80, 20, 300, 50, ?3)",
        rusqlite::params![directory.to_string_lossy(), model_json, created_ms],
    )
    .unwrap();
}

#[test]
#[serial(opencode_db_env)]
fn a_step_dispatched_opencode_run_yields_an_opencode_usage_span() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    let worktree = root
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{ISSUE}"));
    std::fs::create_dir_all(&worktree).unwrap();
    let db = tmp.path().join("opencode.db");
    write_opencode_db(&db, &worktree, "glm-unpriced-fixture");
    // An OpenCode step's own log carries the resolved-runtime marker, exactly
    // like a Codex step's.
    let log = root
        .join(".loom")
        .join("logs")
        .join(format!("step-builder-{ISSUE}.log"));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "# LOOM_RUNTIME_RESOLVED runtime=opencode\n").unwrap();
    let runtime = crate::usage_source::sweep_usage_runtime_from_log(None, Some(&log), &root, ISSUE);
    assert_eq!(runtime.as_deref(), Some("opencode"));

    std::env::set_var(crate::opencode_usage::OPENCODE_DB_ENV, &db);
    let window = window(Some(Utc::now() - Duration::hours(1)), 3_600);
    let usage = resolve(runtime.as_deref(), &root, ISSUE, window, None);
    std::env::remove_var(crate::opencode_usage::OPENCODE_DB_ENV);
    assert_eq!(usage.status, TokensStatus::Measured, "{usage:?}");
    let rows = usage.tokens_by_model.clone().expect("opencode rows read");

    TraceStore::new(&root)
        .load_or_create(&root, SWEEP_ID)
        .unwrap();
    let spans = runtime_usage::journal_usage(
        &root,
        SWEEP_ID,
        window.unwrap(),
        Some(&rows),
        runtime.as_deref(),
    )
    .unwrap();
    let [span] = spans.as_slice() else {
        panic!("one model, one usage span: {spans:?}");
    };
    assert_eq!(span.name.as_str(), "loom.runtime.usage");
    let attr = |key: &str| span.attributes.get(key).map(String::as_str);
    assert_eq!(attr("loom.runtime"), Some("opencode"));
    assert_eq!(attr("loom.sweep_id"), Some(SWEEP_ID));
    assert_eq!(attr("loom.model"), Some("glm-unpriced-fixture"));
    assert_eq!(attr("loom.tokens.input"), Some("700"));
    assert_eq!(attr("loom.tokens.cache_read"), Some("300"));
    // Reasoning tokens are folded into output by the reader.
    assert_eq!(attr("loom.tokens.output"), Some("100"));
    // OpenCode reports a cache-write counter, so it is exported.
    assert_eq!(attr("loom.tokens.cache_write"), Some("50"));
    // No invented dollars for a model off the rate card.
    assert_eq!(attr("llm.billing"), Some("unknown"));
    assert_eq!(attr("loom.cost.usd_estimate"), None);
}

// ---------------------------------------------------------------------------
// Issue #14406: the step's END reaches a `loom.runtime.usage` span.
//
// The tests above prove the usage READERS resolve a step; these drive the
// entry the registry's step end really calls (`emit_for_step`, from
// `step_spawn::capture::emit_step_capture`) and read the span back from the
// step's own trace journal. Before #14406 that call passed no trace, so no
// step-dispatched Builder/Judge/Curator/Doctor ever journalled usage, on any
// runtime, whatever the readers could resolve.
// ---------------------------------------------------------------------------

use crate::role_runner::RoleTickOutcome;
use crate::role_tick_telemetry::{emit_for_step, ResolvedLaunch, StepTelemetry};
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::SpanRecord;

/// The step's own end, as `emit_step_capture` calls it.
fn end_step(root: &Path, role: &str, sweep_id: &str, item: (u32, bool)) {
    emit_for_step(
        root,
        role,
        Utc::now() - Duration::hours(1),
        &RoleTickOutcome::Success,
        Some(ResolvedLaunch::new("fixture-model", "high")),
        StepTelemetry {
            failure: None,
            sweep_id,
            step_outcome: None,
            item,
        },
    );
}

/// Every `loom.runtime.usage` span in the step's own trace journal.
fn usage_spans(root: &Path, sweep_id: &str) -> Vec<SpanRecord> {
    let store = TraceStore::new(root);
    let journal = Journal::for_context(&store.path(root, sweep_id));
    if !journal.path().exists() {
        return Vec::new();
    }
    journal
        .completed()
        .unwrap()
        .into_iter()
        .filter(|span| span.name.as_str() == "loom.runtime.usage")
        .collect()
}

/// One Claude transcript whose first message is `command_args` to `/loom:<role>`
/// and which spent `input`/`output` tokens a minute ago.
fn write_claude_session(
    config: &Path,
    root: &Path,
    file: &str,
    role: &str,
    command_args: &str,
    input: i64,
    output: i64,
) {
    let project = config
        .join("projects")
        .join(crate::transcript_tokens::project_slug(root));
    std::fs::create_dir_all(&project).unwrap();
    let at = (Utc::now() - Duration::minutes(1)).to_rfc3339();
    let head = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\
         \"<command-message>loom:{role}</command-message>\\n\
         <command-name>/loom:{role}</command-name>\\n\
         <command-args>{command_args}</command-args>\"}}}}\n"
    );
    let usage = format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"message\":{{\"id\":\"{file}\",\
         \"model\":\"claude-fixture-5\",\"usage\":{{\"input_tokens\":{input},\
         \"output_tokens\":{output}}}}}}}\n"
    );
    std::fs::write(project.join(file), format!("{head}{usage}")).unwrap();
}

#[test]
#[serial_test::serial]
fn a_step_dispatched_claude_builder_yields_a_usage_span_for_its_own_issue_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let config = tmp.path().join("claude-config");
    // This step's session, and a concurrent sibling Builder step on another
    // issue: same role, same window, so only the item tells them apart.
    write_claude_session(&config, &root, "mine.jsonl", "builder", &ISSUE.to_string(), 900, 120);
    write_claude_session(&config, &root, "sibling.jsonl", "builder", "99999", 5_000, 700);
    TraceStore::new(&root)
        .load_or_create(&root, SWEEP_ID)
        .unwrap();

    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    end_step(&root, "builder", SWEEP_ID, (ISSUE, false));
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let spans = usage_spans(&root, SWEEP_ID);
    let [span] = spans.as_slice() else {
        panic!("one model, one usage span: {spans:?}");
    };
    let attr = |key: &str| span.attributes.get(key).map(String::as_str);
    assert_eq!(attr("loom.role"), Some("builder"));
    // A Claude step names no runtime in its log; the transcript source is
    // Claude's, so the span says so rather than leaving it blank.
    assert_eq!(attr("loom.runtime"), Some("claude"));
    assert_eq!(attr("loom.sweep_id"), Some(SWEEP_ID));
    assert_eq!(attr("loom.model"), Some("claude-fixture-5"));
    assert_eq!(attr("loom.tokens.input"), Some("900"));
    assert_eq!(attr("loom.tokens.output"), Some("120"));
}

#[test]
#[serial_test::serial]
fn a_step_dispatched_pr_role_matches_its_pr_not_a_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let config = tmp.path().join("claude-config");
    let judge_id = "step-judge-14407-1";
    write_claude_session(&config, &root, "mine.jsonl", "judge", "14407", 300, 40);
    write_claude_session(&config, &root, "sibling.jsonl", "judge", "14408", 9_000, 900);
    TraceStore::new(&root)
        .load_or_create(&root, judge_id)
        .unwrap();

    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    end_step(&root, "judge", judge_id, (14407, true));
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let spans = usage_spans(&root, judge_id);
    let [span] = spans.as_slice() else {
        panic!("one model, one usage span: {spans:?}");
    };
    let attr = |key: &str| span.attributes.get(key).map(String::as_str);
    assert_eq!(attr("loom.role"), Some("judge"));
    assert_eq!(attr("loom.tokens.input"), Some("300"));
}

#[test]
#[serial_test::serial(codex_home_env)]
fn a_step_dispatched_codex_builder_yields_a_codex_usage_span_at_its_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    let home = tmp.path().join(".codex");
    let worktree = root
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{ISSUE}"));
    std::fs::create_dir_all(&worktree).unwrap();
    write_rollout(&home, &worktree, "gpt-unpriced-fixture");
    // The step's OWN log names the runtime. The role runner's
    // `role-builder.log`, which the tick reader used to consult, does not exist.
    step_log(&root);
    TraceStore::new(&root)
        .load_or_create(&root, SWEEP_ID)
        .unwrap();

    std::env::set_var(crate::codex_usage::CODEX_HOME_ENV, &home);
    end_step(&root, "builder", SWEEP_ID, (ISSUE, false));
    std::env::remove_var(crate::codex_usage::CODEX_HOME_ENV);

    let spans = usage_spans(&root, SWEEP_ID);
    let [span] = spans.as_slice() else {
        panic!("one model, one usage span: {spans:?}");
    };
    let attr = |key: &str| span.attributes.get(key).map(String::as_str);
    assert_eq!(attr("loom.role"), Some("builder"));
    assert_eq!(attr("loom.runtime"), Some("codex"));
    assert_eq!(attr("loom.model"), Some("gpt-unpriced-fixture"));
    assert_eq!(attr("loom.tokens.input"), Some("600"));
    assert_eq!(attr("loom.tokens.cache_read"), Some("400"));
    assert_eq!(attr("loom.tokens.output"), Some("100"));
}

#[test]
#[serial_test::serial]
fn a_step_with_no_session_or_no_trace_journals_no_usage_span() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let config = tmp.path().join("claude-config");
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);

    // Trace persisted but the session left no transcript: unknown is not zero.
    TraceStore::new(&root)
        .load_or_create(&root, SWEEP_ID)
        .unwrap();
    end_step(&root, "builder", SWEEP_ID, (ISSUE, false));
    assert!(usage_spans(&root, SWEEP_ID).is_empty());

    // A transcript exists but the step has no persisted trace (tracing off):
    // nothing to parent a span to, and no span, no panic.
    write_claude_session(&config, &root, "mine.jsonl", "builder", &ISSUE.to_string(), 10, 1);
    let untraced = "step-builder-14166-2";
    end_step(&root, "builder", untraced, (ISSUE, false));
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    assert!(usage_spans(&root, untraced).is_empty());
}
