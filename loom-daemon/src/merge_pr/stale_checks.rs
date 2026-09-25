//! The required-check freshness guard (#8248, slice 3 of the merge-pr port).
//!
//! # The incident
//!
//! `main` went red on 2026-09-18 while **every gate worked correctly**:
//!
//! | when (2026) | what |
//! |---|---|
//! | 09-17 22:54 | PR #8078's `File Size Ratchet` runs and **passes** — its tree holds `main_health_gate.rs` at 1816 code lines against a baseline entry of 1845. Correct: 1816 ≤ 1845. |
//! | 09-18 11:45 | PR #8204 tightens that entry **1845 → 1815** (legitimate hygiene: the file had shrunk). All slack is gone. |
//! | 09-18 20:31 | PR #8078 hand-merges. Its ratchet result is **22 hours old** and never re-ran. The merged tree is 1816 vs 1815. **main is red.** |
//!
//! A check result is evidence about **one tree**. A ratchet baseline is
//! **repo-global mutable state**. When a baseline tightens, every in-flight
//! PR's green ratchet result silently becomes a statement about a world that no
//! longer exists — and the forge keeps displaying it as a green check, because
//! branch protection only asks "did this check succeed on this head SHA?", not
//! "is the thing it verified still the thing that will be merged onto?".
//!
//! This is structurally worse than an ordinary stale check: a stale *test*
//! result means "we don't know"; a stale *ratchet* result means "we are
//! asserting slack that has since been spent". The failure is guaranteed rather
//! than probabilistic — the moment the merge lands, `main` is red and every
//! other open PR inherits it. The exposure scales with how far behind PRs run
//! (#8078 was 125 commits behind, which is ordinary here).
//!
//! # The rule this enforces
//!
//! A green run of a **required** check whose `started_at` predates the commit
//! time of the base branch's current tip is not evidence about the tree the PR
//! will actually merge onto. The merge must be refused until the check re-runs
//! against the current head (any no-op push or re-run re-dates it).
//!
//! Deliberate scope choices, so review does not have to infer them:
//!
//! - **Only green runs.** A stale *failure* already blocks the merge: branch
//!   protection re-evaluates the check's current conclusion on the head SHA,
//!   so red evidence cannot silently slip through the way stale green can.
//! - **Only required contexts.** Informational checks are not merge evidence.
//! - **A required context with no run at all is left to the forge**: with
//!   nothing green to re-validate, branch protection's own BLOCKED state owns
//!   the refusal — this guard would add a second mechanism for a behaviour
//!   the forge already guarantees (CI-principles rule 4: one mechanism per
//!   behaviour).
//! - **A pending run is left to the wait paths**: there is no verdict yet, so
//!   there is no stale evidence. `merge-pr.sh`'s check-wait loops and the
//!   forge's merge-time enforcement handle pending checks.
//! - **Fails closed on an undeterminable timestamp.** A green run with a
//!   missing/unparseable `started_at`, or a base tip whose commit time cannot
//!   be resolved, is an *unknown*, and an unknown must refuse the merge
//!   (CI-principles rule 6: `exit 0` has to mean "verified", never "skipped").
//!   This is the difference from an ordinary stale check spelled out above:
//!   where the failure is guaranteed, guessing is how it lands.
//!
//! # Companions
//!
//! This guard is option (1) of #8248. Option (2) — narrowing
//! `check-file-size-budget.sh --update` to touched files by default — removes
//! the *source* of the trap (a routine hygiene commit spending the slack of
//! files nobody in that PR was thinking about). Both shipped; option (3)
//! (branch ruleset requiring up-to-date branches) was rejected as forcing a
//! rebase per merge at a cadence this fleet would pay constantly.

use chrono::{DateTime, Utc};

/// The only stdout a caller may treat as "every required check is fresh".
///
/// A sentinel rather than silence, for the same reason
/// [`super::labels::CLEAN`] is: passing requires a POSITIVE signal, so a
/// missing, old, or substituted binary cannot produce a pass by falling over
/// quietly. See `cli::merge_pr_stale_checks` for the exit-code contract.
pub const CLEAN: &str = "LOOM-STALE-CHECKS-CLEAN";

/// One check run on the PR head, as the Checks API reports it.
///
/// `started_at` is `None` for a run that has not started yet (GitHub leaves it
/// null while a run is queued) and for API responses that omitted it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRun {
    pub name: String,
    /// `completed`, `in_progress`, `queued`, …
    pub status: String,
    /// `Some("success")`, `Some("failure")`, …; `None` until completed.
    pub conclusion: Option<String>,
    /// When the run started; `None` until it has.
    pub started_at: Option<DateTime<Utc>>,
    /// The GitHub Actions workflow run this check belongs to, when it is an
    /// Actions job (`None` for any other app). The freshness verdict ignores
    /// it; it is what the in-place re-run remedy (#8914) re-runs.
    pub actions_run_id: Option<u64>,
}

impl CheckRun {
    /// Is this run green, completed evidence — the only kind whose *age* can
    /// mislead a merge decision?
    fn is_completed_success(&self) -> bool {
        self.status == "completed" && self.conclusion.as_deref() == Some("success")
    }
}

/// The guard's verdict over one PR's evidence.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every required check is either fresh (started at/after the base tip),
    /// pending, absent, or non-green — nothing stale is being trusted.
    Fresh,
    /// A required check's green run predates the base tip: merging would trust
    /// evidence about a tree that no longer exists.
    Stale {
        check: String,
        started_at: DateTime<Utc>,
        base_tip: DateTime<Utc>,
    },
    /// A timestamp this decision depends on could not be determined. The
    /// caller must refuse the merge (fail closed), with the reason.
    Unknown(String),
}

/// The latest run bearing `name` — max `started_at`, `None` ranking oldest —
/// because branch protection (and the PR's Checks tab) evaluate a context by
/// its most recent run; older re-runs are shadowed evidence.
fn latest_run<'a>(runs: &'a [CheckRun], name: &str) -> Option<&'a CheckRun> {
    runs.iter()
        .filter(|r| r.name == name)
        .max_by_key(|r| r.started_at)
}

/// Decide whether merging would trust stale green evidence.
///
/// `required` is the branch-protection required status check context list for
/// the PR's base branch; `runs` is the check-runs rollup for the PR head SHA;
/// `base_tip` is the commit time of the base branch's current tip — "the tree
/// this PR will actually merge onto".
///
/// Required contexts are examined in sorted order so the first `Stale` verdict
/// is a function of the evidence alone, never of API ordering.
#[must_use]
pub fn assess(base_tip: DateTime<Utc>, required: &[String], runs: &[CheckRun]) -> Verdict {
    let mut required_sorted: Vec<&String> = required.iter().collect();
    required_sorted.sort();
    let mut unknown: Option<String> = None;
    for ctx in required_sorted {
        let Some(run) = latest_run(runs, ctx) else {
            continue; // Nothing green to re-validate; the forge owns absence.
        };
        if !run.is_completed_success() {
            continue; // Pending/red evidence never gets *staler* trust.
        }
        let Some(started) = run.started_at else {
            // Green evidence whose age we cannot determine is exactly the
            // unknown this guard exists to refuse on.
            if unknown.is_none() {
                unknown = Some(format!(
                    "required check '{ctx}' is green on this head but reports no \
                     started_at, so its freshness cannot be determined"
                ));
            }
            continue;
        };
        if started < base_tip {
            return Verdict::Stale {
                check: (*ctx).clone(),
                started_at: started,
                base_tip,
            };
        }
    }
    match unknown {
        Some(why) => Verdict::Unknown(why),
        None => Verdict::Fresh,
    }
}

/// The refusal text: names the check and BOTH timestamps, plus the remedy.
#[must_use]
pub fn stale_message(
    pr: &str,
    check: &str,
    started_at: DateTime<Utc>,
    base_tip: DateTime<Utc>,
    tip_sha: &str,
) -> String {
    format!(
        "Merge blocked: PR #{pr}'s required check `{check}` last ran at {started_at}, \
before the current base-branch tip ({base_tip}, {tip_sha}) — its green result is evidence \
about a tree that no longer exists (#8248).\n\nA ratchet baseline (or any repo-global \
state a check verifies) can tighten on the base branch while this PR was in flight, so a \
green run that predates the tip may be asserting slack that has since been spent. Merging \
on it is what red-lined main on 2026-09-18: PR #8078's File Size Ratchet was green against \
a baseline that PR #8204 had tightened 22 hours underneath it.\n\nRe-run the check against \
the current head (re-run the job, or push any no-op commit to re-date every check), then \
re-run this merge. The check's result must not predate the base tip it will merge onto."
    )
}

/// The fail-closed refusal for [`Verdict::Unknown`] and for a guard that
/// could not run at all.
#[must_use]
pub fn unknown_message(pr: &str, why: &str) -> String {
    format!(
        "Merge blocked: PR #{pr}'s required-check freshness guard (#8248) could not \
determine whether the green required checks predate the base tip — {why}.\n\nA stale \
green is not evidence (CI-principles rule 6: a check that cannot run must not look like a \
check that passed), so this guard fails closed rather than guessing. Resolve the lookup \
failure (network, quota, token scope) or re-run the required checks, then re-run this merge."
    )
}

pub mod fetch;
pub use fetch::LiveInputs;

#[cfg(test)]
mod tests;
