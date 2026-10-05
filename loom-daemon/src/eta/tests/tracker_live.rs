//! `live_items`: only in-flight work is current fleet state (#10196).
//! Entries retained for a pending outcome read keep their stage for ETA
//! scoring but are not exported.

use super::{as_of, provenance};
use crate::eta::tracker::{ItemKey, PrState, PrView, Tracker};
use chrono::{DateTime, Duration, Utc};

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

fn pr(number: u32, issue: u32) -> PrView {
    PrView {
        number,
        issue,
        labels: vec!["loom:review-requested".to_string()],
        created_at: Some(t(-7200)),
        updated_at: Some(t(-600)),
    }
}

#[test]
fn pre_pr_terminal_sweep_is_not_live_before_its_issue_read_answers() {
    let mut tracker = Tracker::new(provenance());
    tracker.on_dispatch(REPO, 70, "sweep-issue-70-1", t(0));
    assert_eq!(tracker.live_items().len(), 1, "a running sweep is live");

    let ended = tracker.on_terminal(REPO, 70, "exited", Some(0), t(100));
    assert!(ended.dirty.contains(&ItemKey::new(REPO, 70)), "the issue read is queued");
    assert!(
        tracker.live_items().is_empty(),
        "an ended sweep awaiting its issue read must not export its stage"
    );
    assert_eq!(tracker.item_keys(), vec![ItemKey::new(REPO, 70)], "the item is still tracked");
}

#[test]
fn pr_gone_from_a_complete_listing_is_not_live_while_its_read_is_deferred() {
    let mut tracker = Tracker::new(provenance());
    tracker.on_listing(REPO, &[pr(701, 71)], t(0), 300);
    let live = tracker.live_items();
    assert_eq!(live.len(), 1, "a PR under a review label is live");
    assert_eq!(live[0].pr, Some(701));

    // Gone from the listing: a pulls read is queued but never answered
    // (deferred over the forge read budget).
    let gone = tracker.on_listing(REPO, &[], t(600), 300);
    assert_eq!(gone.pr_checks, vec![(ItemKey::new(REPO, 71), 701)]);
    assert!(
        tracker.live_items().is_empty(),
        "the departed PR must not export review_wait/merge_wait"
    );
    assert_eq!(
        tracker.item_keys(),
        vec![ItemKey::new(REPO, 71)],
        "still retained for the outcome"
    );

    // Offered again, and it is still not live.
    let again = tracker.on_listing(REPO, &[], t(900), 300);
    assert_eq!(again.pr_checks, vec![(ItemKey::new(REPO, 71), 701)]);
    assert!(tracker.live_items().is_empty());

    // The read resolving open (just unlabelled) is not live either.
    tracker.on_pr_resolved(&ItemKey::new(REPO, 71), PrState::Open, t(1000));
    assert!(tracker.live_items().is_empty());
}
