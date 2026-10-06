//! The inherited star as the label, and its removal (#10012 §2–§3, AC 1, 4,
//! 5, 6, 7, 12).

use super::fake::{issue, issue_with_body, repo_input, settings, t, tick_row, Host, World, STAR};
use crate::star_liveness::forge::ForgeComment;
use crate::star_liveness::inherited_star::{self, Owner, RootState, StarEvent, StarKind};
use crate::star_liveness::materialize::{self, Classified, Known, MAX_STAR_WRITES_PER_PASS};
use crate::star_liveness::task::RepoInput;
use crate::star_liveness::Settings;
use crate::types::QueueDisposition;

const AT: &str = "2026-09-28T09:00:00Z";
const EARLY: &str = "2026-09-20T09:00:00Z";

fn starred(input: &mut RepoInput, n: u32, at: &str) {
    let mut row = tick_row(&input.root, n, QueueDisposition::Parked);
    row.operator_priority_at = Some(at.into());
    input.tick_rows.push(row);
}

fn park_body(children: &[u32]) -> String {
    let mut b = String::from("Decomposed.\n");
    for c in children {
        b.push_str(&format!("<!-- loom:park Blocked by: #{c} by=builder -->\n"));
    }
    b
}

/// loom-ui#1177: a starred P park-records C1..C3 and goes `loom:blocked`.
fn decomposition(world: &World, slug: &str) -> RepoInput {
    world.add(slug, issue_with_body(10, &[STAR, "loom:blocked"], &park_body(&[11, 12, 13])));
    for n in 11..=13 {
        world.add(slug, issue(n, &["loom:triage"]));
    }
    let mut input = repo_input(slug);
    starred(&mut input, 10, AT);
    input
}

fn markers(world: &World, slug: &str) -> Vec<(u32, inherited_star::InheritedMarker)> {
    world
        .posted(slug)
        .into_iter()
        .filter_map(|(n, body)| inherited_star::parse_marker(&body).map(|m| (n, m)))
        .collect()
}

/// AC 1: one pass stars all three, each with a marker naming P and P's
/// starred-at (which `STARRED_AT_JQ` orders by).
#[test]
fn a_park_recorded_decomposition_gets_the_star_label_on_every_child() {
    let world = World::default();
    let slug = "m/park";
    let input = decomposition(&world, slug);
    Host::new("a").pass(&world, &[input], Vec::new(), t(10, 0));
    for n in 11..=13 {
        assert!(world.starred(slug, n), "#{n} carries the star");
    }
    let got = markers(&world, slug);
    assert_eq!(got.len(), 3);
    for (n, m) in got {
        assert!((11..=13).contains(&n));
        assert_eq!(m.root, 10);
        assert_eq!(m.requested_at.as_deref(), Some(AT));
    }
}

/// The materialized children stay children: they are walked from P (rows
/// marked `inherited_from`), and a second pass writes nothing more.
#[test]
fn a_materialized_child_is_not_a_root_of_its_own() {
    let world = World::default();
    let slug = "m/idem";
    let input = decomposition(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    let before = world.posted(slug).len();
    let r = host.pass(&world, &[input], Vec::new(), t(10, 2));
    assert_eq!(world.posted(slug).len(), before, "nothing new is written");
    for n in 11..=13 {
        let row = r.rows.iter().find(|row| row.issue == n).unwrap();
        assert_eq!(row.inherited_from, Some(10), "#{n} is still P's child");
        assert_eq!(row.operator_priority_at.as_deref(), Some(AT));
    }
}

/// AC 4: unstarring P removes the inherited stars on the next pass; a child
/// the operator starred directly keeps its star.
#[test]
fn unstarring_the_parent_removes_inherited_stars_but_not_an_operator_star() {
    let world = World::default();
    let slug = "m/unstar";
    let input = decomposition(&world, slug);
    // #13 was starred by the operator before propagation saw it.
    world.human_star(slug, 13);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    assert!(world.starred(slug, 11) && world.starred(slug, 12));
    world.human_unstar(slug, 10);
    let r = host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(!world.starred(slug, 11), "#11's inherited star is removed");
    assert!(!world.starred(slug, 12), "#12's inherited star is removed");
    assert!(world.starred(slug, 13), "the operator's own star stays");
    assert!(r.rows.iter().all(|row| row.issue != 11 && row.issue != 12));
    let unstar = world
        .posted(slug)
        .into_iter()
        .filter(|(_, b)| b.contains("action=unstar"))
        .count();
    assert_eq!(unstar, 2, "each removal is announced first");
}

/// A human re-star after propagation's marker makes the star the
/// operator's: unstarring P no longer removes it.
#[test]
fn a_human_restar_after_the_marker_is_the_operators_own() {
    let world = World::default();
    let slug = "m/restar";
    let input = decomposition(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    world.human_unstar(slug, 11);
    world.human_star(slug, 11);
    world.human_unstar(slug, 10);
    host.pass(&world, &[input], Vec::new(), t(10, 2));
    assert!(world.starred(slug, 11), "re-starred by hand: kept");
    assert!(!world.starred(slug, 12));
}

/// A child the operator unstarred by hand is not starred again while P stays
/// starred.
#[test]
fn a_child_unstarred_by_hand_is_left_alone() {
    let world = World::default();
    let slug = "m/respect";
    let input = decomposition(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    world.human_unstar(slug, 12);
    host.pass(&world, &[input], Vec::new(), t(10, 2));
    assert!(!world.starred(slug, 12), "the operator's unstar stands");
    assert!(world.starred(slug, 11));
}

/// Unstar then re-star of P: propagation removed the stars itself, so it
/// puts them back.
#[test]
fn a_restarred_parent_stars_its_children_again() {
    let world = World::default();
    let slug = "m/cycle";
    let input = decomposition(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    world.human_unstar(slug, 10);
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(!world.starred(slug, 11));
    world.human_star(slug, 10);
    host.pass(&world, &[input], Vec::new(), t(10, 4));
    for n in 11..=13 {
        assert!(world.starred(slug, n), "#{n} is starred again");
    }
}

/// AC 5: P closes while starred; its children keep the star.
#[test]
fn a_parent_closed_while_starred_leaves_its_children_starred() {
    let world = World::default();
    let slug = "m/closed";
    let input = decomposition(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    world.repo(slug).items.get_mut(&10).unwrap().state = "closed".into();
    let r = host.pass(&world, &[input], Vec::new(), t(10, 2));
    for n in 11..=13 {
        assert!(world.starred(slug, n), "#{n} keeps the star");
        assert!(r.rows.iter().any(|row| row.issue == n), "#{n} is still a starred row");
    }
}

/// AC 6: a child of two starred parents keeps the star until both are
/// unstarred, and it carries the earlier parent's starred-at.
#[test]
fn a_child_of_two_starred_parents_keeps_the_star_until_both_lose_theirs() {
    let world = World::default();
    let slug = "m/two";
    world.add(slug, issue_with_body(1, &[STAR], "- [ ] #3\n"));
    world.add(slug, issue_with_body(2, &[STAR], "- [ ] #3\n"));
    world.add(slug, issue(3, &["loom:issue"]));
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    starred(&mut input, 2, EARLY);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    let got = markers(&world, slug);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.root, 2, "the earlier star is the root");
    assert_eq!(got[0].1.requested_at.as_deref(), Some(EARLY));
    // The root the marker names loses its star; the other still reaches #3.
    world.human_unstar(slug, 2);
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(world.starred(slug, 3), "#1 still reaches it");
    world.human_unstar(slug, 1);
    host.pass(&world, &[input], Vec::new(), t(10, 4));
    assert!(!world.starred(slug, 3), "no starred parent is left");
}

/// AC 7: a four-deep task-list chain is starred to depth 3 only; a
/// cross-repo child is never touched.
#[test]
fn the_label_follows_the_depth_cap_and_never_crosses_repos() {
    let world = World::default();
    let slug = "m/chain";
    world.add(slug, issue_with_body(1, &[STAR], "- [ ] #2\n- [ ] other/repo#9\n"));
    world.add(slug, issue_with_body(2, &[], "- [ ] #3\n"));
    world.add(slug, issue_with_body(3, &[], "- [ ] #4\n"));
    world.add(slug, issue_with_body(4, &[], "- [ ] #5\n- [ ] #1\n"));
    world.add(slug, issue(5, &[]));
    world.add("other/repo", issue(9, &[]));
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    let mut host = Host::new("a");
    for m in 0..3 {
        host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, m * 2));
    }
    for n in 2..=4 {
        assert!(world.starred(slug, n), "#{n} is within depth 3");
    }
    assert!(!world.starred(slug, 5), "depth 4 is not starred");
    assert!(!world.starred("other/repo", 9), "stars do not cross repos");
}

/// AC 12: writes are capped per pass, so a wide epic converges over passes.
#[test]
fn a_wide_decomposition_converges_under_the_write_cap() {
    let world = World::default();
    let slug = "m/wide";
    let children: Vec<u32> = (100..125).collect();
    world.add(slug, issue_with_body(10, &[STAR, "loom:blocked"], &park_body(&children)));
    for &n in &children {
        world.add(slug, issue(n, &["loom:triage"]));
    }
    let mut input = repo_input(slug);
    starred(&mut input, 10, AT);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    let count = |w: &World| children.iter().filter(|n| w.starred(slug, **n)).count();
    assert_eq!(count(&world), MAX_STAR_WRITES_PER_PASS, "one pass makes at most the cap");
    for m in 1..4 {
        host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, m * 2));
    }
    assert_eq!(count(&world), children.len(), "later passes finish the job");
}

/// AC 12: `propagate: false` (and `escalate: false`) writes no label.
#[test]
fn propagate_or_escalate_off_writes_no_label() {
    for s in [
        Settings {
            propagate: false,
            ..settings()
        },
        Settings {
            escalate: false,
            ..settings()
        },
    ] {
        let world = World::default();
        let slug = "m/off";
        let input = decomposition(&world, slug);
        Host::new("a").pass_with(&world, &[input], Vec::new(), t(10, 0), s);
        assert!((11..=13).all(|n| !world.starred(slug, n)), "{s:?}");
    }
}

/// A peer host's fresh star (a marker the stale listing does not show yet)
/// is not written twice.
#[test]
fn a_second_host_does_not_star_the_same_child_again() {
    let world = World::default();
    let slug = "m/hosts";
    let input = decomposition(&world, slug);
    Host::new("a").pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    Host::new("b").pass(&world, &[input], Vec::new(), t(10, 1));
    assert_eq!(markers(&world, slug).len(), 3, "one marker per child fleet-wide");
}

/// Operator, unknown and stale stars: roots or held, never orphaned; only a
/// fresh inherited star whose root is unstarred can be removed.
#[test]
fn classify_splits_the_listing_by_owner() {
    let known = std::collections::BTreeMap::from([
        (11, Known::Fresh(Owner::Inherited { root: 10 })),
        (12, Known::Stale(Owner::Inherited { root: 10 })),
        (13, Known::Unknown),
        (14, Known::Fresh(Owner::Operator)),
        (15, Known::Fresh(Owner::Inherited { root: 99 })),
    ]);
    let c = materialize::classify(&known, |root| {
        if root == 10 {
            RootState::Unstarred
        } else {
            RootState::Unknown
        }
    });
    assert_eq!(
        c,
        Classified {
            roots: [13, 14].into_iter().collect(),
            held: [(12, 10), (15, 99)].into_iter().collect(),
            orphaned: [(11, 10)].into_iter().collect(),
        }
    );
}

#[test]
fn timeline_events_keep_star_labels_and_trusted_star_comments_only() {
    let page = serde_json::json!([
        {"event": "labeled", "created_at": "2026-09-28T01:00:00Z", "label": {"name": STAR}},
        {"event": "labeled", "created_at": "2026-09-28T01:01:00Z", "label": {"name": "loom:issue"}},
        {"event": "commented", "created_at": "2026-09-28T01:02:00Z",
         "body": inherited_star::marker(4, 7, Some(AT)),
         "user": {"login": "loom-fleet-dispatch[bot]"}, "author_association": "CONTRIBUTOR"},
        {"event": "commented", "created_at": "2026-09-28T01:03:00Z",
         "body": inherited_star::marker(5, 7, Some(AT)),
         "user": {"login": "drive-by"}, "author_association": "NONE"},
        {"event": "commented", "created_at": "2026-09-28T01:04:00Z",
         "body": "<!-- loom:operator-priority-intent=ui-1 action=star requested_at=2026-09-28T01:00:00Z label=loom:operator-priority -->",
         "user": {"login": "rjwalters"}, "author_association": "OWNER"},
        {"event": "commented", "created_at": "2026-09-28T01:05:00Z",
         "body": inherited_star::unstar_marker(4, 7),
         "user": {"login": "loom-fleet-dispatch[bot]"}, "author_association": "CONTRIBUTOR"},
    ]);
    let got = inherited_star::star_events_from_timeline(&page, None);
    let kinds: Vec<StarKind> = got.iter().map(|e| e.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            StarKind::Labeled,
            StarKind::Inherited { root: 4 },
            StarKind::Intent
        ]
    );
    let first: Vec<StarEvent> = got.into_iter().take(2).collect();
    assert_eq!(inherited_star::owner(&first), Owner::Inherited { root: 4 });
}

#[test]
fn operator_removed_reads_the_latest_trusted_inherited_marker() {
    let bot = |b: String| ForgeComment {
        body: b,
        author: Some("loom-fleet-dispatch[bot]".into()),
        author_association: Some("CONTRIBUTOR".into()),
        ..ForgeComment::default()
    };
    let outsider = |b: String| ForgeComment {
        body: b,
        author: Some("drive-by".into()),
        author_association: Some("NONE".into()),
        ..ForgeComment::default()
    };
    let star = inherited_star::marker(1, 2, Some(AT));
    let unstar = inherited_star::unstar_marker(1, 2);
    assert!(!inherited_star::operator_removed(&[], None));
    assert!(inherited_star::operator_removed(&[bot(star.clone())], None));
    assert!(!inherited_star::operator_removed(
        &[bot(star.clone()), bot(unstar.clone())],
        None
    ));
    assert!(inherited_star::operator_removed(
        &[bot(unstar.clone()), bot(star.clone())],
        None
    ));
    assert!(
        !inherited_star::operator_removed(&[bot(unstar), outsider(star)], None),
        "an outsider's marker counts for nothing"
    );
}

// ---- PRs (#10591, #10012 AC 8) ----

use super::fake::pr;

/// A starred issue 10 with child 11 (task list), each with an open PR.
fn with_prs(world: &World, slug: &str) -> RepoInput {
    world.add(slug, issue_with_body(10, &[STAR], "- [ ] #11\n"));
    world.add(slug, issue(11, &["loom:issue"]));
    world.add(slug, pr(20, 10, &["loom:review-requested"]));
    world.add(slug, pr(21, 11, &["loom:review-requested"]));
    let mut input = repo_input(slug);
    starred(&mut input, 10, AT);
    input
}

fn marker_root(world: &World, slug: &str, n: u32) -> Option<u32> {
    markers(world, slug)
        .into_iter()
        .find(|(c, _)| *c == n)
        .map(|(_, m)| m.root)
}

/// AC 1 + 2: the PR of a directly starred issue is marked with that issue;
/// the PR of an inherited child with the root; both lose it after unstar.
#[test]
fn prs_of_starred_and_inherited_issues_are_starred_and_unstarred() {
    let world = World::default();
    let slug = "m/prs";
    let input = with_prs(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    assert!(world.starred(slug, 11) && world.starred(slug, 20) && world.starred(slug, 21));
    assert_eq!(marker_root(&world, slug, 20), Some(10));
    assert_eq!(marker_root(&world, slug, 21), Some(10));
    world.human_unstar(slug, 10);
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(!world.starred(slug, 11));
    assert!(!world.starred(slug, 20), "the direct issue's PR loses it");
    assert!(!world.starred(slug, 21), "the child's PR loses it");
    // Nothing starred is left: a later pass is quiet.
    let before = world.posted(slug).len();
    host.pass(&world, &[input], Vec::new(), t(10, 4));
    assert_eq!(world.posted(slug).len(), before);
}

/// AC 3: a PR the operator starred keeps it when its issue's root is unstarred.
#[test]
fn an_operator_starred_pr_keeps_its_star() {
    let world = World::default();
    let slug = "m/prop";
    let input = with_prs(&world, slug);
    world.human_star(slug, 21);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    assert!(world.starred(slug, 20));
    world.human_unstar(slug, 10);
    host.pass(&world, &[input], Vec::new(), t(10, 2));
    assert!(!world.starred(slug, 11) && !world.starred(slug, 20));
    assert!(world.starred(slug, 21), "the operator's PR star stays");
}

/// AC 4: propagate or escalate off writes nothing, PRs included.
#[test]
fn propagate_or_escalate_off_stars_no_pr() {
    for s in [
        Settings {
            propagate: false,
            ..settings()
        },
        Settings {
            escalate: false,
            ..settings()
        },
    ] {
        let world = World::default();
        let slug = "m/proff";
        let input = with_prs(&world, slug);
        Host::new("a").pass_with(&world, &[input], Vec::new(), t(10, 0), s);
        assert!(!world.starred(slug, 20) && !world.starred(slug, 21), "{s:?}");
        assert!(markers(&world, slug).is_empty());
    }
}

/// AC 6: no PR is unstarred after an incomplete walk.
#[test]
fn no_pr_star_is_removed_after_an_incomplete_walk() {
    let world = World::default();
    let slug = "m/inc";
    let input = with_prs(&world, slug);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    world.human_unstar(slug, 10);
    // A level listing failing hides a root: the walk is incomplete.
    world.repo(slug).fail_listing = true;
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(world.starred(slug, 20) && world.starred(slug, 21));
    world.repo(slug).fail_listing = false;
    host.pass(&world, &[input], Vec::new(), t(10, 4));
    assert!(!world.starred(slug, 20) && !world.starred(slug, 21));
}

/// AC 6: PR writes count against the per-pass cap.
#[test]
fn pr_writes_count_against_the_write_cap() {
    let world = World::default();
    let slug = "m/prcap";
    let nums: Vec<u32> = (100..110).collect();
    for &n in &nums {
        world.add(slug, issue_with_body(n, &[STAR], ""));
        world.add(slug, pr(n + 100, n, &["loom:review-requested"]));
    }
    let mut input = repo_input(slug);
    for &n in &nums {
        starred(&mut input, n, AT);
    }
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    let count = |w: &World| nums.iter().filter(|n| w.starred(slug, **n + 100)).count();
    assert_eq!(count(&world), MAX_STAR_WRITES_PER_PASS);
    host.pass(&world, &[input], Vec::new(), t(10, 2));
    assert_eq!(count(&world), nums.len());
}

/// A creation-time copy (label, no marker) reads as the operator's: kept.
#[test]
fn a_creation_time_copied_pr_star_is_operator_owned() {
    let world = World::default();
    let slug = "m/copy";
    let input = with_prs(&world, slug);
    world.human_star(slug, 20);
    let mut host = Host::new("a");
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 0));
    world.human_unstar(slug, 10);
    host.pass(&world, &[input], Vec::new(), t(10, 2));
    assert!(world.starred(slug, 20));
}

/// A starred issue 10 linking child 11 (task list); one pass materializes
/// #11's inherited star. No PR exists yet.
fn materialized_child(world: &World, slug: &str, host: &mut Host) -> RepoInput {
    world.add(slug, issue_with_body(10, &[STAR], "- [ ] #11\n"));
    world.add(slug, issue(11, &["loom:issue"]));
    let mut input = repo_input(slug);
    starred(&mut input, 10, AT);
    host.pass(&world.clone(), std::slice::from_ref(&input), Vec::new(), t(10, 0));
    assert!(world.starred(slug, 11), "#11 inherits the star");
    input
}

/// #10591 review: the root closes while starred, so the walk no longer
/// reaches #11 (kept as a held row). Its new PR still gets the star, naming
/// the original root — not #11 as a new root.
#[test]
fn the_pr_of_a_child_whose_root_closed_while_starred_gets_the_star() {
    let world = World::default();
    let slug = "m/prclosed";
    let mut host = Host::new("a");
    let input = materialized_child(&world, slug, &mut host);
    world.repo(slug).items.get_mut(&10).unwrap().state = "closed".into();
    world.add(slug, pr(21, 11, &["loom:review-requested"]));
    let r = host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(world.starred(slug, 11), "#11 keeps the star");
    assert!(r.rows.iter().any(|row| row.issue == 11), "#11 is still a starred row");
    assert!(world.starred(slug, 21), "#11's PR inherits the star");
    assert_eq!(marker_root(&world, slug, 21), Some(10), "the marker names the original root");
    // Settled: a later pass writes nothing more.
    let before = world.posted(slug).len();
    host.pass(&world, &[input], Vec::new(), t(10, 4));
    assert_eq!(world.posted(slug).len(), before);
}

/// #10591 review: #11 is linked to #10 only from its own side
/// (`<!-- loom:parent #10 -->`), which the walk does not follow. Its PR still
/// gets the star with root #10, and loses it once #10 is unstarred.
#[test]
fn the_pr_of_a_child_linked_only_child_side_gets_the_star() {
    let world = World::default();
    let slug = "m/prchildside";
    let mut host = Host::new("a");
    let input = materialized_child(&world, slug, &mut host);
    {
        let mut repo = world.repo(slug);
        repo.items.get_mut(&10).unwrap().body = Some("Epic.\n".into());
        repo.items.get_mut(&11).unwrap().body = Some("Child.\n<!-- loom:parent #10 -->\n".into());
    }
    world.add(slug, pr(21, 11, &["loom:review-requested"]));
    host.pass(&world, std::slice::from_ref(&input), Vec::new(), t(10, 2));
    assert!(world.starred(slug, 11), "#11 keeps the star");
    assert!(world.starred(slug, 21), "#11's PR inherits the star");
    assert_eq!(marker_root(&world, slug, 21), Some(10), "the marker names the original root");
    world.human_unstar(slug, 10);
    host.pass(&world, &[input], Vec::new(), t(10, 4));
    assert!(!world.starred(slug, 11), "#11's inherited star is removed");
    assert!(!world.starred(slug, 21), "and so is its PR's");
}
