//! Operator priority levels in the liveness pass (#10307): the level-2
//! intent relay and level-2 issues tracked like stars.

use super::fake::{issue, repo_input, t, Host, World, STAR};
use crate::star_liveness::intents::{marker_label, StarIntent};

const HIGH: &str = "loom:operator-high-priority";
const INHERITED: &str = "loom:high-priority-inherited";

fn intent(id: &str, repo: &str, number: u32, action: &str, label: &str) -> StarIntent {
    StarIntent {
        id: id.into(),
        repo: repo.into(),
        number,
        action: action.into(),
        label: label.into(),
        requested_at: Some("2026-10-04T20:00:00Z".into()),
        requested_by: Some("joseph".into()),
    }
}

fn labels(world: &World, slug: &str, n: u32) -> Vec<String> {
    world.repo(slug).items[&n].labels.clone()
}

#[test]
fn a_level_two_intent_applies_exactly_its_label_and_never_the_star() {
    let world = World::default();
    let slug = "ui/level2";
    world.add(slug, issue(7, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &repos, vec![intent("d-1", slug, 7, "star", HIGH)], t(20, 0));

    let l = labels(&world, slug, 7);
    assert!(l.iter().any(|x| x == HIGH), "{l:?}");
    assert!(!l.iter().any(|x| x == STAR), "a level-2 intent does not add the star: {l:?}");
    let posted = world.posted(slug);
    assert_eq!(posted.len(), 1);
    assert!(posted[0].1.contains(
        "<!-- loom:operator-priority-intent=d-1 action=star requested_at=2026-10-04T20:00:00Z label=loom:operator-high-priority -->"
    ));
    assert!(posted[0]
        .1
        .contains("⭐⭐ Raised to operator high priority"));
    // Level >= 2 counts as starred: the pass tracks it like a star.
    assert_eq!(r.rows.iter().filter(|r| r.issue == 7).count(), 1);

    // A resend posts nothing more.
    a.pass(&world, &repos, vec![intent("d-1", slug, 7, "star", HIGH)], t(20, 2));
    assert_eq!(world.posted(slug).len(), 1);
}

#[test]
fn a_level_two_unstar_removes_only_its_label_and_keeps_the_star() {
    let world = World::default();
    let slug = "ui/level2-off";
    world.add(slug, issue(8, &["loom:issue", STAR, HIGH]));
    let repos = vec![repo_input(slug)];
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &repos, vec![intent("d-2", slug, 8, "unstar", HIGH)], t(20, 0));
    let l = labels(&world, slug, 8);
    assert!(!l.iter().any(|x| x == HIGH), "{l:?}");
    assert!(l.iter().any(|x| x == STAR), "levels nest: the star stays: {l:?}");
    assert!(world.posted(slug).iter().any(
        |(_, b)| b.contains("action=unstar") && b.contains("label=loom:operator-high-priority")
    ));
    assert_eq!(r.rows.len(), 1, "still starred, still tracked");
}

#[test]
fn an_inherited_label_is_never_accepted_from_an_intent() {
    let world = World::default();
    let slug = "ui/level2-inh";
    world.add(slug, issue(9, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &repos, vec![intent("x-1", slug, 9, "star", INHERITED)], t(20, 0));
    assert_eq!(r.dropped_intents.len(), 1);
    assert_eq!(r.dropped_intents[0].reason, "wrong-label");
    assert!(labels(&world, slug, 9).iter().all(|l| l == "loom:issue"));
    assert!(world.posted(slug).is_empty());
}

#[test]
fn a_marker_without_a_label_field_means_the_star() {
    assert_eq!(
        marker_label("<!-- loom:operator-priority-intent=i-1 action=star requested_at=2026-09-28T08:15:00Z -->"),
        STAR
    );
    assert_eq!(
        marker_label("<!-- loom:operator-priority-intent=i-1 action=star label=loom:operator-high-priority -->"),
        HIGH
    );
    assert_eq!(
        marker_label("<!-- loom:operator-priority-intent=i-1 action=star label=loom:operator-high-priority-->"),
        HIGH
    );
}
