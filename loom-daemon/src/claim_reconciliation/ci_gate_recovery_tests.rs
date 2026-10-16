#![allow(clippy::unwrap_used)]
//! #10178: the CI-gate recovery pass, against an in-memory forge.

use super::*;
use crate::claim_reconciliation::{VerdictKind, VERDICT_MARKER_PREFIX};

const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

fn ci_verdict(head: &str) -> String {
    format!(
        "❌ **Changes Requested - CI Failing**\n\nRun 1 finished red.\n\n{CI_ONLY_MARKER}\n\
         {VERDICT_MARKER_PREFIX}{head} verdict={} -->",
        VerdictKind::ChangesRequested.marker_token()
    )
}

fn park(reason: &str) -> String {
    format!("body\n\n<!-- loom:park by=judge at=2026-10-03T09:58:30Z reason=\"{reason}\" -->")
}

#[derive(Default)]
struct Fake {
    labels: Vec<String>,
    body: Option<String>,
    head: String,
    green_head: Option<String>,
    comments: Vec<String>,
    head_after_ci: Option<String>,
    posted: Vec<String>,
    swaps: Vec<(Vec<String>, Vec<String>)>,
    comments_unreadable: bool,
}

impl Fake {
    fn gated(head: &str) -> Self {
        Self {
            labels: s(&["loom:changes-requested", "loom:ci-failure"]),
            head: head.into(),
            comments: vec![ci_verdict(HEAD_A)],
            ..Self::default()
        }
    }
    fn pr(&self) -> RestPull {
        RestPull {
            number: 10073,
            state: "open".into(),
            draft: false,
            title: None,
            created_at: None,
            updated_at: None,
            author: None,
            labels: self.labels.clone(),
            head_ref: None,
            head_sha: Some(self.head.clone()),
            base_ref: None,
            body: self.body.clone(),
            author_association: None,
            author_is_bot: false,
            head_repo: None,
        }
    }
}

impl Forge for Fake {
    fn ci(&mut self, head: &str) -> Ci {
        if self.green_head.as_deref() == Some(head) {
            if let Some(moved) = self.head_after_ci.take() {
                self.head = moved;
            }
            Ci::Green
        } else {
            Ci::NotGreen("LOOM-CHECKS-PENDING".into())
        }
    }
    fn current_head(&mut self, _: u32) -> Option<String> {
        Some(self.head.clone())
    }
    fn trusted_comments(&mut self, _: u32) -> Option<Vec<String>> {
        (!self.comments_unreadable).then(|| {
            let mut all = self.comments.clone();
            all.extend(self.posted.clone());
            all
        })
    }
    fn disarm(&mut self, _: u32) -> Option<String> {
        None
    }
    fn comment(&mut self, _: u32, body: &str) -> Result<()> {
        self.posted.push(body.to_string());
        Ok(())
    }
    fn swap_labels(&mut self, _: u32, remove: &[&str], add: &[&str]) -> Result<()> {
        self.labels.retain(|l| !remove.contains(&l.as_str()));
        self.labels.extend(add.iter().map(|a| (*a).to_string()));
        self.swaps.push((s(remove), s(add)));
        Ok(())
    }
}

fn run(f: &mut Fake) -> Stats {
    let pr = f.pr();
    run_pass(f, &[pr])
}

#[test]
fn replays_the_10073_sequence_and_ends_on_review_requested_in_one_pass() {
    let mut f = Fake::gated(HEAD_A);
    f.labels.push("loom:blocked".into());
    f.body = Some(park(CI_GATE_PARK_REASON));
    // Verdict while red, then two merges from main, still red.
    assert_eq!(run(&mut f).recovered, 0);
    f.head = HEAD_B.into();
    assert_eq!(run(&mut f).recovered, 0);
    f.head = HEAD_C.into();
    assert_eq!(run(&mut f).recovered, 0);
    assert!(f.swaps.is_empty() && f.posted.is_empty());
    // Final head goes green: one pass returns it to review.
    f.green_head = Some(HEAD_C.into());
    assert_eq!(run(&mut f).recovered, 1);
    assert_eq!(f.labels, s(&["loom:review-requested"]));
    assert_eq!(f.posted.len(), 1);
    assert!(f.posted[0].starts_with(&format!("{RECOVERY_MARKER_PREFIX}{HEAD_C} -->")));
    // Idempotent: a second pass writes nothing.
    assert_eq!(run(&mut f).recovered, 0);
    assert_eq!((f.swaps.len(), f.posted.len()), (1, 1));
}

#[test]
fn an_unblocked_gated_pr_is_recovered_and_blocked_is_not_touched() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    assert_eq!(run(&mut f).recovered, 1);
    assert_eq!(f.swaps[0].0, s(&["loom:ci-failure", "loom:changes-requested"]));
}

#[test]
fn a_legacy_ci_failing_header_without_the_marker_still_counts() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.comments = vec![format!(
        "❌ **Changes Requested - CI Failing**\n{VERDICT_MARKER_PREFIX}{HEAD_A} verdict=changes-requested -->"
    )];
    assert_eq!(run(&mut f).recovered, 1);
}

#[test]
fn real_holds_are_never_cleared() {
    for hold in [
        "loom:operator",
        "loom:operator-only",
        "loom:operator-decision",
        "loom:frozen",
    ] {
        let mut f = Fake::gated(HEAD_C);
        f.green_head = Some(HEAD_C.into());
        f.labels.push(hold.into());
        assert_eq!(run(&mut f).recovered, 0, "{hold}");
        assert!(f.swaps.is_empty(), "{hold}");
    }
}

#[test]
fn a_blocked_pr_is_cleared_only_for_a_ci_gate_park() {
    let cases: [(Option<String>, bool); 5] = [
        (None, false),
        (Some("no record at all".into()), false),
        (Some(park("operator")), false),
        (Some("<!-- loom:park blocker=#77 by=builder reason=\"waits\" -->".into()), false),
        (Some(park(CI_GATE_PARK_REASON)), true),
    ];
    for (body, recovered) in cases {
        let mut f = Fake::gated(HEAD_C);
        f.green_head = Some(HEAD_C.into());
        f.labels.push("loom:blocked".into());
        f.body.clone_from(&body);
        assert_eq!(run(&mut f).recovered == 1, recovered, "{body:?}");
        if !recovered {
            assert!(f.labels.contains(&"loom:blocked".to_string()));
        }
    }
}

#[test]
fn a_named_blocker_beside_a_ci_gate_record_is_a_real_hold() {
    let body = format!("{}\n<!-- loom:park blocker=#77 by=builder -->", park(CI_GATE_PARK_REASON));
    assert!(!is_ci_gate_park(&body));
}

#[test]
fn a_champion_hold_is_never_cleared() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.comments
        .push(format!("{}\nheld", crate::champion_hold::CRITICAL_FILE_HOLD_MARKER));
    assert_eq!(run(&mut f).recovered, 0);
}

#[test]
fn approval_claim_and_review_labels_are_left_alone() {
    for l in [
        "loom:pr",
        "loom:reviewing",
        "loom:treating",
        "loom:review-requested",
    ] {
        let mut f = Fake::gated(HEAD_C);
        f.green_head = Some(HEAD_C.into());
        f.labels.push(l.into());
        assert_eq!(run(&mut f).recovered, 0, "{l}");
    }
}

#[test]
fn a_pr_missing_either_gate_label_is_not_a_candidate() {
    for drop in ["loom:ci-failure", "loom:changes-requested"] {
        let mut f = Fake::gated(HEAD_C);
        f.green_head = Some(HEAD_C.into());
        f.labels.retain(|l| l != drop);
        assert_eq!(run(&mut f), Stats::default());
    }
}

#[test]
fn a_ci_state_that_is_not_green_changes_nothing() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_A.into()); // green for another head only
    assert_eq!(run(&mut f).recovered, 0);
    assert!(f.swaps.is_empty() && f.posted.is_empty());
}

#[test]
fn a_verdict_with_a_non_ci_finding_is_left_alone() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.comments = vec![format!(
        "Changes Requested: the parser drops the last row.\n\
         {VERDICT_MARKER_PREFIX}{HEAD_A} verdict=changes-requested -->"
    )];
    assert_eq!(run(&mut f).recovered, 0);
}

#[test]
fn a_newer_non_ci_verdict_supersedes_an_older_ci_one() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.comments.push(format!(
        "Changes Requested: wrong approach.\n\
         {VERDICT_MARKER_PREFIX}{HEAD_B} verdict=changes-requested -->"
    ));
    assert_eq!(run(&mut f).recovered, 0);
}

#[test]
fn a_head_that_moves_between_the_green_read_and_the_write_is_not_cleared() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.head_after_ci = Some(HEAD_B.into());
    assert_eq!(run(&mut f).recovered, 0);
    assert!(f.swaps.is_empty() && f.posted.is_empty());
}

#[test]
fn unreadable_comments_fail_closed() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.comments_unreadable = true;
    assert_eq!(run(&mut f).recovered, 0);
    assert!(f.swaps.is_empty());
}

#[test]
fn a_notice_already_posted_for_the_head_is_not_reposted_when_the_label_write_retries() {
    let mut f = Fake::gated(HEAD_C);
    f.green_head = Some(HEAD_C.into());
    f.comments
        .push(format!("{RECOVERY_MARKER_PREFIX}{HEAD_C} -->\nearlier attempt"));
    assert_eq!(run(&mut f).recovered, 1);
    assert!(f.posted.is_empty());
    assert_eq!(f.swaps.len(), 1);
}
