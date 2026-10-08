//! Phaseless deaths vs. the PR-less retry bound (Issue #10642).
//!
//! The observed shape: on `2AMLogic/2am`, 278 sweeps in a day ended `failure`
//! with `failure_class = unclassified:no-phase-signal` and retried with no
//! cap — one issue 47 times. These tests drive that shape end to end through
//! `reap_once`: a sweep that runs ~90 s, writes no checkpoint, and dies with
//! no classifier label.
//!
//! The first test pins what the Curator's hypothesis got wrong. A
//! `no-phase-signal` record can never be a pre-flight death or a pool death:
//! both carve-outs are driven by a classifier label (`death_class` /
//! `no-usable-account`), and that label *becomes* `failure_class`, so a record
//! whose class had to be synthesized as `unclassified:no-phase-signal` had
//! neither. On one long-lived registry the bound already counts these deaths.
//!
//! The tests after it reproduce the two ways it does not: the tally lives in
//! memory, so a daemon restart forgets it, and a streak more than
//! `max_backoff` (1 h) old goes cold, so attempts spaced out by the backoff
//! itself (or by other hosts winning the claim) never add up. Both are closed
//! by the durable floor in [`super::durable`], which counts this host's
//! `no-phase-signal` records for the issue from the outcome journal.

use super::*;
use crate::sweep_registry::test_support::{fixture_registry, insert_dead_running_at};
use crate::sweep_registry::SweepRegistry;
use crate::telemetry::NO_PHASE_SIGNAL_CLASS;
use serial_test::serial;
use std::path::Path;
use tempfile::tempdir;

/// A phaseless death of the measured shape: started 90 s ago (past the 60 s
/// spawn-death / insta-crash windows), no checkpoint, a dead pid with no
/// retained child (so no exit status), no log, then one reap.
fn phaseless_death(reg: &mut SweepRegistry, issue: u32, seq: u32) -> String {
    let started = Utc::now() - chrono::Duration::seconds(90);
    let sweep_id = insert_dead_running_at(reg, issue, seq, started);
    reg.reap_once();
    sweep_id
}

/// The `sweep.outcome` record the reap just wrote for `sweep_id`.
fn outcome_for(ws: &Path, sweep_id: &str) -> crate::telemetry::SweepOutcomeRecord {
    crate::sweep_outcomes::read_all_sweep_outcomes(&ws.join("test-sweep-outcome-telemetry.jsonl"))
        .into_iter()
        .rev()
        .find(|r| r.sweep_id == sweep_id)
        .expect("the reap must write a sweep.outcome record")
}

fn clear_repo_env() {
    std::env::remove_var("LOOM_REPO");
}

/// Hypothesis check. Three phaseless deaths on ONE registry: each record is
/// `unclassified:no-phase-signal`, none of the reaper's carve-outs fires, and
/// the third reaches the hold. So the carve-outs are not what lets the loop
/// run; the tests below are.
#[test]
#[serial]
fn on_one_live_registry_phaseless_deaths_already_reach_the_hold() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let threshold = reg.prless_retry_config().threshold;

    for seq in 0..threshold {
        let id = phaseless_death(&mut reg, 10_642, seq);
        let record = outcome_for(dir.path(), &id);
        assert_eq!(record.failure_class.as_deref(), Some(NO_PHASE_SIGNAL_CLASS));
        assert_eq!(record.disposition, crate::telemetry::SweepDisposition::Unknown);
        // AC1: the record says how the run ended. No retained child, so no
        // exit status; no per-sweep log, so nothing about how far it got.
        let cause = record
            .no_phase_cause
            .expect("a no-phase-signal record carries its cause");
        assert_eq!(cause.exit, "none_observed");
        assert_eq!(cause.last_step, "none");
        assert_eq!(cause.reason, "log_unreadable");
    }

    assert_eq!(reg.prless_release_count(10_642), threshold);
    assert!(reg.prless_retry_held(10_642));
}

/// Gap 1: a daemon restart between attempts. Every restart builds a fresh
/// registry, so the in-memory tally restarts at 1 and never reaches the
/// threshold. The durable floor reads the journal the previous process wrote.
#[test]
#[serial]
fn a_daemon_restart_between_attempts_does_not_reset_the_count() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    let threshold = fixture_registry(dir.path())
        .0
        .prless_retry_config()
        .threshold;

    let mut last = None;
    for seq in 0..threshold {
        let (mut reg, _) = fixture_registry(dir.path());
        phaseless_death(&mut reg, 10_642, seq);
        assert_eq!(
            reg.prless_release_count(10_642),
            seq + 1,
            "attempt {} after a restart must count the earlier ones",
            seq + 1
        );
        last = Some(reg);
    }

    let reg = last.unwrap();
    assert!(reg.prless_retry_held(10_642), "the threshold-th death must hold the issue");
    let reason = reg.prless_retry_reason(10_642).unwrap();
    assert!(
        reason.contains("recorded cause:") && reason.contains("exit=none_observed"),
        "the hold names the recorded cause (#10642 AC4): {reason}"
    );
}

/// Gap 2: attempts spaced more than `max_backoff` apart. The in-memory streak
/// goes cold and restarts at 1 each time; the durable floor counts within a
/// day, so the bound still converges.
#[test]
#[serial]
fn attempts_spaced_past_the_cold_window_still_count_within_a_day() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let threshold = reg.prless_retry_config().threshold;
    let ceiling = i64::try_from(reg.prless_retry_config().max_backoff.as_secs()).unwrap();

    let mut windows = Vec::new();
    for seq in 0..threshold {
        if let Some(state) = reg.prless_retry.get_mut(&10_642) {
            state.recorded_at = Utc::now() - chrono::Duration::seconds(ceiling + 1);
        }
        phaseless_death(&mut reg, 10_642, seq);
        windows.push(reg.prless_retry_remaining(10_642, Utc::now()).unwrap());
    }

    assert_eq!(reg.prless_release_count(10_642), threshold);
    assert!(reg.prless_retry_held(10_642));
    assert!(windows[1] > windows[0], "the backoff grows between attempts: {windows:?}");
}

/// AC5: a clear (open linked PR, merge phase, self-reported no-op) still
/// resets the tally, and the durable floor must not resurrect the deaths
/// recorded before it.
#[test]
#[serial]
fn a_clear_is_not_undone_by_the_durable_floor() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());

    phaseless_death(&mut reg, 10_642, 0);
    phaseless_death(&mut reg, 10_642, 1);
    assert_eq!(reg.prless_release_count(10_642), 2);

    reg.note_prless_terminal_outcome(10_642, "sweep-other", Some(OpenPrProbe::Open(1)), "n/a");
    assert_eq!(reg.prless_release_count(10_642), 0);

    phaseless_death(&mut reg, 10_642, 2);
    assert_eq!(
        reg.prless_release_count(10_642),
        1,
        "deaths before the clear must not count again"
    );
    assert!(!reg.prless_retry_held(10_642));
}

/// A clear must survive a daemon restart: two deaths, a clear (open PR here,
/// with no PR-bearing outcome written), a restart, then one more death counts
/// as one — not three.
#[test]
#[serial]
fn a_clear_survives_a_daemon_restart() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    {
        let (mut reg, _) = fixture_registry(dir.path());
        phaseless_death(&mut reg, 10_642, 0);
        phaseless_death(&mut reg, 10_642, 1);
        reg.note_prless_terminal_outcome(10_642, "sweep-other", Some(OpenPrProbe::Open(1)), "n/a");
        assert_eq!(reg.prless_release_count(10_642), 0);
    }
    let (mut reg, _) = fixture_registry(dir.path());
    phaseless_death(&mut reg, 10_642, 2);
    assert_eq!(
        reg.prless_release_count(10_642),
        1,
        "deaths before a clear must not count again after a restart"
    );
    assert!(!reg.prless_retry_held(10_642));
}

/// A self-reported no-op clears through the same path and must also survive
/// a restart.
#[test]
#[serial]
fn a_noop_clear_survives_a_daemon_restart() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    {
        let (mut reg, _) = fixture_registry(dir.path());
        phaseless_death(&mut reg, 10_642, 0);
        phaseless_death(&mut reg, 10_642, 1);
        assert!(reg.clear_prless_retry(10_642));
    }
    let (mut reg, _) = fixture_registry(dir.path());
    phaseless_death(&mut reg, 10_642, 2);
    assert_eq!(reg.prless_release_count(10_642), 1);
}

/// A landed record in the journal ends the durable streak too, so a restart
/// after a landing does not count the deaths that preceded it.
#[test]
#[serial]
fn a_landing_in_the_journal_ends_the_durable_streak() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    {
        let (mut reg, _) = fixture_registry(dir.path());
        phaseless_death(&mut reg, 10_642, 0);
        phaseless_death(&mut reg, 10_642, 1);
    }
    // A landing written by an earlier process (any record carrying a PR).
    let path = dir.path().join("test-sweep-outcome-telemetry.jsonl");
    let mut landed = outcome_for(dir.path(), "sweep-issue-10642-1");
    landed.sweep_id = "sweep-issue-10642-landed".into();
    landed.pr_number = Some(77);
    landed.failure_class = None;
    landed.no_phase_cause = None;
    landed.disposition = crate::telemetry::SweepDisposition::Landed;
    landed.result = crate::telemetry::SweepResult::Success;
    crate::sweep_outcomes::append_outcome_telemetry(
        &path,
        &crate::telemetry::TelemetryEnvelope::new(
            "host",
            crate::telemetry::TelemetryRecord::SweepOutcome(landed),
        ),
    )
    .unwrap();

    let (mut reg, _) = fixture_registry(dir.path());
    phaseless_death(&mut reg, 10_642, 2);
    assert_eq!(reg.prless_release_count(10_642), 1);
}

/// Other issues' records never feed this issue's floor.
#[test]
#[serial]
fn the_durable_floor_is_per_issue() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    {
        let (mut reg, _) = fixture_registry(dir.path());
        phaseless_death(&mut reg, 1, 0);
        phaseless_death(&mut reg, 2, 0);
    }
    let (mut reg, _) = fixture_registry(dir.path());
    phaseless_death(&mut reg, 3, 0);
    assert_eq!(reg.prless_release_count(3), 1);
}

/// AC1: the cause reads the per-sweep log. A dispatch whose log shows the
/// harness started is `cli_started`; one that never got there is `pre_cli`.
#[test]
#[serial]
fn the_cause_reads_how_far_the_log_got() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let log = reg.compute_log_path(10_642);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();

    std::fs::write(&log, "spawn-claude: dispatching\n# CLAUDE_CLI_START\nworking\n").unwrap();
    let started = phaseless_death(&mut reg, 10_642, 0);
    let cause = outcome_for(dir.path(), &started).no_phase_cause.unwrap();
    assert_eq!(
        (cause.last_step.as_str(), cause.reason.as_str()),
        ("cli_started", "killed_or_exit_unobserved")
    );

    std::fs::write(&log, "spawn-claude: dispatching\nsomething failed early\n").unwrap();
    let early = phaseless_death(&mut reg, 10_642, 1);
    let cause = outcome_for(dir.path(), &early).no_phase_cause.unwrap();
    assert_eq!(
        (cause.last_step.as_str(), cause.reason.as_str()),
        ("pre_cli", "died_before_cli_start")
    );
}

/// AC1: other classes are unchanged — a sub-60 s phaseless death is still
/// `unclassified:spawn-death` and carries no cause, and is not floored.
#[test]
#[serial]
fn other_classes_carry_no_cause_and_get_no_floor() {
    clear_repo_env();
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let started = Utc::now() - chrono::Duration::seconds(10);
    let id = insert_dead_running_at(&mut reg, 10_643, 0, started);
    reg.reap_once();
    let record = outcome_for(dir.path(), &id);
    assert_eq!(record.failure_class.as_deref(), Some("unclassified:spawn-death"));
    assert!(record.no_phase_cause.is_none());
    assert!(reg
        .durable_no_phase_streak(10_643, &id, Utc::now())
        .is_none());
}
