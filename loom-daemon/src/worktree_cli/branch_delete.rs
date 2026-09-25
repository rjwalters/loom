//! The squash-aware local-branch delete rule, as `worktree.sh remove` uses it
//! (#8195 slice 3, epic #7810).
//!
//! # What this replaces, and why a Rust copy is the *end* of the duplication
//!
//! Bash has no import mechanism, so `worktree.sh remove` obtained this rule by
//! reading `merge-pr.sh`'s source at runtime, `awk`-slicing
//! `_maybe_delete_local_branch` plus three private helpers out of it, and
//! `eval`-ing the text into its own process — with a `return 1` fallback for
//! "upstream renamed it" that silently degrades to a bare `git branch -d`,
//! which can never delete a squash-merged branch (#4889, the exact defect the
//! rule exists to fix).
//!
//! That contraption existed to avoid a *second implementation*. This module is
//! a second implementation, for now, and that is a deliberate trade with a
//! stated end state: `merge-pr.sh`'s own port (#8191) imports this module
//! instead of re-deriving the rule, at which point there is exactly one copy
//! and no `eval`. Until then the anti-drift guarantee is mechanical, not
//! aspirational — `tests::messages_match_the_merge_pr_shell_twin` greps the
//! live `merge-pr.sh` for every message string below and fails if either side
//! is reworded.
//!
//! # The rule
//!
//! `git branch -d` only, EXCEPT when [`super::branch_landed`] returns
//! `landed`, in which case `-D`. Nothing else escalates: not a `-d` failure,
//! not a forge probe that could not run, not an `unknown` verdict. The
//! escalate-on-any-failure shape is what force-deleted a Doctor's unpushed
//! local commits in #5939's precedent, and `-d` failing is the *normal* state
//! for a squash-merged branch — so "escalate when `-d` fails" would escalate
//! essentially always.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::branch_landed::{self, ForgeStatus, Verdict};
use super::wip::Out;

/// What happened, for the caller's `branchStatus` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOutcome {
    /// `refs/heads/<branch>` is gone.
    Deleted,
    /// It survived — refused, checked out elsewhere, or genuinely unmerged.
    Kept,
}

/// Everything the rule needs from its caller, so the decision itself stays
/// inspectable without a repo.
pub struct DeleteContext<'a> {
    pub repo_root: &'a Path,
    /// The repo's default branch, or `None` when it could not be resolved.
    /// `None` disables both the default-branch guard's named arm and the
    /// #5015 auto-cleanup — the conservative direction in both cases.
    pub default_branch: Option<&'a str>,
    /// `CLEANUP_PRIMARY_CHECKOUT` in the shell. `false` opts out of #5015.
    pub cleanup_primary_checkout: bool,
}

/// `_maybe_delete_local_branch <branch>` with no caller-known merged head SHA
/// — which is what `worktree.sh remove` always passed: it is not the process
/// that merged the PR, so it holds no `$PR_HEAD_SHA`.
pub fn maybe_delete_local_branch(ctx: &DeleteContext, out: &Out, branch: &str) -> BranchOutcome {
    if branch.is_empty() {
        return BranchOutcome::Kept;
    }
    if !branch_exists(ctx.repo_root, branch) {
        out.info(&format!("Local branch '{branch}' does not exist — skipping branch delete"));
        return BranchOutcome::Kept;
    }
    // Never delete the repo's default branch. Cheap belt-and-suspenders: the
    // attached branch of an `issue-<N>` worktree should never legitimately BE
    // the default branch, but a misdetected one must not take this out.
    if ctx
        .default_branch
        .is_some_and(|d| !d.is_empty() && d == branch)
        || branch == "main"
        || branch == "master"
    {
        out.warning(&format!(
            "Refusing to delete local branch '{branch}' — it is the repository's default branch"
        ));
        return BranchOutcome::Kept;
    }

    let landed = branch_landed::probe(ctx.repo_root, branch, ctx.default_branch, "");

    // Fail closed: ONLY a `landed` verdict force-deletes. `not-landed` and
    // `unknown` both keep `git branch -d`, which still deletes a branch git
    // itself can prove merged and refuses (loudly) otherwise.
    let (flag, safety_note) = if landed.verdict == Verdict::Landed {
        (
            "-D",
            format!(" (branch has landed: {} — safe force-delete)", landed.evidence.as_str()),
        )
    } else {
        if landed.forge_status == ForgeStatus::Unavailable {
            out.info(&format!(
                "Could not query the forge for a merged PR on '{branch}' — fell back to the \
                 offline tree-equality check (verdict: {}) and kept the conservative 'git branch \
                 -d'",
                landed.verdict.as_str()
            ));
        } else if landed.verdict == Verdict::Unknown {
            out.info(&format!(
                "Could not determine whether '{branch}' has landed — keeping the conservative \
                 'git branch -d'"
            ));
        }
        ("-d", String::new())
    };

    let Ok(attempt) = Command::new("git")
        .arg("-C")
        .arg(ctx.repo_root)
        .args(["branch", flag, branch])
        .output()
    else {
        out.warning(&format!(
            "Could not delete local branch '{branch}': git could not be executed"
        ));
        return BranchOutcome::Kept;
    };
    if attempt.status.success() {
        out.success(&format!("Local branch '{branch}' deleted{safety_note}"));
        return BranchOutcome::Deleted;
    }

    let mut delete_output = String::from_utf8_lossy(&attempt.stderr).into_owned();
    delete_output.push_str(&String::from_utf8_lossy(&attempt.stdout));
    let delete_output = delete_output.trim().to_string();

    if is_checked_out_refusal(&delete_output) {
        return handle_checked_out(ctx, out, branch, flag, &safety_note);
    }
    if flag == "-d" {
        out.warning(&format!(
            "Could not delete local branch '{branch}' (may have unpushed commits — use 'git \
             branch -D {branch}' if intentional)"
        ));
    } else {
        out.warning(&format!("Could not delete local branch '{branch}': {delete_output}"));
    }
    BranchOutcome::Kept
}

/// Distinguish "checked out somewhere" from a genuine "not fully merged"
/// refusal (#4100 AC #4). Case-insensitive, matching the shell's `grep -qiE`.
#[must_use]
pub fn is_checked_out_refusal(git_stderr: &str) -> bool {
    let lower = git_stderr.to_ascii_lowercase();
    lower.contains("checked out at")
        || lower.contains("is currently checked out")
        || lower.contains("used by worktree")
}

/// The branch is checked out somewhere. #4171: say WHERE, because the generic
/// message routes an operator toward worktree-cleanup advice that does not
/// exist for the primary checkout.
fn handle_checked_out(
    ctx: &DeleteContext,
    out: &Out,
    branch: &str,
    flag: &str,
    safety_note: &str,
) -> BranchOutcome {
    let checkout_loc = find_worktree_by_branch(ctx.repo_root, branch);
    let is_primary = checkout_loc
        .as_deref()
        .is_some_and(|loc| is_primary_worktree_path(ctx.repo_root, loc));
    let Some(checkout_loc) = checkout_loc.filter(|_| is_primary) else {
        out.warning(&format!(
            "Could not delete local branch '{branch}' — it is checked out (current HEAD or \
             another worktree)"
        ));
        return BranchOutcome::Kept;
    };
    let loc = checkout_loc.display().to_string();
    let default_label = ctx.default_branch.unwrap_or("<default-branch>");

    // #5015 auto-cleanup, gated on all four conditions the shell gates on.
    // Condition 3 (`flag == "-D"`) is the load-bearing one: it means
    // `branch_landed` already answered `landed`, so the default branch
    // already contains every change on this branch and nothing is lost.
    // Condition 4 is re-checked HERE, immediately before the mutating
    // `checkout`, rather than cached earlier — a TOCTOU gap against a
    // concurrent process in the same checkout is the whole reason.
    if ctx.cleanup_primary_checkout
        && ctx.default_branch.is_some_and(|d| !d.is_empty())
        && flag == "-D"
        && is_clean_checkout(&checkout_loc)
        && has_no_stashes(&checkout_loc)
    {
        let default = ctx.default_branch.unwrap_or_default();
        let switched = Command::new("git")
            .arg("-C")
            .arg(&checkout_loc)
            .args(["checkout", "-q", default])
            .output()
            .is_ok_and(|o| o.status.success());
        let deleted = switched
            && Command::new("git")
                .arg("-C")
                .arg(ctx.repo_root)
                .args(["branch", "-D", branch])
                .output()
                .is_ok_and(|o| o.status.success());
        if deleted {
            out.success(&format!("Local branch '{branch}' deleted{safety_note}"));
            out.info(&format!(
                "Primary checkout ({loc}) was on '{branch}' — automatically switched to \
                 '{default}' to free it up for deletion"
            ));
            return BranchOutcome::Deleted;
        }
        out.warning(&format!(
            "Attempted to auto-clean up '{branch}' in the primary checkout ({loc}) but the \
             checkout or delete failed — falling back to manual instructions"
        ));
    }

    out.warning(&format!(
        "Could not delete local branch '{branch}' — it is checked out in the primary repository \
         checkout ({loc})."
    ));
    out.warning(&format!(
        "To clean it up: git -C '{loc}' checkout {default_label} && git -C '{loc}' branch -D \
         {branch}"
    ));
    BranchOutcome::Kept
}

/// Does `refs/heads/<branch>` resolve?
#[must_use]
pub fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The first `worktree ` line of `git worktree list --porcelain` is always the
/// primary checkout.
fn primary_worktree_path(repo_root: &Path) -> Option<PathBuf> {
    worktree_entries(repo_root)
        .into_iter()
        .next()
        .map(|(path, _)| path)
}

fn is_primary_worktree_path(repo_root: &Path, check: &Path) -> bool {
    let Some(primary) = primary_worktree_path(repo_root) else {
        return false;
    };
    real(check) == real(&primary)
}

fn find_worktree_by_branch(repo_root: &Path, want: &str) -> Option<PathBuf> {
    let want = format!("refs/heads/{want}");
    worktree_entries(repo_root)
        .into_iter()
        .find(|(_, branch)| branch.as_deref() == Some(want.as_str()))
        .map(|(path, _)| path)
}

/// `(path, branch-ref)` for every entry `git worktree list --porcelain`
/// reports, in order.
///
/// A worktree path may contain spaces, so the path is everything after the
/// literal `worktree ` prefix — never field 2 of a split. That is the #7858
/// class in its read-only form, and here it is structural: nothing
/// word-splits. (A path containing a newline is still unrepresentable in this
/// porcelain format; that caveat is git's, and predates the port.)
pub(crate) fn worktree_entries(repo_root: &Path) -> Vec<(PathBuf, Option<String>)> {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_worktree_porcelain(&String::from_utf8_lossy(&out.stdout))
}

/// Split out from [`worktree_entries`] so the space-in-path behaviour is
/// testable without provisioning a repo under a space-containing path.
pub(crate) fn parse_worktree_porcelain(text: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut entries: Vec<(PathBuf, Option<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.push((PathBuf::from(path), None));
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(last) = entries.last_mut() {
                last.1 = Some(branch.to_string());
            }
        }
    }
    entries
}

fn is_clean_checkout(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.is_empty())
}

fn has_no_stashes(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["stash", "list"])
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.is_empty())
}

/// `cd "$p" && pwd -P` semantics, with the input preserved when it cannot be
/// resolved (a path that no longer exists).
fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests;
