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
        // The back-off reads the fresh aggregate, never the #9414 width view.
        ..HostDebt::default()
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
            low: 25,
            host: None,
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
            low: 3,
            host: None,
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
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn step_warns_once_per_distinct_bad_pair_and_reads_the_given_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    let write =
        |cfg: &str| std::fs::write(tmp.path().join(".loom").join("config.json"), cfg).unwrap();
    write(r#"{"autonomous":{"workFinder":{"buildBackoff":{"high":5,"low":9}}}}"#);
    let ledger = DemandLedger::default();
    let roots = [tmp.path().to_path_buf()];
    let mut b = BuildBackoffs::default();
    assert!(!b.step(tmp.path(), &roots, &ledger).any(), "empty ledger: fail open");
    assert_eq!(b.warned, Some((5, 9)));
    assert_eq!(ledger.reads(), 1, "one root, no host ceiling: one read");

    // Defaults apply after the rejection: 41 engages.
    ledger.record(tmp.path(), DebtAxis::Merge, 41);
    assert!(b.step(tmp.path(), &roots, &ledger).any());

    write(r#"{"autonomous":{"workFinder":{"buildBackoff":{"enabled":false}}}}"#);
    assert!(!b.step(tmp.path(), &roots, &ledger).any(), "disabled releases");
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
    let root = PathBuf::from("/srv/repo-a");
    let c = cfg();
    let scope = Scope::Repo(&root, &c);
    let line = Edge::Engaged(d).log_line(&scope);
    for part in [
        "repo /srv/repo-a",
        "ENGAGED",
        "debt 87",
        "review=28 changes=0 merge=59",
        "high=40",
        "low=25",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
    let line = Edge::Released(ReleaseReason::BelowLow, Some(DebtReading { total: 20, ..d }))
        .log_line(&scope);
    for part in [
        "repo /srv/repo-a",
        "RELEASED",
        "debt 20",
        "low=25",
        "high=40",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
    let line = Edge::Released(ReleaseReason::Unobserved, None).log_line(&scope);
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

// -- #10624: per-repo WIP limit and the optional host ceiling ------------------

/// A primary workspace whose `buildBackoff` block is `block`.
fn primary(block: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(
        tmp.path().join(".loom").join("config.json"),
        format!(r#"{{"autonomous":{{"workFinder":{{"buildBackoff":{block}}}}}}}"#),
    )
    .unwrap();
    tmp
}

fn repos(n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| PathBuf::from(format!("/tmp/loom-10624-repo-{i}")))
        .collect()
}

/// Record `debt` as `root`'s whole (changes-axis) debt, observed now.
fn debt(ledger: &DemandLedger, root: &Path, debt: usize) {
    for axis in [DebtAxis::Review, DebtAxis::Merge] {
        ledger.record(root, axis, 0);
    }
    ledger.record(root, DebtAxis::Changes, debt);
}

#[test]
fn repo_debt_reads_only_that_roots_fresh_entries() {
    let ledger = DemandLedger::default();
    let [a, b] = [
        PathBuf::from("/tmp/loom-10624-a"),
        PathBuf::from("/tmp/loom-10624-b"),
    ];
    let stale = Duration::from_secs(1800);
    let t0 = Instant::now();
    ledger.record_at(&a, DebtAxis::Review, 30, t0);
    ledger.record_at(&a, DebtAxis::Changes, 20, t0);
    ledger.record_at(&b, DebtAxis::Merge, 5, t0);
    assert_eq!(
        debt_from(&ledger.repo_debt_at(&a, t0, stale))
            .unwrap()
            .total,
        50
    );
    let d = debt_from(&ledger.repo_debt_at(&b, t0, stale)).unwrap();
    assert_eq!((d.total, d.review, d.merge), (5, None, Some(5)));
    assert_eq!(debt_from(&ledger.host_debt_at(t0, stale)).unwrap().total, 55);
    // Unknown and stale roots are unobserved: fail open per repo.
    assert_eq!(debt_from(&ledger.repo_debt_at(Path::new("/nope"), t0, stale)), None);
    let later = t0 + Duration::from_secs(1801);
    assert_eq!(debt_from(&ledger.repo_debt_at(&a, later, stale)), None);
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn a_high_debt_repo_does_not_hold_a_zero_debt_repo() {
    let cfg = primary("{}");
    let ledger = DemandLedger::default();
    let roots = repos(3);
    debt(&ledger, &roots[0], 60);
    debt(&ledger, &roots[1], 0);
    // roots[2] never observed: unobserved, fails open.
    let mut b = BuildBackoffs::default();
    let holds = b.step(cfg.path(), &roots, &ledger);
    assert_eq!(holds.per_workspace, vec![true, false, false]);
    assert_eq!((holds.repos_engaged, holds.host), (1, false));
    assert_eq!(holds.to_string(), "1/3 repos");
    assert!(b.repo_held(&roots[0]) && !b.repo_held(&roots[1]));
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn hysteresis_is_per_repo() {
    let cfg = primary("{}");
    let ledger = DemandLedger::default();
    let roots = repos(2);
    let mut b = BuildBackoffs::default();
    let mut tick = |a: usize, c: usize| {
        debt(&ledger, &roots[0], a);
        debt(&ledger, &roots[1], c);
        b.step(cfg.path(), &roots, &ledger).per_workspace
    };
    assert_eq!(tick(41, 30), vec![true, false], "A engages, B inside the band stays released");
    assert_eq!(tick(30, 41), vec![true, true], "A holds inside the band, B engages");
    assert_eq!(tick(24, 30), vec![false, true], "A releases below low, B holds");
    assert_eq!(tick(40, 24), vec![false, false]);
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn the_host_ceiling_is_off_by_default() {
    // 4 repos at 30 each: 120 host-wide, which engaged the pre-#10624 back-off.
    let cfg = primary("{}");
    let ledger = DemandLedger::default();
    let roots = repos(4);
    roots.iter().for_each(|r| debt(&ledger, r, 30));
    let mut b = BuildBackoffs::default();
    let holds = b.step(cfg.path(), &roots, &ledger);
    assert!(!holds.any(), "{holds}");
    assert_eq!(ledger.reads(), 4, "one read per repo, none for the host");
    assert_eq!(BuildBackoffConfig::parse(&json!({})).config.host, None);
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn an_engaged_host_ceiling_holds_every_repo_and_releases_below_host_low() {
    let cfg = primary(r#"{"hostHigh": 50, "hostLow": 20}"#);
    let ledger = DemandLedger::default();
    let roots = repos(3);
    debt(&ledger, &roots[0], 30);
    debt(&ledger, &roots[1], 30);
    debt(&ledger, &roots[2], 0);
    let mut b = BuildBackoffs::default();
    let holds = b.step(cfg.path(), &roots, &ledger);
    assert_eq!(holds.per_workspace, vec![true; 3], "60 > hostHigh=50 holds every repo");
    assert_eq!((holds.repos_engaged, holds.host), (0, true));
    assert_eq!(holds.to_string(), "0/3 repos + host ceiling");

    debt(&ledger, &roots[0], 15);
    assert!(b.step(cfg.path(), &roots, &ledger).host, "45 >= hostLow holds");
    debt(&ledger, &roots[1], 4);
    assert!(!b.step(cfg.path(), &roots, &ledger).any(), "19 < hostLow releases");

    // Unsetting the ceiling while engaged releases it.
    debt(&ledger, &roots[0], 60);
    assert!(b.step(cfg.path(), &roots, &ledger).host);
    let off = primary("{}");
    let holds = b.step(off.path(), &roots, &ledger);
    assert!(!holds.host);
    assert_eq!(holds.per_workspace, vec![true, false, false], "A's own limit still holds");
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn a_crossed_or_half_set_host_pair_leaves_the_ceiling_off() {
    for (block, pair) in [
        (json!({"hostHigh": 30, "hostLow": 30}), (Some(30), Some(30))),
        (json!({"hostHigh": 20, "hostLow": 50}), (Some(20), Some(50))),
        (json!({"hostHigh": 80}), (Some(80), None)),
        (json!({"hostLow": 10}), (None, Some(10))),
        (json!({"hostHigh": 0, "hostLow": 10}), (None, Some(10))),
    ] {
        let p = BuildBackoffConfig::parse(&block);
        assert_eq!(p.config.host, None, "{block}");
        assert_eq!(p.rejected_host_pair, Some(pair), "{block}");
        assert_eq!((p.config.high, p.config.low), (40, 25), "per-repo pair unaffected");
    }
    let p = BuildBackoffConfig::parse(&json!({"hostHigh": 120, "hostLow": 60}));
    assert_eq!(p.config.host, Some(HostCeiling { high: 120, low: 60 }));
    assert_eq!(p.rejected_host_pair, None);
    // A crossed per-repo pair keeps a valid host ceiling.
    let p = BuildBackoffConfig::parse(&json!({"high": 5, "low": 9, "hostHigh": 9, "hostLow": 5}));
    assert_eq!(p.config.host, Some(HostCeiling { high: 9, low: 5 }));
    assert_eq!(p.rejected_pair, Some((5, 9)));

    // `step` warns once per distinct bad host pair and runs with no ceiling.
    let cfg = primary(r#"{"hostHigh": 10, "hostLow": 50}"#);
    let ledger = DemandLedger::default();
    let roots = repos(2);
    roots.iter().for_each(|r| debt(&ledger, r, 30));
    let mut b = BuildBackoffs::default();
    assert!(!b.step(cfg.path(), &roots, &ledger).any());
    assert_eq!(b.warned_host, Some((Some(10), Some(50))));
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn a_departed_root_loses_its_state() {
    let cfg = primary("{}");
    let ledger = DemandLedger::default();
    let roots = repos(2);
    debt(&ledger, &roots[0], 60);
    let mut b = BuildBackoffs::default();
    assert!(b.step(cfg.path(), &roots, &ledger).per_workspace[0]);
    let holds = b.step(cfg.path(), &roots[1..], &ledger);
    assert_eq!(holds.per_workspace, vec![false]);
    assert!(!b.repo_held(&roots[0]));
}

#[test]
fn host_edge_lines_name_the_host_ceiling() {
    let d = DebtReading {
        total: 130,
        review: Some(30),
        changes: Some(60),
        merge: Some(40),
    };
    let scope = Scope::Host(HostCeiling { high: 120, low: 60 });
    let line = Edge::Engaged(d).log_line(&scope);
    for part in [
        "the host ceiling",
        "ENGAGED",
        "host-wide",
        "hostHigh=120",
        "hostLow=60",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
    assert!(line.contains("in every repo"), "{line}");
}

#[test]
fn deferrals_are_broken_down_per_repo() {
    use crate::types::QueueDisposition as Qd;
    let roots = repos(2);
    let row = |ws: usize, n: u32, d: Qd| TickQueueRow {
        key: PriorityCandidate {
            workspace_idx: ws,
            number: n,
            ..PriorityCandidate::default()
        },
        tier: None,
        disposition: Some(d),
        detail: None,
        updated_at: None,
        held_until: None,
        story_points: None,
    };
    let queue = vec![
        row(1, 1, Qd::DeferredBuildBackoff),
        row(0, 2, Qd::DeferredBuildBackoff),
        row(1, 3, Qd::DeferredBuildBackoff),
        row(0, 4, Qd::Dispatched),
    ];
    let by_repo = deferred_by_repo(&queue, &roots);
    let name = |i: usize| roots[i].display().to_string();
    assert_eq!(by_repo, vec![(name(1), 2), (name(0), 1)]);
    let mut b = BuildBackoffs::default();
    b.log_deferred(&queue, &roots);
    assert_eq!(b.last_deferred, vec![name(1), name(0)]);
}

#[test]
#[serial_test::serial] // reads the effective config (LOOM_HYPERPARAMS)
fn a_crossed_hyperparams_overlay_keeps_the_host_ceiling_and_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(
        tmp.path().join(".loom").join("config.json"),
        r#"{"hyperparameters": {"rework": {"buildBackoffLow": 100}},
            "autonomous": {"workFinder": {"buildBackoff":
              {"enabled": false, "high": 60, "low": 10, "hostHigh": 200, "hostLow": 90}}}}"#,
    )
    .unwrap();
    let p = BuildBackoffConfig::read(tmp.path());
    assert_eq!((p.config.high, p.config.low), (DEFAULT_HIGH, DEFAULT_LOW));
    assert_eq!(p.rejected_pair, Some((60, 100)));
    assert!(!p.config.enabled);
    assert_eq!(p.config.host, Some(HostCeiling { high: 200, low: 90 }));
}
