//! The post-merge closed-issue `loom:building` cleanup decision (#6199), a
//! slice of the merge-pr port #8191.
//!
//! # What it decides
//!
//! #2838 decided Loom does not clean labels off closed issues, and for almost
//! every label that is right — each queue query filters on open state, so a
//! stale label can neither cause a duplicate build nor block a candidate.
//! #6199 found the one exception: `loom:building` *names* a liveness claim, so
//! any consumer that reads it without also filtering on state (a dashboard, a
//! capacity check, an operator's `gh issue list --label loom:building`) sees
//! pure noise once closed-but-still-labelled issues accumulate — 20 stale
//! claims against 0 real ones on one consumer repo.
//!
//! `merge-pr.sh`'s `_strip_one_closed_issue_building_label` removes the label
//! from each issue THIS merge closed. Given a fresh read of that issue (the
//! `gh api repos/<nwo>/issues/<n>` body the shell still fetches), [`plan`]
//! answers the one question the shell needs: strip, or leave it alone and why.
//! The mutation itself stays in the shell behind the #4856 rate-limit-safe
//! `forge_gh_remove_label_rl_safe` wrapper.
//!
//! # Why it is worth porting despite being three `if`s
//!
//! The three `if`s are not the risk; the three `jq` filters under them are.
//! This function reads the same body through the same
//! `has("pull_request")` / `.state // ""` / `.labels[]?.name` pipelines as
//! [`super::partial_reset`], on inputs a *failed* `gh api` produces — an error
//! body, the `|| echo '{}'` fallback appended to it, a truncated read — where
//! jq's per-document error recovery decides what the shell saw. Every one of
//! its three skips is silent, so a filter that answered wrongly would strip a
//! LIVE builder's claim (the `.state` filter) or a PR's labels (the
//! `has` filter) with nothing in the transcript to show for it.
//!
//! That is also why this reuses [`IssueView`] rather than re-deriving it:
//! the two functions were written from the same three filters against the same
//! endpoint, so one model — held against both retired functions by two
//! differential tests — is the accurate description, not a convenience.
//!
//! # Fail direction
//!
//! Every skip is a no-op and the whole pass runs after the merge has already
//! happened, so the port keeps the shell's best-effort stance: a daemon that
//! cannot answer leaves the label alone and warns, never guesses. The one
//! thing the wrapper must NOT do is read "no output" as "skip" — which is why
//! the skip is a positive `SKIP` line rather than silence.

use super::partial_reset::IssueView;

/// Why the pass declined to touch an issue.
///
/// These tokens are the wire protocol's second field. They are never printed
/// by `merge-pr.sh` (the retired function's skips were silent, and the port
/// keeps its stdout byte-identical); they exist so the verb is diagnosable
/// when run by hand, and so the differential harness can assert WHICH skip
/// each side took rather than only that both said nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// `has("pull_request")` said `true`. The GitHub issues endpoint also
    /// serves PRs; a PR that reached here through a close-target lookup must
    /// never be mutated.
    IsPullRequest,
    /// `.state // ""` was not `closed` — the issue is live (or the read
    /// failed, or the #4569 partial-increment revert just reopened it, which
    /// is why this pass runs after that one). A live claim is not stale.
    NotClosed,
    /// No `.labels[]?.name` line was exactly `loom:building`. Idempotent
    /// re-runs land here, as does an issue somebody already cleaned up.
    NotBuilding,
}

impl Skip {
    /// The stable token for this skip.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Skip::IsPullRequest => "is-pull-request",
            Skip::NotClosed => "not-closed",
            Skip::NotBuilding => "not-building",
        }
    }
}

/// What the shell should do with one issue this merge closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Remove `loom:building` via `forge_gh_remove_label_rl_safe`.
    Strip,
    /// Leave the issue untouched.
    Skip(Skip),
}

/// Decide, from a fresh read of the issue, whether its `loom:building` label
/// is a stale claim this merge is responsible for.
///
/// The order of the three tests is the retired function's own and is
/// load-bearing: a PR is rejected before its `state` is consulted (a merged PR
/// *is* `closed`), and `state` is consulted before the labels (an open issue's
/// `loom:building` is a live claim, not litter).
#[must_use]
pub fn plan(view: &IssueView) -> Plan {
    if view.is_pr {
        return Plan::Skip(Skip::IsPullRequest);
    }
    if view.state != "closed" {
        return Plan::Skip(Skip::NotClosed);
    }
    if !view.building {
        return Plan::Skip(Skip::NotBuilding);
    }
    Plan::Strip
}

/// Render the plan as the shell wrapper's one-line protocol: `STRIP`, or
/// `SKIP<TAB><token>`.
///
/// Always exactly one line, always led by a token the wrapper recognises —
/// so an empty read, a truncated read, or a binary predating this verb is
/// distinguishable from a decision, and cannot be replayed as one.
#[must_use]
pub fn render(plan: Plan) -> String {
    match plan {
        Plan::Strip => "STRIP\n".to_string(),
        Plan::Skip(why) => format!("SKIP\t{}\n", why.token()),
    }
}

#[cfg(test)]
mod tests;
