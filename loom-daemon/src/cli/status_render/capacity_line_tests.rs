//! Issue #9131: the below-cap capacity line must name an active dispatch hold
//! instead of "the limiter is work availability".
use super::below_cap_line;
use crate::cli::status::sample_report::sample_report;
use chrono::Utc;
use loom_daemon::main_health_gate::WorkspaceHealthStates;
use loom_daemon::types::{
    AdmissionBrakeStatus, DaemonStatusReport, HostBreakerStatus, PoolExhaustionHoldStatus,
    RepoStatus,
};
use loom_daemon::work_finder::halt_cause::{causes_per_root, HaltCause};
use std::path::PathBuf;

const GENERIC: &str = "the limiter is work availability";
/// Both CLI branches' `resources` tails (ranking present / absent).
const TAILS: [&str; 2] = ["disk/RAM/CPU", "tokens/disk/CPU"];

fn line(report: &DaemonStatusReport, tail: &str) -> String {
    below_cap_line(report, 12, tail).expect("a line is printed")
}

fn draining() -> DaemonStatusReport {
    let mut r = sample_report();
    r.draining = true;
    r
}

fn breaker(phase: &str) -> HostBreakerStatus {
    HostBreakerStatus {
        enabled: true,
        phase: phase.to_string(),
        suppressed: true,
        reason: Some("sustained load".to_string()),
        tripped_at: Some(Utc::now()),
        releases_at: None,
        last_load_per_core: Some(5.0),
        load_per_core_threshold: 4.0,
        sustain_ticks: 3,
        cooldown_secs: 300,
    }
}

fn repo(root: &str, halted: bool) -> RepoStatus {
    RepoStatus {
        root: PathBuf::from(root),
        priority: 100,
        in_flight_count: 0,
        health_gate_halted: halted,
        quarantined_issues: vec![],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: Some(true),
        health_gate_verdict_at: None,
        root_missing: false,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: None,
        role_runner_enabled: false,
        role_runner_roles: vec![],
        role_runner_intervals: std::collections::BTreeMap::new(),
        role_runner_on_idle_roles: vec![],
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: None,
        role_runner_shard: None,
        token_pool_dir: None,
        ranking_present: false,
        ranking_age_secs: None,
        stash_total_count: 0,
        stash_quarantine_count: 0,
        stash_oldest_age_secs: None,
        stash_non_quarantine_unrecoverable_count: 0,
        stash_non_quarantine_unrecoverable_oldest_age_secs: None,
        sweep_command_missing: false,
    }
}

#[test]
fn active_drain_names_the_drain_not_work_availability_in_both_branches() {
    for tail in TAILS {
        let l = line(&draining(), tail);
        assert!(l.contains("dispatch is HELD"), "{l}");
        assert!(l.contains("drain (`restart --drain` armed"), "{l}");
        assert!(!l.contains(GENERIC), "a drain-paused host is not short of work: {l}");
    }
}

#[test]
fn drain_with_zero_in_flight_still_names_the_drain() {
    let r = draining();
    assert!(r.in_flight.is_empty());
    let l = line(&r, TAILS[0]);
    assert!(l.contains("(0 in flight, cap 12)") && l.contains("drain"), "{l}");
}

#[test]
fn drain_plus_breaker_names_both() {
    let mut r = draining();
    r.host_breaker = Some(Box::new(breaker("cooldown")));
    let l = line(&r, TAILS[0]);
    assert!(l.contains("drain") && l.contains("host breaker COOLING DOWN"), "{l}");
    assert!(!l.contains(GENERIC), "{l}");
}

#[test]
fn open_breaker_alone_is_a_host_wide_hold() {
    let mut r = sample_report();
    r.host_breaker = Some(Box::new(breaker("open")));
    let l = line(&r, TAILS[1]);
    assert!(l.contains("host breaker OPEN") && !l.contains(GENERIC), "{l}");
}

#[test]
fn ended_drain_with_retained_note_reads_work_availability() {
    let mut r = sample_report();
    r.draining = false;
    r.drain_note = Some("drain timed out — restart refused".to_string());
    let l = line(&r, TAILS[0]);
    assert!(l.contains(GENERIC), "a historical drain note is not a hold: {l}");
    assert!(!l.contains("HELD"), "{l}");
}

#[test]
fn no_hold_below_capacity_is_byte_identical_to_the_pre_9131_lines() {
    let r = sample_report();
    assert_eq!(
        line(&r, "disk/RAM/CPU"),
        "  not capacity-bound (0 in flight, cap 12 — the limiter is work availability, not \
         disk/RAM/CPU)"
    );
    assert_eq!(
        line(&r, "tokens/disk/CPU"),
        "  not capacity-bound (0 in flight, cap 12 — the limiter is work availability, not \
         tokens/disk/CPU)"
    );
}

#[test]
fn a_single_red_workspace_is_scoped_not_host_wide() {
    let mut r = sample_report();
    r.per_repo = vec![repo("/a", true), repo("/b", false), repo("/c", false)];
    let l = line(&r, TAILS[0]);
    assert!(l.contains("held for some workspaces"), "{l}");
    assert!(l.contains("main-health gate halted (1 of 3 workspace(s))"), "{l}");
    assert!(l.contains("Unheld workspaces are limited by work availability"), "{l}");
    assert!(!l.contains("dispatch is HELD"), "one red repo does not stop the host: {l}");
}

#[test]
fn every_workspace_red_is_host_wide() {
    let mut r = sample_report();
    r.per_repo = vec![repo("/a", true), repo("/b", true)];
    let l = line(&r, TAILS[0]);
    assert!(l.contains("dispatch is HELD") && l.contains("every workspace"), "{l}");
    assert!(!l.contains("work availability,"), "{l}");
}

#[test]
fn exhausted_pool_is_scoped() {
    let mut r = sample_report();
    r.pool_exhaustion_holds = vec![PoolExhaustionHoldStatus {
        dir: PathBuf::from("/pool"),
        total: 4,
        since: Utc::now(),
        next_clear_at: Utc::now(),
        wrapper_observed: false,
    }];
    let l = line(&r, TAILS[0]);
    assert!(l.contains("token pool exhausted (1 pool(s)"), "{l}");
    assert!(l.contains("held for some workspaces"), "{l}");
}

#[test]
fn preflight_advisory_alone_still_suppresses_the_line() {
    let mut r = sample_report();
    r.preflight_advisory_active = true;
    assert!(below_cap_line(&r, 12, TAILS[0]).is_none());
    // ...but it never hides a drain.
    r.draining = true;
    let l = line(&r, TAILS[0]);
    assert!(l.contains("drain") && l.contains("pre-flight advisory"), "{l}");
}

#[test]
fn admission_brake_keeps_its_own_diagnosis() {
    let mut r = draining();
    r.admission_brake = Some(AdmissionBrakeStatus {
        enabled: true,
        held: true,
        load_per_core: Some(9.0),
        load_per_core_threshold: 4.0,
        held_since: Some(Utc::now()),
        held_ticks: 2,
        starving_since: None,
        starving_ticks: 0,
        escape_hatch_grants: 0,
    });
    let l = line(&r, TAILS[0]);
    assert!(l.contains("ADMISSION BRAKE HOLDING"), "{l}");
}

/// AC2: for the same synthetic hold state, `queue`'s per-root classification
/// (`causes_per_root`) and `status`'s capacity line agree — a queue that
/// would render every row held never pairs with a "work availability" line,
/// and an unheld queue keeps it.
#[test]
fn status_agrees_with_queue_classification_for_equivalent_states() {
    let roots = vec![PathBuf::from("/a"), PathBuf::from("/b")];
    let health = WorkspaceHealthStates::new();
    let none = [None, None];
    for (drain, breaker_on) in [(true, false), (false, true), (true, true), (false, false)] {
        let causes = causes_per_root(&health, &roots, false, &none, drain, breaker_on);
        let queue_halted = causes.iter().all(Option::is_some);
        let mut r = sample_report();
        r.draining = drain;
        if breaker_on {
            r.host_breaker = Some(Box::new(breaker("open")));
        }
        let l = line(&r, TAILS[0]);
        assert_eq!(queue_halted, !l.contains(GENERIC), "drain={drain} breaker={breaker_on}: {l}");
        if drain {
            assert_eq!(causes[0], Some(HaltCause::Drain));
        }
    }
}
