//! A stale `loom:blocked` resolves itself instead of going to the operator
//! (#10151).
//!
//! Replays the 2AMLogic/2am#2127 shape: starred, `loom:blocked`, and every
//! blocker it cites already closed. It used to land `blocked-unnamed` at once
//! and wait ~16.5h for a human. It must now be unblocked (or handed to
//! Curator when nothing is cited), and `blocked-unnamed` must come only after
//! a Curator pass failed to name a blocker.

use super::fake::{
    issue, issue_with_body, outsider, pr, repo_input, settings, t, Host, World, STAR,
};
use crate::star_liveness::landing::{
    classify, BlockerRef, ItemFacts, PrFacts, StaleAction, StarFacts,
};
use crate::star_liveness::stale::{self, HANDOFF_MARKER, UNBLOCKED_PREFIX};
use crate::star_liveness::Settings;
use crate::types::{AskKind, LandingStage};

const BLOCKED: &str = "loom:blocked";

fn facts(labels: &[&str], blockers: Vec<BlockerRef>) -> StarFacts {
    StarFacts {
        repo: "o/r".into(),
        managed: true,
        issue: ItemFacts {
            number: 10,
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            open: true,
            ..ItemFacts::default()
        },
        blockers,
        host: "host-a".into(),
        ..StarFacts::default()
    }
}

fn same(n: u32, open: Option<bool>) -> BlockerRef {
    BlockerRef {
        display: format!("#{n}"),
        number: Some(n),
        open,
        cross_repo_managed: None,
    }
}

fn labels(world: &World, slug: &str, n: u32) -> Vec<String> {
    world.repo(slug).items[&n].labels.clone()
}

// ---------------------------------------------------------------------------
// The classifier
// ---------------------------------------------------------------------------

#[test]
fn all_cited_blockers_closed_is_a_stale_block_with_no_ask() {
    let f = facts(&[STAR, BLOCKED], vec![same(11, Some(false)), same(5, Some(false))]);
    let l = classify(&f);
    assert_eq!(l.stage, LandingStage::StaleBlock);
    assert_eq!(l.next_actor, "curator");
    assert!(l.ask.is_none(), "no operator ask for a stale block");
    assert_eq!(l.inherits, None);
    assert_eq!(
        l.stale,
        Some(StaleAction::Unblock {
            cleared: vec!["#5".into(), "#11".into()],
            key: "#5,#11".into()
        }),
        "numeric order, so every host derives one key"
    );
}

#[test]
fn a_mixed_set_stays_blocked_by_the_open_one_and_unreadable_counts_as_open() {
    let f = facts(&[STAR, BLOCKED], vec![same(5, Some(false)), same(6, Some(true))]);
    let l = classify(&f);
    assert_eq!((l.stage, l.inherits), (LandingStage::BlockedBy, Some(6)));
    assert!(l.stale.is_none());

    let f = facts(&[STAR, BLOCKED], vec![same(5, Some(false)), same(7, None)]);
    let l = classify(&f);
    assert_eq!(l.stage, LandingStage::BlockedBy, "a blocker we cannot read is not proof");
    assert_eq!(l.blocked_by.as_deref(), Some("#7"));
}

#[test]
fn nothing_named_goes_to_curator_first_and_to_the_operator_only_after() {
    let mut f = facts(&[STAR, "loom:issue", BLOCKED], Vec::new());
    let l = classify(&f);
    assert_eq!((l.stage, l.ask.clone()), (LandingStage::StaleBlock, None));
    assert_eq!(l.stale, Some(StaleAction::CuratorHandoff));

    // A self-reference names nothing.
    let mut own = facts(&[STAR, BLOCKED], Vec::new());
    own.blockers = vec![BlockerRef {
        display: "o/r#10".into(),
        number: None,
        open: Some(false),
        cross_repo_managed: None,
    }];
    assert_eq!(classify(&own).stale, Some(StaleAction::CuratorHandoff));

    f.curator_handoff = true;
    let l = classify(&f);
    assert_eq!(l.stage, LandingStage::NeedsOperator);
    let ask = l.ask.unwrap();
    assert_eq!((ask.kind, ask.key.as_str()), (AskKind::BlockedUnnamed, "blocked-unnamed"));
    assert!(ask.text.contains("handed it to Curator"), "{}", ask.text);
}

#[test]
fn re_blocked_on_the_same_closed_set_asks_instead_of_unblocking_again() {
    let mut f = facts(&[STAR, BLOCKED], vec![same(5, Some(false))]);
    f.unblocked_before = vec!["#5".into()];
    let l = classify(&f);
    let ask = l.ask.expect("never a second unblock");
    assert_eq!(ask.kind, AskKind::BlockedUnnamed);
    assert_eq!(ask.key, "blocked-unnamed:reblocked:#5");
    // A different closed set is a new stale block.
    f.blockers.push(same(6, Some(false)));
    assert!(matches!(classify(&f).stale, Some(StaleAction::Unblock { .. })));
}

#[test]
fn a_held_or_unreadable_row_waits_without_a_write() {
    let parked = |pr_labels: &[&str], issue_labels: &[&str], blockers| {
        let mut f = facts(issue_labels, blockers);
        f.pr = Some(PrFacts {
            item: ItemFacts {
                number: 20,
                labels: pr_labels.iter().map(|l| (*l).to_string()).collect(),
                open: true,
                ..ItemFacts::default()
            },
            refusal: None,
        });
        classify(&f)
    };
    // Its own PR is parked: the #4492 / #8925 superseding block.
    for pr_label in [
        "loom:changes-requested",
        "loom:blocked",
        "loom:merge-conflict",
    ] {
        let l = parked(&[pr_label], &[STAR, BLOCKED], vec![same(5, Some(false))]);
        assert_eq!(
            (l.stage, l.stale.clone(), l.ask.clone()),
            (LandingStage::StaleBlock, None, None)
        );
    }
    // A PR in review is not parked: the stale block is cleared.
    let l = parked(&["loom:review-requested"], &[STAR, BLOCKED], vec![same(5, Some(false))]);
    assert!(matches!(l.stale, Some(StaleAction::Unblock { .. })));
    // Unnamed with a PR or a Builder holding it: nothing for Curator to release.
    let l = parked(&["loom:review-requested"], &[STAR, BLOCKED], Vec::new());
    assert_eq!(l.stale, None);
    let l = classify(&facts(&[STAR, "loom:building", BLOCKED], Vec::new()));
    assert_eq!((l.stage, l.stale), (LandingStage::StaleBlock, None));
    // Comments unreadable: no write, and no ask either.
    let mut f = facts(&[STAR, BLOCKED], vec![same(5, Some(false))]);
    f.comments_unread = true;
    let l = classify(&f);
    assert_eq!((l.stale, l.ask), (None, None));
}

// ---------------------------------------------------------------------------
// Whole passes
// ---------------------------------------------------------------------------

#[test]
fn a_pass_unblocks_an_all_closed_block_once_and_never_flip_flops() {
    let world = World::default();
    let slug = "s/closed";
    world.add(
        slug,
        issue_with_body(10, &[STAR, "loom:issue", BLOCKED], "Blocked by #11\nRequires #12\n"),
    );
    let mut b11 = issue(11, &[]);
    b11.state = "closed".into();
    let mut b12 = issue(12, &[]);
    b12.state = "closed".into();
    world.add(slug, b11);
    world.add(slug, b12);
    let repos = vec![repo_input(slug)];
    let mut host = Host::new("host-a");

    let r = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(r.rows[0].stage, LandingStage::StaleBlock);
    assert!(r.rows[0].ask.is_none());
    assert_eq!(r.escalations_posted, 0);
    assert_eq!(labels(&world, slug, 10), vec![STAR.to_string(), "loom:issue".to_string()]);
    let posted = world.posted(slug);
    assert_eq!(posted.len(), 1);
    assert!(posted[0].1.contains(&stale::unblocked_marker("#11,#12")), "{}", posted[0].1);
    assert!(posted[0].1.contains("(#11, #12)"), "names the closed blockers");
    assert!(
        crate::dep_classify::refs::parse_named_blocker_refs(&posted[0].1, slug).is_empty(),
        "the comment itself must not read as naming a blocker"
    );

    // Next pass: an ordinary ready row, nothing written.
    let r = host.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(r.rows[0].stage, LandingStage::Ready);
    assert_eq!(world.posted(slug).len(), 1);

    // Someone re-applies the label on the same evidence: ask, do not fight.
    world
        .repo(slug)
        .items
        .get_mut(&10)
        .unwrap()
        .labels
        .push(BLOCKED.into());
    let r = host.pass(&world, &repos, Vec::new(), t(10, 4));
    let ask = r.rows[0]
        .ask
        .as_ref()
        .expect("an agent pass already cleared it once");
    assert_eq!(ask.kind, AskKind::BlockedUnnamed);
    assert!(labels(&world, slug, 10).iter().any(|l| l == BLOCKED), "label left alone");
    assert_eq!(world.posted(slug).len(), 2, "the one escalation");
}

#[test]
fn a_pass_hands_an_unnamed_block_to_curator_then_escalates_when_it_comes_back() {
    let world = World::default();
    let slug = "s/unnamed";
    world.add(
        slug,
        issue_with_body(10, &[STAR, "loom:curated", "loom:issue", BLOCKED], "Needs a resync.\n"),
    );
    let repos = vec![repo_input(slug)];
    let mut host = Host::new("host-a");

    let r = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!((r.rows[0].stage, r.rows[0].ask.clone()), (LandingStage::StaleBlock, None));
    assert_eq!(
        labels(&world, slug, 10),
        vec![STAR.to_string(), "loom:curated".to_string()],
        "blocked and the approval withdrawn; the star kept"
    );
    let posted = world.posted(slug);
    assert_eq!(posted.len(), 1);
    assert!(posted[0].1.contains(HANDOFF_MARKER));

    // Curator's starred queue now holds it.
    let r = host.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(r.rows[0].stage, LandingStage::Curating);
    assert_eq!(world.posted(slug).len(), 1);

    // Curator could not name a blocker and re-blocked it: now the operator.
    world
        .repo(slug)
        .items
        .get_mut(&10)
        .unwrap()
        .labels
        .push(BLOCKED.into());
    let r = host.pass(&world, &repos, Vec::new(), t(10, 4));
    assert_eq!(r.rows[0].ask.as_ref().map(|a| a.kind), Some(AskKind::BlockedUnnamed));
    assert_eq!(world.posted(slug).len(), 2);
    assert!(labels(&world, slug, 10).iter().any(|l| l == BLOCKED));
    // …once.
    host.pass(&world, &repos, Vec::new(), t(10, 6));
    assert_eq!(world.posted(slug).len(), 2);
}

#[test]
fn a_blocker_named_only_in_a_trusted_comment_counts() {
    let world = World::default();
    let slug = "s/comment";
    world.add(
        slug,
        issue_with_body(10, &[STAR, "loom:curated", BLOCKED], "Body names none.\n"),
    );
    world.add(slug, issue(11, &["loom:issue"]));
    world.comment(slug, 10, "Curator: Blocked by #11 until the API lands.");
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!((r.rows[0].stage, r.rows[0].ask.clone()), (LandingStage::BlockedBy, None));
    assert_eq!(r.rows[0].blocked_by.as_deref(), Some("#11"));
    assert!(r
        .rows
        .iter()
        .any(|row| row.issue == 11 && row.inherited_from == Some(10)));
    assert!(world.posted(slug).is_empty());
    assert!(labels(&world, slug, 10).iter().any(|l| l == BLOCKED));

    // A closed blocker named only in a comment is a stale block.
    world.repo(slug).items.get_mut(&11).unwrap().state = "closed".into();
    let r = Host::new("host-b").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 2));
    assert_eq!(r.rows[0].stage, LandingStage::StaleBlock);
    assert!(!labels(&world, slug, 10).iter().any(|l| l == BLOCKED));
}

#[test]
fn an_outsider_cannot_name_a_blocker_or_forge_a_marker() {
    let world = World::default();
    let slug = "s/outsider";
    world.add(slug, issue_with_body(10, &[STAR, BLOCKED], "Body names none.\n"));
    world.add(slug, issue(11, &["loom:issue"]));
    world.comment_full(slug, 10, outsider("Blocked by #11"));
    world.comment_full(slug, 10, outsider(HANDOFF_MARKER));
    world.comment_full(slug, 10, outsider(&format!("{UNBLOCKED_PREFIX}#11 -->")));
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(r.rows[0].stage, LandingStage::StaleBlock, "not blocked-by #11, not escalated");
    assert!(r.rows[0].ask.is_none());
    assert!(!r.rows.iter().any(|row| row.issue == 11));
}

#[test]
fn building_priority_and_held_rows_are_never_touched() {
    let world = World::default();
    let slug = "s/held";
    world.add(slug, issue_with_body(10, &[STAR, "loom:building", BLOCKED], "x\n"));
    world.add(slug, issue_with_body(30, &[STAR, BLOCKED], "Blocked by #31\n"));
    let mut b31 = issue(31, &[]);
    b31.state = "closed".into();
    world.add(slug, b31);
    world.add(slug, pr(32, 30, &["loom:changes-requested"]));
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    for n in [10, 30] {
        let row = r.rows.iter().find(|row| row.issue == n).unwrap();
        assert_eq!((row.stage, row.ask.clone()), (LandingStage::StaleBlock, None), "#{n}");
    }
    assert_eq!(labels(&world, slug, 10), vec![STAR, "loom:building", BLOCKED]);
    assert_eq!(labels(&world, slug, 30), vec![STAR, BLOCKED]);
    assert!(world.posted(slug).is_empty());
}

#[test]
fn escalate_false_computes_the_stage_but_writes_nothing() {
    let world = World::default();
    let slug = "s/off";
    world.add(slug, issue_with_body(10, &[STAR, BLOCKED], "x\n"));
    let off = Settings {
        escalate: false,
        ..settings()
    };
    let r = Host::new("host-a").pass_with(&world, &[repo_input(slug)], Vec::new(), t(10, 0), off);
    assert_eq!(r.rows[0].stage, LandingStage::StaleBlock);
    assert_eq!(labels(&world, slug, 10), vec![STAR, BLOCKED]);
    assert!(world.posted(slug).is_empty());
}

#[test]
fn applying_twice_posts_one_comment() {
    let world = World::default();
    let slug = "s/twice";
    world.add(slug, issue(10, &[STAR, "loom:issue", BLOCKED]));
    let mut forge = world.forge(slug);
    let current = labels(&world, slug, 10);
    for action in [
        StaleAction::Unblock {
            cleared: vec!["#5".into()],
            key: "#5".into(),
        },
        StaleAction::CuratorHandoff,
    ] {
        stale::apply(forge.as_mut(), 10, &current, &action, "host-a").unwrap();
        stale::apply(forge.as_mut(), 10, &current, &action, "host-b").unwrap();
    }
    assert_eq!(world.posted(slug).len(), 2, "one per marker");
    assert_eq!(labels(&world, slug, 10), vec![STAR.to_string()]);
    // The markers read back as the gate's facts.
    let comments: Vec<_> = world.repo(slug).comments[&10].clone();
    let facts = stale::comment_facts(&comments, None);
    assert!(facts.handoff);
    assert_eq!(facts.unblocked, vec!["#5".to_string()]);
    assert!(facts.bodies.is_empty(), "the pass's own comments name no blocker");
}
