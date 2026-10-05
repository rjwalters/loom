//! Star ownership and the removal rule (#10012 §3, AC 4, 5, 6).

use crate::star_liveness::edges::Inheritance;
use crate::star_liveness::inherited_star::{
    decide, marker, owner, parse_marker, Decision, InheritedMarker, Owner, RootState, StarEvent,
    StarKind,
};

fn ev(at: &str, kind: StarKind) -> StarEvent {
    StarEvent {
        at: at.to_string(),
        kind,
    }
}

const T1: &str = "2026-10-01T00:00:00Z";
const T2: &str = "2026-10-01T00:00:05Z";
const T3: &str = "2026-10-02T00:00:00Z";

fn inherited(root: u32) -> Vec<StarEvent> {
    // The daemon's label write, then its audit comment.
    vec![
        ev(T1, StarKind::Labeled),
        ev(T2, StarKind::Inherited { root }),
    ]
}

fn reached(root: u32) -> Inheritance {
    Inheritance {
        child: 11,
        via: root,
        root,
        starred_at: Some(T1.to_string()),
        depth: 1,
    }
}

#[test]
fn the_marker_round_trips_and_keeps_the_intent_shape() {
    let m = marker(10, 11, Some(T1));
    assert!(
        m.starts_with("<!-- loom:operator-priority-intent=inherit-10-11 action=star"),
        "{m}"
    );
    assert!(m.contains(&format!("requested_at={T1}")), "STARRED_AT_JQ reads it: {m}");
    assert_eq!(
        parse_marker(&format!("Inherited.\n{m}\n")),
        Some(InheritedMarker {
            root: 10,
            requested_at: Some(T1.to_string())
        })
    );
    assert_eq!(parse_marker(&marker(10, 11, None)).map(|p| p.root), Some(10));
    // A plain loom-ui intent is not an inherited marker.
    let ui =
        "<!-- loom:operator-priority-intent=abc action=star requested_at=2026-10-01T00:00:00Z -->";
    assert_eq!(parse_marker(ui), None);
}

#[test]
fn the_latest_star_event_decides_the_owner() {
    assert_eq!(owner(&inherited(10)), Owner::Inherited { root: 10 });
    assert_eq!(owner(&[ev(T1, StarKind::Labeled)]), Owner::Operator);
    assert_eq!(owner(&[]), Owner::Operator, "nothing proves we wrote it");
    // A human re-star after the daemon's: the operator's own.
    let mut restarred = inherited(10);
    restarred.push(ev(T3, StarKind::Labeled));
    assert_eq!(owner(&restarred), Owner::Operator);
    // A loom-ui / --direction intent after the inherited one: the operator's.
    let mut directed = inherited(10);
    directed.push(ev(T3, StarKind::Intent));
    assert_eq!(owner(&directed), Owner::Operator);
}

#[test]
fn unstarring_the_parent_removes_only_inherited_stars() {
    let unstarred = |_: u32| RootState::Unstarred;
    assert_eq!(decide(true, &inherited(10), None, unstarred), Decision::Remove { root: 10 });
    let direct = [ev(T1, StarKind::Labeled)];
    assert_eq!(
        decide(true, &direct, None, unstarred),
        Decision::Keep,
        "the operator's own star"
    );
    let ui = [ev(T1, StarKind::Labeled), ev(T2, StarKind::Intent)];
    assert_eq!(decide(true, &ui, None, unstarred), Decision::Keep);
    assert_eq!(decide(true, &inherited(10), None, |_| RootState::Unknown), Decision::Keep);
}

#[test]
fn a_closed_parent_that_is_still_starred_keeps_its_children_starred() {
    // Closed P is out of the open listing, so the child is not `reached`;
    // P's label survives the close, so its state reads as starred.
    assert_eq!(decide(true, &inherited(10), None, |_| RootState::Starred), Decision::Keep);
}

#[test]
fn a_child_still_reached_from_another_starred_ancestor_keeps_the_star() {
    // Marker names #10 (now unstarred) but #20 still reaches it.
    let d = decide(true, &inherited(10), Some(&reached(20)), |_| RootState::Unstarred);
    assert_eq!(d, Decision::Keep);
}

#[test]
fn a_reached_unstarred_child_gets_the_star_at_the_roots_time() {
    let d = decide(false, &[], Some(&reached(10)), |_| RootState::Starred);
    assert_eq!(
        d,
        Decision::Add {
            root: 10,
            starred_at: Some(T1.to_string())
        }
    );
    assert_eq!(decide(false, &[], None, |_| RootState::Starred), Decision::Keep);
}
