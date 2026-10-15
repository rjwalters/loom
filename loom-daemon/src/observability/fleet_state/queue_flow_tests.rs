//! `annotate_flow` wiring: per-repo `queue_depth` and arrival/drain flow on a
//! seeded ready queue (#11645).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{
    annotate_flow, FleetView, QueueFlowTracker, ReadyItem, ReadyQueue, QUEUE_FLOW_WINDOW_SECS,
};

const REPO: &str = "acme/a";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 12, 0, 0).unwrap()
}

fn ready(issue: u32) -> ReadyItem {
    ReadyItem {
        repo: REPO.to_string(),
        issue,
        rank: issue,
        level: 0,
        weight: 100,
        created_at: None,
        main_red_fix: false,
        eligible_reason: None,
        eligible_at: None,
    }
}

fn one(repo: &str) -> BTreeSet<String> {
    [repo.to_string()].into_iter().collect()
}

/// A complete ready listing drives `queue_depth` (its size) and the flow
/// counts (its diff across ticks), on a seeded queue.
#[test]
fn annotate_flow_tracks_depth_arrivals_and_drains() {
    let rq = |items: Vec<ReadyItem>| ReadyQueue {
        listed: items.iter().map(|i| i.repo.clone()).collect(),
        complete: items.iter().map(|i| i.repo.clone()).collect(),
        items,
        slots: None,
        halts: BTreeMap::new(),
        limits: BTreeMap::new(),
    };
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);

    // Pass 1: baseline — issues 5 and 6 waiting.
    let mut v1 = FleetView::default();
    let q1 = rq(vec![ready(5), ready(6)]);
    annotate_flow(&mut v1, &mut tracker, Some(&q1), t0());
    let s1 = v1.repos.get(REPO).unwrap();
    assert_eq!(s1.queue_depth, Some(2));
    let f1 = s1.flow.unwrap();
    assert_eq!((f1.arrivals, f1.drains), (0, 0), "first sight seeds, no counts");
    assert_eq!(f1.since, t0());

    // Pass 2 (ten minutes on): 5 drained, 7 and 8 arrived, 6 stayed.
    let mut v2 = FleetView::default();
    let later = t0() + Duration::minutes(10);
    let q2 = rq(vec![ready(6), ready(7), ready(8)]);
    annotate_flow(&mut v2, &mut tracker, Some(&q2), later);
    let s2 = v2.repos.get(REPO).unwrap();
    assert_eq!(s2.queue_depth, Some(3));
    let f2 = s2.flow.unwrap();
    assert_eq!((f2.arrivals, f2.drains), (2, 1), "7,8 in; 5 out");
    assert_eq!(f2.since, t0(), "window start is stable");

    // A complete repo with an empty queue reports depth 0, not unknown.
    let mut v3 = FleetView::default();
    let q3 = ReadyQueue {
        items: Vec::new(),
        listed: one(REPO),
        complete: one(REPO),
        slots: None,
        halts: BTreeMap::new(),
        limits: BTreeMap::new(),
    };
    annotate_flow(&mut v3, &mut tracker, Some(&q3), t0() + Duration::minutes(20));
    assert_eq!(v3.repos.get(REPO).unwrap().queue_depth, Some(0));
}

/// A repo whose ready listing came back partial is not observed: its depth and
/// flow stay unknown rather than reading the missing rows as drains.
#[test]
fn annotate_flow_skips_a_repo_that_was_not_read_whole() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    let mut view = FleetView::default();
    let q = ReadyQueue {
        items: vec![ready(5)],
        listed: one(REPO),
        // Read, but not known whole.
        complete: BTreeSet::new(),
        slots: None,
        halts: BTreeMap::new(),
        limits: BTreeMap::new(),
    };
    annotate_flow(&mut view, &mut tracker, Some(&q), t0());
    assert!(view.repos.get(REPO).and_then(|r| r.queue_depth).is_none());
    assert!(view.repos.get(REPO).and_then(|r| r.flow).is_none());
}
