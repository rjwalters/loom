//! Outcome-coverage gauge tests (Issue #10933, slice 1).

use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::eta::score::{score, Score};
use crate::eta::{Kind, NoEstimateReason, Provenance, Stage};
use crate::telemetry::ops::{MetricValue, OPS_METRIC_LABEL_KEYS};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

/// A pending estimate of `issue` by `heuristic`, made `hours_ago` before
/// [`now`]; a refusal when `refused`.
fn est(issue: u32, heuristic: &str, hours_ago: i64, refused: bool) -> EstimateSummary {
    EstimateSummary {
        estimate_id: format!("{issue}-{heuristic}-{hours_ago}"),
        kind: Kind::Land,
        heuristic: heuristic.to_string(),
        loom: Provenance {
            version: "0.19.900".into(),
            revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
            tree_state: "clean".into(),
            complete: true,
        },
        repo: "rjwalters/loom".into(),
        repo_id: Some(1),
        issue,
        pr_number: None,
        as_of: now() - Duration::hours(hours_ago),
        stage: Some(Stage::ReviewWait),
        age_sec: None,
        p25_sec: (!refused).then_some(600),
        p50_sec: (!refused).then_some(1200),
        p75_sec: (!refused).then_some(2400),
        p90_sec: (!refused).then_some(3600),
        samples_min: (!refused).then_some(9),
        no_estimate_reason: refused.then_some(NoEstimateReason::NoModel),
        stage_quartiles: Vec::new(),
        tail_extrapolated: false,
        stall_cause: None,
        stage_predictions: Default::default(),
    }
}

fn resolved(estimate: EstimateSummary, outcome: OutcomeKind) -> Resolved {
    let s: Score = score(&estimate, outcome, now(), &[]);
    Resolved {
        estimate,
        score: s,
        outcome_source: "bus".into(),
        outcome_resolution_sec: None,
        result: None,
    }
}

fn value(p: &MetricPoint) -> i64 {
    match p.value {
        MetricValue::Int(v) => v,
        MetricValue::Double(_) => panic!("expected int"),
    }
}

fn label<'a>(p: &'a MetricPoint, key: &str) -> &'a str {
    p.labels.get(key).map_or("", String::as_str)
}

fn of(points: &[MetricPoint], name: MetricName) -> Vec<&MetricPoint> {
    points.iter().filter(|p| p.name == name).collect()
}

#[test]
fn age_buckets_split_at_their_bounds() {
    assert_eq!(age_bucket(0), "lt_4h");
    assert_eq!(age_bucket(4 * 3600 - 1), "lt_4h");
    assert_eq!(age_bucket(4 * 3600), "4h_24h");
    assert_eq!(age_bucket(86_400), "1d_3d");
    assert_eq!(age_bucket(3 * 86_400), "3d_7d");
    assert_eq!(age_bucket(7 * 86_400), "gt_7d");
    assert_eq!(age_bucket(400 * 86_400), "gt_7d");
}

#[test]
fn a_synthetic_store_buckets_series_by_their_earliest_estimate() {
    let pending = vec![
        // Series 1: refreshed every hour for 30 h. One series, aged 30 h,
        // not 31 estimates and not the newest refresh's 1 h.
        est(1, "land-v1", 30, false),
        est(1, "land-v1", 20, false),
        est(1, "land-v1", 1, false),
        // Series 2: 2 h old.
        est(2, "land-v1", 2, false),
        // Series 3: 10 days old, the oldest.
        est(3, "land-v1", 240, false),
        // Series 4: 2 days old, a different heuristic.
        est(4, "land-v2", 48, true),
    ];
    let ages = pending_ages(&pending, now());
    let key = |h: &str, b: &str| ("land".to_string(), h.to_string(), b.to_string());
    assert_eq!(ages.series.get(&key("land-v1", "1d_3d")), Some(&1));
    assert_eq!(ages.series.get(&key("land-v1", "lt_4h")), Some(&1));
    assert_eq!(ages.series.get(&key("land-v1", "gt_7d")), Some(&1));
    assert_eq!(ages.series.get(&key("land-v2", "1d_3d")), Some(&1));
    assert_eq!(ages.series.values().sum::<u64>(), 4, "series, not estimates");
    let oldest = |h: &str| {
        ages.oldest
            .get(&("land".to_string(), h.to_string()))
            .copied()
    };
    assert_eq!(oldest("land-v1"), Some(240 * 3600));
    assert_eq!(oldest("land-v2"), Some(48 * 3600));
}

#[test]
fn a_series_key_ignores_repo_case() {
    let mut upper = est(1, "land-v1", 5, false);
    upper.repo = "RJWalters/Loom".into();
    let ages = pending_ages(&[upper, est(1, "land-v1", 1, false)], now());
    assert_eq!(ages.series.values().sum::<u64>(), 1);
}

#[test]
fn pending_gauges_carry_their_labels_and_zero_an_emptied_bucket() {
    let mut memory = Memory::default();
    let first = gather(&memory, Some(pending_ages(&[est(1, "land-v1", 2, false)], now())));
    let p = points(&first);
    let pending = of(&p, MetricName::EtaHealthPending);
    assert_eq!(pending.len(), 1);
    assert_eq!(label(pending[0], "kind"), "land");
    assert_eq!(label(pending[0], "heuristic"), "land-v1");
    assert_eq!(label(pending[0], "age_bucket"), "lt_4h");
    assert_eq!(value(pending[0]), 1);
    let oldest = of(&p, MetricName::EtaHealthPendingOldestAgeSeconds);
    assert_eq!(value(oldest[0]), 2 * 3600);
    for point in &p {
        assert!(point
            .labels
            .keys()
            .all(|k| OPS_METRIC_LABEL_KEYS.contains(&k.as_str())));
    }
    memory.remember(&first);

    // The item resolves: the bucket is zeroed, the oldest age just stops.
    let second = gather(&memory, Some(PendingAges::default()));
    let p = points(&second);
    let pending = of(&p, MetricName::EtaHealthPending);
    assert_eq!(pending.len(), 1);
    assert_eq!(value(pending[0]), 0);
    assert!(of(&p, MetricName::EtaHealthPendingOldestAgeSeconds).is_empty());
}

#[test]
fn unknown_is_not_zero() {
    let memory = Memory::default();
    assert!(points(&gather(&memory, None)).is_empty(), "no tracker, no counts: no points");
}

#[test]
fn each_loss_path_lands_in_its_reason() {
    let mut memory = Memory::default();
    let expired = Expired {
        undecided: 5,
        cap_censored: 1,
        ..Expired::default()
    };
    let dropped = Dropped {
        orphaned: 3,
        over_cap: 10,
        series_over_cap: 4,
    };
    memory.pass(&expired, &dropped);
    memory.lost(LossReason::DroppedAuthority, 7);
    memory.lost(LossReason::RetiredHeuristic, 2);
    // A second pass adds to the cumulative counts.
    let orphan = Dropped {
        orphaned: 1,
        ..Dropped::default()
    };
    memory.pass(&Expired::default(), &orphan);
    let p = points(&gather(&memory, None));
    let lost: BTreeMap<&str, i64> = of(&p, MetricName::EtaHealthPendingLost)
        .into_iter()
        .map(|p| (label(p, "reason"), value(p)))
        .collect();
    assert_eq!(
        lost,
        BTreeMap::from([
            ("evicted_cap", 9), // 10 evicted, 1 censored on the way out
            ("dropped_authority", 7),
            ("expired_undecided", 5),
            ("orphaned_post_outcome", 4),
            ("retired_heuristic", 2),
        ])
    );
}

#[test]
fn a_pass_with_no_losses_reports_measured_zeros() {
    let mut memory = Memory::default();
    memory.pass(&Expired::default(), &Dropped::default());
    let p = points(&gather(&memory, None));
    let lost = of(&p, MetricName::EtaHealthPendingLost);
    assert_eq!(lost.len(), LossReason::ALL.len());
    assert!(lost.iter().all(|p| value(p) == 0));
}

#[test]
fn outcomes_are_counted_by_kind_heuristic_and_outcome_with_refusals_apart() {
    let mut memory = Memory::default();
    memory.outcomes(&[
        resolved(est(1, "land-v1", 5, false), OutcomeKind::Landed),
        resolved(est(1, "land-v1", 4, false), OutcomeKind::Landed),
        resolved(est(2, "land-v1", 5, false), OutcomeKind::Abandoned),
        resolved(est(3, "land-v1", 5, false), OutcomeKind::Censored),
        // A refusal resolves too, but is never scored: `refused`, whatever
        // happened to the item.
        resolved(est(4, "land-v1", 5, true), OutcomeKind::Landed),
    ]);
    let p = points(&gather(&memory, None));
    let outcomes: BTreeMap<(&str, &str, &str), i64> = of(&p, MetricName::EtaHealthOutcomes)
        .into_iter()
        .map(|p| ((label(p, "kind"), label(p, "heuristic"), label(p, "outcome")), value(p)))
        .collect();
    assert_eq!(
        outcomes,
        BTreeMap::from([
            (("land", "land-v1", "abandoned"), 1),
            (("land", "land-v1", "censored"), 1),
            (("land", "land-v1", "landed"), 2),
            (("land", "land-v1", "refused"), 1),
        ])
    );
}

#[test]
fn keep_emitted_takes_only_the_export_memory() {
    let mut global = Memory::default();
    let mut pass = global.clone();
    let facts = gather(&pass, Some(pending_ages(&[est(1, "land-v1", 1, false)], now())));
    pass.remember(&facts);
    // A writer counted a loss while the pass ran.
    global.lost(LossReason::EvictedCap, 3);
    global.keep_emitted(pass);
    let p = points(&gather(&global, Some(PendingAges::default())));
    assert_eq!(value(of(&p, MetricName::EtaHealthPending)[0]), 0, "the bucket is zeroed");
    let evicted = of(&p, MetricName::EtaHealthPendingLost)
        .into_iter()
        .find(|p| label(p, "reason") == "evicted_cap")
        .map(value);
    assert_eq!(evicted, Some(3), "the mid-pass loss survived");
}
