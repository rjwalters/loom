//! `annotate_triage` wiring: per-repo `triage_depth` and arrival/drain flow
//! (the Curator drain rate numerator) on a seeded `loom:triage` queue (#11642).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::flow::annotate_triage;
use super::{FleetView, QueueFlowTracker, QUEUE_FLOW_WINDOW_SECS};

const REPO: &str = "acme/a";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 12, 0, 0).unwrap()
}

fn sets(issues: &[u32]) -> BTreeMap<String, BTreeSet<u32>> {
    [(REPO.to_string(), issues.iter().copied().collect())]
        .into_iter()
        .collect()
}

/// A complete triage listing drives `triage_depth` (its size) and the flow
/// counts (its diff across collector passes), on a seeded queue. `drains` is
/// the Curator drain-rate numerator a reader divides by the window.
#[test]
fn annotate_triage_tracks_depth_arrivals_and_drains() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);

    // Pass 1: baseline — issues 10, 11, 12 waiting in loom:triage.
    let mut v1 = FleetView::default();
    annotate_triage(&mut v1, &mut tracker, Some(&sets(&[10, 11, 12])), t0());
    let s1 = v1.repos.get(REPO).unwrap();
    assert_eq!(s1.triage_depth, Some(3));
    let f1 = s1.triage_flow.unwrap();
    assert_eq!((f1.arrivals, f1.drains), (0, 0), "first sight seeds, no counts");
    assert_eq!(f1.since, t0());

    // Pass 2 (an hour on): 10 and 11 curated/closed (left triage), 13 filed.
    let mut v2 = FleetView::default();
    let later = t0() + Duration::hours(1);
    annotate_triage(&mut v2, &mut tracker, Some(&sets(&[12, 13])), later);
    let s2 = v2.repos.get(REPO).unwrap();
    assert_eq!(s2.triage_depth, Some(2));
    let f2 = s2.triage_flow.unwrap();
    assert_eq!(f2.arrivals, 1, "13 filed");
    assert_eq!(f2.drains, 2, "10, 11 drained — the curator_rate numerator");
    assert_eq!(f2.window_secs, u64::try_from(QUEUE_FLOW_WINDOW_SECS).unwrap());
    assert_eq!(f2.since, t0(), "window start is stable");

    // An empty, completely-read triage queue reports depth 0, not unknown.
    let mut v3 = FleetView::default();
    annotate_triage(&mut v3, &mut tracker, Some(&sets(&[])), later + Duration::hours(1));
    assert_eq!(v3.repos.get(REPO).unwrap().triage_depth, Some(0));
}

/// A tick pass (no forge read, `triage: None`) re-stamps the depth and flow
/// from the tracker's last collector-pass observation, so the facts persist
/// unchanged between the collector's forge passes.
#[test]
fn a_tick_pass_carries_the_last_observed_triage_facts() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    // A collector pass observed a depth-2 queue with one arrival counted.
    let mut collector = FleetView::default();
    annotate_triage(&mut collector, &mut tracker, Some(&sets(&[1, 2])), t0());
    annotate_triage(
        &mut collector,
        &mut tracker,
        Some(&sets(&[1, 2, 3])),
        t0() + Duration::minutes(10),
    );
    let observed = collector.repos.get(REPO).unwrap().clone();
    assert_eq!(observed.triage_depth, Some(3));

    // A later tick pass reads no triage listing (None): it must re-stamp the
    // same depth and flow, not drop them to unknown.
    let mut tick = FleetView::default();
    annotate_triage(&mut tick, &mut tracker, None, t0() + Duration::minutes(11));
    let s = tick.repos.get(REPO).unwrap();
    assert_eq!(s.triage_depth, Some(3), "last observed depth persists");
    assert_eq!(s.triage_flow.unwrap().arrivals, 1, "flow persists");
}

/// A repo this host has never observed a triage listing for stays unknown —
/// absent, never zero.
#[test]
fn an_unobserved_repo_stays_unknown() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    let mut view = FleetView::default();
    annotate_triage(&mut view, &mut tracker, None, t0());
    assert!(!view.repos.contains_key(REPO), "nothing stamped");
}
