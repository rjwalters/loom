//! The one open-PR listing every PR-side reconciliation pass reads (#10349).
//!
//! The claim, verdict, review-conflict and merge-sequence passes each used a
//! GraphQL `gh pr list` (five per workspace per pass), which has no
//! conditional-request mechanism. They now read the ETag-cached REST listing
//! ([`crate::forge_pull_listing`]): an unchanged workspace answers every pass
//! with a free `304`, and a changed one bills one REST core point per changed
//! page. Each pass lists for itself, so it still sees the preceding passes'
//! writes (a write changes the listing's ETag, so the next read is a `200`).
//!
//! A sibling module rather than inline in `claim_reconciliation.rs`, which is
//! frozen by the file-size ratchet (`.loom/docs/file-size-policy.md`). Calls
//! are recorded as `claim_reconciliation.pr_list` /
//! `claim_reconciliation.pr_mergeable` in the #9251 forge-call accounting.

use std::path::Path;

use anyhow::Result;

use super::review_conflict::Mergeable;
use super::MAX_ISSUES_PER_WORKSPACE;
use crate::forge_pull_listing::{list_open_pulls_cached_as, pull_state_cached_as};
use crate::rate_limit_breaker::report::{BreakerHandle, FailureContext};

pub use crate::forge_pull_listing::RestPull;

/// Page budget: 10 × 100, the same as [`crate::forge_listing::MAX_PAGES`]. A
/// workspace with at most 100 open PRs costs one request, and unchanged pages
/// are free `304`s. More open PRs than this is an error, never a silently
/// truncated listing (#10382: `pr.list-open` is `complete-required`).
pub(crate) const MAX_PAGES: usize = 10;

/// Every open PR of `root`'s repository (`LOOM_REPO` wins), newest first.
pub(super) fn list_open_prs(gh_bin: &Path, root: &Path) -> Result<Vec<RestPull>> {
    list_open_pulls_cached_as("claim_reconciliation.pr_list", gh_bin, Some(root), None, MAX_PAGES)
}

/// The open PRs carrying `label`, newest first, capped at
/// [`MAX_ISSUES_PER_WORKSPACE`] — what `gh pr list --label <label> --limit
/// 100` returned.
pub(super) fn list_with_label(gh_bin: &Path, root: &Path, label: &str) -> Result<Vec<RestPull>> {
    Ok(with_label(list_open_prs(gh_bin, root)?, label))
}

/// The client-side half of [`list_with_label`].
#[must_use]
pub(super) fn with_label(rows: Vec<RestPull>, label: &str) -> Vec<RestPull> {
    let cap = usize::try_from(MAX_ISSUES_PER_WORKSPACE).unwrap_or(usize::MAX);
    rows.into_iter()
        .filter(|r| r.has_label(label))
        .take(cap)
        .collect()
}

/// PR `number`'s mergeability via a conditional `GET pulls/{number}`, paired
/// with the head sha of the same response (#10382) — the sha GitHub computed
/// `mergeable` for, which a verdict must name instead of the listing's head.
/// `breaker` is the rate-limit breaker the read reports to (production:
/// [`BreakerHandle::global`], fetched once per pass; `None` = unregistered).
/// Any failure is [`Mergeable::Unknown`] — "no information", the same answer
/// as GitHub still computing it, so the conflict pass changes nothing and
/// re-reads next tick. The facade's accounting only records the call in
/// forge-call stats; it does not feed the breaker, so a rate-limit failure is
/// reported here (AC5). While the breaker is suppressing, the read is skipped
/// outright and answers [`Mergeable::Unknown`], so one trip mid-pass stops
/// the pass's remaining per-PR reads instead of spending N doomed calls.
pub(super) fn mergeable_of(
    gh_bin: &Path,
    root: &Path,
    number: u32,
    breaker: Option<&BreakerHandle>,
) -> (Mergeable, Option<String>) {
    if breaker.is_some_and(BreakerHandle::is_suppressed) {
        log::debug!(
            "claim_reconciliation: mergeability of PR #{number} in {} skipped: rate-limit breaker suppressing",
            root.display()
        );
        return (Mergeable::Unknown, None);
    }
    match pull_state_cached_as(
        "claim_reconciliation.pr_mergeable",
        gh_bin,
        Some(root),
        None,
        number,
    ) {
        Ok(s) => (Mergeable::from_rest(s.mergeable), s.head_sha),
        Err(e) => {
            log::debug!(
                "claim_reconciliation: mergeability of PR #{number} in {} unknown: {e}",
                root.display()
            );
            if let Some(handle) = breaker {
                handle.report(
                    &e.to_string(),
                    "claim_reconciliation",
                    FailureContext::for_root(root, gh_bin.to_string_lossy()),
                );
            }
            (Mergeable::Unknown, None)
        }
    }
}

#[cfg(test)]
#[path = "open_pr_listing_tests.rs"]
mod tests;

/// Fake-`gh` building blocks for the PR-side pass tests: REST rows and the
/// shell arms that serve them.
#[cfg(test)]
#[path = "open_pr_listing_test_support.rs"]
pub(crate) mod test_support;
