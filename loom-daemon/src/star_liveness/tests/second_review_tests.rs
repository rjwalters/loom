//! The second #9320 review: the incident search reads only the specific
//! forge phrases, word-bounded, from trusted authors; and the no-progress
//! key ignores host-local stage. The third: the newest matching issue wins.

use super::fake::{issue, issue_with_body, pr, repo_input, t, tick_row, Host, World, STAR};
use crate::forge_listing::RestIssue;
use crate::types::{AskKind, LandingStage, QueueDisposition, StarLivenessReport};

/// Starred #1 whose approved PR #2 was refused with `error`, plus open
/// issue #30 with `incident_body` filed by `author` / `association`.
fn refused(slug: &str, error: &str, incident_body: &str, author: &str, association: &str) -> World {
    let world = World::default();
    world.add(slug, issue(1, &[STAR, "loom:building"]));
    world.add(slug, pr(2, 1, &["loom:pr"]));
    world.comment(
        slug,
        2,
        &format!("**Champion: Merge Failed**\n```\nFailed to merge PR #2: {error}\n```"),
    );
    world.add(
        slug,
        RestIssue {
            author: Some(author.into()),
            author_association: None,
            ..issue_with_body(30, &["loom:triage"], incident_body)
        },
    );
    world.repo(slug).associations.insert(30, association.into());
    world
}

fn inherited(report: &StarLivenessReport) -> Vec<u32> {
    report
        .rows
        .iter()
        .filter(|r| r.inherited_from.is_some())
        .map(|r| r.issue)
        .collect()
}

#[test]
fn a_generic_405_refusal_never_searches_and_goes_straight_to_the_ask() {
    let slug = "i/generic";
    let world = refused(
        slug,
        "HTTP 405: 405 Method Not Allowed",
        "Follow-up to #4050 and #14050; merge method notes (HTTP 405).",
        "turian",
        "MEMBER",
    );
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(world.repo(slug).searches, 0, "a generic refusal has no signature");
    assert!(inherited(&r).is_empty(), "#30 mentioning #4050 / 405 does not inherit");
    let ask = r
        .rows
        .iter()
        .find(|r| r.issue == 1)
        .unwrap()
        .ask
        .clone()
        .unwrap();
    assert_eq!(ask.kind, AskKind::MergeRefused);
    assert!(ask.text.contains("No open incident issue tracks it"), "{}", ask.text);
    assert!(ask.text.contains("405 Method Not Allowed"), "raw text: {}", ask.text);
}

#[test]
fn an_outsider_filed_issue_quoting_the_phrase_does_not_inherit() {
    let slug = "i/outsider";
    let error = "Merge commits are not allowed on this repository. (HTTP 405)";
    let quote = "Our merges fail: merge commits are not allowed on this repository.";
    for (author, association) in [("drive-by", "NONE"), ("drive-by", "CONTRIBUTOR")] {
        let world = refused(slug, error, quote, author, association);
        let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
        assert_eq!(world.repo(slug).searches, 1);
        assert!(inherited(&r).is_empty(), "{author}/{association} is not trusted");
        let row = r.rows.iter().find(|r| r.issue == 1).unwrap();
        assert_eq!(row.ask.as_ref().unwrap().kind, AskKind::MergeRefused);
    }
    // The same issue filed by an insider, or by the fleet App, is the incident.
    for (author, association) in [
        ("turian", "COLLABORATOR"),
        ("loom-fleet-dispatch-2[bot]", "NONE"),
    ] {
        let world = refused(slug, error, quote, author, association);
        let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
        assert_eq!(inherited(&r), vec![30], "{author}/{association}");
    }
}

#[test]
fn a_phrase_inside_a_longer_word_is_no_incident() {
    let slug = "i/bounded";
    let world = refused(
        slug,
        "Merge commits are not allowed on this repository.",
        "Premerge commits are not allowedlisted in CI.",
        "turian",
        "MEMBER",
    );
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert!(inherited(&r).is_empty());
}

#[test]
fn two_hosts_with_different_capacity_post_one_no_progress_comment() {
    let world = World::default();
    let slug = "w/capacity";
    world.add(slug, issue(5, &[STAR, "loom:issue"]));
    // host-a is at its limit (no-capacity); host-b has room (ready) but, say,
    // its dispatch keeps failing. Neither moves the issue on the forge.
    let mut a_in = vec![repo_input(slug)];
    a_in[0].tick_rows = vec![tick_row(
        &a_in[0].root,
        5,
        QueueDisposition::DeferredCapacity,
    )];
    let b_in = vec![repo_input(slug)];
    let (mut a, mut b) = (Host::new("host-a"), Host::new("host-b"));
    let ra = a.pass(&world, &a_in, Vec::new(), t(10, 0));
    let rb = b.pass(&world, &b_in, Vec::new(), t(10, 0));
    assert_eq!(ra.rows[0].stage, LandingStage::NoCapacity);
    assert_eq!(rb.rows[0].stage, LandingStage::Ready);

    let ra = a.pass(&world, &a_in, Vec::new(), t(10, 31));
    let rb = b.pass(&world, &b_in, Vec::new(), t(10, 32));
    let (ka, kb) = (ra.rows[0].ask.as_ref().unwrap(), rb.rows[0].ask.as_ref().unwrap());
    assert_eq!(ka.kind, AskKind::NoProgress);
    assert_eq!(ka.key, kb.key, "the key carries no host-local stage");
    assert_eq!(world.posted(slug).len(), 1, "one comment for one idle star");
}

/// Third #9320 review: of several trusted open issues quoting the phrase,
/// the **newest** is the incident (an older one is likely unrelated, e.g. an
/// install-verification issue that once quoted the same refusal).
#[test]
fn the_newest_trusted_matching_issue_is_the_incident() {
    let slug = "i/newest";
    let error = "Merge commits are not allowed on this repository. (HTTP 405)";
    let quote = "Merges fail: merge commits are not allowed on this repository.";
    let dated = |number: u32, created: &str| RestIssue {
        author: Some("turian".into()),
        author_association: None,
        created_at: Some(created.into()),
        ..issue_with_body(number, &["loom:triage"], quote)
    };
    // #30 (older) and #40 (newer): the higher-numbered, newer one inherits.
    let world = refused(slug, error, quote, "turian", "MEMBER");
    world.add(slug, dated(30, "2026-09-02T00:00:00Z"));
    world.add(slug, dated(40, "2026-09-27T00:00:00Z"));
    world
        .repo(slug)
        .associations
        .insert(40, "COLLABORATOR".into());
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(inherited(&r), vec![40]);

    // `created_at` decides before the number does.
    let slug = "i/newest-by-date";
    let world = refused(slug, error, quote, "turian", "MEMBER");
    world.add(slug, dated(30, "2026-09-26T00:00:00Z"));
    world.add(slug, dated(40, "2026-09-03T00:00:00Z"));
    world
        .repo(slug)
        .associations
        .insert(40, "COLLABORATOR".into());
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(inherited(&r), vec![30]);
}
