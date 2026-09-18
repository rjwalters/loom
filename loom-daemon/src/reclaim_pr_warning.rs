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
//! One `gh pr list --head feature/issue-<N>` per **successful reclaim** — not
//! per candidate. A healthy pass, which reclaims nothing, pays nothing.

use std::path::Path;

use anyhow::Result;

/// Reclaim `issue`'s `loom:building` label, then warn if the forge says
/// somebody had already opened a PR from its worktree branch.
///
/// A drop-in wrapper around [`crate::claim_reconciliation::forge::reclaim`]:
/// the return value is that call's, unchanged, and the diagnostic runs only on
/// success (a failed flip changed nothing worth explaining).
pub(crate) fn reclaim_and_warn(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
    let result = crate::claim_reconciliation::forge::reclaim(gh_bin, root, issue);
    if result.is_ok() {
        if let Some(pr) = open_pr_on_issue_branch(gh_bin, root, issue) {
            log::warn!("{}", open_pr_warning(issue, pr, root));
        }
    }
    result
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

/// `gh pr list --head feature/issue-<N> --state open`, reduced to the first PR
/// number.
///
/// `None` covers BOTH "no such PR" and every failure (missing/failed `gh`,
/// unparseable output) — this is a diagnostic, so an unanswerable probe simply
/// produces no warning and never affects the reclaim that already happened.
fn open_pr_on_issue_branch(gh_bin: &Path, root: &Path, issue: u32) -> Option<u32> {
    let mut cmd = std::process::Command::new(gh_bin);
    cmd.arg("pr")
        .arg("list")
        .arg("--head")
        .arg(issue_branch(issue))
        .arg("--state")
        .arg("open")
        .arg("--json")
        .arg("number")
        .arg("--limit")
        .arg("1");
    cmd.current_dir(root);
    // #5401: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_first_pr_number(&String::from_utf8_lossy(&output.stdout))
}

/// First `number` in a `gh pr list --json number` payload, or `None` for an
/// empty list or anything unparseable.
#[must_use]
pub fn parse_first_pr_number(stdout: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    value
        .as_array()?
        .first()?
        .get("number")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parses_the_first_pr_number_from_a_gh_listing() {
        assert_eq!(parse_first_pr_number(r#"[{"number":8081},{"number":8082}]"#), Some(8081));
    }

    #[test]
    fn an_empty_listing_yields_no_warning() {
        assert_eq!(parse_first_pr_number("[]"), None);
        assert_eq!(parse_first_pr_number("[]\n"), None);
    }

    /// A diagnostic must never manufacture a verdict out of garbage — an
    /// unparseable payload simply produces no warning.
    #[test]
    fn unparseable_output_yields_no_warning() {
        assert_eq!(parse_first_pr_number(""), None);
        assert_eq!(parse_first_pr_number("gh: rate limit exceeded"), None);
        assert_eq!(parse_first_pr_number(r#"[{"number":"eight"}]"#), None);
        assert_eq!(parse_first_pr_number(r#"[{}]"#), None);
    }
}
