//! End-to-end tests for per-phase token attribution on `sweep.outcome` (Issue
//! #9443): each `phase_durations` entry carries its own attempt index and token
//! usage, and `tokens_unattributed` reports the remainder so the per-phase
//! breakdown is a partition of the sweep total rather than a second,
//! unreconciled measurement.
//!
//! In their own sibling module for the same file-size reason `runtime_tests`,
//! `credential_tests` and `timeline_tests` already are.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;

/// A usage record with a distinct `message.id` (so the fold counts it as its own
/// message) and a top-level `timestamp` — the instant per-phase attribution
/// partitions on.
fn stamped(id: &str, at: DateTime<Utc>, input: i64, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"id\":\"{id}\",\
         \"model\":\"claude-sonnet-5\",\"usage\":{{\"input_tokens\":{input},\
         \"output_tokens\":{output},\"cache_read_input_tokens\":0,\
         \"cache_creation_input_tokens\":0}}}}}}\n",
        at.to_rfc3339()
    )
}

/// The same record with no `timestamp` — counted in the sweep totals,
/// attributable to no phase.
fn unstamped(id: &str, input: i64, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"message\":{{\"id\":\"{id}\",\
         \"model\":\"claude-sonnet-5\",\"usage\":{{\"input_tokens\":{input},\
         \"output_tokens\":{output},\"cache_read_input_tokens\":0,\
         \"cache_creation_input_tokens\":0}}}}}}\n"
    )
}

/// Point `CLAUDE_CONFIG_DIR` at a fresh tree and write one `/loom:sweep <issue>`
/// session transcript holding `lines`.
fn seed_sweep_transcript(
    root: &Path,
    registry: &SweepRegistry,
    issue: u32,
    lines: &str,
) -> PathBuf {
    let config_dir = root.join("claude-config");
    std::fs::create_dir_all(config_dir.join("projects")).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
    let project = config_dir
        .join("projects")
        .join(crate::transcript_tokens::project_slug(&registry.config.workspace_root));
    std::fs::create_dir_all(&project).unwrap();
    let head = format!(
        "{{\"type\":\"user\",\"message\":{{\"content\":\
         \"<command-name>/loom:sweep</command-name>\\n\
         <command-args>{issue} --claim-owned {issue}</command-args>\"}}}}\n"
    );
    std::fs::write(project.join("uuid-1.jsonl"), format!("{head}{lines}")).unwrap();
    config_dir
}

/// Overwrite the sampled transition history with observations at exact instants,
/// so a test can place transcript records inside known phase windows. The reaper
/// stamps `Utc::now()` on a real observation, which is not controllable from a
/// test — the history itself is the honest seam.
fn seed_phase_history(
    registry: &mut SweepRegistry,
    sweep_id: &str,
    marks: &[(&str, DateTime<Utc>)],
) {
    registry.phase_history.insert(
        sweep_id.to_string(),
        marks
            .iter()
            .map(|(phase, at)| PhaseObservation {
                phase: (*phase).to_string(),
                at: *at,
                pr_number: None,
                jev: None,
            })
            .collect(),
    );
}

fn outcome_for(registry: &SweepRegistry, issue: u32) -> telemetry::SweepOutcomeRecord {
    let path = registry.config().resolve_outcome_telemetry_path();
    sweep_outcomes::read_all_sweep_outcomes(&path)
        .into_iter()
        .find(|r| r.issue == issue)
        .expect("terminal transition must journal a sweep.outcome record")
}

/// The raw JSON of the `sweep.outcome` line — the only way to tell an **omitted**
/// optional field from a `null`/`0` one, which is what "unknown != zero" turns
/// on.
fn raw_outcome(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).expect("telemetry journal must exist");
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["record"]["kind"] == "sweep.outcome" && v["record"]["issue"] == issue)
        .expect("a sweep.outcome line for this issue")["record"]
        .clone()
}

/// AC: a `curator → builder → judge(fail) → doctor → judge(pass)` sweep emits
/// five phase entries, each with usage, and
/// `Σ phase tokens + tokens_unattributed == sweep tokens` on both axes.
#[test]
#[serial]
fn a_five_phase_lifecycle_emits_five_attributed_entries_that_sum_to_the_sweep_total() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9443;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    let mark = |secs: i64| started_at + chrono::Duration::seconds(secs);
    seed_phase_history(
        &mut registry,
        &sweep_id,
        &[
            ("curator-done", mark(100)),
            ("builder-done", mark(200)),
            // The Judge↔Doctor cycle: two `judge` entries, distinguished by
            // `attempt`, never one collapsed total.
            ("judge-rejected", mark(300)),
            ("doctor-done", mark(400)),
            ("judge-done", mark(500)),
        ],
    );
    let lines = [
        stamped("curator", mark(50), 10, 1),
        stamped("builder", mark(150), 100, 10),
        stamped("judge-1", mark(250), 20, 2),
        stamped("doctor", mark(350), 200, 20),
        stamped("judge-2", mark(450), 30, 3),
        // The trailing in-flight segment (past the last observation) and a
        // record with no instant: real spend, attributable to no phase.
        stamped("trailing", mark(550), 7, 5),
        unstamped("no-clock", 3, 1),
    ]
    .concat();
    seed_sweep_transcript(dir.path(), &registry, issue, &lines);

    registry.reap_once();
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let record = outcome_for(&registry, issue);
    assert_eq!(
        record
            .phase_durations
            .iter()
            .map(|p| (p.phase.as_str(), p.attempt))
            .collect::<Vec<_>>(),
        vec![
            ("curator", Some(1)),
            ("builder", Some(1)),
            ("judge", Some(1)),
            ("doctor", Some(1)),
            ("judge", Some(2)),
        ],
        "five entries, with judge attempts 1 and 2 separately addressable"
    );
    assert_eq!(
        record
            .phase_durations
            .iter()
            .map(|p| p.token_split())
            .collect::<Vec<_>>(),
        vec![
            Some((10, 1)),
            Some((100, 10)),
            Some((20, 2)),
            Some((200, 20)),
            Some((30, 3)),
        ],
        "each phase attempt carries its own usage"
    );
    for entry in &record.phase_durations {
        let rows = entry
            .tokens_by_model
            .as_ref()
            .expect("an attributed phase also carries its per-model breakdown");
        assert_eq!(
            rows.iter().map(|r| r.model.as_str()).collect::<Vec<_>>(),
            vec!["claude-sonnet-5"]
        );
    }

    // #9430's clean-landing cost: curator + builder + FIRST judge, which is only
    // computable because the second judge is a separate entry.
    let clean_landing: u64 = record
        .phase_durations
        .iter()
        .filter(|p| matches!(p.phase.as_str(), "curator" | "builder") || p.attempt == Some(1))
        .filter(|p| p.phase != "doctor")
        .filter_map(|p| p.tokens_in)
        .sum();
    assert_eq!(clean_landing, 130, "10 curator + 100 builder + 20 first judge");

    // The reconciliation invariant, on both axes.
    let (attributed_in, attributed_out) = record
        .phase_durations
        .iter()
        .filter_map(telemetry::PhaseDuration::token_split)
        .fold((0u64, 0u64), |(a, b), (i, o)| (a + i, b + o));
    let remainder = record
        .tokens_unattributed
        .expect("known sweep totals must carry an explicit remainder");
    assert_eq!(
        (attributed_in + remainder.tokens_in, attributed_out + remainder.tokens_out),
        (record.tokens_in.unwrap(), record.tokens_out.unwrap()),
        "Σ phase tokens + tokens_unattributed == sweep tokens"
    );
    assert_eq!(
        (remainder.tokens_in, remainder.tokens_out),
        (10, 6),
        "the remainder is exactly the trailing (7,5) and clock-less (3,1) records"
    );
}

/// AC: unknown per-phase usage is **absent**, not `0`, and its value is counted
/// in `tokens_unattributed` — so "this phase was free" and "this phase was not
/// measured" are distinguishable on the wire.
#[test]
#[serial]
fn an_unmeasured_phase_omits_its_token_keys_and_its_share_lands_in_the_remainder() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9444;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-2", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    let mark = |secs: i64| started_at + chrono::Duration::seconds(secs);
    seed_phase_history(
        &mut registry,
        &sweep_id,
        &[
            ("curator-done", mark(100)),
            ("builder-done", mark(200)),
            ("judge-done", mark(300)),
        ],
    );
    // Only the builder window has records; curator and judge are unmeasured.
    let lines = [
        stamped("builder", mark(150), 100, 10),
        unstamped("no-clock", 3, 1),
    ]
    .concat();
    seed_sweep_transcript(dir.path(), &registry, issue, &lines);

    registry.reap_once();
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let record = outcome_for(&registry, issue);
    assert_eq!(
        record
            .phase_durations
            .iter()
            .map(|p| p.token_split())
            .collect::<Vec<_>>(),
        vec![None, Some((100, 10)), None],
        "unmeasured phases carry no split at all"
    );
    assert_eq!(
        record.tokens_unattributed,
        Some(telemetry::TokenTotals {
            tokens_in: 3,
            tokens_out: 1
        }),
        "the clock-less record is the whole remainder"
    );

    // On the wire: absent keys, not zeros.
    let raw = raw_outcome(&registry, issue);
    let phases = raw["phase_durations"].as_array().unwrap();
    for (index, phase) in [(0usize, "curator"), (2, "judge")] {
        let entry = &phases[index];
        assert_eq!(entry["phase"], phase);
        for key in ["tokens_in", "tokens_out", "tokens_by_model"] {
            assert!(
                entry.get(key).is_none(),
                "an unmeasured phase must OMIT {key}, never publish 0: {entry}"
            );
        }
        assert!(entry.get("attempt").is_some(), "the attempt index is still known: {entry}");
    }
}

/// A sweep whose transitions were never sampled (a daemon restart mid-sweep)
/// falls back to one best-effort entry. It must carry NO usage: the sweep ran
/// phases the daemon never saw, so attributing the whole total to the one phase
/// it happens to know would be a fabrication. The whole total is the remainder.
#[test]
#[serial]
fn the_unsampled_fallback_entry_attributes_nothing_and_reports_the_whole_total_as_remainder() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9445;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-3", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    registry.entries.get_mut(&sweep_id).unwrap().latest_phase = Some("builder-done".to_string());
    let lines = stamped("builder", started_at + chrono::Duration::seconds(150), 100, 10);
    seed_sweep_transcript(dir.path(), &registry, issue, &lines);

    registry.reap_once();
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let record = outcome_for(&registry, issue);
    assert_eq!(record.phase_durations.len(), 1);
    assert_eq!(record.phase_durations[0].phase, "builder");
    assert_eq!(record.phase_durations[0].token_split(), None);
    assert_eq!(
        record.tokens_unattributed,
        Some(telemetry::TokenTotals {
            tokens_in: 100,
            tokens_out: 10
        }),
        "with no sampled window, the whole measured total is unattributed"
    );
}

/// No attributable transcript at all ⇒ no sweep totals ⇒ no remainder either.
/// `0` there would falsely claim the (usage-less) phase entries account for
/// everything.
#[test]
#[serial]
fn an_unmeasured_sweep_omits_the_remainder_rather_than_reporting_zero() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9446;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-4", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    seed_phase_history(
        &mut registry,
        &sweep_id,
        &[("curator-done", started_at + chrono::Duration::seconds(100))],
    );
    // A project directory with no matching session.
    seed_sweep_transcript(dir.path(), &registry, 4242, "");

    registry.reap_once();
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let record = outcome_for(&registry, issue);
    assert_eq!(record.tokens_in, None);
    assert_eq!(record.tokens_unattributed, None);
    let raw = raw_outcome(&registry, issue);
    assert!(
        raw.get("tokens_unattributed").is_none(),
        "with no total to take a remainder of, the key is omitted: {raw}"
    );
}

/// `phase_windows_for` numbers attempts 1-based **per phase name**, which is what
/// makes the Judge↔Doctor cycle's repeated entries addressable, and never emits a
/// window whose end precedes its start.
#[test]
fn phase_windows_number_attempts_per_phase_and_never_invert() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let sweep_id = "sweep-issue-9443-9";
    let started_at = Utc::now() - Duration::from_secs(600);
    seed_phase_history(
        &mut registry,
        sweep_id,
        &[
            ("judge-rejected", started_at + chrono::Duration::seconds(100)),
            ("doctor-done", started_at + chrono::Duration::seconds(200)),
            ("judge-rejected", started_at + chrono::Duration::seconds(300)),
            ("doctor-done", started_at + chrono::Duration::seconds(400)),
            // A clock step backwards: the window must clamp, not invert.
            ("judge-done", started_at + chrono::Duration::seconds(350)),
        ],
    );

    let windows = registry.phase_windows_for(sweep_id, started_at);
    assert_eq!(
        windows
            .iter()
            .map(|w| (w.phase.as_str(), w.attempt))
            .collect::<Vec<_>>(),
        vec![
            ("judge", 1),
            ("doctor", 1),
            ("judge", 2),
            ("doctor", 2),
            ("judge", 3)
        ]
    );
    assert!(
        windows.iter().all(|w| w.end >= w.start),
        "a backwards clock step clamps rather than producing an inverted window"
    );
    assert_eq!(
        windows
            .iter()
            .map(PhaseWindow::to_duration)
            .map(|d| d.duration_sec)
            .collect::<Vec<_>>(),
        vec![100, 100, 100, 100, 0]
    );
}
