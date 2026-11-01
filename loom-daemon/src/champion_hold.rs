//! Champion's own pull-request holds (#11861): the merge-risk hold
//! (`champion-pr-merge.md`'s `merge-risk` criterion), the critical-file hold
//! (`champion-critical-file-hold.md`, the `critical-files` criterion) and the
//! ratification hold (`champion-ratification-hold.md`, #12992, [`ratification`]).
//!
//! # The shape
//!
//! A held PR keeps `loom:pr` (the Judge's approval stands) and carries:
//!
//! - `loom:blocked` ([`crate::work_finder::BLOCKED_HOLD_LABEL`]);
//! - a reason-only park record in the body
//!   (`<!-- loom:park Blocked by: (unstated) by=champion reason="…" -->`,
//!   [`MERGE_RISK_HOLD_REASON`] / [`CRITICAL_FILE_HOLD_REASON`]); only
//!   Champion or a human releases it;
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
//! [`classify`] is that janitor rule in code (#11932): it tells this hold
//! from another actor's `loom:blocked` park, which Champion skips silently.
//!
//! The marker strings are a wire format: they live in comments on open PRs.

pub mod calibration;
pub mod classify;
pub mod ratification;

/// The merge-risk hold notice's first line.
pub const MERGE_RISK_HOLD_MARKER: &str = "<!-- champion:merge-risk-hold -->";
/// The reversal block a merge after a merge-risk hold carries.
pub const MERGE_RISK_CLEARED_MARKER: &str = "<!-- champion:merge-risk-hold-cleared -->";
/// Champion judged a merge-risk hold clear to release on its own confidence
/// (#12950); carried in the pre-merge comment beside the reversal block.
pub const MERGE_RISK_SELF_RELEASE_MARKER: &str = "<!-- champion:merge-risk-self-release -->";
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
    /// The ratification hold (#12992): merging ratifies a spec/decision record.
    Ratification,
}

impl HoldKind {
    /// The park record `reason` this hold writes.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            HoldKind::MergeRisk => MERGE_RISK_HOLD_REASON,
            HoldKind::CriticalFile => CRITICAL_FILE_HOLD_REASON,
            HoldKind::Ratification => ratification::HOLD_REASON,
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
/// - Ratification (#12992): the latest of its three markers, as for the
///   critical-file hold. It is read first and never waived by a standing
///   authorization class.
///
/// A critical-file hold wins over merge-risk when both stand: it never
/// auto-merges, so it is the stricter of the two. A ratification hold wins over
/// both: only a person's release ends it.
#[must_use]
pub fn standing_hold<S: AsRef<str>>(comments: &[S]) -> Option<HoldKind> {
    let last = |pred: &dyn Fn(&str) -> bool| comments.iter().rposition(|c| pred(c.as_ref()));
    let rt_hold = last(&|c| c.starts_with(ratification::HOLD_MARKER));
    let rt_end = last(&|c| {
        c.starts_with(ratification::CLEARED_MARKER) || c.starts_with(ratification::RELEASED_MARKER)
    });
    if rt_hold.is_some_and(|h| rt_end.is_none_or(|e| h > e)) {
        return Some(HoldKind::Ratification);
    }
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

/// The blocked hold and no other park (`loom:operator` under any name,
/// `loom:frozen`).
fn only_the_blocked_park<L: AsRef<str>>(labels: &[L]) -> bool {
    crate::work_finder::is_blocked_hold(labels)
        && !labels.iter().any(|l| {
            let l = l.as_ref();
            crate::work_finder::PARK_LABELS.contains(&l)
                && !crate::work_finder::BLOCKED_HOLD_NAMES.contains(&l)
        })
}

/// Whether a trusted comment is an operator release of a hold: it *starts*
/// with one of the instructions `champion-pr-merge.md` ("Sticky holds") names,
/// or with the `loom:release-condition-met` marker that closes an operator
/// release condition (#12594) — a held-at-head PR must be redrawn so Champion
/// re-reads the condition. Phrasing that merely mentions them does not match.
fn is_operator_release(comment: &str) -> bool {
    let c = comment.trim_start().to_ascii_lowercase();
    [
        "clear the hold",
        "cleared:",
        "override:",
        "merge anyway",
        "ok to merge",
        "<!-- loom:release-condition-met -->",
    ]
    .iter()
    .any(|p| c.starts_with(p))
}

/// The head SHA a Champion hold recorded (`champion:hold-state head=<sha>`),
/// while nothing has released it (#12900). `trusted` is the PR's trusted
/// comment bodies, oldest first. `None` when no comment records a head, or
/// when a comment after the newest record is a release signal: a trusted
/// operator release ([`is_operator_release`]) or a Judge verdict
/// ([`crate::claim_reconciliation::VERDICT_MARKER_PREFIX`]). Either means the
/// hold should be re-read at the same head, so the caller draws it again.
#[must_use]
pub fn unreleased_hold_head<C: AsRef<str>>(trusted: &[C]) -> Option<String> {
    unreleased_hold(trusted).map(|(head, _)| head)
}

/// [`unreleased_hold_head`] plus the standing-authorization digest the same
/// record carries (`auth=<digest>`, #12933): `Some((head, None))` is a legacy
/// hold. Only trusted bodies are read, so an untrusted quote of the marker
/// supplies neither.
#[must_use]
pub fn unreleased_hold<C: AsRef<str>>(trusted: &[C]) -> Option<(String, Option<String>)> {
    let at = trusted
        .iter()
        .rposition(|c| crate::merge_pr::hold_state::recorded_head(c.as_ref()).is_some())?;
    let released = trusted[at + 1..].iter().any(|c| {
        let c = c.as_ref();
        is_operator_release(c) || c.contains(crate::claim_reconciliation::VERDICT_MARKER_PREFIX)
    });
    if released {
        return None;
    }
    let body = trusted[at].as_ref();
    let head = crate::merge_pr::hold_state::recorded_head(body)?;
    Some((head, crate::merge_pr::hold_state::recorded_auth(body)))
}

/// A short digest of the effective `champion.standingAuthorizations` for the
/// repo at `root` (#12933): the first 16 hex of the sha256 of its canonical
/// JSON (object keys sorted; array order kept). Absent, null and non-array
/// values digest as `[]`, the empty set the prompts also treat them as.
/// Champion records it in the hold marker (`auth=<digest>`); a later change to
/// the set differs from it and releases the hold for one redraw.
#[must_use]
pub fn standing_auth_digest(root: &std::path::Path) -> String {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let set = crate::config_resolver::get_path(&effective, "champion.standingAuthorizations");
    digest_of(set)
}

fn digest_of(set: Option<&serde_json::Value>) -> String {
    use sha2::{Digest, Sha256};
    fn canonical(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let sorted: std::collections::BTreeMap<_, _> =
                    m.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(canonical).collect())
            }
            other => other.clone(),
        }
    }
    let canon = match set {
        Some(v @ serde_json::Value::Array(_)) => canonical(v),
        _ => serde_json::Value::Array(Vec::new()),
    };
    let sum = Sha256::digest(canon.to_string().as_bytes());
    hex::encode(sum)[..16].to_string()
}

/// The label shape of a Champion-held PR that Champion sent to Doctor
/// (#11861): `loom:changes-requested` beside the blocked hold, and no other
/// park. Doctor may work it **only** when a standing trusted hold marker
/// confirms it ([`doctor_may_work`]); the same labels without one are a
/// Doctor-cycle-cap park or another actor's, which Doctor skips.
/// `loom:blocked` stays on throughout the round trip (Doctor, then Judge), so
/// the PR is never a plain approval while the hold stands.
#[must_use]
pub fn doctor_shape<L: AsRef<str>>(labels: &[L]) -> bool {
    labels
        .iter()
        .any(|l| l.as_ref() == "loom:changes-requested")
        && only_the_blocked_park(labels)
}

/// The label shape of a Champion-held PR that Doctor handed back for review
/// (#11861): `loom:review-requested` (or Judge's `loom:reviewing` claim)
/// beside the blocked hold, and no other park. Judge reviews it only when a
/// standing trusted hold marker confirms it ([`judge_may_review`]); both
/// verdicts leave `loom:blocked` on, so an approval lands as `loom:pr` +
/// `loom:blocked`, Champion's hold again.
#[must_use]
pub fn judge_shape<L: AsRef<str>>(labels: &[L]) -> bool {
    labels
        .iter()
        .any(|l| matches!(l.as_ref(), "loom:review-requested" | "loom:reviewing"))
        && only_the_blocked_park(labels)
}

/// The durable record the sweep's Doctor-cycle cap writes (#11861): one
/// comment opening with this marker when it parks a PR
/// (`sweep-wave-lifecycle.md`, `sweep-mode-c-lifecycle.md`).
pub const DOCTOR_CYCLE_CAP_MARKER: &str = "<!-- loom:doctor-cycle-cap -->";

/// The phrase older cap parks logged ("PR #P blocked: doctor cycle exhausted
/// after <k> Doctor→Judge round(s)"), still recognised in a comment.
pub const DOCTOR_CYCLE_CAP_PHRASE: &str = "doctor cycle exhausted";

/// Whether the Doctor-cycle cap was hit after the latest hold notice: a
/// trusted comment carrying [`DOCTOR_CYCLE_CAP_MARKER`] (or the legacy
/// phrase), later than the newest hold marker. Doctor stops working such a
/// PR; the Capped-PR Recovery Pass posts one recommendation for a human and
/// leaves `loom:blocked` on (#11861 review).
#[must_use]
pub fn doctor_cycle_capped<C: AsRef<str>>(trusted: &[C]) -> bool {
    let hold = trusted.iter().rposition(|c| {
        let c = c.as_ref();
        c.starts_with(MERGE_RISK_HOLD_MARKER)
            || c.starts_with(CRITICAL_FILE_HOLD_MARKER)
            || c.starts_with(ratification::HOLD_MARKER)
    });
    trusted.iter().enumerate().any(|(i, c)| {
        let c = c.as_ref();
        hold.is_none_or(|h| i > h)
            && (c.starts_with(DOCTOR_CYCLE_CAP_MARKER)
                || c.to_ascii_lowercase().contains(DOCTOR_CYCLE_CAP_PHRASE))
    })
}

/// The Capped-PR Recovery Pass's one recommendation on a Champion-held PR
/// whose Doctor cycle is capped (`champion-pr-merge.md`). Posted once, with
/// `loom:operator`, which routes the PR to the operator inbox (#11905).
pub const CAPPED_HOLD_RECOMMENDATION_MARKER: &str = "<!-- champion:capped-hold-recommendation -->";

/// Whether the capped-hold recommendation stands on a held PR (#11905): a
/// trusted comment opening with [`CAPPED_HOLD_RECOMMENDATION_MARKER`] after
/// the newest hold marker. Its `loom:operator` is a routing to a human, not
/// the pre-#11861 hold label, so `labels migrate --holds` leaves it on.
#[must_use]
pub fn capped_hold_routed<C: AsRef<str>>(trusted: &[C]) -> bool {
    let hold = trusted.iter().rposition(|c| {
        let c = c.as_ref();
        c.starts_with(MERGE_RISK_HOLD_MARKER)
            || c.starts_with(CRITICAL_FILE_HOLD_MARKER)
            || c.starts_with(ratification::HOLD_MARKER)
    });
    trusted.iter().enumerate().any(|(i, c)| {
        hold.is_none_or(|h| i > h) && c.as_ref().starts_with(CAPPED_HOLD_RECOMMENDATION_MARKER)
    })
}

/// The park record reasons the sweep's Doctor-cycle cap writes
/// (`sweep-*-lifecycle.md`: `doctor cycle exhausted`;
/// `sweep-execution-model.md`: `doctor cap`), compared lowercased.
const DOCTOR_CAP_REASONS: [&str; 2] = [DOCTOR_CYCLE_CAP_PHRASE, "doctor cap"];

/// Whether a PR body declares a Doctor-cycle-cap park (#11905): a park
/// record whose reason opens with a cap reason, or the cap marker itself. A
/// listing-only hint, like [`body_declares_hold`], used only to skip a gate
/// read that could not serve the PR.
#[must_use]
pub fn body_declares_doctor_cap(body: &str) -> bool {
    body.contains(DOCTOR_CYCLE_CAP_MARKER)
        || crate::park_record::parse(body).iter().any(|r| {
            r.reason.as_deref().is_some_and(|x| {
                let x = x.to_ascii_lowercase();
                DOCTOR_CAP_REASONS.iter().any(|c| x.starts_with(c))
            })
        })
}

/// The label shape of a Champion hold at `loom:pr` (#11905): the approval
/// beside the blocked hold and no operator hold, so a count of it never
/// overlaps a `loom:operator` count. This is the label shape only: no code
/// strips `loom:pr` from the pair, so a markerless pair (another actor's park
/// at approval, #11932) is counted too. Telling them apart needs the trusted
/// hold marker ([`is_champion_hold`]), a per-PR comment read.
#[must_use]
pub fn held_approval_shape<L: AsRef<str>>(labels: &[L]) -> bool {
    labels.iter().any(|l| l.as_ref() == "loom:pr")
        && crate::work_finder::is_blocked_hold(labels)
        && !crate::work_finder::is_operator_hold(labels)
}

/// Whether Doctor may work a PR (#11861): [`doctor_shape`] labels, a Champion
/// hold standing in its **trusted** comments, and the Doctor-cycle cap not hit
/// since the hold.
#[must_use]
pub fn doctor_may_work<L: AsRef<str>, C: AsRef<str>>(labels: &[L], trusted: &[C]) -> bool {
    doctor_shape(labels) && standing_hold(trusted).is_some() && !doctor_cycle_capped(trusted)
}

/// Whether Judge may review a PR (#11861): [`judge_shape`] labels and a
/// Champion hold standing in its **trusted** comments.
#[must_use]
pub fn judge_may_review<L: AsRef<str>, C: AsRef<str>>(labels: &[L], trusted: &[C]) -> bool {
    judge_shape(labels) && standing_hold(trusted).is_some()
}

/// Whether a PR body carries a Champion hold's park record (#11861). A cheap,
/// listing-only hint (the body is author-editable), used only to decide that
/// a Doctor run is worth launching; [`doctor_may_work`] on trusted comments
/// decides what Doctor actually works.
#[must_use]
pub fn body_declares_hold(body: &str) -> bool {
    crate::park_record::parse(body).iter().any(|r| {
        r.blocker.is_none()
            && r.reason.as_deref().is_some_and(|x| {
                x == MERGE_RISK_HOLD_REASON
                    || x == CRITICAL_FILE_HOLD_REASON
                    || x == ratification::HOLD_REASON
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_self_release_rides_the_reversal_block_and_is_never_a_hold() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc -->");
        // The self-release block is embedded in the pre-merge comment beside
        // the reversal block (#12950): that, not the self-release marker, ends
        // the episode.
        let merge =
            format!("{MERGE_RISK_CLEARED_MARKER}\n{MERGE_RISK_SELF_RELEASE_MARKER}\nconf 0.93");
        assert_eq!(standing_hold(&[hold.as_str(), merge.as_str()]), None);
        // The self-release marker alone neither opens nor closes a hold.
        let lone = format!("{MERGE_RISK_SELF_RELEASE_MARKER}\nconf 0.93");
        assert_eq!(standing_hold(&[lone.as_str()]), None);
        assert_eq!(standing_hold(&[hold.as_str(), lone.as_str()]), Some(HoldKind::MergeRisk));
    }

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
    fn a_ratification_hold_outranks_the_others_and_ends_only_on_its_markers() {
        use ratification::{CLEARED_MARKER, HOLD_MARKER, RELEASED_MARKER};
        let rt = format!("{HOLD_MARKER}\nmerging ratifies");
        let cf = format!("{CRITICAL_FILE_HOLD_MARKER}\ny");
        let mr = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        assert_eq!(
            standing_hold(&[cf.as_str(), mr.as_str(), rt.as_str()]),
            Some(HoldKind::Ratification)
        );
        let released = format!("{RELEASED_MARKER}\nreleased");
        let cleared = format!("{CLEARED_MARKER}\ncleared");
        assert_eq!(standing_hold(&[rt.as_str(), released.as_str()]), None);
        assert_eq!(standing_hold(&[rt.as_str(), cleared.as_str()]), None);
        // The hold is a Champion hold: the janitor leaves `loom:pr` beside it,
        // and the capped-PR pass sees it as the hold it must not outlive.
        assert!(is_champion_hold(&["loom:pr", "loom:blocked"], &[rt.as_str()]));
        assert!(body_declares_hold(&crate::park_record::render_park(
            &[],
            Some(HOLD_BY),
            Some("2026-10-21T00:00:00Z"),
            Some(HoldKind::Ratification.reason()),
        )));
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
    fn judge_reviews_a_champion_hold_handed_back_only_with_a_trusted_marker() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        for stage in ["loom:review-requested", "loom:reviewing"] {
            assert!(judge_may_review(&[stage, "loom:blocked"], &[hold.as_str()]), "{stage}");
            assert!(!judge_may_review::<&str, &str>(&[stage, "loom:blocked"], &[]), "{stage}");
            assert!(!judge_may_review(&[stage, "loom:blocked", "loom:operator"], &[hold.as_str()]));
        }
        assert!(!judge_may_review(&["loom:changes-requested", "loom:blocked"], &[hold.as_str()]));
    }

    #[test]
    fn a_capped_held_pr_is_not_doctor_work() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        let cap = "PR #7 blocked: doctor cycle exhausted after 3 Doctor→Judge round(s)";
        let shape = ["loom:changes-requested", "loom:blocked"];
        assert!(!doctor_may_work(&shape, &[hold.as_str(), cap]));
        // A cap from before the current hold episode does not count.
        assert!(doctor_may_work(&shape, &[cap, hold.as_str()]));
        // The durable marker is the cap record (#11861).
        let marked = format!("{DOCTOR_CYCLE_CAP_MARKER}\nPR #7 parked");
        assert!(!doctor_may_work(&shape, &[hold.as_str(), marked.as_str()]));
    }

    #[test]
    fn a_capped_hold_recommendation_counts_only_after_the_current_hold() {
        let hold = format!("{MERGE_RISK_HOLD_MARKER}\nx");
        let rec = format!("{CAPPED_HOLD_RECOMMENDATION_MARKER}\nreview, close or release");
        assert!(capped_hold_routed(&[hold.as_str(), rec.as_str()]));
        assert!(!capped_hold_routed(&[rec.as_str(), hold.as_str()]), "a new hold episode");
        assert!(!capped_hold_routed(&[hold.as_str()]));
        let quoted = format!("see {CAPPED_HOLD_RECOMMENDATION_MARKER}");
        assert!(!capped_hold_routed(&[hold.as_str(), quoted.as_str()]), "quoted");
    }

    #[test]
    fn a_body_doctor_cap_is_recognised_by_reason_or_marker() {
        let rec = |reason: &str| {
            crate::park_record::render_park(
                &[],
                Some("sweep"),
                Some("2026-10-14T00:00:00Z"),
                Some(reason),
            )
        };
        assert!(body_declares_doctor_cap(&rec("doctor cycle exhausted")));
        assert!(body_declares_doctor_cap(&rec("doctor cap")));
        assert!(body_declares_doctor_cap(&format!("body\n{DOCTOR_CYCLE_CAP_MARKER}")));
        assert!(!body_declares_doctor_cap(&rec(MERGE_RISK_HOLD_REASON)));
        assert!(!body_declares_doctor_cap(&rec("rebase budget exhausted")));
        assert!(!body_declares_doctor_cap("no record"));
    }

    #[test]
    fn the_held_approval_shape_excludes_operator_holds() {
        assert!(held_approval_shape(&["loom:pr", "loom:blocked"]));
        assert!(held_approval_shape(&["loom:pr", "loom:needs-capability"]), "retired name");
        assert!(!held_approval_shape(&["loom:pr"]));
        assert!(!held_approval_shape(&["loom:blocked", "loom:changes-requested"]));
        for op in ["loom:operator", "loom:operator-only"] {
            assert!(!held_approval_shape(&["loom:pr", "loom:blocked", op]), "{op}");
        }
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

    fn hold_note(head: &str) -> String {
        format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head={head} -->\nHolding")
    }

    #[test]
    fn a_hold_records_its_head_until_something_releases_it() {
        let hold = hold_note("abc1234");
        assert_eq!(unreleased_hold_head(&[hold.as_str()]), Some("abc1234".into()));
        let empty: [&str; 0] = [];
        assert_eq!(unreleased_hold_head(&empty), None, "no record fails open");
        assert_eq!(unreleased_hold_head(&["just prose"]), None);
        // The latest record wins.
        let later = hold_note("def5678");
        assert_eq!(unreleased_hold_head(&[hold.as_str(), later.as_str()]), Some("def5678".into()));
        // Unrelated chatter after the hold does not release it.
        assert_eq!(
            unreleased_hold_head(&[hold.as_str(), "do not clear the hold yet"]),
            Some("abc1234".into())
        );
    }

    #[test]
    fn an_operator_clear_or_a_judge_verdict_after_the_hold_releases_it() {
        let hold = hold_note("abc1234");
        for release in [
            "clear the hold \u{2014} QA signed off",
            "  Cleared: fine",
            "merge anyway, it is urgent",
            "<!-- loom:verdict-sha sha=abc1234 verdict=approved -->\n**Approved**",
            // #12594: closing an operator release condition must redraw the PR.
            "<!-- loom:release-condition-met -->\npredecessor merged, healthy on every host",
        ] {
            assert_eq!(unreleased_hold_head(&[hold.as_str(), release]), None, "{release}");
        }
        // The condition itself, or a notice quoting the -met marker, is not a release.
        assert_eq!(
            unreleased_hold_head(&[
                hold.as_str(),
                "<!-- loom:release-condition -->\nrelease after #12000 merges",
                "Not merging: it closes when a comment opens with `<!-- loom:release-condition-met -->`",
            ]),
            Some("abc1234".into())
        );
        // A release that predates a fresh hold record does not carry over.
        let fresh = hold_note("def5678");
        assert_eq!(
            unreleased_hold_head(&[hold.as_str(), "clear the hold", fresh.as_str()]),
            Some("def5678".into())
        );
    }

    #[test]
    fn a_hold_records_its_standing_authorization_digest() {
        let with = format!(
            "{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc1234 auth=0123456789abcdef -->"
        );
        assert_eq!(
            unreleased_hold(&[with.as_str()]),
            Some(("abc1234".into(), Some("0123456789abcdef".into())))
        );
        // Legacy marker: head only.
        let legacy = hold_note("abc1234");
        assert_eq!(unreleased_hold(&[legacy.as_str()]), Some(("abc1234".into(), None)));
        assert_eq!(unreleased_hold_head(&[with.as_str()]), Some("abc1234".into()));
        // The newest record's digest wins, not an older one's.
        assert_eq!(
            unreleased_hold(&[with.as_str(), legacy.as_str()]),
            Some(("abc1234".into(), None))
        );
        // A release after the hold drops the digest with the head.
        assert_eq!(unreleased_hold(&[with.as_str(), "clear the hold"]), None);
    }

    #[test]
    fn the_digest_ignores_key_order_and_treats_absent_as_empty() {
        use serde_json::json;
        let a = json!([{"id": "x", "classes": ["a"]}]);
        let b = json!([{"classes": ["a"], "id": "x"}]);
        assert_eq!(digest_of(Some(&a)), digest_of(Some(&b)));
        assert_ne!(digest_of(Some(&a)), digest_of(Some(&json!([{"id": "y"}]))));
        let empty = digest_of(Some(&json!([])));
        assert_eq!(digest_of(None), empty);
        assert_eq!(digest_of(Some(&json!(null))), empty);
        assert_eq!(empty.len(), 16);
    }
}
