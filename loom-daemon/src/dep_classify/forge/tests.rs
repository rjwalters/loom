//! Tests for the forge-shaped helpers (epic #7810, PR 3).
//!
//! The network-touching functions are covered end-to-end by the shell suite's
//! `gh` stub. What is tested here is the shaping — which is where the subtle
//! rules live.

use super::*;

fn view(comments: &[&str]) -> IssueView {
    IssueView {
        body: String::new(),
        labels: Vec::new(),
        comments: comments
            .iter()
            .map(|b| Comment {
                body: (*b).to_string(),
            })
            .collect(),
    }
}

#[test]
fn the_last_matching_comment_wins_not_the_first() {
    // An issue accumulates verdicts. The decision is about the most recent one;
    // taking the first would re-litigate a finding a later pass superseded.
    let v = view(&[
        "NEEDLE older verdict",
        "unrelated chatter",
        "NEEDLE newer verdict",
    ]);
    assert_eq!(v.last_comment_containing("NEEDLE"), "NEEDLE newer verdict");
}

#[test]
fn no_matching_comment_yields_empty_rather_than_panicking() {
    let v = view(&["nothing relevant"]);
    assert_eq!(v.last_comment_containing("NEEDLE"), "");
}

#[test]
fn an_issue_with_no_comments_yields_empty() {
    assert_eq!(view(&[]).last_comment_containing("NEEDLE"), "");
}

#[test]
fn comments_join_with_newlines_for_marker_searching() {
    let v = view(&["first", "second"]);
    assert_eq!(v.comments_joined(), "first\nsecond");
}

#[test]
fn label_names_are_extracted_in_order() {
    let v = IssueView {
        body: String::new(),
        labels: vec![Label { name: "a".into() }, Label { name: "b".into() }],
        comments: Vec::new(),
    };
    assert_eq!(v.label_names(), vec!["a", "b"]);
}

#[test]
fn an_issue_view_deserialises_with_every_field_absent() {
    // `gh` omits empty collections in some responses. All three fields carry
    // `#[serde(default)]` so an otherwise-valid issue does not fail to decode
    // and get reported as unreadable.
    let v: IssueView = serde_json::from_str("{}").expect("must decode");
    assert!(v.body.is_empty());
    assert!(v.labels.is_empty());
    assert!(v.comments.is_empty());
}

#[test]
fn a_fully_empty_issue_is_still_a_real_issue() {
    // The `is_empty` predicate passed to `gh_query` is deliberately `false`:
    // an issue with no body, labels or comments is readable, and calling it
    // Empty would make the caller exit 2 on a perfectly good issue.
    let v: IssueView = serde_json::from_str(r#"{"body":"","labels":[],"comments":[]}"#).unwrap();
    assert!(v.body.is_empty() && v.labels.is_empty() && v.comments.is_empty());
}
