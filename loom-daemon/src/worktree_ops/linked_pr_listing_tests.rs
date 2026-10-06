//! Unit tests for [`super::classify_open_linked_pr_rows`] (#10514).

use super::*;
use crate::forge_identity::FleetLogins;

const REPO: &str = "rjwalters/loom";

/// No fleet, no self login, no allowlist: only an `OWNER`/`MEMBER`/
/// `COLLABORATOR` association is trusted.
fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&crate::forge_identity::Roster::default()), None, Vec::new())
}

/// An open same-repo PR `number` by an outside user (`NONE`), with `body`.
fn pr(number: u32, body: &str) -> RestPull {
    RestPull {
        number,
        state: "open".to_string(),
        author: Some("someone".to_string()),
        author_association: Some("NONE".to_string()),
        head_ref: Some(format!("topic-{number}")),
        head_repo: Some(REPO.to_string()),
        body: Some(body.to_string()),
        ..RestPull::default()
    }
}

fn fork(mut row: RestPull) -> RestPull {
    row.head_repo = Some("outsider/loom".to_string());
    row
}

fn verdict(rows: &[RestPull], issue: u32) -> OpenPrProbe {
    classify_open_linked_pr_rows(rows, issue, REPO, &policy())
}

#[test]
fn a_closing_phrase_links() {
    assert_eq!(verdict(&[pr(7, "Closes #123")], 123), OpenPrProbe::Open(7));
    assert_eq!(verdict(&[pr(7, "fixes: #123\n")], 123), OpenPrProbe::Open(7));
}

#[test]
fn a_part_of_phrase_links() {
    assert_eq!(verdict(&[pr(8, "**Part of** #123")], 123), OpenPrProbe::Open(8));
    assert_eq!(verdict(&[pr(8, "Contributes to #123")], 123), OpenPrProbe::Open(8));
}

/// #8940: a passing mention is not a linkage.
#[test]
fn a_bare_mention_does_not_link() {
    assert_eq!(verdict(&[pr(9, "filed #123 to track it")], 123), OpenPrProbe::NoneOpen);
}

#[test]
fn a_longer_number_is_not_a_prefix_match() {
    assert_eq!(verdict(&[pr(9, "Closes #1234")], 123), OpenPrProbe::NoneOpen);
}

/// The deliberate widening: the Builder branch alone links, in this repo only.
#[test]
fn the_builder_branch_links_without_a_phrase() {
    let mut row = pr(10, "no phrase here");
    row.head_ref = Some("feature/issue-123".to_string());
    assert_eq!(verdict(&[row.clone()], 123), OpenPrProbe::Open(10));
    assert_eq!(verdict(&[row.clone()], 12), OpenPrProbe::NoneOpen);
    // The same branch name pushed to a fork does not.
    row.author_association = Some("MEMBER".to_string());
    assert_eq!(verdict(&[fork(row)], 123), OpenPrProbe::NoneOpen);
}

/// H14 (#9548): a fork PR by a known-untrusted author is dropped; a trusted
/// one, a same-repo one, or one with no author fields still counts.
#[test]
fn the_fork_trust_rule_matches_the_closes_graph_leg() {
    assert_eq!(verdict(&[fork(pr(11, "Closes #123"))], 123), OpenPrProbe::NoneOpen);
    let mut trusted = fork(pr(11, "Closes #123"));
    trusted.author_association = Some("COLLABORATOR".to_string());
    assert_eq!(verdict(&[trusted], 123), OpenPrProbe::Open(11));
    assert_eq!(verdict(&[pr(11, "Closes #123")], 123), OpenPrProbe::Open(11));
    let mut unknown = fork(pr(11, "Closes #123"));
    unknown.author = None;
    unknown.author_association = None;
    assert_eq!(verdict(&[unknown], 123), OpenPrProbe::Open(11));
    // A deleted fork (no head repo) is not known to be same-repo.
    let mut deleted = pr(11, "Closes #123");
    deleted.head_repo = None;
    assert_eq!(verdict(&[deleted], 123), OpenPrProbe::NoneOpen);
}

#[test]
fn the_lowest_linked_number_wins() {
    let rows = [
        pr(40, "Closes #123"),
        pr(5, "unrelated"),
        pr(31, "Part of #123"),
    ];
    assert_eq!(verdict(&rows, 123), OpenPrProbe::Open(31));
}

#[test]
fn an_empty_listing_is_a_verified_absence() {
    assert_eq!(verdict(&[], 123), OpenPrProbe::NoneOpen);
}

#[test]
fn a_non_open_row_never_links() {
    let mut row = pr(12, "Closes #123");
    row.state = "closed".to_string();
    assert_eq!(verdict(&[row], 123), OpenPrProbe::NoneOpen);
}
