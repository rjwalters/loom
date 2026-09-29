//! Unit tests for the pure pieces of `notify-cleared-blockers` (#9102).
//!
//! The `gh`-calling paths are exercised by the stubbed-`gh` shell suite,
//! `defaults/scripts/tests/test-merge-pr-notify-cleared-blockers.sh` — the
//! same split `cli/stale_blocked.rs` uses. The population filter itself
//! (`cited_among`) is tested in `stale_blocked/tests.rs`.

use super::*;

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
