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

/// One `# LOOM_LAUNCH` record line for `tap`, as the child writes it.
fn launch_line(tap: &str, source: &str, provider: Option<&str>) -> String {
    let mut record = serde_json::json!({
        "schema": 1,
        "tap": tap,
        "runtime": tap.split(':').next().unwrap(),
        "model": "glm-5.3",
        "credentialSource": source,
        "credentialAccount": "alpha",
    });
    if let Some(provider) = provider {
        record
            .as_object_mut()
            .unwrap()
            .insert("credentialProvider".to_string(), provider.into());
    }
    format!("# LOOM_LAUNCH {record}")
}

/// A native-harness log whose anchored region holds SEVERAL launch records —
/// the multi-record shape #8633 documents (a re-dispatch inside one sweep, a
/// containment re-exec, or an orchestrated sweep whose phases pin their own
/// runtime) and whose non-final launches #8659 is about.
fn region_log(sweep_id: &str, issue: u32, body: &str) -> String {
    format!(
        "==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\n\
         spawn-worker: runtime=opencode (from config (runtimes.default))\n{body}\n"
    )
}

fn outcome_line(registry: &SweepRegistry, issue: u32) -> String {
    std::fs::read_to_string(registry.config().resolve_outcomes_journal_path())
        .expect("outcomes journal written")
        .lines()
        .find(|line| line.contains(&format!("\"issue\":{issue}")))
        .expect("this issue's journal line")
        .to_string()
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

/// AC (Issue #8659): a region holding two launch records on **different** taps
/// journals both. The outcome's row still names the launch the outcome belongs
/// to (the region's last record, with only its own tap's usage — #8633), and
/// the earlier metered launch is recorded beside it instead of vanishing.
#[test]
fn a_multi_tap_regions_earlier_launch_is_journaled_instead_of_dropped() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8659-0";
    let log = region_log(
        sweep_id,
        8659,
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000,\"output\":800},\"cost\":0.25}",
            launch_line("codex", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}"
        ),
    );
    insert_dead_running_with_log(&mut registry, 8659, 0, "unknown", &log);
    registry.reap_once();

    let record = outcome_for(&registry, 8659);
    let outcome = record.tap_usage.expect("the outcome's own tap row");
    assert_eq!(outcome.key(), "codex@env");
    assert_eq!(
        outcome.usage.input,
        Some(7),
        "the earlier launch's 9000 tokens are the metered tap's, not codex's (#8633)"
    );
    assert_eq!(outcome.usage.cost_estimate, None);

    // …and the region's non-final launch is now visible to a spend reader.
    assert_eq!(
        record
            .tap_usage_all
            .iter()
            .map(crate::tap_usage::TapAccounting::key)
            .collect::<Vec<_>>(),
        vec![
            "codex@env".to_string(),
            "opencode:zai-metered@api_keys:zai".to_string()
        ],
        "outcome's tap first, then the rest in order of first appearance"
    );
    let metered = &record.tap_usage_all[1];
    assert_eq!(metered.usage.input, Some(9000));
    assert_eq!(metered.usage.output, Some(800));
    assert!((metered.usage.cost_estimate.unwrap() - 0.25).abs() < 1e-9);

    // The credential attribution (#8447) still names the launch the outcome
    // belongs to — the fold must not move it to a neighbouring tap's account.
    let credential = record.credential.expect("credential attribution");
    assert_eq!(credential.source, "env");
    assert_eq!(credential.provider, None);

    // The paired telemetry record keeps ONE tap (its `config` is flat strings
    // and `--group-by tap` is one-record-one-bucket) but flags the region so a
    // telemetry-only reader cannot mistake that tap's counters for the total.
    let raw = std::fs::read_to_string(registry.config().resolve_outcome_telemetry_path())
        .expect("telemetry journal written");
    assert!(raw.contains("\"tap\":\"codex@env\""), "{raw}");
    assert!(raw.contains("\"tap_input_tokens\":\"7\""), "{raw}");
    assert!(
        raw.contains("\"tap_region_keys\":\"codex@env,opencode:zai-metered@api_keys:zai\""),
        "{raw}"
    );
}

/// The multi-record shape that is *not* multi-tap — a re-dispatch or a
/// containment re-exec re-announcing the same tap — is where the fold pays off
/// outright: one row now carries the whole region instead of only the last
/// block, and the line keeps its pre-#8659 key set.
#[test]
fn a_same_tap_re_dispatch_journals_the_whole_regions_usage_in_one_row() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8660-0";
    let log = region_log(
        sweep_id,
        8660,
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000},\"cost\":0.25}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":7}}"
        ),
    );
    insert_dead_running_with_log(&mut registry, 8660, 0, "unknown", &log);
    registry.reap_once();

    let record = outcome_for(&registry, 8660);
    let outcome = record.tap_usage.expect("tap row");
    assert_eq!(outcome.key(), "opencode:zai-metered@api_keys:zai");
    assert_eq!(
        outcome.usage.input,
        Some(9007),
        "both blocks are the same tap's spend, so the journaled row must carry both"
    );
    assert_eq!(outcome.usage.usage_events, 2);
    assert!(record.tap_usage_all.is_empty(), "one tap ⇒ nothing to break out");
    // A single-tap line is byte-identical in shape to a pre-#8659 one: neither
    // the journal's new field nor the telemetry flag appears at all.
    let line = outcome_line(&registry, 8660);
    assert!(!line.contains("tap_usage_all"), "{line}");
    let raw = std::fs::read_to_string(registry.config().resolve_outcome_telemetry_path())
        .expect("telemetry journal written");
    assert!(raw.contains("\"tap_input_tokens\":\"9007\""), "{raw}");
    assert!(!raw.contains("tap_region_keys"), "{raw}");
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
    // …and must keep surviving as the tap fields grow: #8659's `tap_usage_all`
    // defaults to empty rather than failing the whole line's deserialization.
    assert!(records[0].tap_usage_all.is_empty());
}

/// The same guarantee one field later: a line written between #8556 and #8659
/// carries `tap_usage` but no `tap_usage_all`, and must parse with the single
/// row intact rather than being dropped by `read_all`.
#[test]
fn a_pre_8659_journal_line_with_only_the_single_tap_row_still_parses() {
    let line = serde_json::json!({
        "timestamp": "2026-09-22T00:00:00Z",
        "repo": "/tmp/repo",
        "issue": 2,
        "sweep_id": "sweep-issue-2-0",
        "outcome": "exited",
        "token_name": "agent-1",
        "tap_usage": {
            "tap": {
                "runtime": "opencode",
                "model_profile": "zai-metered",
                "credential": {"source": "pool", "provider": "zai", "account": "alpha"},
            },
            "usage": {"input": 150, "usage_events": 2},
        },
        "duration_sec": 42,
    })
    .to_string();
    let dir = tempdir().unwrap();
    let path = dir.path().join("sweep-outcomes.jsonl");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let records = sweep_outcomes::read_all(&path);
    assert_eq!(records.len(), 1, "a pre-#8659 line must survive");
    let accounting = records[0].tap_usage.as_ref().expect("single row preserved");
    assert_eq!(accounting.key(), "opencode:zai-metered@api_keys:zai");
    assert_eq!(accounting.usage.input, Some(150));
    assert!(
        records[0].tap_usage_all.is_empty(),
        "an absent breakdown means the single row is the whole region"
    );
}
