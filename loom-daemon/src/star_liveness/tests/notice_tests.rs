//! Fleet-comms escalation notices (#9321) — the publisher half.
//!
//! These prove the once-per-cause property the Matrix post inherits: exactly
//! one notice per `(repo, issue, <kind>:<specifics>)` across ticks, hosts and
//! restarts, none for an unstarred issue, and one `resolved` notice when the
//! ask clears. The *relay* half — notice → Matrix envelope over a fake
//! `safehoused` socket — lives in `safehouse::tests`.

use std::collections::HashSet;

use super::fake::{issue, pr, repo_input, settings, t, Host, World, STAR};
use crate::star_liveness::escalate::{issue_url, Notice};
use crate::star_liveness::Settings;
use crate::types::{AskKind, Event, LandingStage};

/// `(issue, key, resolved)` for every notice, in order.
fn shape(notices: &[Notice]) -> Vec<(u32, String, bool)> {
    notices
        .iter()
        .map(|n| (n.issue, n.key.clone(), n.resolved))
        .collect()
}

/// A starred issue whose PR is held by Champion's merge-risk hold: a
/// `needs-operator` landing state on the very first pass.
fn seed_held(world: &World, slug: &str) {
    world.add(slug, issue(1, &[STAR, "loom:building"]));
    world.add(slug, pr(2, 1, &["loom:pr", "loom:operator"]));
}

#[test]
fn a_starred_needs_operator_issue_notices_once_across_ticks_hosts_and_restarts() {
    let world = World::default();
    let slug = "n/held";
    seed_held(&world, slug);
    let repos = vec![repo_input(slug)];

    let mut a = Host::without_propagation("host-a");
    let report = a.pass(&world, &repos, Vec::new(), t(10, 0));
    let ask = report.rows[0].ask.as_ref().unwrap();
    assert_eq!(report.rows[0].stage, LandingStage::NeedsOperator);
    assert_eq!(
        shape(&a.notices),
        vec![(1, ask.key.clone(), false)],
        "exactly one notice for the new ask"
    );

    let n = &a.notices[0];
    assert_eq!(n.repo, slug);
    assert_eq!(n.kind, AskKind::MergeRiskHold);
    assert_eq!(n.stage, LandingStage::NeedsOperator);
    assert_eq!(n.text, ask.text, "carries the concrete ask verbatim");
    assert_eq!(n.url, "https://github.com/n/held/issues/1", "and a link");
    assert!(n.inherited_from.is_none());

    // Same host, later ticks: the ledger answers, nothing new.
    a.pass(&world, &repos, Vec::new(), t(10, 2));
    a.pass(&world, &repos, Vec::new(), t(10, 30));
    assert_eq!(a.notices.len(), 1, "no repeat across ticks");

    // A second host managing the same repo finds the forge marker.
    let mut b = Host::without_propagation("host-b");
    b.pass(&world, &repos, Vec::new(), t(10, 3));
    assert!(b.notices.is_empty(), "no repeat across hosts");

    // A restart drops the in-memory ledger; the forge marker still suppresses.
    a.restart();
    a.pass(&world, &repos, Vec::new(), t(10, 40));
    assert_eq!(a.notices.len(), 1, "no repeat across a restart");
    assert_eq!(world.posted(slug).len(), 1, "and still one forge comment");
}

#[test]
fn an_unstarred_issue_never_notices() {
    let world = World::default();
    let slug = "n/plain";
    // The same held-PR shape, minus the star.
    world.add(slug, issue(1, &["loom:building"]));
    world.add(slug, pr(2, 1, &["loom:pr", "loom:operator"]));
    let repos = vec![repo_input(slug)];

    let mut host = Host::new("host-a");
    let report = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert!(report.rows.is_empty(), "not starred ⇒ not a liveness row");
    assert!(host.notices.is_empty());
    // And no forge comment either, so nothing to relay from.
    assert!(world.posted(slug).is_empty());
}

#[test]
fn the_watchdog_stall_notices_too_and_resolves_when_progress_returns() {
    let world = World::default();
    let slug = "n/stall";
    world.add(slug, issue(7, &[STAR, "loom:building"]));
    world.add(slug, pr(8, 7, &["loom:review-requested"]));
    let repos = vec![repo_input(slug)];
    let mut host = Host::without_propagation("host-a");

    host.pass(&world, &repos, Vec::new(), t(10, 0));
    host.pass(&world, &repos, Vec::new(), t(10, 29));
    assert!(host.notices.is_empty(), "inside the watchdog window");

    let r = host.pass(&world, &repos, Vec::new(), t(10, 31));
    let ask = r.rows[0].ask.as_ref().expect("past the window");
    assert_eq!(ask.kind, AskKind::NoProgress);
    assert_eq!(shape(&host.notices), vec![(7, ask.key.clone(), false)]);
    assert_eq!(
        host.notices[0].stage,
        LandingStage::InReview,
        "the watchdog keeps the agent-owned stage"
    );

    // Judge relabels the PR: forward progress, the ask clears.
    world.repo(slug).items.get_mut(&8).unwrap().labels = vec!["loom:pr".into()];
    let r = host.pass(&world, &repos, Vec::new(), t(10, 35));
    assert!(r.rows[0].ask.is_none());
    assert_eq!(
        shape(&host.notices),
        vec![(7, ask.key.clone(), false), (7, ask.key.clone(), true)],
        "one recovery notice, same dedupe key"
    );
    assert!(host.notices[1].text.is_empty(), "a recovery carries no ask");

    // And only one: a later pass with the ask still clear says nothing more.
    host.pass(&world, &repos, Vec::new(), t(10, 40));
    assert_eq!(host.notices.len(), 2);
}

#[test]
fn a_repo_that_could_not_be_read_does_not_read_as_resolved() {
    let world = World::default();
    let slug = "n/flaky";
    seed_held(&world, slug);
    let repos = vec![repo_input(slug)];
    let mut host = Host::new("host-a");

    host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(host.notices.len(), 1);

    // The listing fails: no rows, so no ask — but nothing has resolved.
    world.repo(slug).fail_listing = true;
    let r = host.pass(&world, &repos, Vec::new(), t(10, 5));
    assert_eq!(r.failed_repos, vec![slug.to_string()]);
    assert_eq!(host.notices.len(), 1, "an unreadable repo resolves nothing");

    // The repo comes back and the hold is gone: now it resolves, once.
    world.repo(slug).fail_listing = false;
    world.repo(slug).items.get_mut(&2).unwrap().labels = vec!["loom:pr".into()];
    host.pass(&world, &repos, Vec::new(), t(10, 10));
    assert_eq!(host.notices.len(), 2);
    assert!(host.notices[1].resolved);
}

#[test]
fn escalate_false_notices_nothing() {
    let world = World::default();
    let slug = "n/off";
    seed_held(&world, slug);
    let off = Settings {
        escalate: false,
        ..settings()
    };
    let mut host = Host::new("host-a");
    let r = host.pass_with(&world, &[repo_input(slug)], Vec::new(), t(10, 0), off);
    assert_eq!(r.needs_operator().count(), 1, "the ask is still computed");
    assert!(
        host.notices.is_empty(),
        "forge writes off ⇒ no forge comment and no fleet-comms notice"
    );
}

#[test]
fn a_notice_becomes_the_operator_priority_escalation_event() {
    let world = World::default();
    let slug = "n/event";
    seed_held(&world, slug);
    let mut host = Host::new("host-a");
    let report = host.pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    let ask = report.rows[0].ask.as_ref().unwrap();

    let events = host.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topic(), "operator_priority.escalation");
    let Event::OperatorPriorityEscalation {
        slug: got_slug,
        issue: got_issue,
        key,
        kind,
        stage,
        text,
        url,
        host: got_host,
        inherited_from,
        resolved,
    } = &events[0]
    else {
        panic!("wrong variant: {:?}", events[0]);
    };
    assert_eq!(got_slug, slug);
    assert_eq!(*got_issue, 1);
    assert_eq!(key, &ask.key);
    assert_eq!(kind, "merge-risk-hold");
    assert_eq!(stage, "needs-operator");
    assert_eq!(text, &ask.text);
    assert_eq!(url, "https://github.com/n/event/issues/1");
    assert_eq!(got_host, "host-a");
    assert_eq!(*inherited_from, None);
    assert!(!resolved);

    // The payload is secret-free: it is exactly the fields above.
    let json = serde_json::to_value(&events[0]).unwrap();
    let keys: HashSet<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        HashSet::from([
            "type", "slug", "issue", "key", "kind", "stage", "text", "url", "host", "resolved",
        ])
    );
}

#[test]
fn issue_url_honors_a_non_github_web_base() {
    assert_eq!(issue_url("https://github.com", "o/r", 42), "https://github.com/o/r/issues/42");
    assert_eq!(
        issue_url("https://gitea.example.com/", "o/r", 42),
        "https://gitea.example.com/o/r/issues/42",
        "a trailing slash does not double up"
    );
}

#[test]
fn web_base_is_derived_from_the_origin_remote() {
    use crate::star_liveness::task::{web_base_from_remote, DEFAULT_WEB_BASE};
    assert_eq!(
        web_base_from_remote("git@gitea.example.com:o/r.git\n"),
        "https://gitea.example.com"
    );
    assert_eq!(web_base_from_remote("https://github.com/o/r.git"), "https://github.com");
    assert_eq!(web_base_from_remote("http://gitea.local/o/r.git"), "http://gitea.local");
    // `ssh://` is its own remote shape, not covered by the scp-like `git@`
    // branch above: without an explicit arm it fell through to DEFAULT_WEB_BASE,
    // pointing a private Gitea repo's escalation at an unrelated public GitHub
    // repo with the same slug.
    assert_eq!(
        web_base_from_remote("ssh://git@gitea.example.com/o/r.git"),
        "https://gitea.example.com"
    );
    assert_eq!(
        web_base_from_remote("ssh://gitea.example.com/o/r.git\n"),
        "https://gitea.example.com",
        "userinfo is optional"
    );
    assert_eq!(
        web_base_from_remote("ssh://git@gitea.example.com:2222/o/r.git"),
        "https://gitea.example.com",
        "the SSH port is not the forge's web port, so it is dropped"
    );
    assert_eq!(web_base_from_remote("not-a-url"), DEFAULT_WEB_BASE);
    assert_eq!(web_base_from_remote(""), DEFAULT_WEB_BASE);
    assert_eq!(
        web_base_from_remote("ssh://"),
        DEFAULT_WEB_BASE,
        "a scheme with no host is not a web base"
    );
}
