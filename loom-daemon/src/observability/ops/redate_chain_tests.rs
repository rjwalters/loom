//! Tests for the merge-chain re-date gauges (#10163).

use super::*;
use crate::merge_pr::redate::chain_telemetry::{chain_stats, TimedRedate};
use crate::telemetry::ops::MetricValue;
use chrono::DateTime;

fn r(sha: &str, pr: &str, at: &str) -> TimedRedate {
    TimedRedate {
        sha: sha.to_string(),
        pr: pr.to_string(),
        at: DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&Utc),
    }
}

/// The value of `name{state}` (or the unlabelled point when `state` is None).
fn value(points: &[MetricPoint], name: MetricName, state: Option<&str>) -> Option<i64> {
    points
        .iter()
        .find(|p| p.name == name && p.labels.get("state").map(String::as_str) == state)
        .map(|p| match p.value {
            MetricValue::Int(v) => v,
            MetricValue::Double(v) => v as i64,
        })
}

/// The 2026-10-04 shape: the chain head re-dated three times, unlanded, plus
/// one PR that landed after a single re-date.
fn livelock() -> Vec<ChainStat> {
    let commits = [
        r("h1", "9832", "2026-10-04T03:10:00Z"),
        r("h2", "9832", "2026-10-04T03:25:00Z"),
        r("h3", "9832", "2026-10-04T03:50:00Z"),
        r("o1", "10121", "2026-10-04T02:09:00Z"),
    ];
    chain_stats(&commits, |sha| {
        (sha == "o1").then(|| {
            DateTime::parse_from_rfc3339("2026-10-04T02:18:00Z")
                .unwrap()
                .with_timezone(&Utc)
        })
    })
}

#[test]
fn a_three_redate_unlanded_head_reads_as_stuck_on_the_gauges() {
    let points = points(&livelock());
    assert_eq!(value(&points, MetricName::MergeRedatePrs, Some("pending")), Some(1));
    assert_eq!(value(&points, MetricName::MergeRedatePrs, Some("stuck")), Some(1));
    assert_eq!(value(&points, MetricName::MergeRedatePrs, Some("landed")), Some(1));
    assert_eq!(value(&points, MetricName::MergeRedatesMax, Some("pending")), Some(3));
    assert_eq!(value(&points, MetricName::MergeRedatesMax, Some("landed")), Some(1));
    assert_eq!(value(&points, MetricName::MergeTimeToLandMax, None), Some(540));
}

#[test]
fn a_quiet_window_reads_zero_not_missing() {
    let points = points(&[]);
    for state in ["landed", "pending", "stuck"] {
        assert_eq!(value(&points, MetricName::MergeRedatePrs, Some(state)), Some(0), "{state}");
    }
    assert_eq!(value(&points, MetricName::MergeRedatesMax, Some("pending")), Some(0));
    // Nothing landed: no time-to-land point rather than a misleading zero.
    assert_eq!(value(&points, MetricName::MergeTimeToLandMax, None), None);
}

#[test]
fn every_point_survives_the_export_label_policy() {
    use crate::telemetry::ops::bounded_labels;
    for p in points(&livelock()) {
        assert_eq!(bounded_labels(&p.labels), p.labels, "{p:?}");
    }
}
