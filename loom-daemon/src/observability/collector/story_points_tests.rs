//! `sweep.started` story-point plumbing tests (Issue #9432, epic #9429) — in
//! their own sibling module because `tests.rs` is pinned by the file-size
//! ratchet (`scripts/check-file-size-budget.sh`).
//!
//! The collector makes no forge calls of its own, so everything it can know
//! about an issue's size arrives on the `sweep.global.dispatch` event (resolved
//! there from the label list the #4444 park-label guard already read). These
//! tests pin that pass-through, including that an unsized dispatch produces an
//! **absent** value rather than a `0`.

use super::*;
use crate::telemetry::TelemetryRecord;

fn dispatch_event(issue: u32, story_points: Option<u32>) -> Event {
    Event::SweepGlobalDispatch {
        sweep_id: format!("sweep-issue-{issue}-0"),
        kind: SweepKind::Issue(issue),
        runtime: None,
        runtime_source: None,
        repo: Some("/repos/loom".to_string()),
        story_points,
    }
}

fn started_story_points(issue: u32, story_points: Option<u32>) -> Option<u32> {
    let mut dispatches = HashMap::new();
    let records = map_event_to_records(
        &dispatch_event(issue, story_points),
        issue,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );
    match &records[0] {
        TelemetryRecord::SweepStarted(r) => r.story_points,
        other => panic!("expected sweep.started, got {other:?}"),
    }
}

/// A sized dispatch reaches `sweep.started` unchanged, for every value in the
/// closed vocabulary.
#[test]
fn sweep_started_carries_the_dispatch_story_points() {
    for points in [1_u32, 2, 3, 5, 8, 13] {
        assert_eq!(started_story_points(9432, Some(points)), Some(points), "{points}");
    }
}

/// An unsized issue (or one whose points labels were defective, or whose label
/// read was skipped — all of which resolve to `None` at dispatch) produces an
/// absent value here. Never `Some(0)`.
#[test]
fn an_unsized_dispatch_carries_no_story_points() {
    assert_eq!(started_story_points(9432, None), None);
}
