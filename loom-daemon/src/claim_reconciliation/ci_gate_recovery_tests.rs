#![allow(clippy::unwrap_used)]
//! #10178: the CI-gate recovery pass, against an in-memory forge.
//! #14813: the inherited-failure branch update, its evidence gate, and the
//! conflict handoff to Doctor — same fake.

use std::collections::BTreeMap;

use super::*;
use crate::claim_reconciliation::{VerdictKind, VERDICT_MARKER_PREFIX};
use crate::forge_wait_checks::verdict::Rollup;

const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD_C: &str = "cccccccccccccccccccccccccccccccccccccccc";
/// The recovered default-branch tip (#14813 fixtures).
const TIP: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
/// The PR's merge base with the default branch — the commit that was red.
const MB: &str = "dddddddddddddddddddddddddddddddddddddddd";

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

/// One failing check with a failure signature: `(name, output title)`, at the
/// fixed conclusion `failure`.
fn failing(rows: &[(&str, &str)]) -> BTreeMap<String, BTreeSet<(String, String)>> {
    rows.iter()
        .map(|(name, title)| {
            ((*name).to_string(), BTreeSet::from([("failure".into(), (*title).to_string())]))
        })
        .collect()
}

/// A [`ChecksAtSha`] fixture: `red` names fail with the given titles, `pending`
/// names are still running, and `green` names are terminal-success.
fn checks_fixture(red: &[(&str, &str)], pending: &[&str], green: &[&str]) -> ChecksAtSha {
    let seen: BTreeSet<String> = red
        .iter()
        .map(|(n, _)| (*n).to_string())
        .chain(pending.iter().map(|n| (*n).to_string()))
        .chain(green.iter().map(|n| (*n).to_string()))
        .collect();
    ChecksAtSha {
        rollup: Rollup {
            total: seen.len() as u64,
            failing: red.iter().map(|(n, _)| (*n).to_string()).collect(),
            pending: s(pending),
            cancelled: Vec::new(),
            seen,
            failed_detail: Vec::new(),
        },
        failing: failing(red),
        green: green.iter().map(|n| (*n).to_string()).collect(),
    }
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
    fail_label_add: bool,
    fail_label_remove: bool,
    // #14813 fixtures.
    base_ref: Option<String>,
    default_branch: Option<String>,
    /// `None` = unreadable compare; `Some((base_tip, merge_base))`.
    compare: Option<(String, String)>,
    base_required_green: bool,
    /// Per-SHA check evidence; a SHA with no entry reads as unreadable.
    checks_at: BTreeMap<String, ChecksAtSha>,
    update_outcome: Option<UpdateOutcome>,
    updates: Vec<String>,
    /// The head a `current_head` re-read reports once (a move mid-plan).
    head_on_reread: Option<String>,
    /// A changed PR row seen only by the fresh conflict write guard.
    fresh_on_reread: Option<RestPull>,
}

impl Fake {
    fn gated(head: &str) -> Self {
        Self {
            labels: s(&["loom:changes-requested", "loom:ci-failure"]),
            head: head.into(),
            comments: vec![ci_verdict(HEAD_A)],
            base_ref: Some("main".into()),
            default_branch: Some("main".into()),
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
            base_ref: self.base_ref.clone(),
            body: self.body.clone(),
            author_association: None,
            author_is_bot: false,
            head_repo: None,
        }
    }
    /// An inherited-failure PR: the head is red on exactly the checks that
    /// fail the same way on the merge base and are green on the tip.
    fn inherited(head: &str) -> Self {
        let mut f = Self::gated(head);
        f.labels.push("loom:blocked".into());
        f.body = Some(park(CI_GATE_PARK_REASON));
        f.compare = Some((TIP.into(), MB.into()));
        f.base_required_green = true;
        f.checks_at = BTreeMap::from([
            (head.to_string(), checks_fixture(&[("ci", "boom")], &[], &["lint"])),
            (MB.to_string(), checks_fixture(&[("ci", "boom")], &[], &["lint"])),
            (TIP.to_string(), checks_fixture(&[], &[], &["ci", "lint"])),
        ]);
        f
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
            Ci::NotGreen("LOOM-CHECKS-RED".into())
        }
    }
    fn current_head(&mut self, _: u32) -> Option<String> {
        self.head_on_reread
            .take()
            .unwrap_or_else(|| self.head.clone())
            .into()
    }
    fn current_pr(&mut self, _: u32) -> Option<RestPull> {
        self.fresh_on_reread.take().or_else(|| Some(self.pr()))
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
        if (!add.is_empty() && std::mem::take(&mut self.fail_label_add))
            || (!remove.is_empty() && std::mem::take(&mut self.fail_label_remove))
        {
            return Err(anyhow!("temporary fixture label error"));
        }
        self.labels.retain(|l| !remove.contains(&l.as_str()));
        self.labels.extend(add.iter().map(|a| (*a).to_string()));
        self.swaps.push((s(remove), s(add)));
        Ok(())
    }
    fn checks(&mut self, sha: &str) -> Option<ChecksAtSha> {
        self.checks_at.get(sha).cloned()
    }
    fn default_branch(&mut self) -> Option<String> {
        self.default_branch.clone()
    }
    fn compare(&mut self, _: &str, _: &str) -> Option<Compare> {
        self.compare.as_ref().map(|(tip, mb)| Compare {
            base_tip: tip.clone(),
            merge_base: mb.clone(),
        })
    }
    fn base_required_green(&mut self, _: &str, _: &str) -> bool {
        self.base_required_green
    }
    fn update_branch(&mut self, _: u32, expected_head: &str) -> UpdateOutcome {
        self.updates.push(expected_head.to_string());
        self.update_outcome.clone().unwrap_or(UpdateOutcome::Merged)
    }
}

fn run(f: &mut Fake) -> Stats {
    let pr = f.pr();
    run_pass(f, &[pr])
}

fn writes(f: &Fake) -> (usize, usize, usize) {
    (f.updates.len(), f.posted.len(), f.swaps.len())
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

// #14813: the inherited-failure branch update.

#[test]
fn an_inherited_failure_gets_one_update_then_recovers_on_the_new_head() {
    let mut f = Fake::inherited(HEAD_A);
    // Tick 1: one update requested, the gate and the park stay on.
    assert_eq!(
        run(&mut f),
        Stats {
            candidates: 1,
            recovered: 0,
            updated: 1,
            conflicted: 0
        }
    );
    assert_eq!(f.updates, s(&[HEAD_A]));
    assert_eq!(f.posted.len(), 1);
    assert!(f.posted[0].starts_with(&update_marker(HEAD_A, TIP)));
    assert!(f.swaps.is_empty());
    assert_eq!(f.labels, s(&["loom:changes-requested", "loom:ci-failure", "loom:blocked"]));
    // Tick 2: the merge commit landed; its CI is still pending — no write.
    f.head = HEAD_B.into();
    f.checks_at
        .insert(HEAD_B.into(), checks_fixture(&[], &["ci"], &[]));
    assert_eq!(
        run(&mut f),
        Stats {
            candidates: 1,
            recovered: 0,
            updated: 0,
            conflicted: 0
        }
    );
    assert_eq!(writes(&f), (1, 1, 0));
    // Tick 3: the new head is verified exactly green — back to review.
    f.green_head = Some(HEAD_B.into());
    assert_eq!(run(&mut f).recovered, 1);
    assert_eq!(f.labels, s(&["loom:review-requested"]));
    assert!(f.posted[1].starts_with(&format!("{RECOVERY_MARKER_PREFIX}{HEAD_B} -->")));
}

#[test]
fn the_same_head_and_base_pair_is_never_requested_twice() {
    let mut f = Fake::inherited(HEAD_A);
    f.comments.push(update_marker(HEAD_A, TIP));
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // A completed handoff is no longer on the CI gate.
    let mut f = Fake::inherited(HEAD_A);
    f.comments.push(conflict_marker(HEAD_A, TIP));
    f.labels = s(&[CHANGES_REQUESTED, MERGE_CONFLICT]);
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
}

#[test]
fn a_conflicting_update_hands_the_pr_to_the_doctor_conflict_lane() {
    let mut f = Fake::inherited(HEAD_A);
    f.update_outcome = Some(UpdateOutcome::Conflict("merge conflict between HEAD and main".into()));
    assert_eq!(
        run(&mut f),
        Stats {
            candidates: 1,
            recovered: 0,
            updated: 0,
            conflicted: 1
        }
    );
    assert_eq!(f.updates, s(&[HEAD_A]));
    assert!(f.posted[0].starts_with(&conflict_marker(HEAD_A, TIP)));
    // The Doctor lane pair (#14810): changes-requested kept, no park, no gate.
    assert_eq!(f.labels, s(&["loom:changes-requested", "loom:merge-conflict"]));
    assert_eq!(
        f.swaps,
        vec![
            (vec![], s(&[MERGE_CONFLICT])),
            (s(&[CI_FAILURE, BLOCKED]), vec![])
        ]
    );
    // An unblocked ci-gate PR keeps only its own labels removed.
    let mut f = Fake::inherited(HEAD_A);
    f.labels.retain(|l| l != "loom:blocked");
    f.body = None;
    f.update_outcome = Some(UpdateOutcome::Conflict("merge conflict".into()));
    assert_eq!(run(&mut f).conflicted, 1);
    assert_eq!(f.labels, s(&["loom:changes-requested", "loom:merge-conflict"]));
}

#[test]
fn a_partial_conflict_label_handoff_is_resumed_without_another_update_or_note() {
    for added_conflict_label in [false, true] {
        let mut f = Fake::inherited(HEAD_A);
        f.update_outcome = Some(UpdateOutcome::Conflict("fixture conflict".into()));
        f.fail_label_add = !added_conflict_label;
        f.fail_label_remove = added_conflict_label;
        assert_eq!(run(&mut f).conflicted, 0);
        assert_eq!(writes(&f), (1, 1, usize::from(added_conflict_label)));
        assert!(f.posted[0].starts_with(&conflict_marker(HEAD_A, TIP)));
        assert!(f.labels.contains(&CI_FAILURE.to_string()));
        assert!(f.labels.contains(&BLOCKED.to_string()));
        assert_eq!(f.labels.contains(&MERGE_CONFLICT.to_string()), added_conflict_label);

        assert_eq!(run(&mut f).conflicted, 1);
        assert_eq!(writes(&f), (1, 1, 2));
        assert_eq!(f.labels, s(&[CHANGES_REQUESTED, MERGE_CONFLICT]));
        assert_eq!(run(&mut f), Stats::default());
        assert_eq!(writes(&f), (1, 1, 2));
    }
}

#[test]
fn a_resumed_conflict_handoff_rechecks_the_head_and_holds_before_writing() {
    for moved_head in [false, true] {
        let mut f = Fake::inherited(HEAD_A);
        f.comments.push(conflict_marker(HEAD_A, TIP));
        let mut fresh = f.pr();
        if moved_head {
            fresh.head_sha = Some(HEAD_B.into());
        } else {
            fresh.labels.push("loom:operator".into());
        }
        f.fresh_on_reread = Some(fresh);
        assert_eq!(run(&mut f).conflicted, 0);
        assert_eq!(writes(&f), (0, 0, 0));
    }
}

#[test]
fn a_conflict_handoff_cannot_resume_from_a_quote_or_a_different_pair() {
    for comment in [
        format!("> {}", conflict_marker(HEAD_A, TIP)),
        conflict_marker(HEAD_B, TIP),
    ] {
        let mut f = Fake::inherited(HEAD_A);
        f.comments.push(comment);
        let pr = f.pr();
        assert!(apply_conflict_handoff(&mut f, &pr, HEAD_A, TIP, None).is_err());
        assert_eq!(writes(&f), (0, 0, 0));
    }
}

#[test]
fn a_failed_or_stale_update_writes_nothing_and_is_retried_next_tick() {
    for outcome in [
        UpdateOutcome::Failed("HTTP 403 permission".into()),
        UpdateOutcome::StaleHead,
    ] {
        let mut f = Fake::inherited(HEAD_A);
        f.update_outcome = Some(outcome.clone());
        assert_eq!(
            run(&mut f),
            Stats {
                candidates: 1,
                recovered: 0,
                updated: 0,
                conflicted: 0
            },
            "{outcome:?}"
        );
        // The request itself went out; nothing else was written, so the next
        // tick (no marker posted) is free to try again.
        assert_eq!(f.updates.len(), 1, "{outcome:?}");
        assert_eq!((f.posted.len(), f.swaps.len()), (0, 0), "{outcome:?}");
    }
}

#[test]
fn a_pr_local_failure_hiding_behind_an_inherited_name_is_not_updated() {
    // Same check name, different failure: the signature does not match the
    // merge base's, so the red cannot be attributed to the base.
    let mut f = Fake::inherited(HEAD_A);
    f.checks_at
        .insert(HEAD_A.into(), checks_fixture(&[("ci", "local regression")], &[], &["lint"]));
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // A check that was green on the merge base is a PR-local regression.
    let mut f = Fake::inherited(HEAD_A);
    f.checks_at
        .insert(HEAD_A.into(), checks_fixture(&[("ci", "boom"), ("lint", "x")], &[], &[]));
    f.checks_at
        .insert(MB.into(), checks_fixture(&[("ci", "boom")], &[], &["lint"]));
    f.checks_at
        .insert(TIP.into(), checks_fixture(&[], &[], &["ci", "lint"]));
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // A red check not green on the recovered base is not inherited either.
    let mut f = Fake::inherited(HEAD_A);
    f.checks_at
        .insert(TIP.into(), checks_fixture(&[], &[], &["lint"]));
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
}

#[test]
fn a_missing_failure_title_is_not_inherited_evidence() {
    let no_statuses = serde_json::json!({"statuses": []});
    let no_runs = serde_json::json!({"total_count": 0, "check_runs": []});
    let legacy_failure = serde_json::json!({"statuses": [{"context": "ci", "state": "failure"}]});
    let mut cases = vec![checks_at(&no_runs, &legacy_failure).unwrap()];
    for output in [
        serde_json::Value::Null,
        serde_json::json!({"title": ""}),
        serde_json::json!({"title": "  "}),
    ] {
        let runs = serde_json::json!({
            "total_count": 1,
            "check_runs": [{"name": "ci", "status": "completed", "conclusion": "failure", "output": output}],
        });
        cases.push(checks_at(&runs, &no_statuses).unwrap());
    }
    for check in cases {
        let mut f = Fake::inherited(HEAD_A);
        f.checks_at.insert(HEAD_A.into(), check.clone());
        f.checks_at.insert(MB.into(), check);
        assert_eq!(run(&mut f).updated, 0);
        assert_eq!(writes(&f), (0, 0, 0));
    }
}

#[test]
fn an_unrecovered_base_pending_data_or_a_moved_head_updates_nothing() {
    // The default branch's required checks are not green.
    let mut f = Fake::inherited(HEAD_A);
    f.base_required_green = false;
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // A PR not based on the default branch.
    let mut f = Fake::inherited(HEAD_A);
    f.base_ref = Some("develop".into());
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // Pending or cancelled check data on the head.
    let mut f = Fake::inherited(HEAD_A);
    f.checks_at
        .insert(HEAD_A.into(), checks_fixture(&[("ci", "boom")], &["other"], &[]));
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // Missing check data anywhere the evidence needs it.
    for missing in [HEAD_A, MB, TIP] {
        let mut f = Fake::inherited(HEAD_A);
        f.checks_at.remove(missing);
        assert_eq!(run(&mut f).updated, 0, "{missing}");
        assert_eq!(writes(&f), (0, 0, 0), "{missing}");
    }
    // The head moves between the plan and the write.
    let mut f = Fake::inherited(HEAD_A);
    f.head_on_reread = Some(HEAD_B.into());
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
    // An unreadable compare.
    let mut f = Fake::inherited(HEAD_A);
    f.compare = None;
    assert_eq!(run(&mut f).updated, 0);
    assert_eq!(writes(&f), (0, 0, 0));
}

#[test]
fn holds_and_real_parks_block_the_update_like_they_block_the_release() {
    for hold in [
        "loom:operator",
        "loom:operator-only",
        "loom:operator-decision",
        "loom:frozen",
    ] {
        let mut f = Fake::inherited(HEAD_A);
        f.labels.push(hold.into());
        assert_eq!(run(&mut f), Stats::default(), "{hold}");
        assert_eq!(writes(&f), (0, 0, 0), "{hold}");
    }
    // A named blocker beside the ci-gate record is a real hold.
    let mut f = Fake::inherited(HEAD_A);
    f.body = Some(format!(
        "{}\n<!-- loom:park blocker=#77 by=builder -->",
        park(CI_GATE_PARK_REASON)
    ));
    assert_eq!(run(&mut f), Stats::default());
    assert_eq!(writes(&f), (0, 0, 0));
    // A mixed-reason park (the klayout-tools #4334 shape) is not a ci-gate park.
    let mut f = Fake::inherited(HEAD_A);
    f.body = Some(format!(
        "{}\n{}",
        park(CI_GATE_PARK_REASON),
        park("CI red from pre-existing main failure, tracked in #4389")
    ));
    assert_eq!(run(&mut f), Stats::default());
    assert_eq!(writes(&f), (0, 0, 0));
    // An own-diff park names its defect; it is a real hold, never updated.
    let mut f = Fake::inherited(HEAD_A);
    f.body = Some(park("defect in this diff"));
    assert_eq!(run(&mut f), Stats::default());
    assert_eq!(writes(&f), (0, 0, 0));
    // A non-CI-only verdict is never updated around: the label gate passes
    // (a candidate), the verdict gate refuses.
    let mut f = Fake::inherited(HEAD_A);
    f.comments.push(format!(
        "Changes Requested: wrong approach.\n{VERDICT_MARKER_PREFIX}{HEAD_B} verdict=changes-requested -->"
    ));
    assert_eq!(
        run(&mut f),
        Stats {
            candidates: 1,
            recovered: 0,
            updated: 0,
            conflicted: 0
        }
    );
    assert_eq!(writes(&f), (0, 0, 0));
    // A standing Champion hold (merge-risk, critical file, ratification).
    for marker in [
        crate::champion_hold::MERGE_RISK_HOLD_MARKER,
        crate::champion_hold::CRITICAL_FILE_HOLD_MARKER,
        crate::champion_hold::ratification::HOLD_MARKER,
    ] {
        let mut f = Fake::inherited(HEAD_A);
        f.comments.push(format!("{marker}\nheld"));
        assert_eq!(
            run(&mut f),
            Stats {
                candidates: 1,
                recovered: 0,
                updated: 0,
                conflicted: 0
            },
            "{marker}"
        );
        assert_eq!(writes(&f), (0, 0, 0), "{marker}");
    }
}

#[test]
fn update_responses_classify_fail_closed() {
    use UpdateOutcome::*;
    assert_eq!(classify_update_response(true, None, "{\"message\":\"merging...\"}"), Merged);
    assert_eq!(
        classify_update_response(false, Some(422), "{\"message\":\"merge conflict between refs\"}"),
        Conflict("merge conflict between refs".into())
    );
    assert_eq!(classify_update_response(false, Some(409), "gh: conflict (http 409)"), StaleHead);
    assert_eq!(
        classify_update_response(false, None, "gh: expected head sha does not match"),
        StaleHead
    );
    assert_eq!(
        classify_update_response(
            false,
            Some(422),
            "{\"message\":\"there are no new commits on the base branch\"}"
        ),
        UpToDate
    );
    assert_eq!(
        classify_update_response(false, Some(403), "gh: resource not accessible (http 403)"),
        Failed("gh: resource not accessible (http 403)".into())
    );
    // A bare "conflict" word without the 422 shape is not a handoff.
    assert!(matches!(
        classify_update_response(false, Some(403), "gh: conflict of some other kind (http 403)"),
        Failed(_)
    ));
}

#[test]
fn checks_at_extracts_signatures_and_green_names_from_both_sources() {
    let runs = serde_json::json!({
        "total_count": 3,
        "check_runs": [
            {"name": "ci", "status": "completed", "conclusion": "failure",
             "output": {"title": "boom"}},
            {"name": "lint", "status": "completed", "conclusion": "success"},
            {"name": "stale", "status": "completed", "conclusion": "stale"}
        ]
    });
    let status = serde_json::json!({"statuses": [
        {"context": "legacy", "state": "success"}
    ]});
    let view = checks_at(&runs, &status).unwrap();
    assert_eq!(view.rollup.total, 4);
    assert_eq!(
        view.failing["ci"],
        BTreeSet::from([("failure".to_string(), "boom".to_string())])
    );
    assert!(view.failing.contains_key("stale"), "an unknown terminal conclusion is failing");
    assert_eq!(view.green, BTreeSet::from(["lint", "legacy"].map(|n| n.to_string())));
    // A same-named failing row poisons the green name.
    let status = serde_json::json!({"statuses": [
        {"context": "lint", "state": "failure"}
    ]});
    let view = checks_at(&runs, &status).unwrap();
    assert!(!view.green.contains("lint"));
    assert!(checks_at(&serde_json::json!({}), &status).is_none(), "outside the contract");
}
