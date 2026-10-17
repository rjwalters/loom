//! #5305: PR #5304 removed the token axis from the admission cap (#5270)
//! but, in the process, over-removed two *signals* rather than
//! re-scoping them: `resolve_capacity`'s fresh-probe branch hardcoded
//! `token_bound: false`, and the #4344 ranking-divergence detection
//! (`ranking_diverges_from_starvation`, née `dispatch_starved_but_disagrees`)
//! was hardcoded to `false` outright. Both must be reachable again —
//! `token_bound` meaning genuine starvation (zero healthy accounts), not
//! "tokens bind the cap."
use super::{ranking_diverges_from_starvation, resolve_capacity};
use crate::cli::status::sample_report::sample_report;
use loom_daemon::types::CapacityReport;

#[test]
fn resolve_capacity_probe_tier_reports_token_bound_on_zero_healthy() {
    let report = sample_report();
    let usage = serde_json::json!({
        "accounts": [
            {"status": "exhausted", "7d_utilization": 0.99},
            {"status": "blocked", "7d_utilization": 0.99},
        ]
    });
    let rc = resolve_capacity(&report, Some(&usage));
    assert_eq!(rc.source, "probe");
    assert_eq!(rc.healthy, 0);
    assert!(
        rc.token_bound,
        "a fresh probe with 0 healthy accounts must report starvation, \
         not the #5304 hardcoded `false`"
    );
}

#[test]
fn resolve_capacity_probe_tier_not_token_bound_when_any_account_healthy() {
    let report = sample_report();
    let usage = serde_json::json!({
        "accounts": [
            {"status": "available", "7d_utilization": 0.1},
            {"status": "exhausted", "7d_utilization": 0.99},
        ]
    });
    let rc = resolve_capacity(&report, Some(&usage));
    assert_eq!(rc.source, "probe");
    assert_eq!(rc.healthy, 1);
    assert!(!rc.token_bound, "one healthy account remains ⇒ not starved");
}

#[test]
fn ranking_diverges_when_daemons_own_ranking_is_starved_but_probe_is_fresh() {
    let mut report = sample_report();
    report.capacity = CapacityReport {
        ranking_present: true,
        total_accounts: 3,
        healthy_accounts: 0,
        exhausted_accounts: 3,
        token_axis_limit: 0,
        token_bound: true,
    };
    let usage = serde_json::json!({
        "accounts": [
            {"status": "available", "7d_utilization": 0.1},
            {"status": "exhausted", "7d_utilization": 0.99},
            {"status": "exhausted", "7d_utilization": 0.99},
        ]
    });
    let rc = resolve_capacity(&report, Some(&usage));
    assert_eq!(rc.healthy, 1, "the fresher probe sees one healthy account");
    assert!(
        ranking_diverges_from_starvation(&report, &rc),
        "daemon's own ranking reads 0 healthy while a fresher probe disagrees"
    );
}

#[test]
fn ranking_does_not_diverge_when_both_agree_or_daemon_has_capacity() {
    // Daemon's own ranking already shows healthy accounts ⇒ no divergence
    // to report, regardless of what a fresh probe shows.
    let mut report = sample_report();
    report.capacity = CapacityReport {
        ranking_present: true,
        total_accounts: 3,
        healthy_accounts: 2,
        exhausted_accounts: 1,
        token_axis_limit: 2,
        token_bound: false,
    };
    let usage = serde_json::json!({
        "accounts": [
            {"status": "available", "7d_utilization": 0.1},
            {"status": "available", "7d_utilization": 0.1},
        ]
    });
    let rc = resolve_capacity(&report, Some(&usage));
    assert!(!ranking_diverges_from_starvation(&report, &rc));

    // Both the daemon's own ranking AND the fresh probe agree on
    // starvation ⇒ no *disagreement* to headline (the plain "NO healthy
    // accounts" guidance branch handles this case instead).
    let mut starved_report = sample_report();
    starved_report.capacity = CapacityReport {
        ranking_present: true,
        total_accounts: 3,
        healthy_accounts: 0,
        exhausted_accounts: 3,
        token_axis_limit: 0,
        token_bound: true,
    };
    let starved_usage = serde_json::json!({
        "accounts": [
            {"status": "exhausted", "7d_utilization": 0.99},
            {"status": "exhausted", "7d_utilization": 0.99},
        ]
    });
    let rc = resolve_capacity(&starved_report, Some(&starved_usage));
    assert!(!ranking_diverges_from_starvation(&starved_report, &rc));
}
