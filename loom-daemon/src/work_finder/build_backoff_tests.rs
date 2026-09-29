//! Unit tests for the build back-off state machine and config (#9410).
//! None touches the global demand ledger or the network.

use super::*;
use crate::role_runner::demand::{AxisDebt, DebtAxis};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn cfg() -> BuildBackoffConfig {
    BuildBackoffConfig::default()
}

fn reading(total: usize) -> Option<DebtReading> {
    Some(DebtReading {
        total,
        review: Some(total),
        changes: None,
        merge: None,
    })
}

/// A state machine already in `engaged`.
fn engaged() -> BuildBackoff {
    let mut b = BuildBackoff::default();
    assert!(b.observe(reading(1000), &cfg()).is_some());
    assert!(b.held());
    b
}

fn axis(total: usize) -> Option<AxisDebt> {
    Some(AxisDebt {
        total,
        roots_with_debt: usize::from(total > 0),
    })
}

// -- AC1: hysteresis ----------------------------------------------------------

#[test]
fn engages_strictly_above_high() {
    let mut b = BuildBackoff::default();
    assert_eq!(b.observe(reading(40), &cfg()), None, "debt == high does not engage");
    assert!(!b.held());
    let edge = b.observe(reading(41), &cfg());
    assert!(matches!(edge, Some(Edge::Engaged(d)) if d.total == 41), "{edge:?}");
    assert!(b.held());
}

#[test]
fn engaged_holds_down_to_low_and_releases_below_it() {
    let mut b = engaged();
    for debt in [40, 25, 30, 41, 25] {
        assert_eq!(b.observe(reading(debt), &cfg()), None, "{debt} holds while engaged");
        assert!(b.held(), "{debt}");
    }
    let edge = b.observe(reading(24), &cfg());
    assert!(
        matches!(edge, Some(Edge::Released(ReleaseReason::BelowLow, Some(d))) if d.total == 24),
        "{edge:?}"
    );
    assert!(!b.held());
}

#[test]
fn released_stays_released_inside_the_band() {
    let mut b = BuildBackoff::default();
    for debt in [30, 25, 40, 0] {
        assert_eq!(b.observe(reading(debt), &cfg()), None, "{debt} stays released");
        assert!(!b.held());
    }
    // And after a release, too.
    let mut b = engaged();
    assert!(b.observe(reading(24), &cfg()).is_some());
    assert_eq!(b.observe(reading(30), &cfg()), None);
    assert!(!b.held());
}

// -- AC2: fail open -----------------------------------------------------------

#[test]
fn an_unobserved_ledger_never_engages_and_releases_an_engaged_backoff() {
    assert_eq!(debt_from(&HostDebt::default()), None);
    let mut b = BuildBackoff::default();
    assert_eq!(b.observe(None, &cfg()), None);
    assert!(!b.held());

    let mut b = engaged();
    assert_eq!(b.observe(None, &cfg()), Some(Edge::Released(ReleaseReason::Unobserved, None)));
    assert!(!b.held());
}

#[test]
fn a_stale_ledger_reads_as_unobserved_and_a_partial_one_sums_what_it_has() {
    let ledger = DemandLedger::default();
    let root = PathBuf::from("/tmp/loom-9410-backoff");
    let stale = Duration::from_secs(1800);
    let t0 = Instant::now();
    for axis in [DebtAxis::Review, DebtAxis::Changes, DebtAxis::Merge] {
        ledger.record_at(&root, axis, 100, t0);
    }
    let later = t0 + Duration::from_secs(1801);
    assert_eq!(debt_from(&ledger.host_debt_at(later, stale)), None, "all stale");
    let mut b = BuildBackoff::default();
    assert_eq!(b.observe(debt_from(&ledger.host_debt_at(later, stale)), &cfg()), None);

    // Only Merge is fresh: the total is Merge alone.
    ledger.record_at(&root, DebtAxis::Merge, 12, later);
    let d = debt_from(&ledger.host_debt_at(later, stale)).unwrap();
    assert_eq!((d.total, d.review, d.changes, d.merge), (12, None, None, Some(12)));
}

// -- AC3: axes ----------------------------------------------------------------

#[test]
fn debt_sums_review_changes_and_merge() {
    let ledger = DemandLedger::default();
    let root = PathBuf::from("/tmp/loom-9410-axes");
    ledger.record(&root, DebtAxis::Review, 20);
    ledger.record(&root, DebtAxis::Changes, 10);
    ledger.record(&root, DebtAxis::Merge, 15);
    let d = debt_from(&ledger.host_debt(Duration::from_secs(1800))).unwrap();
    assert_eq!(d.total, 45);
    let mut b = BuildBackoff::default();
    assert!(matches!(b.observe(Some(d), &cfg()), Some(Edge::Engaged(_))), "45 > 40");

    let host = HostDebt {
        review: axis(3),
        changes: None,
        merge: axis(4),
    };
    assert_eq!(debt_from(&host).unwrap().total, 7);
}

// -- AC6: config --------------------------------------------------------------

#[test]
fn config_defaults_and_a_valid_custom_pair() {
    let d = BuildBackoffConfig::parse(&Value::Null);
    assert_eq!(
        d.config,
        BuildBackoffConfig {
            enabled: true,
            high: 40,
            low: 25
        }
    );
    assert_eq!(d.rejected_pair, None);
    assert_eq!(BuildBackoffConfig::parse(&json!({})).config, d.config);

    let p = BuildBackoffConfig::parse(&json!({"enabled": true, "high": 10, "low": 3}));
    assert_eq!(
        p.config,
        BuildBackoffConfig {
            enabled: true,
            high: 10,
            low: 3
        }
    );
    assert_eq!(p.rejected_pair, None);
}

#[test]
fn low_at_or_above_high_rejects_the_pair() {
    for (high, low) in [(30, 30), (20, 35)] {
        let p = BuildBackoffConfig::parse(&json!({"high": high, "low": low}));
        assert_eq!((p.config.high, p.config.low), (40, 25), "{high}/{low}");
        assert_eq!(p.rejected_pair, Some((high, low)));
    }
    // A per-key fallback can create the conflict too: high invalid -> 40.
    let p = BuildBackoffConfig::parse(&json!({"high": -1, "low": 50}));
    assert_eq!((p.config.high, p.config.low), (40, 25));
    assert_eq!(p.rejected_pair, Some((40, 50)));
    // `enabled` survives a rejected pair.
    let p = BuildBackoffConfig::parse(&json!({"enabled": false, "high": 5, "low": 9}));
    assert!(!p.config.enabled);
}

#[test]
fn invalid_values_fall_back_per_key() {
    for bad in [json!(0), json!(-5), json!(12.5), json!("30"), json!(null)] {
        let p = BuildBackoffConfig::parse(&json!({"high": bad.clone(), "low": 5}));
        assert_eq!((p.config.high, p.config.low), (40, 5), "high={bad}");
        let p = BuildBackoffConfig::parse(&json!({"high": 60, "low": bad.clone()}));
        assert_eq!((p.config.high, p.config.low), (60, 25), "low={bad}");
    }
    let p = BuildBackoffConfig::parse(&json!({"enabled": "no"}));
    assert!(p.config.enabled, "non-bool enabled -> default");
}

#[test]
fn disabled_never_engages_and_releases_if_engaged() {
    let off = BuildBackoffConfig {
        enabled: false,
        ..cfg()
    };
    let mut b = BuildBackoff::default();
    assert_eq!(b.observe(reading(1000), &off), None);
    assert!(!b.held());

    let mut b = engaged();
    let edge = b.observe(reading(1000), &off);
    assert!(matches!(edge, Some(Edge::Released(ReleaseReason::Disabled, _))), "{edge:?}");
    assert!(!b.held());
}

#[test]
fn config_is_read_from_the_effective_config() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(
        tmp.path().join(".loom").join("config.json"),
        r#"{"autonomous":{"workFinder":{"buildBackoff":{"high":12,"low":4}}}}"#,
    )
    .unwrap();
    let p = BuildBackoffConfig::read(tmp.path());
    assert_eq!((p.config.high, p.config.low), (12, 4));
}

#[test]
fn step_warns_once_per_distinct_bad_pair_and_reads_the_given_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    let write =
        |cfg: &str| std::fs::write(tmp.path().join(".loom").join("config.json"), cfg).unwrap();
    write(r#"{"autonomous":{"workFinder":{"buildBackoff":{"high":5,"low":9}}}}"#);
    let ledger = DemandLedger::default();
    let mut b = BuildBackoff::default();
    assert!(!b.step(tmp.path(), &ledger), "empty ledger: fail open");
    assert_eq!(b.warned, Some((5, 9)));
    assert_eq!(ledger.reads(), 1);

    // Defaults apply after the rejection: 41 engages.
    ledger.record(tmp.path(), DebtAxis::Merge, 41);
    assert!(b.step(tmp.path(), &ledger));

    write(r#"{"autonomous":{"workFinder":{"buildBackoff":{"enabled":false}}}}"#);
    assert!(!b.step(tmp.path(), &ledger), "disabled releases");
    assert_eq!(b.warned, None, "a fixed config re-arms the warning");
    assert_eq!(ledger.reads(), 2, "disabled does not read the ledger");
}

// -- AC7: one INFO line per edge ----------------------------------------------

#[test]
fn edge_lines_name_the_debt_split_and_both_thresholds() {
    let d = DebtReading {
        total: 87,
        review: Some(28),
        changes: Some(0),
        merge: Some(59),
    };
    let line = Edge::Engaged(d).log_line(&cfg());
    for part in [
        "ENGAGED",
        "debt 87",
        "review=28 changes=0 merge=59",
        "high=40",
        "low=25",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
    let line = Edge::Released(ReleaseReason::BelowLow, Some(DebtReading { total: 20, ..d }))
        .log_line(&cfg());
    for part in ["RELEASED", "debt 20", "low=25", "high=40"] {
        assert!(line.contains(part), "{part} in {line}");
    }
    let line = Edge::Released(ReleaseReason::Unobserved, None).log_line(&cfg());
    assert!(line.contains("debt unobserved — failing open"), "{line}");
    let partial = DebtReading { changes: None, ..d };
    assert!(partial.split().contains("changes=?"));
}

#[test]
fn only_transitions_return_an_edge() {
    // A long run of ticks: exactly two edges (engage, release).
    let mut b = BuildBackoff::default();
    let edges = [10, 41, 50, 45, 30, 25, 24, 10, 40]
        .into_iter()
        .filter_map(|debt| b.observe(reading(debt), &cfg()))
        .count();
    assert_eq!(edges, 2);
}

// -- AC8: no forge call -------------------------------------------------------

#[test]
fn the_module_makes_no_forge_call() {
    let src = include_str!("build_backoff.rs");
    for needle in ["forge_listing", "gh_bin", "Command::new"] {
        assert!(!src.contains(needle), "build_backoff.rs must not reference {needle}");
    }
}
