//! Whether a currently-failing check-runs rollup refuses the merge, lets it
//! proceed, or leaves the poll loop waiting on other still-pending checks
//! (#8191 slice of `_wait_for_checks_then_sync_merge`'s per-poll
//! classification, `merge-pr.sh` lines ~2018-2045 before this port).
//!
//! # What it decides
//!
//! `--auto`'s settle wait polls the check-runs rollup for the head SHA being
//! merged. Whenever that poll finds at least one FAILING check (terminal
//! `failure`/`timed_out`/`cancelled`/`action_required`), three outcomes are
//! possible:
//!
//! - a failing check is also a **required** status-check context for the
//!   base branch — that context can never turn green on this SHA, so the
//!   merge must be refused now rather than waiting out the timeout;
//! - every failing check is merely informational (not required) and nothing
//!   else is still running — the wait is over, exactly the #3486 UNSTABLE
//!   fallback the synchronous merge path already relies on;
//! - every failing check is informational, but some OTHER check is still
//!   pending — the informational failure changes nothing yet, so the loop
//!   must keep waiting on the pending check(s) exactly as if nothing had
//!   failed.
//!
//! [`classify`] is that three-way choice, reproducing the retired
//! `comm -12 <(sort -u $failing) <(sort -u $required)` overlap test with a set
//! intersection (same semantics: sorted, de-duplicated, exact string match —
//! a required context is never matched by a differently-spelled failing
//! check name).
//!
//! # What stays in the shell
//!
//! Both input lists are forge reads the shell already performs for the wait
//! loop's other branches: `failing`/`pending` come from the check-runs
//! rollup already fetched (`forge_get_check_runs`), and `required` from
//! `forge_get_required_status_check_contexts` — the two-source (rulesets +
//! classic branch protection) lookup that also backs GitHub AND Gitea, unlike
//! the GitHub-only `loom-daemon merge-pr stale-checks` lookup this deliberately
//! does not reuse. A failure of THAT lookup is refused by the shell before
//! this classification ever runs (fails closed, mirroring the #6104 UNSTABLE
//! fallback) — [`classify`] only runs once a `required` answer already exists.

use std::collections::BTreeSet;

/// The verdict over one poll's failing-check set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// At least one failing check is also a required status-check context —
    /// refuse the merge. Carries the overlap, sorted and de-duplicated
    /// exactly as `comm -12 <(sort -u) <(sort -u)` left it.
    RequiredFailed(Vec<String>),
    /// Every failing check is informational and nothing else is pending —
    /// the wait is over; proceed to the synchronous merge.
    InformationalOnly,
    /// Every failing check is informational, but another check is still
    /// pending — keep waiting exactly as if nothing had failed.
    StillPending,
}

/// `comm -12 <(sort -u $failing) <(sort -u $required)`, then routed by
/// whether anything else is still `pending`.
#[must_use]
pub fn classify(failing: &[String], required: &[String], pending: bool) -> Verdict {
    let required: BTreeSet<&str> = required.iter().map(String::as_str).collect();
    let overlap: BTreeSet<&str> = failing
        .iter()
        .map(String::as_str)
        .filter(|f| required.contains(f))
        .collect();
    if !overlap.is_empty() {
        return Verdict::RequiredFailed(overlap.into_iter().map(String::from).collect());
    }
    if pending {
        Verdict::StillPending
    } else {
        Verdict::InformationalOnly
    }
}

#[cfg(test)]
mod tests;
