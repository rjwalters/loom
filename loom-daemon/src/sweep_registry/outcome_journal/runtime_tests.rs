//! End-to-end tests for the runtime/provider/profile attribution and the
//! OpenCode `tokens_by_model` fallback the terminal telemetry journal carries
//! (Issue #8507).
//!
//! In their own sibling module for the same file-size reason `credential_tests`
//! and `timeline_tests` already are.

use super::*;
use crate::sweep_registry::test_support::*;
use std::time::Duration;
use tempfile::tempdir;

/// The dispatch header + `# LOOM_LAUNCH` record a native spawn leaves in its
/// own per-sweep log, verbatim in shape (see `worker_spawn::run`).
fn launch_log(sweep_id: &str, issue: u32, runtime: &str, provider: &str, profile: &str) -> String {
    format!(
        "==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\n\
         spawn-worker: runtime={runtime} (from config (runtimes.default))\n\
         # LOOM_LAUNCH {}\n",
        serde_json::json!({
            "schema": 1,
            "runtime": runtime,
            "provider": provider,
            "model": "zai-org/GLM-5.3",
            "profile": profile,
            "effort": serde_json::Value::Null,
            "credentialSource": "pool",
            "credentialProvider": provider,
            "credentialAccount": "alpha",
        })
    )
}

/// A pi (native, non-opencode) launch's telemetry record carries the
/// launch's own runtime/provider/profile — not the free-form `config` map's
/// dispatch-time `runtime` string, and independent of `tokens_by_model`'s
/// own source.
#[test]
fn journal_entry_carries_runtime_provider_and_profile_from_the_launch_record() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8507;
    let sweep_id = format!("sweep-issue-{issue}-0");

    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "unknown",
        &launch_log(&sweep_id, issue, "pi", "zai-coding-plan", "zai-flash"),
    );
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == issue)
        .expect("journaled");
    assert_eq!(record.runtime.as_deref(), Some("pi"));
    assert_eq!(record.provider.as_deref(), Some("zai-coding-plan"));
    assert_eq!(record.profile.as_deref(), Some("zai-flash"));
}

/// A Claude/legacy-adapter spawn writes no launch record at all — the three
/// fields must then be entirely absent, never a fabricated `"claude"`
/// default. This is the byte-identical-for-Claude-sweeps guarantee.
#[test]
fn a_claude_spawn_with_no_launch_record_leaves_the_three_fields_absent() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8508;
    let sweep_id = format!("sweep-issue-{issue}-0");

    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "unknown",
        &format!("==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\nclean run\n"),
    );
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == issue)
        .expect("journaled");
    assert_eq!(record.runtime, None);
    assert_eq!(record.provider, None);
    assert_eq!(record.profile, None);

    let value = serde_json::to_value(record).unwrap();
    for key in ["runtime", "provider", "profile"] {
        assert!(value.get(key).is_none(), "{key} must be an absent key, not null: {value}");
    }
}

/// AC: an `opencode`-runtime sweep's `tokens_by_model` comes from the
/// OpenCode session-store database, filtered by this workspace's directory
/// and the sweep's own wall-clock window — NOT from the (in this test,
/// nonexistent) Claude JSONL transcripts.
#[test]
#[serial_test::serial(opencode_db_env)]
fn an_opencode_launch_sources_tokens_by_model_from_the_session_db() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8509;
    let sweep_id = format!("sweep-issue-{issue}-0");

    // Seed a fixture opencode.db whose one session matches this workspace's
    // directory and falls inside the sweep's window.
    let db_dir = tempdir().unwrap();
    let db_path = db_dir.path().join("opencode.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER,
                 tokens_reasoning INTEGER, tokens_cache_read INTEGER,
                 tokens_cache_write INTEGER, directory TEXT, time_created INTEGER
             );",
        )
        .unwrap();
        let directory = registry
            .config
            .workspace_root
            .to_string_lossy()
            .into_owned();
        let now_ms = Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO session VALUES (?1, 1000, 200, 0, 50, 10, ?2, ?3)",
            rusqlite::params![
                serde_json::json!({"id": "zai-org/GLM-5.3", "providerID": "friendli"}).to_string(),
                directory,
                now_ms,
            ],
        )
        .unwrap();
    }
    std::env::set_var(crate::opencode_usage::OPENCODE_DB_ENV, &db_path);

    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "unknown",
        &launch_log(&sweep_id, issue, "opencode", "friendli", "glm-flash"),
    );
    registry.entries.get_mut(&sweep_id).unwrap().started_at = Utc::now() - Duration::from_secs(60);
    registry.reap_once();

    std::env::remove_var(crate::opencode_usage::OPENCODE_DB_ENV);

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == issue)
        .expect("journaled");
    assert_eq!(record.runtime.as_deref(), Some("opencode"));
    let rows = record
        .tokens_by_model
        .as_ref()
        .expect("opencode session attributed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "zai-org/GLM-5.3");
    assert_eq!(rows[0].input, 1000);
    assert_eq!(rows[0].output, 200);
}

/// The counterpart: an `opencode` launch with NO matching session-store row
/// yields `tokens_by_model: None` — never a fabricated zero, and never a
/// silent fallback to the (also empty, for a native launch) Claude reader.
#[test]
#[serial_test::serial(opencode_db_env)]
fn an_opencode_launch_with_no_matching_session_omits_tokens_by_model() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8510;
    let sweep_id = format!("sweep-issue-{issue}-0");

    std::env::remove_var(crate::opencode_usage::OPENCODE_DB_ENV);
    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "unknown",
        &launch_log(&sweep_id, issue, "opencode", "friendli", "glm-flash"),
    );
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == issue)
        .expect("journaled");
    assert_eq!(record.tokens_by_model, None);
}
