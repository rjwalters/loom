//! The #9320 review fixes, as whole passes: forged markers, the cross-host
//! watchdog key, the pools-exhausted grace and key, expiring inheritance
//! for an unreadable repo, and cross-repo blockers.

use super::fake::{
    bot, issue, issue_with_body, outsider, repo_input, settings, t, tick_row, Host, World, STAR,
};
use crate::star_liveness::escalate::marker;
use crate::star_liveness::forge::ForgeComment;
use crate::star_liveness::inherit;
use crate::star_liveness::task::{RepoInput, MAX_FAILED_PASSES};
use crate::star_liveness::Settings;
use crate::types::{AskKind, LandingStage, QueueDisposition};

#[test]
fn a_forged_escalation_marker_suppresses_nothing() {
    let world = World::default();
    let slug = "m/forged";
    world.add(slug, issue(1, &[STAR, "loom:operator-decision"]));
    world.comment_full(slug, 1, outsider(&marker("operator-decision:issue")));
    Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(world.posted(slug).len(), 1, "an outsider's marker is ignored");

    // The same marker from the fleet App is the real dedupe.
    let world = World::default();
    world.add(slug, issue(1, &[STAR, "loom:operator-decision"]));
    world.comment(slug, 1, &marker("operator-decision:issue"));
    Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert!(world.posted(slug).is_empty());
}

fn lease(updated_at: &str) -> ForgeComment {
    ForgeComment {
        created_at: Some("2026-09-28T10:00:00Z".into()),
        updated_at: Some(updated_at.into()),
        ..bot("<!-- loom:lease host=host-a sweep=sweep-1 -->\nLease held by host-a.")
    }
}

fn renew(world: &World, slug: &str, at: &str) {
    world.repo(slug).comments.get_mut(&5).unwrap()[0] = lease(at);
}

fn hosted(slug: &str, disposition: QueueDisposition) -> RepoInput {
    let mut r = repo_input(slug);
    r.tick_rows = vec![tick_row(&r.root, 5, disposition)];
    r
}

#[test]
fn two_hosts_one_stall_one_comment_and_a_live_lease_is_progress_everywhere() {
    let world = World::default();
    let slug = "w/two-hosts";
    world.add(slug, issue(5, &[STAR, "loom:building"]));
    world.comment_full(slug, 5, lease("2026-09-28T10:00:00Z"));
    // host-a runs the sweep; host-b only sees the peer claim.
    let (a_in, b_in) = (
        vec![hosted(slug, QueueDisposition::InFlight)],
        vec![hosted(slug, QueueDisposition::PeerClaim)],
    );
    let (mut a, mut b) = (Host::new("host-a"), Host::new("host-b"));
    a.pass(&world, &a_in, Vec::new(), t(10, 0));
    b.pass(&world, &b_in, Vec::new(), t(10, 0));

    // A long Builder phase: no label change for 31 min, but the lease was
    // renewed at 10:28. Neither host calls that a stall.
    renew(&world, slug, "2026-09-28T10:28:00Z");
    for (host, input) in [(&mut a, &a_in), (&mut b, &b_in)] {
        let r = host.pass(&world, input, Vec::new(), t(10, 31));
        assert_eq!(r.rows[0].stage, LandingStage::Building);
        assert!(r.rows[0].ask.is_none(), "{}: a fresh lease is progress", host.id);
        assert_eq!(r.rows[0].last_progress_at, Some(t(10, 28)));
    }
    assert!(world.posted(slug).is_empty());

    // The sweep dies: no renewal after 10:28. Both hosts trip, on one key.
    let ra = a.pass(&world, &a_in, Vec::new(), t(11, 0));
    let rb = b.pass(&world, &b_in, Vec::new(), t(11, 1));
    let (ka, kb) = (ra.rows[0].ask.as_ref().unwrap(), rb.rows[0].ask.as_ref().unwrap());
    assert_eq!(ka.kind, AskKind::NoProgress);
    assert_eq!(ka.key, kb.key, "the key is built from forge facts only");
    assert_eq!(world.posted(slug).len(), 1, "one comment for one stall across hosts");
    assert_eq!(ra.escalations_posted + rb.escalations_posted, 1);
}

fn pooled(slug: &str, detail: &str) -> RepoInput {
    let mut r = repo_input(slug);
    r.pool = Some(detail.into());
    r
}

#[test]
fn pools_exhausted_waits_out_the_grace_then_asks_once_across_hosts() {
    let world = World::default();
    let slug = "q/pool";
    world.add(slug, issue(1, &[STAR, "loom:issue"]));
    let a_in = vec![pooled(
        slug,
        "all 2 token(s) exhausted since 2026-09-28 09:50Z",
    )];
    let b_in = vec![pooled(
        slug,
        "all 3 token(s) exhausted since 2026-09-28 10:07Z",
    )];
    let (mut a, mut b) = (Host::new("host-a"), Host::new("host-b"));

    let r = a.pass(&world, &a_in, Vec::new(), t(10, 0));
    assert_eq!(r.rows[0].stage, LandingStage::NoCapacity, "inside the grace window");
    assert!(r.rows[0]
        .no_capacity
        .as_deref()
        .unwrap()
        .contains("peer host"));
    b.pass(&world, &b_in, Vec::new(), t(10, 7));
    assert!(world.posted(slug).is_empty(), "no ask while a peer may still claim it");

    let ra = a.pass(&world, &a_in, Vec::new(), t(10, 10));
    assert_eq!(ra.rows[0].stage, LandingStage::NeedsOperator);
    let key = ra.rows[0].ask.as_ref().unwrap().key.clone();
    assert!(key.starts_with("pools-exhausted:"), "{key}");
    assert_eq!(world.posted(slug).len(), 1);

    // host-b's episode started later and its hold reads differently: it
    // still names the same key, finds the marker, and posts nothing.
    let rb = b.pass(&world, &b_in, Vec::new(), t(10, 12));
    assert_eq!(rb.rows[0].stage, LandingStage::NoCapacity, "b's own grace");
    let rb = b.pass(&world, &b_in, Vec::new(), t(10, 17));
    assert_eq!(rb.rows[0].ask.as_ref().unwrap().key, key);
    assert_eq!(world.posted(slug).len(), 1);
}

#[test]
fn a_re_exhaustion_of_an_issue_that_has_not_moved_posts_nothing_new() {
    let world = World::default();
    let slug = "q/again";
    world.add(slug, issue(1, &[STAR, "loom:issue"]));
    let dry = vec![pooled(slug, "episode one")];
    let wet = vec![repo_input(slug)];
    let mut a = Host::new("host-a");
    a.pass(&world, &dry, Vec::new(), t(10, 0));
    a.pass(&world, &dry, Vec::new(), t(10, 10));
    assert_eq!(world.posted(slug).len(), 1);

    // The pool recovers, then runs dry again (a new episode).
    let r = a.pass(&world, &wet, Vec::new(), t(10, 20));
    assert_eq!(r.rows[0].stage, LandingStage::Ready);
    let dry2 = vec![pooled(slug, "episode two, different since")];
    let r = a.pass(&world, &dry2, Vec::new(), t(10, 30));
    assert_eq!(r.rows[0].stage, LandingStage::NoCapacity, "the grace restarts");
    a.pass(&world, &dry2, Vec::new(), t(10, 41));
    // A host that never saw episode one.
    let mut c = Host::new("host-c");
    c.pass(&world, &dry2, Vec::new(), t(11, 0));
    c.pass(&world, &dry2, Vec::new(), t(11, 11));
    assert_eq!(world.posted(slug).len(), 1, "same issue state, same key: no repost");

    // The issue moves (curation relabels it): a new state may ask again.
    world.repo(slug).items.get_mut(&1).unwrap().labels = vec![
        STAR.into(),
        "loom:issue".into(),
        "tier:goal-supporting".into(),
    ];
    a.pass(&world, &dry2, Vec::new(), t(11, 20));
    a.pass(&world, &dry2, Vec::new(), t(11, 31));
    assert_eq!(world.posted(slug).len(), 2);
}

#[test]
fn a_peer_claim_inside_the_grace_means_no_ask() {
    let world = World::default();
    let slug = "q/peer";
    world.add(slug, issue(1, &[STAR, "loom:issue"]));
    let mut a = Host::new("host-a");
    let mut input = pooled(slug, "dry");
    a.pass(&world, &[input.clone()], Vec::new(), t(10, 0));
    input.tick_rows = vec![tick_row(&input.root, 1, QueueDisposition::PeerClaim)];
    let r = a.pass(&world, &[input], Vec::new(), t(10, 11));
    assert_eq!(r.rows[0].stage, LandingStage::Building);
    assert!(world.posted(slug).is_empty());
}

#[test]
fn the_pools_grace_is_configurable_and_zero_asks_at_once() {
    let d = Settings::from_block(None);
    assert_eq!(d.pools_grace.as_secs(), 600);
    let zero = Settings::from_block(Some(&serde_json::json!({"poolsExhaustedGraceMinutes": 0})));
    assert_eq!(zero.pools_grace.as_secs(), 0);
    let world = World::default();
    let slug = "q/zero";
    world.add(slug, issue(1, &[STAR]));
    let s = Settings {
        pools_grace: zero.pools_grace,
        ..settings()
    };
    let r = Host::new("host-a").pass_with(&world, &[pooled(slug, "dry")], Vec::new(), t(10, 0), s);
    assert_eq!(r.rows[0].ask.as_ref().map(|a| a.kind), Some(AskKind::PoolsExhausted));
}

#[test]
fn an_unreadable_repo_loses_its_inherited_stars_after_repeated_failures() {
    let world = World::default();
    let slug = "e/expire";
    world.add(
        slug,
        issue_with_body(10, &[STAR, "loom:curated", "loom:blocked"], "Blocked by #11\n"),
    );
    world.add(slug, issue(11, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    let root = repos[0].root.clone();
    let mut a = Host::new("host-a");
    a.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(inherit::current(&root).len(), 1);

    world.repo(slug).fail_listing = true;
    for i in 1..MAX_FAILED_PASSES {
        a.pass(&world, &repos, Vec::new(), t(10, i * 2));
        assert_eq!(inherit::current(&root).len(), 1, "one bad read is not enough ({i})");
    }
    a.pass(&world, &repos, Vec::new(), t(10, 30));
    assert!(inherit::current(&root).is_empty(), "stale inheritance withdrawn");

    world.repo(slug).fail_listing = false;
    a.pass(&world, &repos, Vec::new(), t(10, 40));
    assert_eq!(inherit::current(&root).len(), 1, "restored by the next good pass");
}

#[test]
fn a_cross_repo_blocker_escalates_once_worded_by_whether_it_is_managed() {
    let world = World::default();
    world.add("x/a", issue_with_body(1, &[STAR, "loom:blocked"], "Blocked by other/b#3\n"));
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &[repo_input("x/a")], Vec::new(), t(10, 0));
    let ask = r.rows[0].ask.as_ref().expect("never a silent state");
    assert_eq!(ask.kind, AskKind::BlockedCrossRepo);
    assert!(ask.text.contains("can't act on"), "{}", ask.text);
    assert_eq!(r.rows[0].blocked_by.as_deref(), Some("other/b#3"));
    a.pass(&world, &[repo_input("x/a")], Vec::new(), t(10, 2));
    assert_eq!(world.posted("x/a").len(), 1);

    let world = World::default();
    world.add("x/a", issue_with_body(1, &[STAR, "loom:blocked"], "Blocked by other/b#3\n"));
    let both = vec![repo_input("x/a"), repo_input("other/b")];
    let r = Host::new("host-a").pass(&world, &both, Vec::new(), t(10, 0));
    let row = r.rows.iter().find(|r| r.repo == "x/a").unwrap();
    assert!(row.ask.as_ref().unwrap().text.contains("star other/b#3"));
}
