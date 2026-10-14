//! Post-reclaim diagnostic for the one label flip that is almost always a bug
//! (Issue #8116, ask 4).
//!
//! # The signature
//!
//! [`crate::claim_reconciliation`] flips `loom:building` back to `loom:issue`
//! when none of its evidence sources can prove a live claim. Every source it
//! consults is *registry-shaped*: a journal entry, a checkpoint→run-registry
//! join, a `loom:lease` comment, a claim lock. An **in-session Task-tool
//! builder** — an operator spawning `/loom:builder` subagents directly rather
//! than through `/loom:sweep` — produces none of them, so the reconciler
//! correctly follows its rules to an incorrect conclusion and un-claims live
//! work. Three of six issues in the 2026-09-17 wave behind #8116 were flipped
//! this way while their builders were mid-task.
//!
//! There is one forge-visible fact that contradicts the reclaim in exactly that
//! case and nowhere else: **an open PR whose head branch is
//! `feature/issue-<N>`** — the branch name `worktree.sh` gives issue `N`'s
//! worktree and nothing else produces. If that branch has an open PR, a worker
//! got far enough to push and open one, so "nobody is working this" is false on
//! its face.
//!
//! # Why this warns instead of refusing
//!
//! Deliberate, and what #8116 asks for. Refusing the reclaim here would make
//! this a *fourth* liveness gate layered on top of the lease record (#6286),
//! the live-claim veto (#4556) and the closed-issue recheck (#7367), each with
//! its own fail-direction to reason about — and it would do so on evidence that
//! is strictly weaker than theirs, since a `feature/issue-<N>` branch can also
//! carry an *abandoned* PR whose builder really is gone. The label flip is also
//! not the expensive half of the bug: the work finder's own open-PR guard
//! (#4123) refuses to re-dispatch an issue with an open linked PR — including,
//! since #7859 and #8116's `worktree_ops` half, one that says `Part of #N`
//! rather than `Closes #N` — so a flipped label mostly *misreports* state
//! rather than causing a duplicate build.
//!
//! What was missing was the ability to see it happen. `daemon.log` recorded the
//! reclaim and its reason, and an operator had no way to tell that particular
//! line apart from the thousands of legitimate ones without manually
//! cross-referencing the forge. This module makes that one line self-evident.
//!
//! # Cost
//!
//! One REST `pulls?head=owner:feature/issue-<N>` read per **successful
//! reclaim** — not per candidate. A healthy pass, which reclaims nothing, pays nothing.

use std::path::Path;

use anyhow::Result;

/// What [`reclaim_and_warn`] did on success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reclaim {
    /// The claim was released (fully, or the claim only under a veto).
    Released,
    /// The issue is `loom:frozen` (#11743): nothing was written, so the
    /// caller must not count or log it as a reclaim.
    Frozen,
}

/// Reclaim `issue`'s `loom:building` label, then warn if the forge says
/// somebody had already opened a PR from its worktree branch.
///
/// A wrapper around [`crate::claim_reconciliation::forge::reclaim`]: an error
/// is that call's, unchanged, and the diagnostic runs only on success (a
/// failed flip changed nothing worth explaining). `Ok(Reclaim::Frozen)` is the
/// frozen skip, which wrote nothing.
pub(crate) fn reclaim_and_warn(gh_bin: &Path, root: &Path, issue: u32) -> Result<Reclaim> {
    // #11480: an item another hold took out of the queue loses only the stale
    // claim; `loom:issue` is not re-added.
    if let Some(label) = crate::release_veto::veto(gh_bin, root, issue, &[]) {
        if label == crate::work_finder::FROZEN_LABEL {
            log::warn!(
                "claim_reconciliation: #{issue} in {} is `loom:frozen` — not reclaiming, no \
                 label write (#11743)",
                root.display()
            );
            return Ok(Reclaim::Frozen);
        }
        log::warn!(
            "claim_reconciliation: #{issue} in {} carries `{label}` — removing the stale \
             `loom:building` claim only, NOT re-adding `loom:issue` (#11480)",
            root.display()
        );
        return release_claim_only(gh_bin, root, issue).map(|()| Reclaim::Released);
    }
    crate::claim_reconciliation::forge::reclaim(gh_bin, root, issue)?;
    if let Some(pr) = open_pr_on_issue_branch(gh_bin, root, issue) {
        log::warn!("{}", open_pr_warning(issue, pr, root));
    }
    Ok(Reclaim::Released)
}

/// The vetoed reclaim (#11480): remove `loom:building`, add nothing.
fn release_claim_only(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
    use crate::claim_reconciliation::gh_call;
    let n = issue.to_string();
    let out = gh_call::output(
        gh_call::write("claim.issue_reclaim", gh_bin, root)
            .args(["issue", "edit", &n, "--remove-label", "loom:building"])
            .args(gh_call::loom_repo_flag()),
    )?;
    anyhow::ensure!(
        out.status.success(),
        "gh issue edit failed for #{issue} in {}: {}",
        root.display(),
        gh_call::stderr(&out)
    );
    Ok(())
}

/// The `feature/issue-<N>` branch convention `worktree.sh` establishes.
#[must_use]
pub fn issue_branch(issue: u32) -> String {
    crate::worktree_ops::naming::branch_name(issue)
}

/// The WARN line emitted when a reclaim lands on an issue whose worktree branch
/// already has an open PR. Pure, so its wording is testable without a forge.
#[must_use]
pub fn open_pr_warning(issue: u32, pr: u32, root: &Path) -> String {
    format!(
        "claim_reconciliation: reclaimed #{issue} in {} even though PR #{pr} is OPEN from its \
         worktree branch `{}` — a worker got far enough to push and open a PR, so this claim was \
         probably live and is most likely an in-session builder with no lease record (#8116)",
        root.display(),
        issue_branch(issue),
    )
}

/// The open PRs whose head is issue `issue`'s worktree branch, via the REST
/// by-head lookup [`crate::forge_pull_listing::open_pulls_for_head_as`]
/// (#10382: was the GraphQL `gh pr list --head`). Recorded against `caller`.
///
/// # Errors
/// An inconclusive read — never collapsed into "no open PR" (#7863).
pub(crate) fn open_prs_on_issue_branch(
    caller: &'static str,
    gh_bin: &Path,
    root: &Path,
    issue: u32,
) -> Result<Vec<u32>> {
    let branch = issue_branch(issue);
    crate::forge_pull_listing::open_pulls_for_head_as(caller, gh_bin, Some(root), None, &branch)
}

/// The first open PR on issue `issue`'s worktree branch.
///
/// `None` covers BOTH "no such PR" and every failure (missing/failed `gh`,
/// unparseable output) — this is a diagnostic, so an unanswerable probe simply
/// produces no warning and never affects the reclaim that already happened.
pub(crate) fn open_pr_on_issue_branch(gh_bin: &Path, root: &Path, issue: u32) -> Option<u32> {
    let prs = open_prs_on_issue_branch("reclaim.open_pr_probe", gh_bin, root, issue).ok()?;
    prs.first().copied()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::write_scope_test_support::WritableRoot;
    use std::os::unix::fs::PermissionsExt;

    /// #11480: a reclaim on an item another hold took out of the queue
    /// removes the stale claim only; a plain item is still reclaimed.
    #[test]
    #[serial_test::serial]
    fn a_vetoed_reclaim_removes_the_claim_only() {
        for (labels, restores) in [
            (r#"["loom:building","loom:operator-only"]"#, false),
            (r#"["loom:building"]"#, true),
            // #11743: frozen — no edit at all (checked below).
            (r#"["loom:building","loom:frozen"]"#, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("repo");
            std::fs::create_dir_all(&root).unwrap();
            let log = dir.path().join("gh.log");
            let gh = dir.path().join("gh");
            let script = format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$*\" in\n\
                 *'/issues/42 --jq'*) echo '{labels}' ;;\n\
                 'issue view'*) echo '{{\"labels\":[{{\"name\":\"loom:issue\"}}]}}' ;;\n\
                 esac\nexit 0\n",
                log.display()
            );
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let ws = WritableRoot::register_with_gh(&root, &gh);

            let done = reclaim_and_warn(&ws.gh, &root, 42).unwrap();

            let calls = std::fs::read_to_string(&log).unwrap();
            if labels.contains("loom:frozen") {
                assert!(!calls.contains("issue edit"), "{labels}: {calls}");
                // #11743: distinguishable, so the caller does not count it.
                assert_eq!(done, Reclaim::Frozen);
                continue;
            }
            assert_eq!(done, Reclaim::Released, "{labels}");
            assert!(calls.contains("issue edit 42 --remove-label loom:building"), "{calls}");
            assert_eq!(calls.contains("--add-label loom:issue"), restores, "{labels}: {calls}");
        }
    }

    #[test]
    fn warning_names_the_issue_the_pr_and_the_branch_convention() {
        let msg = open_pr_warning(8056, 8081, Path::new("/srv/loom"));
        assert!(msg.contains("#8056"), "{msg}");
        assert!(msg.contains("PR #8081 is OPEN"), "{msg}");
        assert!(msg.contains("feature/issue-8056"), "{msg}");
        assert!(msg.contains("/srv/loom"), "{msg}");
        // The whole point is that an operator can grep this one line out of a
        // log full of legitimate reclaims.
        assert!(msg.contains("#8116"), "{msg}");
    }

    #[test]
    fn issue_branch_matches_the_worktree_naming_convention() {
        assert_eq!(issue_branch(8116), "feature/issue-8116");
    }
}
