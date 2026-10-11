use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};

use super::*;
use crate::telemetry::ops::{MetricKind, MetricValue};
use crate::work_finder::WorkItem;

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn row(repo: &str, issue: u32, d: QueueDisposition, updated: Option<&str>) -> DwellRow {
    DwellRow {
        repo: repo.to_string(),
        issue,
        disposition: d,
        updated_at: updated.map(t),
    }
}

fn int(p: &MetricPoint) -> i64 {
    match p.value {
        MetricValue::Int(v) => v,
        MetricValue::Double(_) => panic!("expected an int point"),
    }
}

fn find<'a>(
    points: &'a [MetricPoint],
    name: MetricName,
    key: &str,
    value: &str,
) -> Option<&'a MetricPoint> {
    points
        .iter()
        .find(|p| p.name == name && p.labels.get(key).map(String::as_str) == Some(value))
}

#[test]
fn metric_names_kinds_and_units() {
    assert_eq!(MetricName::QueueOldestWait.as_str(), "loom.queue.oldest_wait");
    assert_eq!(MetricName::QueueStarved.as_str(), "loom.queue.starved");
    assert_eq!(MetricName::QueueStarvedByReason.as_str(), "loom.queue.starved.by_reason");
    assert_eq!(MetricName::QueueDispatchWait.as_str(), "loom.queue.dispatch_wait");
    assert_eq!(
        MetricName::QueueDispatchWaitSamples.as_str(),
        "loom.queue.dispatch_wait.samples"
    );
    assert_eq!(MetricName::QueueOldestWait.kind(), MetricKind::Gauge);
    assert_eq!(MetricName::QueueStarved.kind(), MetricKind::Gauge);
    assert_eq!(MetricName::QueueStarvedByReason.kind(), MetricKind::Gauge);
    assert_eq!(MetricName::QueueDispatchWait.kind(), MetricKind::DeltaCounter);
    assert_eq!(MetricName::QueueDispatchWaitSamples.kind(), MetricKind::DeltaCounter);
    assert_eq!(MetricName::QueueOldestWait.unit(), "s");
    assert_eq!(MetricName::QueueDispatchWait.unit(), "s");
    assert_eq!(MetricName::QueueStarved.unit(), "{issue}");
    // serde name matches as_str, so a restored on-disk queue round-trips.
    let json = serde_json::to_string(&MetricName::QueueStarvedByReason).unwrap();
    assert_eq!(json, "\"loom.queue.starved.by_reason\"");
}

#[test]
fn wait_class_splits_ready_blocked_and_not_waiting() {
    use QueueDisposition as D;
    assert_eq!(wait_class(D::DeferredCapacity), Some(WaitClass::Ready));
    assert_eq!(wait_class(D::DeferredOutOfSlice), Some(WaitClass::Ready));
    assert_eq!(wait_class(D::WorkspaceHalted), Some(WaitClass::Blocked));
    assert_eq!(wait_class(D::DispatchBackoff), Some(WaitClass::Blocked));
    for d in [
        D::Dispatched,
        D::InFlight,
        D::Parked,
        D::HardExclusion,
        D::Declined,
        D::HostConstraint,
        D::PeerClaim,
        D::OpenPr,
        D::OpenPrBackoff,
        D::RecheckInterval,
        D::Unknown,
    ] {
        assert_eq!(wait_class(d), None, "{d:?}");
    }
}

#[test]
fn clock_seeds_from_updated_at_and_stays_fixed() {
    let mut tracker = DwellTracker::default();
    let now = t("2026-09-25T12:00:00Z");
    let rows = [row(
        "/r",
        1,
        QueueDisposition::DeferredCapacity,
        Some("2026-09-25T10:00:00Z"),
    )];
    let obs = tracker.observe(&rows, &[], now);
    assert_eq!(obs.waiting[0].secs, 2 * 3600);

    // A later comment bumps updatedAt; the clock keeps its first seed.
    let rows = [row(
        "/r",
        1,
        QueueDisposition::DeferredCapacity,
        Some("2026-09-25T12:30:00Z"),
    )];
    let obs = tracker.observe(&rows, &[], now + Duration::hours(1));
    assert_eq!(obs.waiting[0].secs, 3 * 3600);
}

#[test]
fn missing_or_future_updated_at_seeds_now() {
    let mut tracker = DwellTracker::default();
    let now = t("2026-09-25T12:00:00Z");
    let rows = [
        row("/r", 1, QueueDisposition::DeferredCapacity, None),
        row("/r", 2, QueueDisposition::DeferredCapacity, Some("2026-09-25T13:00:00Z")),
    ];
    let obs = tracker.observe(&rows, &[], now);
    assert!(obs.waiting.iter().all(|w| w.secs == 0));
}

#[test]
fn clock_dropped_when_issue_leaves_or_stops_waiting() {
    let mut tracker = DwellTracker::default();
    let now = t("2026-09-25T12:00:00Z");
    let seed = Some("2026-09-25T08:00:00Z");
    tracker.observe(
        &[
            row("/r", 1, QueueDisposition::DeferredCapacity, seed),
            row("/r", 2, QueueDisposition::DeferredCapacity, seed),
        ],
        &[],
        now,
    );
    assert_eq!(tracker.len(), 2);
    // #1 leaves the listing, #2 gets parked: both clocks go.
    let obs = tracker.observe(&[row("/r", 2, QueueDisposition::Parked, seed)], &[], now);
    assert!(obs.waiting.is_empty());
    assert!(tracker.is_empty());
    // Unparked later: a fresh clock from the (new) updatedAt.
    let later = now + Duration::hours(1);
    let obs = tracker.observe(
        &[row(
            "/r",
            2,
            QueueDisposition::DeferredCapacity,
            Some("2026-09-25T12:30:00Z"),
        )],
        &[],
        later,
    );
    assert_eq!(obs.waiting[0].secs, 30 * 60);
}

#[test]
fn failed_listing_keeps_that_repos_clocks() {
    let mut tracker = DwellTracker::default();
    let now = t("2026-09-25T12:00:00Z");
    let seed = Some("2026-09-25T08:00:00Z");
    tracker.observe(
        &[
            row("/a", 1, QueueDisposition::DeferredCapacity, seed),
            row("/b", 1, QueueDisposition::DeferredCapacity, seed),
        ],
        &[],
        now,
    );
    // /a's listing failed (no rows); /b listed empty.
    tracker.observe(&[], &["/a".to_string()], now);
    assert_eq!(tracker.len(), 1);
    let obs = tracker.observe(
        &[row(
            "/a",
            1,
            QueueDisposition::DeferredCapacity,
            Some("2026-09-25T12:00:00Z"),
        )],
        &[],
        now + Duration::hours(1),
    );
    assert_eq!(obs.waiting[0].secs, 5 * 3600, "clock survived the failed listing");
}

#[test]
fn dispatch_records_wait_and_drops_clock() {
    let mut tracker = DwellTracker::default();
    let now = t("2026-09-25T12:00:00Z");
    tracker.observe(
        &[row(
            "/r",
            1,
            QueueDisposition::DeferredCapacity,
            Some("2026-09-25T11:00:00Z"),
        )],
        &[],
        now,
    );
    let obs = tracker.observe(
        &[
            row("/r", 1, QueueDisposition::Dispatched, Some("2026-09-25T12:05:00Z")),
            // Dispatched at first sight: lower bound from updatedAt.
            row("/r", 2, QueueDisposition::Dispatched, Some("2026-09-25T12:50:00Z")),
        ],
        &[],
        now + Duration::hours(1),
    );
    assert_eq!(obs.dispatch_waits, vec![2 * 3600, 10 * 60]);
    assert!(tracker.is_empty());
    // Still listed as in-flight next tick (stale cache): no clock restarts.
    tracker.observe(&[row("/r", 1, QueueDisposition::InFlight, None)], &[], now);
    assert!(tracker.is_empty());
}

#[test]
fn points_oldest_starved_by_reason_and_dispatch_wait() {
    let obs = DwellObservation {
        waiting: vec![
            Waiting {
                class: WaitClass::Ready,
                disposition: QueueDisposition::DeferredCapacity,
                secs: 100,
            },
            Waiting {
                class: WaitClass::Ready,
                disposition: QueueDisposition::DeferredCapacity,
                secs: 900,
            },
            Waiting {
                class: WaitClass::Ready,
                disposition: QueueDisposition::DeferredSaturation,
                secs: 700,
            },
            Waiting {
                class: WaitClass::Blocked,
                disposition: QueueDisposition::DispatchBackoff,
                secs: 50,
            },
        ],
        dispatch_waits: vec![30, 90],
    };
    let points = points(&obs, 600);
    assert_eq!(int(find(&points, MetricName::QueueOldestWait, "state", "ready").unwrap()), 900);
    assert_eq!(int(find(&points, MetricName::QueueOldestWait, "state", "blocked").unwrap()), 50);
    assert_eq!(int(find(&points, MetricName::QueueStarved, "state", "ready").unwrap()), 2);
    assert_eq!(int(find(&points, MetricName::QueueStarved, "state", "blocked").unwrap()), 0);
    assert_eq!(
        int(find(&points, MetricName::QueueStarvedByReason, "reason", "deferred_capacity").unwrap()),
        1
    );
    assert_eq!(
        int(find(&points, MetricName::QueueStarvedByReason, "reason", "deferred_saturation")
            .unwrap()),
        1
    );
    assert!(find(&points, MetricName::QueueStarvedByReason, "reason", "dispatch_backoff").is_none());
    let sum = points
        .iter()
        .find(|p| p.name == MetricName::QueueDispatchWait)
        .unwrap();
    let samples = points
        .iter()
        .find(|p| p.name == MetricName::QueueDispatchWaitSamples)
        .unwrap();
    assert_eq!((int(sum), int(samples)), (120, 2));
    // Only allowlisted label keys, so nothing is dropped at export.
    for p in &points {
        assert_eq!(crate::telemetry::ops::bounded_labels(&p.labels), p.labels);
    }
}

#[test]
fn empty_queue_still_reports_zero_starved_for_both_states() {
    let points = points(&DwellObservation::default(), 600);
    assert_eq!(points.len(), 2);
    assert!(points
        .iter()
        .all(|p| p.name == MetricName::QueueStarved && int(p) == 0));
}

#[test]
fn starvation_threshold_parsing() {
    assert_eq!(starvation_secs(None), DEFAULT_STARVATION_SECS);
    assert_eq!(starvation_secs(Some("3600")), 3600);
    assert_eq!(starvation_secs(Some(" 60 ")), 60);
    assert_eq!(starvation_secs(Some("0")), DEFAULT_STARVATION_SECS);
    assert_eq!(starvation_secs(Some("-5")), DEFAULT_STARVATION_SECS);
    assert_eq!(starvation_secs(Some("6h")), DEFAULT_STARVATION_SECS);
}

#[test]
fn rows_from_report_carry_repo_and_parsed_updated_at() {
    let mut report = crate::work_finder::TickReport::default();
    let item = WorkItem::with_created_at(7, vec![], Some("2026-09-01T00:00:00Z".into()))
        .with_updated_at(Some("2026-09-25T10:00:00Z".into()));
    let key = ready_queue::key_of(0, 100, &item);
    ready_queue::record_skip(
        &mut report.queue,
        key.clone(),
        &item,
        QueueDisposition::DispatchBackoff,
        None,
    );
    ready_queue::record_candidate(
        &mut report.queue,
        &key,
        &item.clone().with_updated_at(Some("garbage".into())),
    );
    let rows = rows_from_report(&report, &[PathBuf::from("/repo")]);
    assert_eq!(rows[0].repo, "/repo");
    assert_eq!(rows[0].issue, 7);
    assert_eq!(rows[0].disposition, QueueDisposition::DispatchBackoff);
    assert_eq!(rows[0].updated_at, Some(t("2026-09-25T10:00:00Z")));
    // Unresolved candidate reads as a capacity deferral; bad timestamp is None.
    assert_eq!(rows[1].disposition, QueueDisposition::DeferredCapacity);
    assert_eq!(rows[1].updated_at, None);
}

#[test]
fn record_tick_without_sink_is_a_noop() {
    // No ops sink is registered in unit tests: nothing panics, nothing emits.
    record_tick(&crate::work_finder::TickReport::default(), &[PathBuf::from("/r")], Utc::now());
}

/// The committed SigNoz alert rule queries names this module actually emits,
/// on the `ready` state, above zero.
#[test]
fn committed_starvation_alert_matches_emitted_names() {
    let rule: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../defaults/observability/signoz/alerts/queue-starvation.json"
    ))
    .unwrap();
    let condition = &rule["condition"];
    let query = condition["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .unwrap();
    assert!(query.contains(&format!("'{}'", MetricName::QueueStarved.as_str())));
    assert!(query.contains(&format!("'state') = '{}'", WaitClass::Ready.as_str())));
    assert_eq!(condition["op"], "1", "above");
    assert_eq!(condition["target"], 0);
    let sql = include_str!("../../../../../defaults/observability/signoz/queue-dwell.sql");
    for name in [
        MetricName::QueueStarved,
        MetricName::QueueStarvedByReason,
        MetricName::QueueOldestWait,
        MetricName::QueueDispatchWait,
        MetricName::QueueDispatchWaitSamples,
    ] {
        assert!(sql.contains(&format!("'{}'", name.as_str())), "{name:?}");
    }
}
