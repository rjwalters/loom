//! The async-close-race worktree-cleanup gate (#4186), a slice of the
//! merge-pr port #8191.
//!
//! # What it decides
//!
//! `merge-pr.sh`'s post-merge cleanup removes the worktree for a merged PR's
//! branch — but only when the issue that branch was working on is actually
//! finished. A `Part of #N` / `Contributes to #N` partial-increment PR (#3667)
//! merges while issue N stays deliberately open, and the next Builder
//! increment (or an agent still inside it) needs that worktree.
//!
//! Naively querying the issue's LIVE state right after the merge has a race
//! (#4186, adapted from fork PR #77): GitHub closes `Closes #N` issues
//! ASYNCHRONOUSLY, after the merge webhook fires, so a lookup taken here would
//! see "open" for essentially every normal merge and silently defeat cleanup
//! entirely. [`decide`] resolves that by gating the live lookup on whether
//! this PR is actually a close target of the issue:
//!
//! - `$issue_number` IS a close target of the PR ([`Decision::CloseTarget`])
//!   — the merge itself closes it; clean up with no live lookup and no race.
//! - `$issue_number` is NOT a close target (partial increment, or no closing
//!   keyword at all) — the live state decides: `CLOSED` ⇒
//!   [`Decision::StateClosed`], anything else (including "no state supplied
//!   yet" ⇒ [`Decision::NeedState`], and "the lookup failed or returned an
//!   unrecognized value" ⇒ [`Decision::Preserve`]).
//!
//! # Why the live lookup stays lazy
//!
//! The overwhelming common case — a PR whose body says `Closes #N` — never
//! needs the second forge read at all: [`decide`] answers
//! [`Decision::CloseTarget`] from `close_targets` alone. `merge-pr.sh` keeps
//! that optimization by calling this verb once with no `--state`, and only
//! paying for `forge_get_issue_state` (a full extra GitHub API round trip)
//! when [`Decision::NeedState`] comes back — mirroring the two-call shape
//! `loom-pr-guard` → `hold-state` already uses elsewhere in this file for the
//! same reason (avoid an unconditional forge call on the fast path).
//!
//! # Fail direction: unsafe-to-preserve
//!
//! This gates a **destructive** step (`git worktree remove --force`, and
//! transitively `git branch -d`), so any answer this module or its caller
//! cannot fully corroborate must resolve to [`Decision::Preserve`] — never to
//! "clean it up". A skipped cleanup here is always recoverable later (a
//! future merge that actually closes the issue, or `loom-clean` by hand); a
//! wrongly-removed worktree/branch is not. `merge-pr.sh`'s own #6694 landing
//! is the backstop for the one case this leaves inefficient (a programme
//! issue intentionally designed to never close): it still removes the
//! worktree once `branch_has_landed` proves the branch's content is already
//! on the default branch, regardless of what this gate said.

/// The outcome of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `issue_number` is one of this PR's own closing references — the
    /// merge itself closes it, so no live-state race is possible. Safe to
    /// clean up.
    CloseTarget,
    /// A live state was supplied and it read `CLOSED`. Safe to clean up.
    StateClosed,
    /// Neither `close_targets` nor a supplied `state` could establish
    /// closure, because no `state` was supplied at all. The caller must
    /// fetch `forge_get_issue_state` and call again with it.
    NeedState,
    /// A live state was supplied and it was not `CLOSED` — open, or an
    /// empty/unrecognized value from a lookup failure. Preserve.
    Preserve,
}

impl Decision {
    /// Whether this decision authorizes cleanup — the boolean
    /// `_issue_is_closed_for_cleanup` returns to its caller.
    #[must_use]
    pub fn is_closed_for_cleanup(self) -> bool {
        matches!(self, Decision::CloseTarget | Decision::StateClosed)
    }

    /// The stable wire token, the second field of the `LOOM-ISSUE-CLEANUP
    /// <token>` line the CLI prints.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Decision::CloseTarget => "CLOSE-TARGET",
            Decision::StateClosed => "STATE-CLOSED",
            Decision::NeedState => "NEED-STATE",
            Decision::Preserve => "PRESERVE",
        }
    }
}

/// `close_targets` is `forge_pr_close_targets`'s output: one issue number per
/// line, sorted and de-duplicated. `state` is `forge_get_issue_state`'s
/// output (`"OPEN"` / `"CLOSED"`, or empty/absent on any lookup failure) —
/// `None` when the caller has not fetched it yet (the fast-path call).
///
/// Membership is an EXACT line match, mirroring the retired
/// `grep -qx "$issue_number" <<< "$close_targets"` — issue numbers are plain
/// digit strings, so no normalization (trimming, case-folding) is needed or
/// wanted: a close-targets line that is not exactly `issue_number` is not a
/// match, precisely as `grep -x` requires the whole line.
#[must_use]
pub fn decide(close_targets: &str, issue_number: &str, state: Option<&str>) -> Decision {
    if close_targets.lines().any(|line| line == issue_number) {
        return Decision::CloseTarget;
    }
    match state {
        None => Decision::NeedState,
        Some("CLOSED") => Decision::StateClosed,
        Some(_) => Decision::Preserve,
    }
}

#[cfg(test)]
mod tests;
