//! Champion's own pull-request holds (#11861): the merge-risk hold
//! (`champion-pr-merge.md` criterion #2) and the critical-file hold
//! (`champion-critical-file-hold.md`, criterion #3).
//!
//! # The shape
//!
//! A held PR keeps `loom:pr` (the Judge's approval stands) and carries:
//!
//! - `loom:blocked` ([`crate::work_finder::BLOCKED_HOLD_LABEL`]);
//! - a reason-only park record in the body
//!   (`<!-- loom:park Blocked by: (unstated) by=champion reason="…" -->`,
//!   [`MERGE_RISK_HOLD_REASON`] / [`CRITICAL_FILE_HOLD_REASON`]), which the
//!   stale-blocked release pass skips (`Skip::Unstated`), so only Champion or
//!   a human releases it;
//! - Champion's hold-state marker comment ([`MERGE_RISK_HOLD_MARKER`] /
//!   [`CRITICAL_FILE_HOLD_MARKER`], carrying `champion:hold-state head=<sha>`).
//!
//! Before #11861 the label was `loom:operator`. That label is now a no-op for
//! every role (only a human removes it), and Champion must keep re-evaluating
//! and releasing its own holds, so they moved to `loom:blocked`.
//!
//! # The one exception to the verdict-label invariant
//!
//! `loom:pr` never coexists with `loom:blocked` (`label-state-machine.md`,
//! "PR verdict-label mutual exclusion") — except in exactly this shape, keyed
//! on a standing hold-state marker ([`standing_hold`]). `merge-pr.sh` still
//! refuses to merge it (the hold is a hold); the exception only stops the
//! janitor from stripping the approval, so a released hold merges without a
//! fresh review.
//!
//! The marker strings are a wire format: they live in comments on open PRs.

/// The merge-risk hold notice's first line.
pub const MERGE_RISK_HOLD_MARKER: &str = "<!-- champion:merge-risk-hold -->";
/// The reversal block a merge after a merge-risk hold carries.
pub const MERGE_RISK_CLEARED_MARKER: &str = "<!-- champion:merge-risk-hold-cleared -->";
/// The critical-file hold notice's first line.
pub const CRITICAL_FILE_HOLD_MARKER: &str = "<!-- champion:critical-file-hold -->";
/// A critical-file episode closed by a push that narrowed the diff.
pub const CRITICAL_FILE_CLEARED_MARKER: &str = "<!-- champion:critical-file-hold-cleared -->";
/// A human released a critical-file hold (#9016).
pub const CRITICAL_FILE_RELEASED_MARKER: &str = "<!-- champion:critical-file-release-respected -->";

/// The park record `reason` of a merge-risk hold.
pub const MERGE_RISK_HOLD_REASON: &str = "champion merge-risk hold";
/// The park record `reason` of a critical-file hold.
pub const CRITICAL_FILE_HOLD_REASON: &str = "champion critical-file hold";
/// The park record `by` of either hold.
pub const HOLD_BY: &str = "champion";

/// Which of Champion's holds stands on a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldKind {
    MergeRisk,
    CriticalFile,
}

impl HoldKind {
    /// The park record `reason` this hold writes.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            HoldKind::MergeRisk => MERGE_RISK_HOLD_REASON,
            HoldKind::CriticalFile => CRITICAL_FILE_HOLD_REASON,
        }
    }
}

/// The Champion hold standing on a PR, from its **trusted** comment bodies,
/// oldest first (`comment_trust`, #9548: a marker from an untrusted author is
/// prose, not state).
///
/// - Critical-file: the latest of its three markers (`startswith`, #5371) is
///   the hold notice. A `-release-respected` (a human released it) or
///   `-cleared` (the diff left every pattern) after it ends the episode.
/// - Merge-risk: a hold notice (`startswith`) with no reversal block
///   (`contains`: it is embedded in the pre-merge comment) after it.
///
/// A critical-file hold wins when both stand: it never auto-merges, so it is
/// the stricter of the two.
#[must_use]
pub fn standing_hold<S: AsRef<str>>(comments: &[S]) -> Option<HoldKind> {
    let last = |pred: &dyn Fn(&str) -> bool| comments.iter().rposition(|c| pred(c.as_ref()));
    let cf_hold = last(&|c| c.starts_with(CRITICAL_FILE_HOLD_MARKER));
    let cf_end = last(&|c| {
        c.starts_with(CRITICAL_FILE_CLEARED_MARKER) || c.starts_with(CRITICAL_FILE_RELEASED_MARKER)
    });
    // `starts_with(CRITICAL_FILE_HOLD_MARKER)` cannot match the `-cleared`
    // marker: the hold marker ends `hold -->`, the cleared one `hold-cleared -->`.
    if cf_hold.is_some_and(|h| cf_end.is_none_or(|e| h > e)) {
        return Some(HoldKind::CriticalFile);
    }
    let mr_hold = last(&|c| c.starts_with(MERGE_RISK_HOLD_MARKER));
    let mr_end = last(&|c| c.contains(MERGE_RISK_CLEARED_MARKER));
    if mr_hold.is_some_and(|h| mr_end.is_none_or(|e| h > e)) {
        return Some(HoldKind::MergeRisk);
    }
    None
}

/// Whether `labels` + `comments` are a Champion hold: `loom:pr` beside the
/// blocked hold, with a hold standing ([`standing_hold`]). The one shape in
/// which the approval and `loom:blocked` may coexist (#11861).
#[must_use]
pub fn is_champion_hold<L: AsRef<str>, C: AsRef<str>>(labels: &[L], comments: &[C]) -> bool {
    labels.iter().any(|l| l.as_ref() == "loom:pr")
        && crate::work_finder::is_blocked_hold(labels)
        && standing_hold(comments).is_some()
}

/// The label shape of a Champion-held PR that Champion sent to Doctor
/// (#11861): `loom:changes-requested` beside the blocked hold, and no other
/// park (`loom:operator` under any name, `loom:frozen`). Doctor may work it
/// **only** when a standing trusted hold marker confirms it
/// ([`doctor_may_work`]); the same labels without one are a Doctor-cycle-cap
/// park or another actor's, which Doctor skips. `loom:blocked` stays on
/// throughout, so the PR is never a plain approval while the hold stands.
#[must_use]
pub fn doctor_shape<L: AsRef<str>>(labels: &[L]) -> bool {
    let has = |n: &str| labels.iter().any(|l| l.as_ref() == n);
    has("loom:changes-requested")
        && crate::work_finder::is_blocked_hold(labels)
        && !labels.iter().any(|l| {
            let l = l.as_ref();
            crate::work_finder::PARK_LABELS.contains(&l)
                && !crate::work_finder::BLOCKED_HOLD_NAMES.contains(&l)
        })
}

/// Whether Doctor may work a PR (#11861): [`doctor_shape`] labels and a
/// Champion hold standing in its **trusted** comments.
#[must_use]
pub fn doctor_may_work<L: AsRef<str>, C: AsRef<str>>(labels: &[L], trusted: &[C]) -> bool {
    doctor_shape(labels) && standing_hold(trusted).is_some()
}

/// Whether a PR body carries a Champion hold's park record (#11861). A cheap,
/// listing-only hint (the body is author-editable), used only to decide that
/// a Doctor run is worth launching; [`doctor_may_work`] on trusted comments
/// decides what Doctor actually works.
#[must_use]
pub fn body_declares_hold(body: &str) -> bool {
    crate::park_record::parse(body).iter().any(|r| {
        r.blocker.is_none()
            && r.reason
                .as_deref()
                .is_some_and(|x| x == MERGE_RISK_HOLD_REASON || x == CRITICAL_FILE_HOLD_REASON)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merge_risk_hold_stands_until_a_reversal() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc -->");
        assert_eq!(standing_hold(&[hold.as_str()]), Some(HoldKind::MergeRisk));
        let reversal = format!("Merging.\n{MERGE_RISK_CLEARED_MARKER}\n**Reversing**");
        assert_eq!(standing_hold(&[hold.as_str(), reversal.as_str()]), None);
        // A later hold re-opens it.
        assert_eq!(
            standing_hold(&[hold.as_str(), reversal.as_str(), hold.as_str()]),
            Some(HoldKind::MergeRisk)
        );
    }

    #[test]
    fn a_quoted_marker_is_not_a_hold() {
        let quoted = format!("As the notice says, {MERGE_RISK_HOLD_MARKER} is up.");
        assert_eq!(standing_hold(&[quoted.as_str()]), None);
        assert_eq!(standing_hold::<&str>(&[]), None);
    }

    #[test]
    fn a_critical_file_hold_follows_its_latest_marker() {
        let hold = format!("{CRITICAL_FILE_HOLD_MARKER}\nheld");
        let cleared = format!("{CRITICAL_FILE_CLEARED_MARKER}\ncleared");
        let released = format!("{CRITICAL_FILE_RELEASED_MARKER}\nreleased");
        assert_eq!(standing_hold(&[hold.as_str()]), Some(HoldKind::CriticalFile));
        assert_eq!(standing_hold(&[hold.as_str(), cleared.as_str()]), None);
        assert_eq!(standing_hold(&[hold.as_str(), released.as_str()]), None);
        assert_eq!(
            standing_hold(&[released.as_str(), hold.as_str()]),
            Some(HoldKind::CriticalFile)
        );
    }

    #[test]
    fn critical_file_wins_when_both_stand() {
        let mr = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        let cf = format!("{CRITICAL_FILE_HOLD_MARKER}\ny");
        assert_eq!(standing_hold(&[cf.as_str(), mr.as_str()]), Some(HoldKind::CriticalFile));
    }

    #[test]
    fn the_exception_needs_approval_blocked_and_a_standing_marker() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        let c = [hold.as_str()];
        assert!(is_champion_hold(&["loom:pr", "loom:blocked"], &c));
        assert!(!is_champion_hold(&["loom:pr"], &c), "no park: not held");
        assert!(!is_champion_hold(&["loom:blocked"], &c), "no approval: not this shape");
        assert!(!is_champion_hold::<&str, &str>(&["loom:pr", "loom:blocked"], &[]));
        assert!(!is_champion_hold(&["loom:pr", "loom:operator"], &c), "the old label");
    }

    #[test]
    fn doctor_works_a_champion_held_pr_only_with_a_trusted_marker() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        let shape = ["loom:changes-requested", "loom:blocked"];
        assert!(doctor_may_work(&shape, &[hold.as_str()]));
        // No standing marker: a Doctor-cycle-cap park, not Doctor's.
        assert!(!doctor_may_work::<&str, &str>(&shape, &[]));
        // Another park beside it, or no changes requested: never.
        for extra in ["loom:operator", "loom:operator-only", "loom:frozen"] {
            let l = ["loom:changes-requested", "loom:blocked", extra];
            assert!(!doctor_may_work(&l, &[hold.as_str()]), "{extra}");
        }
        assert!(!doctor_may_work(&["loom:pr", "loom:blocked"], &[hold.as_str()]));
    }

    #[test]
    fn a_body_hold_record_is_recognised_by_reason() {
        let rec = |reason: &str| {
            crate::park_record::render_park(
                &[],
                Some(HOLD_BY),
                Some("2026-10-14T00:00:00Z"),
                Some(reason),
            )
        };
        assert!(body_declares_hold(&rec(MERGE_RISK_HOLD_REASON)));
        assert!(body_declares_hold(&rec(CRITICAL_FILE_HOLD_REASON)));
        assert!(!body_declares_hold(&rec("doctor cycle cap")));
        assert!(!body_declares_hold("no record"));
    }

    #[test]
    fn reasons_are_distinct_and_render_into_a_reason_only_record() {
        assert_ne!(HoldKind::MergeRisk.reason(), HoldKind::CriticalFile.reason());
        let line = crate::park_record::render_park(
            &[],
            Some(HOLD_BY),
            Some("2026-10-14T00:00:00Z"),
            Some(MERGE_RISK_HOLD_REASON),
        );
        let records = crate::park_record::parse(&line);
        assert_eq!(records.len(), 1);
        assert!(records[0].blocker.is_none(), "reason-only: the release pass skips it");
        assert_eq!(records[0].reason.as_deref(), Some(MERGE_RISK_HOLD_REASON));
    }
}
