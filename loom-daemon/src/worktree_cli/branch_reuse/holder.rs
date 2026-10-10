//! Which worktree, if any, still holds the branch the refusal is about (#9319).
//!
//! The already-landed refusal used to print one remedy unconditionally —
//! `git branch -D <branch> && worktree.sh <N>` — and `git branch -D` refuses a
//! branch any worktree holds. That is not a corner: every merged increment of
//! a multi-PR issue leaves a `pr-<M>` worktree pinning the very branch the next
//! increment wants to reuse, so the remedy failed on the path that reaches the
//! refusal most often.
//!
//! This module only *diagnoses*. It removes nothing, and the remedy it words is
//! a command for the operator to run, never one this process runs.
//!
//! The answer comes from [`branch_holders`] — the one shared map every
//! branch-deletion site uses (#10766) — so a worktree that git reports as
//! `detached` but that is mid-rebase / mid-bisect on the branch counts, exactly
//! as it does for `git branch -D` itself. A probe that fails yields an empty
//! map and therefore no holder: the refusal degrades to its old wording rather
//! than failing.

use std::path::{Path, PathBuf};

use crate::worktree_ops::aggressive::LOOM_MANAGED_SENTINEL;
use crate::worktree_ops::branch_holders::branch_holders;

/// What kind of worktree the holder is — which decides what may be suggested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A linked worktree carrying the `.loom-managed` sentinel: Loom made it,
    /// so naming a removal command for it is in bounds.
    Managed,
    /// A linked worktree without the sentinel. User-provisioned worktrees are
    /// never removed by Loom, so none is suggested either.
    Unmanaged,
    /// The main workspace itself. `git worktree remove` cannot remove it.
    Main,
}

/// The worktree holding the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// As `git worktree list --porcelain` spelled it.
    pub path: PathBuf,
    pub kind: Kind,
}

/// The worktree holding `branch` in `repo`, or `None` — also `None` when the
/// probe itself failed.
#[must_use]
pub fn find(repo: &Path, branch: &str) -> Option<Holder> {
    let path = branch_holders(repo).remove(branch)?;
    let kind = classify(repo, &path);
    Some(Holder { path, kind })
}

/// A linked worktree's `.git` is a file pointing into the main repository's
/// `worktrees/` directory; only the main workspace has a `.git` *directory*.
/// The path comparison covers the case where that probe cannot be read.
fn classify(repo: &Path, path: &Path) -> Kind {
    let same_as_repo = match (std::fs::canonicalize(repo), std::fs::canonicalize(path)) {
        (Ok(a), Ok(b)) => a == b,
        _ => repo == path,
    };
    if same_as_repo || path.join(".git").is_dir() {
        Kind::Main
    } else if path.join(LOOM_MANAGED_SENTINEL).is_file() {
        Kind::Managed
    } else {
        Kind::Unmanaged
    }
}

/// `path` as one POSIX-shell word, so a remedy survives copy-paste when the
/// path contains a space, a quote, or any other metacharacter. Always quoted:
/// a rule with no "safe character" list has no list to get wrong.
#[must_use]
pub fn shell_word(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

impl Holder {
    /// The tail of the human refusal line: who holds the branch, and what to
    /// run. `delete_and_rerun` is the pre-#9319 remedy, reused verbatim as the
    /// last step of every variant.
    #[must_use]
    pub fn remedy(&self, default_branch: &str, delete_and_rerun: &str) -> String {
        let shown = self.path.display();
        let word = shell_word(&self.path);
        match self.kind {
            Kind::Managed => format!(
                "Loom-managed worktree {shown} still holds it, so 'git branch -D' alone is refused. Remove that worktree, delete the branch and re-run: git worktree remove {word} --force && {delete_and_rerun}"
            ),
            Kind::Unmanaged => format!(
                "Worktree {shown} still holds it, so 'git branch -D' alone is refused. That worktree is not Loom-managed (no {LOOM_MANAGED_SENTINEL} sentinel), so Loom will not suggest removing it: take it off the branch yourself (git -C {word} switch --detach, or finish/abort a rebase or bisect in progress there), then delete the branch and re-run: {delete_and_rerun}"
            ),
            Kind::Main => format!(
                "The main workspace {shown} itself holds it, so 'git branch -D' alone is refused. Switch the main workspace off the branch first (finish/abort a rebase or bisect in progress there), then delete the branch and re-run: git -C {word} switch {default_branch} && {delete_and_rerun}"
            ),
        }
    }
}
