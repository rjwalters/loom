//! Unit tests for the pure pieces of `notify-cleared-blockers` (#9102).
//!
//! The `gh`-calling paths are exercised by the stubbed-`gh` shell suite,
//! `defaults/scripts/tests/test-merge-pr-notify-cleared-blockers.sh` — the
//! same split `cli/stale_blocked.rs` uses. The population filter itself
//! (`cited_among`) is tested in `stale_blocked/tests.rs`, and the cited-only
//! gathering's forge-call counts in `stale_blocked/notify_tests.rs`.

use super::*;
use crate::dep_recheck::extract;
use crate::stale_blocked::cited_among;
use crate::stale_blocked::notify::has_marker;

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
    let i = input_with_comment(&comment_body(Artifact::Issue, &[180], &[], false));
    assert!(has_marker(&i, 180));
    assert!(!has_marker(&i, 18), "#18's marker must not match inside #180's");
    assert!(!has_marker(&i, 181));
}

#[test]
fn comment_body_names_every_cited_number_and_carries_each_marker() {
    let reasons = vec!["prose reference #180 is CLOSED".to_string()];
    let b = comment_body(Artifact::Issue, &[180, 200], &reasons, false);
    assert!(b.contains("#180, #200 just closed"));
    assert!(b.contains("this issue's `loom:blocked`"));
    assert!(b.contains("- prose reference #180 is CLOSED"));
    assert!(b.contains(&marker_for(180)));
    assert!(b.contains(&marker_for(200)));
}

#[test]
fn comment_body_names_a_pr_as_a_pr() {
    assert!(comment_body(Artifact::Pr, &[7], &[], false).contains("this PR's"));
}

#[test]
fn the_posted_comment_never_cites_its_own_closed_number_as_a_blocker() {
    // Even if the bot login differs from DEFAULT_BOT_LOGIN (so the extractor
    // counts this comment), the notification must not read as a fresh
    // `Blocked by #N` citation that re-triggers itself.
    let i = input_with_comment(&comment_body(Artifact::Issue, &[180], &["x".to_string()], true));
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

/// #9274: an Unticked citer is notified, and the reason names the unticked box.
#[test]
fn unticked_reason_names_the_ref_and_the_unticked_box() {
    let r = unticked_reason(&["#200".to_string()], 0);
    assert!(r.contains("#200"), "{r}");
    assert!(r.contains("checklist box is still unticked"), "{r}");
    assert!(!r.contains("no readable ref"), "{r}");
    let r = unticked_reason(&[], 2);
    assert!(r.contains("2 unchecked line(s) with no readable ref"), "{r}");
}

fn labels(ls: &[&str]) -> Vec<String> {
    ls.iter().map(|l| (*l).to_string()).collect()
}

fn park(by: &str, blocker: i64) -> String {
    format!("body\n<!-- loom:park Blocked by: #{blocker} by={by} -->\n")
}

fn ev(states: &[(i64, &str)]) -> Evidence {
    Evidence {
        prose: states
            .iter()
            .map(|(number, state)| crate::dep_recheck::premise::Ref {
                number: *number,
                state: (*state).to_string(),
            })
            .collect(),
        ..Evidence::default()
    }
}

/// #13331: the observed case. One Curator-authored blocker, merged.
#[test]
fn unpark_single_merged_curator_blocker() {
    let l = labels(&["loom:blocked"]);
    assert!(unpark_eligible(Artifact::Issue, &l, &park("curator", 7), &ev(&[(7, "MERGED")])));
    assert!(unpark_eligible(Artifact::Issue, &l, &park("curator", 7), &ev(&[(7, "CLOSED")])));
}

#[test]
fn no_unpark_while_any_blocker_is_still_open() {
    let l = labels(&["loom:blocked"]);
    let body = format!("{}{}", park("curator", 7), park("curator", 8));
    assert!(!unpark_eligible(Artifact::Issue, &l, &body, &ev(&[(7, "MERGED"), (8, "OPEN")])));
    // A PR closed unmerged is abandoned work, not a cleared blocker.
    assert!(!unpark_eligible(
        Artifact::Issue,
        &l,
        &park("curator", 7),
        &ev(&[(7, "CLOSED_UNMERGED")])
    ));
}

#[test]
fn no_unpark_for_a_park_not_authored_by_the_curator() {
    let l = labels(&["loom:blocked"]);
    for by in ["human", "doctor", "builder"] {
        assert!(
            !unpark_eligible(Artifact::Issue, &l, &park(by, 7), &ev(&[(7, "MERGED")])),
            "{by}"
        );
    }
    let mixed = format!("{}{}", park("curator", 7), park("human", 8));
    assert!(!unpark_eligible(
        Artifact::Issue,
        &l,
        &mixed,
        &ev(&[(7, "MERGED"), (8, "MERGED")])
    ));
    // Reason-only record: no named blocker.
    let reason = "<!-- loom:park Blocked by: (unstated) by=curator reason=\"wait\" -->";
    assert!(!unpark_eligible(Artifact::Issue, &l, reason, &ev(&[(7, "MERGED")])));
}

#[test]
fn no_unpark_under_a_hold_label_or_for_a_pr_or_with_an_unticked_dependency() {
    let body = park("curator", 7);
    let merged = ev(&[(7, "MERGED")]);
    for hold in ["loom:operator", "loom:frozen", "external"] {
        let l = labels(&["loom:blocked", hold]);
        assert!(!unpark_eligible(Artifact::Issue, &l, &body, &merged), "{hold}");
    }
    assert!(!unpark_eligible(Artifact::Issue, &labels(&["loom:curated"]), &body, &merged));
    assert!(!unpark_eligible(Artifact::Pr, &labels(&["loom:blocked"]), &body, &merged));
    let mut unticked = merged.clone();
    unticked.unparsed_unchecked = 1;
    assert!(!unpark_eligible(Artifact::Issue, &labels(&["loom:blocked"]), &body, &unticked));
}

#[test]
fn comment_states_the_unpark_transition_only_when_it_happened() {
    let yes = comment_body(Artifact::Issue, &[7], &[], true);
    assert!(yes.contains("**Unparked**") && yes.contains("`loom:curated`"), "{yes}");
    assert!(!yes.contains("Re-check whether"), "{yes}");
    let no = comment_body(Artifact::Issue, &[7], &[], false);
    assert!(!no.contains("Unparked") && no.contains("Re-check whether"), "{no}");
    assert!(yes.contains(&marker_for(7)), "idempotency marker must survive");
}
