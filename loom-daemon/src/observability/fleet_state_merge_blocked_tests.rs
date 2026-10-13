//! The per-PR merge-readiness rollup `merge_blocked_reason` (Issue #11644):
//! its label-derived precedence, its gating to PR rows, and that `build_view`
//! stamps it. Pure: no ETA state, no forge.

use chrono::{TimeZone, Utc};

use super::{
    build_view, hold_kind, hold_names, merge_blocked_reason, pr_stage, FleetInput, ListedPr,
    RepoListing,
};
use crate::telemetry::kinds::fleet_state::{FleetHoldKind, FleetStage, MergeBlockedReason};

const REPO: &str = "rjwalters/loom";

fn labels(list: &[&str]) -> Vec<String> {
    list.iter().map(|l| (*l).to_string()).collect()
}

/// The reason the row build stamps for a PR carrying `pr_labels`, through the
/// same label-derived fields the build computes.
fn reason_from(pr_labels: &[&str]) -> Option<MergeBlockedReason> {
    let ls = labels(pr_labels);
    let has = |label: &str| ls.iter().any(|l| l == label);
    let stage = pr_stage(&ls)?;
    merge_blocked_reason(
        stage,
        hold_kind(&ls),
        &hold_names(&ls),
        has("loom:merge-conflict"),
        has("loom:ci-failure"),
    )
}

/// An input whose only PR (21, issue 20) carries `pr_labels`.
fn pr_input(pr_labels: &[&str]) -> FleetInput {
    FleetInput {
        managed: [REPO.to_string()].into_iter().collect(),
        listings: vec![RepoListing {
            repo: REPO.to_string(),
            prs: vec![ListedPr {
                number: 21,
                labels: labels(pr_labels),
                issue: Some(20),
            }],
        }],
        ..FleetInput::default()
    }
}

#[test]
fn merge_blocked_reason_follows_the_label_precedence() {
    use MergeBlockedReason::*;
    // Each label set and the reason it must roll up to, exercising the full
    // precedence `operator_hold` > `sequenced` > `conflict` > `ci_red` >
    // `changes_requested` > `approval_pending` > `ready`. Any operator or
    // `loom:blocked` hold collapses to `operator_hold` (Champion's merge-risk
    // hold is `loom:operator` plus a comment marker the emitter never reads).
    let cases: &[(&[&str], MergeBlockedReason)] = &[
        (&["loom:pr"], Ready),
        (&["loom:review-requested"], ApprovalPending),
        (&["loom:changes-requested"], ChangesRequested),
        (&["loom:treating"], ChangesRequested),
        (&["loom:review-requested", "loom:ci-failure"], CiRed),
        (&["loom:pr", "loom:merge-conflict"], Conflict),
        (&["loom:pr", "loom:sequenced"], Sequenced),
        (&["loom:pr", "loom:operator"], OperatorHold),
        (&["loom:pr", "loom:blocked"], OperatorHold),
        (&["loom:pr", "loom:operator-decision"], OperatorHold),
        (&["loom:pr", "loom:operator", "loom:merge-conflict"], OperatorHold),
        (&["loom:pr", "loom:sequenced", "loom:ci-failure"], Sequenced),
        (&["loom:changes-requested", "loom:merge-conflict"], Conflict),
    ];
    for &(lbls, want) in cases {
        assert_eq!(reason_from(lbls), Some(want), "labels: {lbls:?}");
    }
}

#[test]
fn merge_blocked_reason_is_only_for_pr_rows() {
    use FleetStage::{ReadyWait, SweepBuilder, SweepCurator};
    // No reason on a ready or sweep row, even with hold/flags present.
    for stage in [ReadyWait, SweepCurator, SweepBuilder] {
        assert_eq!(merge_blocked_reason(stage, None, &[], false, false), None);
        let hold = Some(FleetHoldKind::Operator);
        let holds = ["sequenced".to_string()];
        assert_eq!(merge_blocked_reason(stage, hold, &holds, true, true), None);
    }
}

#[test]
fn build_view_stamps_the_merge_blocked_reason_on_pr_rows() {
    let now = Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap();
    let cases: &[(&[&str], MergeBlockedReason)] = &[
        (&["loom:pr", "loom:sequenced"], MergeBlockedReason::Sequenced),
        (&["loom:review-requested"], MergeBlockedReason::ApprovalPending),
        (&["loom:pr", "loom:operator"], MergeBlockedReason::OperatorHold),
    ];
    for &(lbls, want) in cases {
        let v = build_view(&pr_input(lbls), None, now);
        assert_eq!(v.repos[REPO].rows[&20].merge_blocked_reason, Some(want));
    }
}
