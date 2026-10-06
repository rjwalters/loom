//! Unit tests for the pure pieces of `notify-cleared-blockers` (#9102).
//!
//! The `gh`-calling paths are exercised by the stubbed-`gh` shell suite,
//! `defaults/scripts/tests/test-merge-pr-notify-cleared-blockers.sh` — the
//! same split `cli/stale_blocked.rs` uses. The population filter itself
//! (`cited_among`) is tested in `stale_blocked/tests.rs`, and the cited-only
//! gathering's forge-call counts in `stale_blocked/notify_tests.rs`.

use super::*;
use loom_daemon::dep_recheck::extract;
use loom_daemon::stale_blocked::cited_among;
use loom_daemon::stale_blocked::notify::has_marker;

fn input_with_comment(body: &str) -> extract::Input {
    extract::Input {
        body: String::new(),
        comments: vec![extract::Comment {
            author: extract::Author::default(),
            body: body.to_string(),
        }],
    }
}

#[test]
fn marker_is_stable_and_number_specific() {
    assert_eq!(marker_for(180), "<!-- loom:blocker-cleared:#180 -->");
    assert_ne!(marker_for(180), marker_for(18));
}

#[test]
fn has_marker_finds_a_prior_notification_for_that_number_only() {
    let i = input_with_comment(&comment_body(Artifact::Issue, &[180], &[]));
    assert!(has_marker(&i, 180));
    assert!(!has_marker(&i, 18), "#18's marker must not match inside #180's");
    assert!(!has_marker(&i, 181));
}

#[test]
fn comment_body_names_every_cited_number_and_carries_each_marker() {
    let reasons = vec!["prose reference #180 is CLOSED".to_string()];
    let b = comment_body(Artifact::Issue, &[180, 200], &reasons);
    assert!(b.contains("#180, #200 just closed"));
    assert!(b.contains("this issue's `loom:blocked`"));
    assert!(b.contains("- prose reference #180 is CLOSED"));
    assert!(b.contains(&marker_for(180)));
    assert!(b.contains(&marker_for(200)));
}

#[test]
fn comment_body_names_a_pr_as_a_pr() {
    assert!(comment_body(Artifact::Pr, &[7], &[]).contains("this PR's"));
}

#[test]
fn the_posted_comment_never_cites_its_own_closed_number_as_a_blocker() {
    // Even if the bot login differs from DEFAULT_BOT_LOGIN (so the extractor
    // counts this comment), the notification must not read as a fresh
    // `Blocked by #N` citation that re-triggers itself.
    let i = input_with_comment(&comment_body(Artifact::Issue, &[180], &["x".to_string()]));
    assert!(cited_among(Artifact::Issue, &i, &[180]).is_empty());
}

#[test]
fn parse_close_targets_reads_every_closing_reference() {
    let out = br#"{"closingIssuesReferences":[{"number":200},{"number":201}]}"#;
    assert_eq!(parse_close_targets(out), Ok(vec![200, 201]));
}

#[test]
fn parse_close_targets_treats_no_references_as_empty_not_an_error() {
    assert_eq!(parse_close_targets(br#"{"closingIssuesReferences":[]}"#), Ok(vec![]));
    assert_eq!(parse_close_targets(b"{}"), Ok(vec![]));
}

#[test]
fn parse_close_targets_reports_unreadable_output_as_an_error() {
    assert!(parse_close_targets(b"not json").is_err());
}

/// #10515: the per-merge path must stay on the batched REST + ETag gatherer.
/// A per-artifact `gh issue view` here is a GraphQL read per open
/// `loom:blocked` artifact per merge — the regression this pins.
#[test]
fn notify_never_reads_the_forge_per_artifact() {
    let src = include_str!("../notify_cleared_blockers.rs");
    for banned in [
        "dep_recheck::forge",
        "gh_query",
        "\"issue\", \"view\"",
        "stale_blocked::gather",
        "list_blocked",
    ] {
        assert!(!src.contains(banned), "notify_cleared_blockers.rs must not use `{banned}`");
    }
}
