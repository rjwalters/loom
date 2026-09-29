//! `sweep.started`'s dispatch-time story-points carriage (Issue #9432),
//! in its own sibling module so `tests.rs` stays under the file-size
//! ratchet (`.loom/docs/file-size-policy.md`).

use super::tests::{dispatch_event, exited_event};
use super::*;

/// Issue #9432: the dispatch event carries the issue's resolved `points:<N>`
/// estimate, and `sweep.started` must carry it through — absent (never `0`)
/// when the guard declined to resolve one. The paired terminal record keeps
/// the same value off the tracked dispatch state.
#[test]
fn sweep_started_carries_the_dispatch_story_points() {
    let mut dispatches = HashMap::new();
    let event = Event::SweepGlobalDispatch {
        sweep_id: "sweep-issue-7-0".to_string(),
        kind: SweepKind::Issue(7),
        runtime: None,
        runtime_source: None,
        repo: Some("/repos/loom".to_string()),
        story_points: Some(8),
    };
    let records =
        map_event_to_records(&event, 7, "rjwalters/loom", RepoVisibility::Public, &mut dispatches);
    match &records[0] {
        TelemetryRecord::SweepStarted(r) => assert_eq!(r.story_points, Some(8)),
        other => panic!("expected sweep.started, got {other:?}"),
    }

    // An unresolved estimate (no label / multiple labels / read failed)
    // serializes NO key — the attribute is absent, never zero.
    let records = map_event_to_records(
        &dispatch_event(8, "sweep-issue-8-0"),
        8,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );
    match &records[0] {
        TelemetryRecord::SweepStarted(r) => {
            assert_eq!(r.story_points, None);
            let value = serde_json::to_value(r).unwrap();
            assert!(
                value.get("story_points").is_none(),
                "an unresolved estimate must omit the key entirely: {value}"
            );
        }
        other => panic!("expected sweep.started, got {other:?}"),
    }

    // The tracked dispatch state keeps the resolved value so the event-path
    // `sweep.outcome` mirror reports the same estimate the journal will.
    let exit = exited_event(7, Some(0), 30);
    let records =
        map_event_to_records(&exit, 7, "rjwalters/loom", RepoVisibility::Public, &mut dispatches);
    match &records[1] {
        TelemetryRecord::SweepOutcome(r) => assert_eq!(r.story_points, Some(8)),
        other => panic!("expected sweep.outcome, got {other:?}"),
    }
}
