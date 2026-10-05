//! Whole passes against the fake forge: immediate escalation, dedupe across
//! ticks and hosts, the watchdog, and blocker inheritance.

use std::time::Duration;

use super::fake::{
    issue, issue_with_body, pr, repo_input, settings, t, tick_row, Host, World, STAR,
};
use crate::star_liveness::escalate::marker;
use crate::star_liveness::inherit;
use crate::star_liveness::Settings;
use crate::types::{AskKind, LandingStage, QueueDisposition};
use crate::work_finder::WorkItem;

/// Seed one starred issue in each non-agent state, in its own repo.
fn seed_non_agent_states(world: &World) -> Vec<(&'static str, AskKind)> {
    // operator-decision on the PR (#8256's shape)
    world.add("p/decision", issue(1, &[STAR, "loom:building"]));
    world.add("p/decision", pr(2, 1, &["loom:pr", "loom:operator-decision"]));
    // forge merge 405 (#8191's shape)
    world.add("p/refused", issue(1, &[STAR, "loom:building"]));
    world.add("p/refused", pr(2, 1, &["loom:pr"]));
    world.comment(
        "p/refused",
        2,
        "**Champion: Merge Failed**\n```\nFailed to merge PR #2: Merge commits are not allowed on this repository. (HTTP 405)\n```",
    );
    // merge-risk hold
    world.add("p/hold", issue(1, &[STAR, "loom:building"]));
    world.add("p/hold", pr(2, 1, &["loom:pr", "loom:operator"]));
    // (pools-exhausted waits out a grace window first: `review_fix_tests`.)
    vec![
        ("p/decision", AskKind::OperatorDecision),
        ("p/refused", AskKind::MergeRefused),
        ("p/hold", AskKind::MergeRiskHold),
    ]
}

fn inputs() -> Vec<crate::star_liveness::task::RepoInput> {
    ["p/decision", "p/refused", "p/hold"]
        .iter()
        .map(|s| repo_input(s))
        .collect()
}

#[test]
fn each_non_agent_state_escalates_on_the_first_pass_once_across_ticks_and_hosts() {
    let world = World::default();
    let expected = seed_non_agent_states(&world);
    let repos = inputs();
    let mut a = Host::new("host-a");
    let report = a.pass(&world, &repos, Vec::new(), t(10, 0));

    for (slug, kind) in &expected {
        let row = report.rows.iter().find(|r| r.repo == *slug).unwrap();
        assert_eq!(row.stage, LandingStage::NeedsOperator, "{slug}");
        let ask = row.ask.as_ref().unwrap();
        assert_eq!(ask.kind, *kind, "{slug}");
        let posted = world.posted(slug);
        assert_eq!(posted.len(), 1, "{slug}: exactly one escalation on the first pass");
        assert_eq!(posted[0].0, 1, "posted on the starred issue");
        assert!(posted[0].1.contains(&marker(&ask.key)), "{slug}: carries the dedupe marker");
        assert!(posted[0].1.contains(&ask.text));
    }
    assert_eq!(report.escalations_posted, expected.len());

    // Same host, next tick: nothing posted, and no forge read for the known keys.
    let reads_before: usize = expected
        .iter()
        .map(|(s, _)| world.repo(s).comment_reads)
        .sum();
    let again = a.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(again.escalations_posted, 0);
    let reads_after: usize = expected
        .iter()
        .map(|(s, _)| world.repo(s).comment_reads)
        .sum();
    assert_eq!(reads_before, reads_after, "the ledger answers repeats without a forge read");

    // Another host managing the same repos: finds the markers, posts nothing.
    let mut b = Host::new("host-b");
    let other = b.pass(&world, &repos, Vec::new(), t(10, 3));
    assert_eq!(other.escalations_posted, 0);
    for (slug, _) in &expected {
        assert_eq!(world.posted(slug).len(), 1, "{slug}: still one comment across hosts");
    }
}

#[test]
fn escalate_false_computes_but_writes_nothing() {
    let world = World::default();
    seed_non_agent_states(&world);
    let mut a = Host::new("host-a");
    let off = Settings {
        escalate: false,
        ..settings()
    };
    let report = a.pass_with(&world, &inputs(), Vec::new(), t(10, 0), off);
    assert_eq!(report.needs_operator().count(), 3);
    for s in ["p/decision", "p/refused", "p/hold"] {
        assert!(world.posted(s).is_empty());
    }
}

#[test]
fn the_watchdog_escalates_after_the_window_and_progress_resets_it() {
    let world = World::default();
    let slug = "w/watch";
    world.add(slug, issue(7, &[STAR, "loom:building"]));
    world.add(slug, pr(8, 7, &["loom:review-requested"]));
    let repos = vec![repo_input(slug)];
    let mut host = Host::new("host-a");

    let r = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(r.rows[0].stage, LandingStage::InReview);
    assert!(r.rows[0].ask.is_none());
    let r = host.pass(&world, &repos, Vec::new(), t(10, 29));
    assert!(r.rows[0].ask.is_none(), "inside the window");
    assert_eq!(r.rows[0].time_in_stage_secs, 29 * 60);

    let r = host.pass(&world, &repos, Vec::new(), t(10, 31));
    let ask = r.rows[0].ask.as_ref().expect("past the window");
    assert_eq!(ask.kind, AskKind::NoProgress);
    assert_eq!(r.rows[0].stage, LandingStage::InReview, "the stage is kept");
    assert!(ask.text.contains("pr=8"), "says what Loom last saw: {}", ask.text);
    assert_eq!(world.posted(slug).len(), 1);
    // The same stall is reported once.
    host.pass(&world, &repos, Vec::new(), t(11, 30));
    assert_eq!(world.posted(slug).len(), 1);

    // Progress (Judge relabels the PR) resets the clock.
    world.repo(slug).items.get_mut(&8).unwrap().labels = vec!["loom:pr".into()];
    let r = host.pass(&world, &repos, Vec::new(), t(11, 40));
    assert_eq!(r.rows[0].stage, LandingStage::Mergeable);
    assert!(r.rows[0].ask.is_none());
    let r = host.pass(&world, &repos, Vec::new(), t(12, 5));
    assert!(r.rows[0].ask.is_none(), "25 min since the progress");
}

#[test]
fn the_watchdog_window_is_configurable() {
    let s = Settings::from_block(Some(&serde_json::json!({"noProgressMinutes": 5})));
    assert_eq!(s.no_progress, Duration::from_secs(300));
    let world = World::default();
    let slug = "w/short";
    world.add(slug, issue(7, &[STAR, "loom:issue"]));
    let repos = vec![repo_input(slug)];
    let mut host = Host::new("host-a");
    host.pass_with(&world, &repos, Vec::new(), t(10, 0), s);
    let r = host.pass_with(&world, &repos, Vec::new(), t(10, 6), s);
    assert_eq!(r.rows[0].ask.as_ref().map(|a| a.kind), Some(AskKind::NoProgress));
    // Defaults: 30 min, writes on.
    let d = Settings::from_block(None);
    assert_eq!(d.no_progress, Duration::from_secs(30 * 60));
    assert!(d.escalate);
}

#[test]
fn a_blocker_inherits_the_star_and_loses_it_when_it_clears() {
    let world = World::default();
    let slug = "i/inherit";
    world.add(
        slug,
        issue_with_body(10, &[STAR, "loom:curated", "loom:blocked"], "Blocked by #11\n"),
    );
    world.add(slug, issue(11, &["loom:issue"]));
    world.add(slug, issue(12, &["loom:issue"]));
    let mut input = repo_input(slug);
    let mut row = tick_row(&input.root, 10, QueueDisposition::Parked);
    row.operator_priority_at = Some("2026-09-28T09:00:00Z".into());
    input.tick_rows = vec![row];
    let repos = vec![input.clone()];
    let mut host = Host::new("host-a");

    let r = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(r.rows.len(), 2);
    assert_eq!((r.rows[0].issue, r.rows[0].stage), (10, LandingStage::BlockedBy));
    assert_eq!(r.rows[0].blocked_by.as_deref(), Some("#11"));
    assert_eq!(r.rows[1].issue, 11);
    assert_eq!(r.rows[1].inherited_from, Some(10), "marked inherited");
    assert_eq!(r.rows[1].stage, LandingStage::Ready);
    assert_eq!(r.rows[1].operator_priority_at.as_deref(), Some("2026-09-28T09:00:00Z"));

    // The work finder's listing gives #11 the star's position.
    let mut items = vec![
        WorkItem::new(12, vec!["loom:issue".into()]),
        WorkItem::new(11, vec!["loom:issue".into()]),
    ];
    inherit::apply(Some(&input.root), &mut items);
    let key = |i: &WorkItem| crate::work_finder::ready_queue::key_of(0, 100, i, false);
    let mut keys: Vec<_> = items.iter().map(key).collect();
    keys.sort_by(crate::work_finder::candidate_cmp);
    assert_eq!(keys[0].number, 11, "the blocker sorts first");
    assert!(keys[0].operator_priority);
    assert!(!items
        .iter()
        .find(|i| i.number == 12)
        .unwrap()
        .is_operator_priority());

    // The blocker closes: it loses the star; #10 (still loom:blocked, now
    // with no open blocker) is a stale block the pass clears, not an
    // operator ask (#10151).
    world.repo(slug).items.get_mut(&11).unwrap().state = "closed".into();
    let r = host.pass(&world, &repos, Vec::new(), t(10, 5));
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].stage, LandingStage::StaleBlock);
    assert!(r.rows[0].ask.is_none());
    assert!(!world.repo(slug).items[&10]
        .labels
        .iter()
        .any(|l| l == "loom:blocked"));
    let mut items = vec![WorkItem::new(11, vec!["loom:issue".into()])];
    inherit::apply(Some(&input.root), &mut items);
    assert!(!items[0].is_operator_priority(), "inheritance cleared");
}

#[test]
fn an_unlisted_blocker_is_added_to_the_ready_rows() {
    let list = vec![inherit::Inherited {
        number: 30,
        from: 10,
        starred_at: Some("2026-09-28T09:00:00Z".into()),
        item: WorkItem::new(30, vec!["loom:triage".into()]),
    }];
    let mut items = vec![WorkItem::new(1, vec!["loom:issue".into()])];
    inherit::apply_list(&mut items, &list);
    let added = items.iter().find(|i| i.number == 30).unwrap();
    assert_eq!(added.operator_priority_inherited_from, Some(10));
    assert!(added.is_operator_priority());
    // A claimed blocker is not added (it is already being worked).
    let claimed = vec![inherit::Inherited {
        item: WorkItem::new(30, vec!["loom:building".into()]),
        ..list[0].clone()
    }];
    let mut items = Vec::new();
    inherit::apply_list(&mut items, &claimed);
    assert!(items.is_empty());
}

#[test]
fn a_failed_listing_is_reported_not_silent() {
    let world = World::default();
    world.add("f/fail", issue(1, &[STAR]));
    world.repo("f/fail").fail_listing = true;
    let mut host = Host::new("host-a");
    let r = host.pass(&world, &[repo_input("f/fail")], Vec::new(), t(10, 0));
    assert!(r.rows.is_empty());
    assert_eq!(r.failed_repos, vec!["f/fail".to_string()]);
}
