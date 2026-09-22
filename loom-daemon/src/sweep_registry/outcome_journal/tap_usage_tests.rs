//! End-to-end tests for the tap-attributed usage accounting both terminal
//! journals carry (Issue #8556).
//!
//! Sibling module rather than an append to `tests`/`credential_tests`, for the
//! same file-size-ratchet reason those two exist
//! (`scripts/check-file-size-budget.sh`).
//!
//! The property under test is the one #8556 turns into a requirement: a
//! terminal transition must leave behind *which tap paid* and *what its own
//! stream said it consumed*, without either journal being able to disagree with
//! the other and without a missing counter ever being written down as a zero.

use super::*;
use crate::sweep_registry::test_support::*;
use tempfile::tempdir;

/// A native-harness log exactly as `worker_spawn::run` writes it post-#8556:
/// the dispatch header, a `# LOOM_LAUNCH` record carrying `tap`, then the
/// harness's own native event stream.
fn metered_log(sweep_id: &str, issue: u32, events: &str) -> String {
    format!(
        "==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\n\
         spawn-worker: runtime=opencode (from config (runtimes.default))\n\
         # LOOM_LAUNCH {}\n{events}",
        serde_json::json!({
            "schema": 1,
            "runtime": "opencode",
            "tap": "opencode:zai-metered",
            "provider": "zai",
            "model": "glm-5.3",
            "profile": "zai-metered",
            "credentialSource": "pool",
            "credentialProvider": "zai",
            "credentialAccount": "alpha",
            "usage": "native-json-events",
            "billing": "not-measured",
        })
    )
}

fn outcome_for(registry: &SweepRegistry, issue: u32) -> sweep_outcomes::OutcomeRecord {
    sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path())
        .into_iter()
        .find(|r| r.issue == issue)
        .expect("terminal outcome must be journaled")
}

/// AC: the journal answers "how much went to the metered backstop" by key,
/// not by reconstruction — and the counters are the ones the stream reported.
#[test]
fn a_metered_native_spawn_journals_its_tap_and_what_its_stream_reported() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8556-0";
    let events = "{\"type\":\"step_finish\",\"tokens\":{\"input\":100,\"output\":20,\"cache\":{\"read\":900}},\"cost\":0.01}\n\
                  {\"type\":\"step_finish\",\"tokens\":{\"input\":50,\"output\":7},\"cost\":0.02}\n";
    insert_dead_running_with_log(
        &mut registry,
        8556,
        0,
        "unknown",
        &metered_log(sweep_id, 8556, events),
    );
    registry.reap_once();

    let accounting = outcome_for(&registry, 8556)
        .tap_usage
        .expect("a native spawn's journal entry must carry its tap accounting");
    assert_eq!(accounting.key(), "opencode:zai-metered@api_keys:zai");
    assert_eq!(accounting.usage.input, Some(150));
    assert_eq!(accounting.usage.output, Some(27));
    assert_eq!(accounting.usage.cache_read, Some(900));
    assert_eq!(accounting.usage.usage_events, 2);
    // A counter no event reported stays absent rather than becoming zero.
    assert_eq!(accounting.usage.cache_write, None);

    // The #8447 credential attribution is a strict subset of the same reading,
    // resolved from the same single log read — the two cannot disagree.
    let credential = outcome_for(&registry, 8556)
        .credential
        .expect("credential attribution");
    assert_eq!(credential, accounting.tap.credential);

    // The paired telemetry journal carries the same reading in its free-form
    // `config` map, which is what `--group-by tap` folds on.
    let raw = std::fs::read_to_string(registry.config().resolve_outcome_telemetry_path())
        .expect("telemetry journal written");
    assert!(raw.contains("\"tap\":\"opencode:zai-metered@api_keys:zai\""), "{raw}");
    assert!(raw.contains("\"tap_input_tokens\":\"150\""), "{raw}");
    assert!(raw.contains("tap_cost_estimate"), "{raw}");
    // Never a zero for something the harness did not report.
    assert!(!raw.contains("tap_cache_write_tokens"), "{raw}");
}

/// "This tap ran and we cannot see what it cost" must stay distinguishable
/// from "this tap ran for free" — the distinction a spend-governance reader
/// depends on.
#[test]
fn a_launch_whose_stream_reported_nothing_is_journaled_as_unmeasured_not_zero() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8557-0";
    insert_dead_running_with_log(
        &mut registry,
        8557,
        0,
        "unknown",
        &metered_log(sweep_id, 8557, "spawn-worker: exec\n"),
    );
    registry.reap_once();

    let accounting = outcome_for(&registry, 8557)
        .tap_usage
        .expect("tap row present");
    assert_eq!(accounting.key(), "opencode:zai-metered@api_keys:zai");
    assert!(!accounting.usage.is_measured());
    assert_eq!(accounting.usage.total_tokens(), None);

    let raw = std::fs::read_to_string(registry.config().resolve_outcome_telemetry_path())
        .expect("telemetry journal written");
    assert!(raw.contains("\"tap\":\"opencode:zai-metered@api_keys:zai\""), "{raw}");
    assert!(!raw.contains("tap_input_tokens"), "no counter may be invented: {raw}");
    assert!(!raw.contains("tap_usage_events"), "{raw}");
}

/// A Claude / legacy-adapter spawn writes no launch record, so there is no tap
/// to name — absent, never fabricated.
#[test]
fn a_spawn_with_no_launch_record_journals_no_tap_accounting() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    insert_dead_running_with_log(
        &mut registry,
        8558,
        0,
        "agent-2",
        "==== loom-daemon dispatch: sweep_id=sweep-issue-8558-0 issue=8558 ====\nclean run\n",
    );
    registry.reap_once();

    let record = outcome_for(&registry, 8558);
    assert_eq!(record.tap_usage, None);
    assert_eq!(record.credential, None);
    assert_eq!(record.token_name, "agent-2");
}

/// `read_all` drops any line it cannot deserialize, so a journal line written
/// before this field existed must still parse.
#[test]
fn a_pre_8556_journal_line_without_the_field_still_parses() {
    let line = serde_json::json!({
        "timestamp": "2026-09-01T00:00:00Z",
        "repo": "/tmp/repo",
        "issue": 1,
        "sweep_id": "sweep-issue-1-0",
        "outcome": "exited",
        "token_name": "agent-1",
        "duration_sec": 42,
    })
    .to_string();
    let dir = tempdir().unwrap();
    let path = dir.path().join("sweep-outcomes.jsonl");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let records = sweep_outcomes::read_all(&path);
    assert_eq!(records.len(), 1, "a pre-#8556 line must survive");
    assert_eq!(records[0].tap_usage, None);
}
