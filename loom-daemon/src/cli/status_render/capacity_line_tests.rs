//! Issue #9131: the below-cap capacity line must name an active dispatch hold
//! instead of "the limiter is work availability".
use super::below_cap_line;
use crate::cli::status::sample_report::sample_report;
use chrono::Utc;
use loom_daemon::types::{DaemonStatusReport, PoolExhaustionHoldStatus, RepoStatus};
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

fn repo(root: &str, halted: bool) -> RepoStatus {
    RepoStatus {
        root: PathBuf::from(root),
        weight: 100,
        in_flight_count: 0,
        health_gate_halted: halted,
        maintain_only: None,
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
