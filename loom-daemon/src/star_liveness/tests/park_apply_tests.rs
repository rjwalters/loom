//! A park names its blocker to star-liveness (#10152, #10151).
//!
//! Replays rjwalters/loom#10129 et al. (2026-10-03/04): starred, `loom:blocked`,
//! blocker mentioned only in a comment. That landing used to be
//! `needs-operator(blocked-unnamed)`. Since #10151 a trusted comment that
//! names an open blocker in a naming form (`Blocked by #N`, …) lands
//! `blocked-by` and hands the star on; a comment in no naming form (#10129's
//! "Blocked on PR #11") goes to Curator as a `stale-block`, not the operator.
//! A park written through `park-record apply` lands `blocked-by` from its body
//! record.

use super::fake::{bot, issue_with_body, repo_input, t, Host, World, STAR};
use crate::park_record::apply::compose_body;
use crate::types::LandingStage;

const BODY: &str = "## Summary\n\nShip the thing.\n";

#[test]
fn a_comment_only_park_lands_blocked_by_and_inherits_the_star() {
    let world = World::default();
    let slug = "i/comment-park";
    world.add(slug, issue_with_body(10, &[STAR, "loom:blocked"], BODY));
    world.comment_full(slug, 10, bot("Blocked by #11 — unblock once it lands."));
    world.add(slug, issue_with_body(11, &["loom:review-requested"], ""));
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));

    let row = r
        .rows
        .iter()
        .find(|row| row.issue == 10)
        .expect("a row for #10");
    assert_eq!(row.stage, LandingStage::BlockedBy);
    assert_eq!(row.blocked_by.as_deref(), Some("#11"));
    assert!(row.ask.is_none(), "no operator ask: {:?}", row.ask);
    assert!(r
        .rows
        .iter()
        .any(|row| row.issue == 11 && row.inherited_from == Some(10)));
}

#[test]
fn a_comment_park_in_no_naming_form_goes_to_curator_not_the_operator() {
    let world = World::default();
    let slug = "i/loose-comment-park";
    world.add(slug, issue_with_body(10, &[STAR, "loom:blocked"], BODY));
    world.comment_full(slug, 10, bot("Blocked on PR #11 — unblock once it lands."));
    world.add(slug, issue_with_body(11, &["loom:review-requested"], ""));
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));

    let row = r
        .rows
        .iter()
        .find(|row| row.issue == 10)
        .expect("a row for #10");
    assert_eq!(row.stage, LandingStage::StaleBlock);
    assert!(row.ask.is_none(), "no operator ask: {:?}", row.ask);
}

#[test]
fn an_applied_park_lands_blocked_by_and_inherits_the_star() {
    let body = compose_body(BODY, &[11], Some("builder"), "2026-10-04T03:00:00Z", None)
        .expect("a record is written");
    let world = World::default();
    let slug = "i/applied-park";
    world.add(slug, issue_with_body(10, &[STAR, "loom:blocked"], &body));
    world.add(slug, issue_with_body(11, &["loom:review-requested"], ""));
    let r = Host::new("host-a").pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));

    let row = r
        .rows
        .iter()
        .find(|row| row.issue == 10)
        .expect("a row for #10");
    assert_eq!(row.stage, LandingStage::BlockedBy);
    assert_eq!(row.blocked_by.as_deref(), Some("#11"));
    assert!(row.ask.is_none(), "{:?}", row.ask);
    assert!(r
        .rows
        .iter()
        .any(|row| row.issue == 11 && row.inherited_from == Some(10)));
}
