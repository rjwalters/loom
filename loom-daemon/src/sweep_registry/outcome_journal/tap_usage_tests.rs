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
    // …and a line written before #11360 has no phase rows rather than failing.
    assert!(records[0].phase_tap_usage.is_empty());
}

// ============================================================================
// Mixed sweep: out-of-process phase logs (Issue #11360)
// ============================================================================

use crate::tap_usage::fixtures::{self as mixed, CODEX_KEY, GLM_KEY};

/// Write a `worker run` phase log where `worker run` itself puts it.
fn write_phase_log(registry: &SweepRegistry, role: &str, timestamp: i64, body: &str) {
    let path = crate::launch_record::phase_log_path(
        &registry.config().workspace_root,
        role,
        u64::from(mixed::ISSUE),
        timestamp,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// The sweep's per-tap totals as a journal reader computes them: the parent's
/// rows plus the phase rows, each line read once.
fn journal_totals(
    record: &sweep_outcomes::OutcomeRecord,
) -> std::collections::BTreeMap<String, crate::tap_usage::TapUsage> {
    let parent: Vec<&crate::tap_usage::TapAccounting> = if record.tap_usage_all.is_empty() {
        record.tap_usage.iter().collect()
    } else {
        record.tap_usage_all.iter().collect()
    };
    crate::tap_usage::fold_by_tap(
        parent
            .into_iter()
            .chain(record.phase_tap_usage.iter().map(|p| &p.accounting)),
    )
}

/// AC 1 + 2 end to end: a Codex parent with GLM Builder and Doctor phases in
/// their own logs. Both journals keep the parent's row as it was, the phase
/// rows land beside it exactly once, and the journal's per-tap totals equal
/// the hand-computed ones.
#[test]
fn a_mixed_sweeps_phase_logs_reach_the_journal_once_beside_the_parent() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let sweep_id = format!("sweep-issue-{}-0", mixed::ISSUE);
    write_phase_log(&registry, "builder", 1_700_000_100, &mixed::builder_log(&sweep_id));
    write_phase_log(&registry, "doctor", 1_700_000_200, &mixed::doctor_log(&sweep_id));
    insert_dead_running_with_log(
        &mut registry,
        mixed::ISSUE,
        0,
        "unknown",
        &mixed::codex_parent_log(&sweep_id),
    );
    registry.reap_once();

    let record = outcome_for(&registry, mixed::ISSUE);
    let parent = record.tap_usage.as_ref().expect("the parent's own row");
    assert_eq!(parent.key(), CODEX_KEY);
    assert_eq!(parent.usage.input, Some(3000), "never overwritten by a phase");
    assert!(record.tap_usage_all.is_empty(), "the parent region is single-tap");
    assert_eq!(
        record
            .phase_tap_usage
            .iter()
            .map(|p| (p.role.clone(), p.accounting.key()))
            .collect::<Vec<_>>(),
        vec![
            ("builder".to_string(), GLM_KEY.to_string()),
            ("doctor".to_string(), GLM_KEY.to_string())
        ]
    );
    let totals = journal_totals(&record);
    assert_eq!(totals.keys().cloned().collect::<Vec<_>>(), vec![CODEX_KEY, GLM_KEY]);
    mixed::assert_glm_total(&totals[GLM_KEY]);
    assert_eq!(totals[CODEX_KEY].input, Some(3000));

    // The paired telemetry record keeps naming the parent's launch and only
    // flags the phase taps, so `--group-by tap` cannot charge GLM spend to it.
    let raw = std::fs::read_to_string(registry.config().resolve_outcome_telemetry_path())
        .expect("telemetry journal written");
    assert!(raw.contains("\"tap\":\"codex@env\""), "{raw}");
    assert!(raw.contains("\"tap_input_tokens\":\"3000\""), "{raw}");
    assert!(
        raw.contains(&format!("\"tap_phase_keys\":\"builder={GLM_KEY},doctor={GLM_KEY}\"")),
        "{raw}"
    );
    // The journal line round-trips: `read_all` drops any line it cannot parse.
    assert!(outcome_line(&registry, mixed::ISSUE).contains("\"phase_tap_usage\""));

    // Issue #11488: `sweep.outcome` carries the SAME rows (same roles, taps
    // and counters), plus the launch record's model.
    let telemetry_record = sweep_outcomes::read_all_sweep_outcomes(
        &registry.config().resolve_outcome_telemetry_path(),
    )
    .into_iter()
    .find(|r| r.issue == mixed::ISSUE)
    .expect("sweep.outcome journaled");
    assert_eq!(
        telemetry_record.phase_tap_usage,
        crate::telemetry::phase_tap_usage::rows_from(&record.phase_tap_usage)
    );
    assert_eq!(telemetry_record.phase_tap_usage.len(), 2);
    for (row, journal) in telemetry_record
        .phase_tap_usage
        .iter()
        .zip(&record.phase_tap_usage)
    {
        assert_eq!(row.role, journal.role);
        assert_eq!(row.tap, journal.accounting.key());
        assert_eq!(row.input, journal.accounting.usage.input);
        assert_eq!(row.output, journal.accounting.usage.output);
        assert_eq!(row.model.as_deref(), Some("glm-fixture-flash"));
    }
    assert!(raw.contains("\"phase_tap_usage\""), "{raw}");
}

/// AC 3 + 4 end to end: a Claude parent (no tap of its own) whose Builder
/// fell back to Claude (no phase log), a predecessor sweep's Builder log still
/// on disk, and a Doctor that launched on GLM but reported nothing. Only this
/// sweep's Doctor is journaled, as unmeasured, with no counter invented.
#[test]
fn a_fallback_and_resumed_sweep_journals_only_its_own_phases_and_no_invented_counters() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let sweep_id = format!("sweep-issue-{}-1", mixed::ISSUE);
    let predecessor = format!("sweep-issue-{}-0", mixed::ISSUE);
    write_phase_log(&registry, "builder", 1_700_000_100, &mixed::builder_log(&predecessor));
    write_phase_log(
        &registry,
        "doctor",
        1_700_000_300,
        &mixed::phase_log("doctor", &sweep_id, "spawn-worker: exec"),
    );
    insert_dead_running_with_log(
        &mut registry,
        mixed::ISSUE,
        1,
        "agent-2",
        &mixed::claude_parent_log(&sweep_id),
    );
    registry.reap_once();

    let record = outcome_for(&registry, mixed::ISSUE);
    assert_eq!(record.tap_usage, None, "a Claude parent names no tap");
    assert!(record.tap_usage_all.is_empty());
    assert_eq!(record.phase_tap_usage.len(), 1, "the predecessor's Builder is not this sweep's");
    let doctor = &record.phase_tap_usage[0];
    assert_eq!(doctor.role, "doctor");
    assert_eq!(doctor.accounting.key(), GLM_KEY);
    assert!(!doctor.accounting.usage.is_measured());
    assert_eq!(doctor.accounting.usage.total_tokens(), None);

    let raw = std::fs::read_to_string(registry.config().resolve_outcome_telemetry_path())
        .expect("telemetry journal written");
    assert!(raw.contains(&format!("\"tap_phase_keys\":\"doctor={GLM_KEY}\"")), "{raw}");
    // The per-phase rows (#11488) carry their own `tap`; only `config.tap` names the parent's.
    let journaled: serde_json::Value =
        serde_json::from_str(raw.lines().next().expect("journal line")).unwrap();
    assert!(
        journaled["record"]["config"].get("tap").is_none(),
        "no parent tap is fabricated: {raw}"
    );
    assert!(!raw.contains("tap_input_tokens"), "no counter may be invented: {raw}");

    // Issue #11488: the unmeasured row keeps role + tap and writes no counters.
    let telemetry_record = sweep_outcomes::read_all_sweep_outcomes(
        &registry.config().resolve_outcome_telemetry_path(),
    )
    .into_iter()
    .find(|r| r.issue == mixed::ISSUE)
    .expect("sweep.outcome journaled");
    assert_eq!(telemetry_record.phase_tap_usage.len(), 1);
    let row = serde_json::to_value(&telemetry_record.phase_tap_usage[0]).unwrap();
    let keys: Vec<&str> = row
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for counter in [
        "input",
        "output",
        "reasoning",
        "cache_read",
        "cache_write",
        "cost_estimate",
        "usage_events",
    ] {
        assert!(!keys.contains(&counter), "unmeasured row must omit {counter}: {row}");
    }
    assert_eq!(row["role"], "doctor");
    assert_eq!(row["tap"], GLM_KEY);
}
