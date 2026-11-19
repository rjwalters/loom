//! The re-date commit's vocabulary, kept for the read-only measurement.
//!
//! The re-date remedy itself (#8508: a tree-identical no-op push when the
//! #8248 freshness guard refused a merge, its per-head budget, back-off and
//! sync hand-off) was deleted by #12063 slice 1 together with the guard: a PR
//! whose head and green checks are unchanged is no longer re-dated because the
//! base moved. What stays is what still reads commits the remedy already
//! wrote: `merge-pr redate-report` ([`report`], [`chain_telemetry`]; the
//! #10888 before/after measurement, removed in slice 4) and the CI telemetry
//! poller's `stale_main_bump` trigger reason (#9337), via
//! [`is_redate_commit_subject`].

pub mod chain_telemetry;
pub mod report;

/// The trailer keys a re-date commit carried (#9746), in the order written.
pub const TRAILER_CHECK: &str = "Stale-Check";
pub const TRAILER_CLAUSE: &str = "Stale-Clause";
pub const TRAILER_BASE_PATH: &str = "Coupled-Base-Path";
pub const TRAILER_PR_PATH: &str = "Coupled-PR-Path";

/// The value written for an absent trailer path.
pub const NONE: &str = "none";

/// The commit message every re-date commit carries — deterministic and
/// greppable, so a later pass (or a human) can tell "this is an automated
/// remedy commit" from the commit log alone without reading this source.
#[must_use]
pub fn commit_message(pr: &str) -> String {
    format!(
        "chore: re-date required checks for PR #{pr} (#8248 guard, automated by #8508)\n\n\
This commit intentionally changes NOTHING in the tree — it exists only to \
give every required check a fresh started_at, because the #8248 \
required-check-freshness guard blocked this merge on evidence that predates \
the base branch's current tip, and the merge token lacks actions:write to \
re-run the stale check directly."
    )
}

/// Whether `subject` (a commit message's FIRST line) is exactly the subject
/// [`commit_message`] writes, for some PR number — i.e. matches
/// `^chore: re-date required checks for PR #[0-9]+ \(#8248 guard, automated by #8508\)$`.
///
/// The CI telemetry poller uses this to attribute a workflow run to this
/// remedy (`loom.ci.trigger_reason = stale_main_bump`, #9337): it sees the
/// run row's `head_commit.message`, never the PR's re-date
/// comment. Kept next to [`commit_message`] and tested against its output so
/// the producer and the classifier cannot drift apart.
#[must_use]
pub fn is_redate_commit_subject(subject: &str) -> bool {
    subject
        .strip_prefix("chore: re-date required checks for PR #")
        .and_then(|rest| rest.strip_suffix(" (#8248 guard, automated by #8508)"))
        .is_some_and(|pr| !pr.is_empty() && pr.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests;
