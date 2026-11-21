//! Return a PR held only by a machine CI gate to review once its exact head is
//! green (#10178), and — when the head is red only because the base was red —
//! move the branch so a green head becomes reachable (#14813).
//!
//! # The stall this closes
//!
//! Judge's CI-failing verdict (`judge-ci-red.md` step 5) leaves
//! `loom:changes-requested` + `loom:ci-failure` on the PR. When the CI later
//! goes green nothing takes them off: head-move invalidation
//! ([`super::verdict_invalidation`]) fires only on a head move and refuses a PR
//! carrying `loom:blocked`; a re-run that goes green on the same SHA, or `main`
//! recovering, moves no head at all; and `stale_blocked.rs` deliberately leaves
//! both labels in place when it unparks. PR #10073 sat about 18.5 hours.
//!
//! #10178 closed the same-head half: release on verified exact-head green. The
//! other half stayed open (#14813): a head that went red **because the base was
//! red** never turns green by itself once the base recovers — nothing re-runs
//! CI or updates the branch while the PR is parked (measured on
//! klayout-tools #6242 / #6078 / #4334, 2026-11-20).
//!
//! # What this pass does
//!
//! For each open PR it clears `loom:ci-failure` and `loom:changes-requested`,
//! and re-adds `loom:review-requested`, only when **every** rule holds:
//!
//! 1. the PR carries both labels and no approval/claim/hold label
//!    (`loom:pr`, `loom:reviewing`, `loom:treating`, `loom:review-requested`,
//!    any hold other than a bare `loom:blocked`);
//! 2. `loom:blocked`, if present, is a machine CI-gate park: the PR body has
//!    park records, every one names no blocker and carries the reason
//!    [`CI_GATE_PARK_REASON`]. A bare `loom:blocked`, a named blocker or any
//!    other reason is a real hold and is left alone;
//! 3. `forge wait-checks` semantics report GREEN for the **exact** head SHA the
//!    listing named (pending, none, unreadable: do nothing);
//! 4. the newest trusted verdict comment is a `changes-requested` verdict that
//!    declares itself a CI-only gate ([`CI_ONLY_MARKER`], or the header of
//!    `judge-ci-red.md`'s template), and no Champion hold is standing;
//! 5. the head is re-read just before the write and still equals the head that
//!    was green;
//! 6. **the update path (#14813).** When rule 3 answers RED instead, the pass
//!    plans exactly one branch update — a server-side merge of the recovered
//!    default branch into the PR branch ([`Forge::update_branch`], a merge
//!    commit, never a force-push, guarded by the expected head) — but only
//!    with **positive evidence that every red check is inherited**: each
//!    failing check on the head must fail the same way (matching conclusion
//!    and output title) on the PR's merge base with the default branch, and
//!    be terminal-green on the default branch's current tip, whose required
//!    checks must themselves be green. A green default branch plus a CI-only
//!    rejection alone also fits a PR-local regression, so any attribution
//!    doubt (a check unknown or pending anywhere, a base that is not the
//!    default branch, a required-context lookup that fails) leaves the park
//!    exactly where it is. The update keeps the gate: release still needs
//!    rule 3 on the NEW head, verified on a later tick. A confirmed merge
//!    conflict on the update hands the PR to Doctor's conflict lane
//!    (`loom:merge-conflict`, the #14810 routing) instead of leaving it
//!    parked. A retry for the same (head, base) pair is suppressed by the
//!    [`UPDATE_MARKER_PREFIX`] comment, so one pair yields one request.
//!
//! The audit comment is posted first (same order as the stale-verdict clear),
//! then the labels are swapped. Nothing is approved or merged here: the Judge
//! reviews normally afterwards.
//!
//! A sibling file because `claim_reconciliation.rs` is frozen by the file-size
//! ratchet. The decision functions are pure; every forge touch goes through
//! [`Forge`], so the #10073 replay runs against an in-memory fake.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::open_pr_listing::RestPull;
use super::{gh_call, VerdictKind};
use crate::forge_wait_checks::{self, reads::GhReads, Clock, Outcome, Selector};

/// The park record `reason` of a `loom:blocked` that only mirrors a CI gate.
/// Anything else on a `loom:blocked` PR is a real hold.
pub const CI_GATE_PARK_REASON: &str = "ci-gate";

/// Judge stamps this into a changes-requested verdict whose only finding is a
/// failing CI run (`judge-ci-red.md` step 5).
pub const CI_ONLY_MARKER: &str = "<!-- loom:ci-gate-only -->";

/// The header of that template, which verdicts posted before the marker carry.
const CI_ONLY_HEADER: &str = "Changes Requested - CI Failing";

/// Does `body` declare itself a CI-only gate — Judge's [`CI_ONLY_MARKER`], or
/// the [`CI_ONLY_HEADER`] that verdicts posted before the marker carry?
///
/// Shared with [`super::verdict_invalidation`] (#13822) so the two passes
/// cannot drift apart on what "CI-only" means. They need the same answer for
/// opposite reasons: this pass recovers such a verdict once the head is green,
/// and that one refuses to carry it across an equivalent head move — because
/// a CI-only verdict records no review, and the head move IS the change it
/// asked for. If one recognised a verdict the other did not, a PR would land
/// back in exactly the #13822 livelock: a rejection re-anchored onto the
/// head that fixed it, with no pass willing to clear it.
#[must_use]
pub fn is_ci_only_verdict(body: &str) -> bool {
    body.contains(CI_ONLY_MARKER) || body.contains(CI_ONLY_HEADER)
}

/// Written by this pass into its audit comment; names the head it cleared for.
pub const RECOVERY_MARKER_PREFIX: &str = "<!-- loom:ci-gate-recovery sha=";

/// Written by this pass when it requests a branch update for a red inherited
/// head (#14813); names the head and the recovered base tip it merged. A
/// later tick finding this exact pair again makes no further request.
pub const UPDATE_MARKER_PREFIX: &str = "<!-- loom:ci-gate-update requested head=";

/// The marker for one (head, base) pair, as posted and as searched for.
#[must_use]
pub fn update_marker(head: &str, base: &str) -> String {
    format!("{UPDATE_MARKER_PREFIX}{head} base={base} -->")
}

/// Written by this pass when a branch update confirmed a merge conflict and
/// the PR was handed to Doctor's conflict lane instead (#14813, #14810).
pub const CONFLICT_MARKER_PREFIX: &str = "<!-- loom:ci-gate-update conflicted head=";

/// The marker for one conflicted (head, base) pair.
#[must_use]
pub fn conflict_marker(head: &str, base: &str) -> String {
    format!("{CONFLICT_MARKER_PREFIX}{head} base={base} -->")
}

const CI_FAILURE: &str = "loom:ci-failure";
const CHANGES_REQUESTED: &str = "loom:changes-requested";
const BLOCKED: &str = "loom:blocked";
const REVIEW_REQUESTED: &str = "loom:review-requested";
/// Doctor's conflict lane (#14810 routing): with `loom:changes-requested` it
/// is the pair Doctor's queue draws a conflicted repair PR on.
const MERGE_CONFLICT: &str = "loom:merge-conflict";

/// A label that means somebody is acting on the PR or has approved it.
const ACTIVE_LABELS: [&str; 4] = [
    "loom:pr",
    "loom:reviewing",
    "loom:treating",
    REVIEW_REQUESTED,
];

/// Operator holds named explicitly, on top of the registry's hold set, so a
/// registry drift can never make this pass clear one.
const OPERATOR_LABELS: [&str; 4] = [
    "loom:operator",
    "loom:operator-only",
    "loom:operator-decision",
    "loom:frozen",
];

/// Why a PR was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Does not carry the CI-gate label pair.
    NotCiGated,
    /// Carries an approval, claim or review label.
    Active(String),
    /// A human/operator hold, or any hold other than `loom:blocked`.
    Hold(String),
    /// `loom:blocked` without a machine CI-gate park record.
    BlockedNotCiGate,
    /// The newest trusted verdict is not a CI-only changes-requested one.
    VerdictNotCiOnly,
    /// A Champion hold is standing.
    ChampionHold,
    /// The exact head is not provably green.
    NotGreen(String),
    /// The head moved between the green read and the write.
    HeadMoved,
    /// A forge read the decision needs failed (fail closed).
    Unreadable(&'static str),
    /// The default branch's required checks are not green (or the PR does not
    /// target the default branch), so there is no recovered base to merge.
    BaseNotRecovered(String),
    /// The red checks are not positively attributed to a prior base failure:
    /// a green default branch plus a CI-only rejection also fits a PR-local
    /// regression, so any doubt leaves the park in place (#14813).
    NotInherited(String),
    /// A branch update for exactly this (head, base) pair was already
    /// requested; the new head's green verdict is what the pass waits for.
    UpdatePending,
}

/// What a clean pass over one PR concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Clear the gate; `clear_blocked` when the `loom:blocked` is a CI-gate park.
    Recover {
        clear_blocked: bool,
    },
    /// Merge the recovered default branch (tip `base`) into the PR branch,
    /// guarded by the listing head (#14813). The gate stays on: release is
    /// still [`Plan::Recover`] on the new head, verified on a later tick.
    UpdateBranch {
        base: String,
    },
    Skip(Skip),
}

/// Rules 1 and 2: decided from the listing row alone (no forge call).
pub fn label_gate(labels: &[String], body: Option<&str>) -> Result<bool, Skip> {
    let has = |l: &str| labels.iter().any(|x| x == l);
    if !(has(CI_FAILURE) && has(CHANGES_REQUESTED)) {
        return Err(Skip::NotCiGated);
    }
    if let Some(l) = ACTIVE_LABELS.iter().find(|l| has(l)) {
        return Err(Skip::Active((*l).to_string()));
    }
    let blocked_names = &crate::work_finder::BLOCKED_HOLD_NAMES;
    let held = labels.iter().find(|l| {
        OPERATOR_LABELS.contains(&l.as_str())
            || (crate::work_finder::HOLD_NAMES.contains(&l.as_str())
                && !blocked_names.contains(&l.as_str()))
    });
    if let Some(l) = held {
        return Err(Skip::Hold(l.clone()));
    }
    if !crate::work_finder::is_blocked_hold(labels) {
        return Ok(false);
    }
    if is_ci_gate_park(body.unwrap_or_default()) {
        Ok(true)
    } else {
        Err(Skip::BlockedNotCiGate)
    }
}

/// Every park record in `body` is the machine CI-gate park: no blocker, the
/// [`CI_GATE_PARK_REASON`] reason. No record at all is not one.
#[must_use]
pub fn is_ci_gate_park(body: &str) -> bool {
    let records = crate::park_record::parse(body);
    !records.is_empty()
        && crate::park_record::blockers(body).is_empty()
        && records.iter().all(|r| {
            r.blocker.is_none() && r.reason.as_deref().map(str::trim) == Some(CI_GATE_PARK_REASON)
        })
}

/// Rule 4, from **trusted** comment bodies oldest first.
pub fn verdict_gate(trusted: &[String]) -> Result<(), Skip> {
    if crate::champion_hold::standing_hold(trusted).is_some() {
        return Err(Skip::ChampionHold);
    }
    let newest = trusted.iter().rev().find_map(|b| {
        let one = std::slice::from_ref(b);
        [VerdictKind::ChangesRequested, VerdictKind::Approved]
            .into_iter()
            .find(|k| super::extract_latest_verdict_sha(one, *k).is_some())
            .map(|k| (k, b))
    });
    match newest {
        Some((VerdictKind::ChangesRequested, b)) if is_ci_only_verdict(b) => Ok(()),
        _ => Err(Skip::VerdictNotCiOnly),
    }
}

/// Whether the listing's own head SHA is green.
pub enum Ci {
    Green,
    NotGreen(String),
}

/// The classified checks of one commit, plus the per-name evidence the update
/// decision needs (#14813): the failure signatures of its failing checks and
/// the names that are terminal-green.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChecksAtSha {
    /// The rollup `forge wait-checks` semantics would report.
    pub rollup: forge_wait_checks::verdict::Rollup,
    /// For each failing check name, the set of `(conclusion, output title)`
    /// signatures of its genuinely failing rows.
    pub failing: BTreeMap<String, BTreeSet<(String, String)>>,
    /// Names with at least one row on the commit, every row terminal-success.
    pub green: BTreeSet<String>,
}

/// `compare/{base}...{head}` reduced to the two commits the update decision
/// reads: the base branch's current tip and the merge base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compare {
    pub base_tip: String,
    pub merge_base: String,
}

/// What [`Forge::update_branch`] concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The forge accepted the merge (202): a merge commit is being created.
    Merged,
    /// Nothing to merge — the head already contains the base.
    UpToDate,
    /// The merge confirmed a conflict; the why is the forge's own message.
    Conflict(String),
    /// The expected head no longer matched — someone moved the branch first.
    StaleHead,
    /// Anything else (permission, rate limit, unknown refusal).
    Failed(String),
}

/// The forge touches the pass needs; production is [`GhForge`].
pub trait Forge {
    /// Is the exact `head` green (every observed and required check)?
    fn ci(&mut self, head: &str) -> Ci;
    /// The PR's head right now, `None` when unreadable.
    fn current_head(&mut self, number: u32) -> Option<String>;
    /// Trusted comment bodies, oldest first; `None` when unreadable.
    fn trusted_comments(&mut self, number: u32) -> Option<Vec<String>>;
    /// Stand down any armed auto-merge/queue entry; a line for the audit comment.
    fn disarm(&mut self, number: u32) -> Option<String>;
    fn comment(&mut self, number: u32, body: &str) -> Result<()>;
    fn swap_labels(&mut self, number: u32, remove: &[&str], add: &[&str]) -> Result<()>;
    /// The classified checks of one commit (#14813); `None` when unreadable.
    fn checks(&mut self, sha: &str) -> Option<ChecksAtSha>;
    /// The repository's default branch name; `None` when unreadable.
    fn default_branch(&mut self) -> Option<String>;
    /// `compare/{base_ref}...{head}`: the base tip and the merge base;
    /// `None` when unreadable.
    fn compare(&mut self, base_ref: &str, head: &str) -> Option<Compare>;
    /// Are the default branch's required checks green at `tip` (falling back
    /// to every observed check when the branch requires none, the
    /// `wait-checks --required-only` rule)?
    fn base_required_green(&mut self, base_ref: &str, tip: &str) -> bool;
    /// Merge the base branch into PR `number`'s branch, guarded by
    /// `expected_head` — a merge commit, never a force-push.
    fn update_branch(&mut self, number: u32, expected_head: &str) -> UpdateOutcome;
}

/// Counts for one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub candidates: usize,
    pub recovered: usize,
    /// Branch updates requested for verified inherited failures (#14813).
    pub updated: usize,
    /// Updates that confirmed a conflict and were handed to Doctor.
    pub conflicted: usize,
}

/// Decide one PR. The label/park/hold gate is free and first; the comments
/// read comes before the CI read (#14813: the update path writes too, so the
/// verdict and hold checks run before ANY write this pass can make); then the
/// ETag'd check-runs, and the head re-read just before any write.
pub fn plan_one(f: &mut impl Forge, pr: &RestPull) -> Plan {
    let clear_blocked = match label_gate(&pr.labels, pr.body.as_deref()) {
        Ok(b) => b,
        Err(s) => return Plan::Skip(s),
    };
    let Some(head) = pr.head_sha.as_deref().filter(|h| !h.is_empty()) else {
        return Plan::Skip(Skip::Unreadable("listing has no head sha"));
    };
    let Some(comments) = f.trusted_comments(pr.number) else {
        return Plan::Skip(Skip::Unreadable("comments"));
    };
    if let Err(s) = verdict_gate(&comments) {
        return Plan::Skip(s);
    }
    match f.ci(head) {
        Ci::Green => match f.current_head(pr.number) {
            Some(h) if h == head => Plan::Recover { clear_blocked },
            Some(_) => Plan::Skip(Skip::HeadMoved),
            None => Plan::Skip(Skip::Unreadable("head")),
        },
        Ci::NotGreen(why) => plan_update(f, pr, head, &comments, &why),
    }
}

/// Rule 6 (#14813): the update decision for a red head (`why` is the CI
/// sentinel that made it red). Every read here is fail-closed — an
/// attribution, required-check or head doubt leaves the park in place — and
/// the head is re-read just before the write.
fn plan_update(
    f: &mut impl Forge,
    pr: &RestPull,
    head: &str,
    comments: &[String],
    why: &str,
) -> Plan {
    let Some(default_branch) = f.default_branch() else {
        return Plan::Skip(Skip::Unreadable("default branch"));
    };
    if pr.base_ref.as_deref() != Some(default_branch.as_str()) {
        return Plan::Skip(Skip::BaseNotRecovered(format!(
            "PR base {:?} is not the default branch {default_branch:?}",
            pr.base_ref
        )));
    }
    let Some(compare) = f.compare(&default_branch, head) else {
        return Plan::Skip(Skip::Unreadable("compare"));
    };
    if !f.base_required_green(&default_branch, &compare.base_tip) {
        return Plan::Skip(Skip::BaseNotRecovered(format!(
            "default branch required checks not green at {}",
            short(&compare.base_tip)
        )));
    }
    let Some(head_checks) = f.checks(head) else {
        return Plan::Skip(Skip::Unreadable("head checks"));
    };
    if head_checks.rollup.total == 0 {
        return Plan::Skip(Skip::NotInherited("no check data on the head".into()));
    }
    if !head_checks.rollup.blocking_pending().is_empty()
        || !head_checks.rollup.blocking_cancelled().is_empty()
    {
        return Plan::Skip(Skip::NotInherited(
            "the head still has pending or cancelled checks".into(),
        ));
    }
    let failing = head_checks.rollup.blocking_failing();
    if failing.is_empty() {
        return Plan::Skip(Skip::NotGreen(format!(
            "red ({why}) but no finished failing check is attributable"
        )));
    }
    let Some(base_checks) = f.checks(&compare.merge_base) else {
        return Plan::Skip(Skip::Unreadable("merge-base checks"));
    };
    let Some(tip_checks) = f.checks(&compare.base_tip) else {
        return Plan::Skip(Skip::Unreadable("base-tip checks"));
    };
    for name in &failing {
        let Some(inherited) = base_checks.failing.get(name) else {
            return Plan::Skip(Skip::NotInherited(format!(
                "{name} was not failing on the merge base {}",
                short(&compare.merge_base)
            )));
        };
        let head_signatures = head_checks
            .failing
            .get(name)
            .into_iter()
            .flatten()
            .collect::<BTreeSet<_>>();
        if !head_signatures.iter().all(|s| inherited.contains(*s)) {
            return Plan::Skip(Skip::NotInherited(format!(
                "{name} fails differently than the merge base"
            )));
        }
        if !tip_checks.green.contains(name) {
            return Plan::Skip(Skip::NotInherited(format!(
                "{name} is not green on the recovered base {}",
                short(&compare.base_tip)
            )));
        }
    }
    // Idempotent for the same pair: one (head, base) request, then the new
    // head's verified-green verdict is the only way out.
    if comments.iter().any(|c| {
        c.contains(&update_marker(head, &compare.base_tip))
            || c.contains(&conflict_marker(head, &compare.base_tip))
    }) {
        return Plan::Skip(Skip::UpdatePending);
    }
    match f.current_head(pr.number) {
        Some(h) if h == head => Plan::UpdateBranch {
            base: compare.base_tip,
        },
        Some(_) => Plan::Skip(Skip::HeadMoved),
        None => Plan::Skip(Skip::Unreadable("head")),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(9).collect()
}

fn audit_body(head: &str, clear_blocked: bool, disarm_line: Option<&str>) -> String {
    let blocked = if clear_blocked {
        "\n- removed `loom:blocked`: its park record names no blocker and the reason \
         `ci-gate`, so it only mirrored the CI gate"
    } else {
        ""
    };
    let disarm = disarm_line.map(|l| format!("\n{l}")).unwrap_or_default();
    format!(
        "{RECOVERY_MARKER_PREFIX}{head} -->\n**CI gate cleared** (#10178). The check runs on \
         `{short}` are all green, and the last verdict on this PR was a CI-only \
         changes-requested.\n\n- removed `loom:ci-failure` and `loom:changes-requested`{blocked}\n\
         - added `loom:review-requested`{disarm}\n\nNothing is approved by this: the Judge \
         reviews the PR as usual. No human or operator hold was touched.",
        short = head.chars().take(9).collect::<String>(),
    )
}

/// Carry out one recovery: comment first (skipped when this head's notice is
/// already there), then the label swap.
fn apply(f: &mut impl Forge, pr: &RestPull, head: &str, clear_blocked: bool) -> Result<()> {
    let disarm = f.disarm(pr.number);
    let recorded = f.trusted_comments(pr.number).is_some_and(|c| {
        c.iter()
            .any(|b| b.contains(&format!("{RECOVERY_MARKER_PREFIX}{head} -->")))
    });
    if !recorded || disarm.is_some() {
        f.comment(pr.number, &audit_body(head, clear_blocked, disarm.as_deref()))?;
    }
    let mut remove = vec![CI_FAILURE, CHANGES_REQUESTED];
    if clear_blocked {
        remove.push(BLOCKED);
    }
    f.swap_labels(pr.number, &remove, &[REVIEW_REQUESTED])
}

fn update_body(head: &str, base: &str) -> String {
    format!(
        "{}\n**CI-gate branch update requested** (#14813). Every failing check on \
         `{}` was verified to fail the same way on this PR's merge base and to be \
         green on the recovered default branch tip `{}`, and the default branch's \
         required checks are green there — so the failure is inherited from the \
         base, not from this diff. The default branch is being merged into the PR \
         branch (a merge commit, never a force-push, guarded by the expected head \
         `{}`). The park and the CI gate stay on: this PR returns to review only \
         once this pass verifies the NEW head's checks are exactly green (#10178).",
        update_marker(head, base),
        short(head),
        short(base),
        short(head),
    )
}

fn conflict_body(head: &str, base: &str, why: &str) -> String {
    format!(
        "{}\n**CI-gate update handed to Doctor** (#14813, #14810). Merging the \
         recovered default branch `{}` into this PR branch conflicts: {why}. The \
         ci-gate park (`loom:blocked`) and `loom:ci-failure` are removed; \
         `loom:changes-requested` stays and `loom:merge-conflict` is added, so \
         Doctor's conflict lane picks the PR up and resolves the rebase. Review \
         resumes through the normal repair flow afterwards.",
        conflict_marker(head, base),
        short(base),
    )
}

/// What applying a [`Plan::UpdateBranch`] did.
enum UpdateDone {
    /// A merge was requested (or there was nothing to merge); the gate stays.
    Requested,
    /// The update confirmed a conflict and the PR went to Doctor's lane.
    Conflicted,
}

/// Carry out one branch update: the forge request first, then the audit
/// comment (so a failed request leaves no marker that would suppress the
/// retry), or — on a confirmed conflict — the handoff comment and Doctor's
/// conflict-lane labels (#14810 routing).
fn apply_update(f: &mut impl Forge, pr: &RestPull, head: &str, base: &str) -> Result<UpdateDone> {
    match f.update_branch(pr.number, head) {
        UpdateOutcome::Merged | UpdateOutcome::UpToDate => {
            f.comment(pr.number, &update_body(head, base))?;
            Ok(UpdateDone::Requested)
        }
        UpdateOutcome::Conflict(why) => {
            f.comment(pr.number, &conflict_body(head, base, &why))?;
            let mut remove = vec![CI_FAILURE];
            if pr.has_label(BLOCKED) {
                remove.push(BLOCKED);
            }
            f.swap_labels(pr.number, &remove, &[MERGE_CONFLICT])?;
            Ok(UpdateDone::Conflicted)
        }
        UpdateOutcome::StaleHead => {
            Err(anyhow!("expected head {head} no longer matched — the branch moved first"))
        }
        UpdateOutcome::Failed(why) => Err(anyhow!("update-branch refused: {why}")),
    }
}

/// One pass over `open` through `f`.
pub fn run_pass(f: &mut impl Forge, open: &[RestPull]) -> Stats {
    let mut stats = Stats::default();
    for pr in open {
        if label_gate(&pr.labels, pr.body.as_deref()).is_ok() {
            stats.candidates += 1;
        }
        match plan_one(f, pr) {
            Plan::Recover { clear_blocked } => {
                let head = pr.head_sha.clone().unwrap_or_default();
                match apply(f, pr, &head, clear_blocked) {
                    Ok(()) => {
                        stats.recovered += 1;
                        log::info!(
                            "claim_reconciliation: PR #{} was held only by a CI gate and its head \
                             {head} is green — returned to loom:review-requested (#10178)",
                            pr.number
                        );
                    }
                    Err(e) => log::warn!(
                        "claim_reconciliation: failed to return PR #{} to review after its CI \
                         gate cleared: {e}",
                        pr.number
                    ),
                }
            }
            Plan::UpdateBranch { base } => {
                let head = pr.head_sha.clone().unwrap_or_default();
                match apply_update(f, pr, &head, &base) {
                    Ok(UpdateDone::Requested) => {
                        stats.updated += 1;
                        log::info!(
                            "claim_reconciliation: PR #{}'s red head {head} is an inherited base \
                             failure and the default branch recovered — merged {} into the PR \
                             branch; the gate stays until the new head is exactly green \
                             (#14813)",
                            pr.number,
                            short(&base)
                        );
                    }
                    Ok(UpdateDone::Conflicted) => {
                        stats.conflicted += 1;
                        log::info!(
                            "claim_reconciliation: PR #{}'s ci-gate update conflicted with {} — \
                             handed to Doctor's conflict lane (#14813, #14810)",
                            pr.number,
                            short(&base)
                        );
                    }
                    Err(e) => log::warn!(
                        "claim_reconciliation: failed to update PR #{}'s branch for its inherited \
                         CI failure: {e} (#14813)",
                        pr.number
                    ),
                }
            }
            Plan::Skip(Skip::NotCiGated) => {}
            Plan::Skip(s) => log::debug!(
                "claim_reconciliation: PR #{} stays on its CI gate: {s:?} (#10178)",
                pr.number
            ),
        }
    }
    stats
}

/// The production pass for one registered `root`. Gated by the stale-verdict
/// switch ([`super::VERDICT_STALENESS_ENABLED_ENV`]): off means no writes.
pub(super) fn reconcile(gh_bin: &Path, root: &Path, open: &[RestPull]) -> Stats {
    if !super::verdict_staleness_enabled() {
        return Stats::default();
    }
    // Nothing gated: no reader is even built.
    if !open
        .iter()
        .any(|p| label_gate(&p.labels, p.body.as_deref()).is_ok())
    {
        return Stats::default();
    }
    run_pass(&mut GhForge::new(gh_bin, root), open)
}

/// A clock for a single-poll `wait`: time never advances, so `--timeout 0`
/// semantics apply — the first poll is the only one.
struct OnePoll;

impl Clock for OnePoll {
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&mut self, _: Duration) {}
}

/// [`Forge`] over `gh`.
pub struct GhForge<'a> {
    gh_bin: &'a Path,
    root: &'a Path,
    reads: Option<Option<GhReads>>,
}

impl<'a> GhForge<'a> {
    fn new(gh_bin: &'a Path, root: &'a Path) -> Self {
        Self {
            gh_bin,
            root,
            reads: None,
        }
    }

    fn reads(&mut self) -> Option<&mut GhReads> {
        let (gh_bin, root) = (self.gh_bin, self.root);
        self.reads
            .get_or_insert_with(|| {
                let repo = std::env::var("LOOM_REPO").ok().filter(|s| !s.is_empty());
                let store = crate::forge_etag_store::daemon_store_dir();
                GhReads::new(
                    gh_bin.to_path_buf(),
                    Some(root.to_path_buf()),
                    store.clone().unwrap_or_default(),
                    repo.as_deref(),
                )
                .map(|r| {
                    let r = r.with_caller("claim_reconciliation.ci_gate");
                    if store.is_some() {
                        r
                    } else {
                        r.ephemeral()
                    }
                })
                .map_err(|e| log::debug!("ci gate recovery: no reads for {}: {e}", root.display()))
                .ok()
            })
            .as_mut()
    }
}

impl Forge for GhForge<'_> {
    fn ci(&mut self, head: &str) -> Ci {
        if crate::rate_limit_breaker::global_skip_pass("claim_reconciliation") {
            return Ci::NotGreen("rate-limit breaker suppressing reads".into());
        }
        let Some(reads) = self.reads() else {
            return Ci::NotGreen("no forge reader".into());
        };
        let opts = forge_wait_checks::Opts {
            timeout: Duration::ZERO,
            required_only: false,
            settle_red: false,
            base: None,
            min_interval: forge_wait_checks::DEFAULT_MIN_INTERVAL,
            max_interval: forge_wait_checks::DEFAULT_MAX_INTERVAL,
            settle_polls: crate::merge_pr::zero_checks::settle_polls(None),
            settle_interval: crate::merge_pr::zero_checks::settle_interval(
                None,
                forge_wait_checks::DEFAULT_MIN_INTERVAL,
            ),
            no_run_grace: Duration::from_secs(forge_wait_checks::no_run::DEFAULT_GRACE),
        };
        let (outcome, _) =
            forge_wait_checks::wait(reads, &Selector::Sha(head.to_string()), &opts, &mut OnePoll);
        match outcome {
            Outcome::Green { sha } if sha == head => Ci::Green,
            other => Ci::NotGreen(other.sentinel()),
        }
    }

    fn current_head(&mut self, number: u32) -> Option<String> {
        self.reads()?.pull(number).ok().map(|h| h.sha)
    }

    fn trusted_comments(&mut self, number: u32) -> Option<Vec<String>> {
        super::forge::fetch_comment_bodies(self.gh_bin, self.root, number)
    }

    fn disarm(&mut self, number: u32) -> Option<String> {
        super::auto_merge_disarm::disarm_before_invalidation(self.gh_bin, self.root, number)
    }

    fn comment(&mut self, number: u32, body: &str) -> Result<()> {
        let n = number.to_string();
        let out = gh_call::output(
            gh_call::write("ci_gate.recovery_comment", self.gh_bin, self.root)
                .args(["pr", "comment", &n, "--body", body])
                .args(gh_call::loom_repo_flag()),
        )?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!("gh pr comment failed for #{n}: {}", gh_call::stderr(&out)))
        }
    }

    fn swap_labels(&mut self, number: u32, remove: &[&str], add: &[&str]) -> Result<()> {
        let n = number.to_string();
        let mut inv = gh_call::write("ci_gate.recovery_labels", self.gh_bin, self.root)
            .args(["pr", "edit", &n]);
        for l in remove {
            inv = inv.args(["--remove-label", l]);
        }
        for l in add {
            inv = inv.args(["--add-label", l]);
        }
        let out = gh_call::output(inv.args(gh_call::loom_repo_flag()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!("gh pr edit failed for #{n}: {}", gh_call::stderr(&out)))
        }
    }

    fn checks(&mut self, sha: &str) -> Option<ChecksAtSha> {
        if crate::rate_limit_breaker::global_skip_pass("claim_reconciliation") {
            return None;
        }
        let reads = self.reads()?;
        let runs = reads.check_runs(sha).ok()?;
        let status = reads.statuses(sha).ok()?;
        checks_at(&runs, &status)
    }

    fn default_branch(&mut self) -> Option<String> {
        self.reads()?.default_branch().ok()
    }

    fn compare(&mut self, base_ref: &str, head: &str) -> Option<Compare> {
        let reads = self.reads()?;
        let url = format!("repos/{}/compare/{base_ref}...{head}", reads.repo()?);
        let body = reads
            .read(&url, crate::forge_call_stats::ops::PR_COMMITS_AND_COMPARE)
            .ok()?;
        let v: Value = serde_json::from_str(&body).ok()?;
        let sha_of = |key: &str| {
            v.get(key)
                .and_then(|c| c.get("sha"))
                .and_then(Value::as_str)
                .map(String::from)
        };
        Some(Compare {
            base_tip: sha_of("base_commit")?,
            merge_base: sha_of("merge_base_commit")?,
        })
    }

    fn base_required_green(&mut self, base_ref: &str, tip: &str) -> bool {
        if crate::rate_limit_breaker::global_skip_pass("claim_reconciliation") {
            return false;
        }
        let Some(reads) = self.reads() else {
            return false;
        };
        let opts = forge_wait_checks::Opts {
            timeout: Duration::ZERO,
            required_only: true,
            settle_red: false,
            base: Some(base_ref.to_string()),
            min_interval: forge_wait_checks::DEFAULT_MIN_INTERVAL,
            max_interval: forge_wait_checks::DEFAULT_MAX_INTERVAL,
            settle_polls: crate::merge_pr::zero_checks::settle_polls(None),
            settle_interval: crate::merge_pr::zero_checks::settle_interval(
                None,
                forge_wait_checks::DEFAULT_MIN_INTERVAL,
            ),
            no_run_grace: Duration::from_secs(forge_wait_checks::no_run::DEFAULT_GRACE),
        };
        let (outcome, _) =
            forge_wait_checks::wait(reads, &Selector::Sha(tip.to_string()), &opts, &mut OnePoll);
        matches!(outcome, Outcome::Green { sha } if sha == tip)
    }

    fn update_branch(&mut self, number: u32, expected_head: &str) -> UpdateOutcome {
        let n = number.to_string();
        let path = format!("repos/{{owner}}/{{repo}}/pulls/{n}/update-branch");
        let field = format!("expected_head_sha={expected_head}");
        let inv = gh_call::write("ci_gate.update_branch", self.gh_bin, self.root)
            .args(["api", &path, "--method", "PUT", "--field", &field]);
        let out = match gh_call::output(inv) {
            Ok(o) => o,
            Err(e) => return UpdateOutcome::Failed(format!("{e:#}")),
        };
        let raw = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), gh_call::stderr(&out));
        // `gh api` prints the HTTP status of a refusal as `(HTTP <code>)`.
        let http = raw
            .to_ascii_lowercase()
            .rsplit("(http ")
            .next()
            .and_then(|tail| tail.get(..3))
            .and_then(|digits| digits.parse::<u16>().ok());
        classify_update_response(out.status.success(), http, &raw)
    }
}

/// Classify one `update-branch` response. Pure, so the message spellings are
/// tested directly (#14813). Fail-closed on ambiguity: an unrecognized
/// refusal is [`UpdateOutcome::Failed`] (retried next tick), never a conflict
/// handoff or a success.
fn classify_update_response(success: bool, http: Option<u16>, text: &str) -> UpdateOutcome {
    if success {
        return UpdateOutcome::Merged;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("merge conflict") || (http == Some(422) && lower.contains("conflict")) {
        return UpdateOutcome::Conflict(refusal_message(text, "conflict"));
    }
    if http == Some(409) || lower.contains("expected head") {
        return UpdateOutcome::StaleHead;
    }
    if lower.contains("no new commits") || lower.contains("up to date") {
        return UpdateOutcome::UpToDate;
    }
    UpdateOutcome::Failed(refusal_message(text, ""))
}

/// The human-readable message of a refusal: the first line containing
/// `needle` (case-insensitive), with a JSON `"message": "…"` value unquoted
/// when that is the line's shape — so a handoff comment quotes the forge's
/// own words, not the JSON wrapping them.
fn refusal_message(text: &str, needle: &str) -> String {
    let needle = needle.to_ascii_lowercase();
    let line = text
        .lines()
        .find(|l| needle.is_empty() || l.to_ascii_lowercase().contains(&needle))
        .unwrap_or("unknown refusal")
        .trim();
    let Some(idx) = line.find("\"message\"") else {
        return line.to_string();
    };
    let after = line[idx + "\"message\"".len()..]
        .trim_start()
        .strip_prefix(':');
    let Some(rest) = after else {
        return line.to_string();
    };
    let value = rest.trim().trim_start_matches('"');
    value
        .trim_end()
        .trim_end_matches(',')
        .trim_end_matches('}')
        .trim_end_matches('"')
        .to_string()
}

/// Fold one commit's check-runs and statuses payloads into the evidence the
/// update decision reads: the [`ChecksAtSha`] view over the same rows
/// [`forge_wait_checks::verdict::fold`] classifies. `None` when either
/// payload is outside the contract (fail closed).
fn checks_at(runs: &Value, status: &Value) -> Option<ChecksAtSha> {
    use forge_wait_checks::verdict::{
        combined_rows, genuinely_failing, name_of, SUCCESS_CONCLUSIONS,
    };
    let rollup = forge_wait_checks::verdict::fold(runs, status).ok()?;
    let (rows, _) = combined_rows(runs, status).ok()?;
    let mut failing: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut green: BTreeMap<String, (bool, bool)> = BTreeMap::new(); // (any row, all success)
    for row in &rows {
        let name = name_of(row);
        let conclusion = row.get("conclusion").and_then(Value::as_str).unwrap_or("");
        let completed = row.get("status").and_then(Value::as_str) == Some("completed");
        if genuinely_failing(row) {
            let title = row
                .get("output")
                .and_then(|o| o.get("title"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            failing
                .entry(name.clone())
                .or_default()
                .insert((conclusion.to_string(), title));
        }
        let success = completed && SUCCESS_CONCLUSIONS.contains(&conclusion);
        let entry = green.entry(name).or_insert((false, true));
        entry.0 = true;
        entry.1 = entry.1 && success;
    }
    Some(ChecksAtSha {
        rollup,
        failing,
        green: green
            .into_iter()
            .filter(|(_, (any, all))| *any && *all)
            .map(|(name, _)| name)
            .collect(),
    })
}

#[cfg(test)]
#[path = "ci_gate_recovery_tests.rs"]
mod tests;
