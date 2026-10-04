//! `create-issue.sh --parent` logic (#10012 §4, AC 10, AC 11 red-main copy).

use super::fake::{issue, World, STAR};
use crate::star_liveness::edges::{self, Node};
use crate::star_liveness::parent_link::{
    child_body, parent_marker, star_child, sub_issue_link_args, StarOutcome, RED_MAIN_MARKER,
};

const SLUG: &str = "o/r";

#[test]
fn body_gets_the_marker_the_edge_resolver_reads() {
    let body = child_body("Do the thing.", 7, None);
    assert!(body.starts_with("Do the thing.\n\n"));
    assert!(body.contains(&parent_marker(7)));
    // Round trip: the resolver sees C -> P from the marker alone.
    let node = Node {
        number: 8,
        title: "t",
        body: &body,
        labels: &[],
        is_pull_request: false,
    };
    let found = edges::parent_edges(SLUG, &node);
    assert!(found.iter().any(|e| e.parent == 7 && e.child == 8), "{found:?}");
}

#[test]
fn marker_is_not_repeated_and_empty_body_works() {
    let once = child_body("", 7, None);
    assert_eq!(once, format!("{}\n", parent_marker(7)));
    assert_eq!(child_body(&once, 7, None), once);
}

#[test]
fn red_main_marker_is_copied_only_when_the_parent_has_it() {
    let parent = format!("Fix main.\n{RED_MAIN_MARKER}\n");
    let with = child_body("Part.", 7, Some(&parent));
    assert!(with.contains(RED_MAIN_MARKER));
    assert_eq!(child_body(&with, 7, Some(&parent)), with, "idempotent");
    let without = child_body("Part.", 7, Some("plain parent"));
    assert!(!without.contains(RED_MAIN_MARKER));
    // Mentioned in prose, not at a line start: not a marker.
    let prose = format!("see {RED_MAIN_MARKER} for the idea");
    assert!(!child_body("Part.", 7, Some(&prose)).contains(RED_MAIN_MARKER));
}

#[test]
fn starred_parent_stars_the_child_with_the_inherited_marker_once() {
    let world = World::default();
    world.add(SLUG, issue(7, &[STAR]));
    world.add(SLUG, issue(8, &[]));
    let mut f = world.forge(SLUG);
    let at = "2026-10-01T00:00:00Z";
    assert_eq!(star_child(&mut *f, 7, 8, Some(at)).unwrap(), StarOutcome::Starred);
    assert!(world.repo(SLUG).items[&8].labels.iter().any(|l| l == STAR));
    let posted = world.posted(SLUG);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0, 8);
    assert!(posted[0].1.contains("inherited_from=#7"));
    assert!(posted[0].1.contains(&format!("requested_at={at}")));
    // A repeat adds nothing.
    assert_eq!(star_child(&mut *f, 7, 8, Some(at)).unwrap(), StarOutcome::AlreadyStarred);
    assert_eq!(world.posted(SLUG).len(), 1);
}

#[test]
fn unstarred_parent_writes_nothing() {
    let world = World::default();
    world.add(SLUG, issue(7, &[]));
    world.add(SLUG, issue(8, &[]));
    let mut f = world.forge(SLUG);
    assert_eq!(star_child(&mut *f, 7, 8, None).unwrap(), StarOutcome::ParentNotStarred);
    assert!(world.repo(SLUG).items[&8].labels.is_empty());
    assert!(world.posted(SLUG).is_empty());
}

#[test]
fn a_child_the_operator_already_starred_is_left_alone() {
    let world = World::default();
    world.add(SLUG, issue(7, &[STAR]));
    world.add(SLUG, issue(8, &[STAR]));
    let mut f = world.forge(SLUG);
    assert_eq!(star_child(&mut *f, 7, 8, None).unwrap(), StarOutcome::AlreadyStarred);
    assert!(world.posted(SLUG).is_empty(), "no provenance stamped over the operator's star");
}

#[test]
fn sub_issue_link_uses_the_database_id() {
    let args = sub_issue_link_args(SLUG, 7, 123_456);
    assert_eq!(args[3], "repos/o/r/issues/7/sub_issues");
    assert_eq!(args[5], "sub_issue_id=123456");
}

#[test]
fn failed_audit_post_rolls_the_star_back_and_a_retry_succeeds() {
    let world = World::default();
    world.add(SLUG, issue(7, &[STAR]));
    world.add(SLUG, issue(8, &[]));
    let mut f = world.forge(SLUG);
    world.repo(SLUG).fail_post = true;
    assert!(star_child(&mut *f, 7, 8, None).is_err());
    assert!(
        !world.repo(SLUG).items[&8].labels.iter().any(|l| l == STAR),
        "no star without its provenance"
    );
    world.repo(SLUG).fail_post = false;
    assert_eq!(star_child(&mut *f, 7, 8, None).unwrap(), StarOutcome::Starred);
    assert_eq!(world.posted(SLUG).len(), 1);
}

#[test]
fn restar_after_unstar_posts_a_fresh_audit_despite_the_old_one() {
    let world = World::default();
    world.add(SLUG, issue(7, &[STAR]));
    world.add(SLUG, issue(8, &[]));
    let mut f = world.forge(SLUG);
    assert_eq!(star_child(&mut *f, 7, 8, None).unwrap(), StarOutcome::Starred);
    f.remove_label(8, STAR).unwrap();
    assert_eq!(star_child(&mut *f, 7, 8, None).unwrap(), StarOutcome::Starred);
    assert_eq!(world.posted(SLUG).len(), 2, "one audit per labeling generation");
}
