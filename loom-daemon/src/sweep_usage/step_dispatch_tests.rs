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
