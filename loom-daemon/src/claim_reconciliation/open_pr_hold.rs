//! Claim reconciliation keeps `loom:building` on an issue whose PR is open
//! (Issue #11852).
//!
//! An issue carries `loom:building` while its PR is open: that is Loom's own
//! state machine (`merge-pr.sh` moves issue N from `loom:building` to
//! `loom:issue` on a `Part of #N` merge), and every work queue already skips
//! `loom:building`. So the daemon's release paths no longer touch the labels
//! of an issue with a verified open linked PR (see
//! `sweep_registry::restore_to_ready::open_pr_veto`). This pass is the other
//! half: it would otherwise find that `loom:building`, see no live claim
//! behind it, and flip it to `loom:issue` beside the open PR.
//!
//! So [`crate::reclaim_pr_warning::reclaim_and_warn`] asks [`open_linked_pr`]
//! first, and holds (writes nothing) on a verified open PR. When the PR later
//! closes **unmerged**, the next pass that reaches the same reclaim decision
//! finds no open PR and reclaims as before, restoring `loom:issue`. A merged
//! PR closes the issue, and the #7367 closed-issue check skips it.
//!
//! # Cost
//!
//! One read of the open-PR listing per reclaim candidate, the same
//! ETag-cached listing ([`super::open_pr_listing::list_open_prs`]) the PR-side
//! passes read every pass, so an unchanged repo answers with a free `304`.
//! A held issue is re-asked on every pass that decides to reclaim it, i.e.
//! once per reconcile interval while its PR stays open.
//!
//! Off with `LOOM_RELEASE_OPEN_PR_VETO=0`, together with the release half.
//!
//! # Fail-open
//!
//! Only a listing that was read and names an open linked PR holds. A failed
//! read, or no linked PR, reclaims as before: a stranded claim is the more
//! common failure.

use std::path::Path;

use crate::comment_trust::TrustPolicy;
use crate::worktree_ops::gh::OpenPrProbe;
use crate::worktree_ops::linked_pr_listing::classify_open_linked_pr_rows;

/// The open PR linked to `issue` (a `Closes/Part of #N` body, or the
/// `feature/issue-N` branch), when the open-PR listing reads and names one.
/// `None` for no linked PR and for every failure.
pub(crate) fn open_linked_pr(gh_bin: &Path, root: &Path, issue: u32) -> Option<u32> {
    if !crate::release_veto::open_pr_veto_enabled() {
        return None;
    }
    let rows = super::open_pr_listing::list_open_prs(gh_bin, root).ok()?;
    // The listing's own repo: `LOOM_REPO` wins, else `root`'s remote. Without
    // one only the body link counts (the branch match needs a same-repo head).
    let nwo = std::env::var("LOOM_REPO")
        .ok()
        .filter(|r| r.contains('/'))
        .or_else(|| crate::credential_preflight::nwo_from_git_remote(root))
        .unwrap_or_default();
    match classify_open_linked_pr_rows(&rows, issue, &nwo, &TrustPolicy::for_root(root)) {
        OpenPrProbe::Open(pr) => Some(pr),
        OpenPrProbe::NoneOpen | OpenPrProbe::ProbeFailed => None,
    }
}

#[cfg(test)]
#[path = "open_pr_hold_tests.rs"]
mod tests;
