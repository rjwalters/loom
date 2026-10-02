//! The local-git plumbing [`super::clean_merge`] needs, and nothing else
//! (Issue #9416).
//!
//! Every function here is **fail-closed by return type**: an answer is only
//! ever `Some`/`Tree(..)` when git actually said so. A missing binary, a
//! missing object, a shallow clone, a git too old for
//! `merge-tree --write-tree`, or any non-zero exit collapses into the
//! indeterminate arm, which [`super::detect`] turns into "invalidate as
//! before" — never into an assumed equivalence.
//!
//! # Read-only, and deliberately network-free
//!
//! Nothing here fetches. An earlier draft of #9416 did a best-effort
//! `git fetch origin <sha>` to bring an absent head in, and that was wrong on
//! two counts: it made an evidence function reach the network as a side effect
//! on a path whose whole job is to answer a question, and it bought nothing —
//! when the objects are absent, [`super::patch_identity`] (which asks the
//! forge, not the object store) already answers the clean-merge shape too,
//! because a clean merge of the base leaves the PR's own merge-base-relative
//! patch untouched. So an absent object is simply
//! [`super::Evidence::Indeterminate`] here and the next kind takes over. The
//! only write any of this makes is `merge-tree --write-tree`'s inert loose
//! tree object (see [`merge_tree`]).
//!
//! The git-version floor (`>= 2.38`, where `merge-tree --write-tree` exists)
//! and the exit-code reading (`0` clean, `1` conflict, anything else a
//! failure) are deliberately the same ones
//! [`crate::overlap_replay::conflict`] and
//! [`crate::merge_pr::mergeable_recheck`] already use — this module does not
//! re-derive them differently, it just answers a different question with them
//! (verdict equivalence, not conflict prediction).

use std::path::Path;
use std::process::{Command, Stdio};

/// Run `git -C <repo> <args>` and return its trimmed stdout, or `None` on any
/// failure (absent binary, non-zero exit, non-UTF-8 output).
pub(super) fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// Is `repo` inside a git working tree at all? The CLI path inherits the
/// caller's cwd, which is normally a managed worktree but is not guaranteed to
/// be a repo at all.
pub(super) fn is_git_repo(repo: &Path) -> bool {
    git(repo, &["rev-parse", "--git-dir"]).is_some()
}

/// Is `repo` a shallow clone? `None` when git could not be asked — treated by
/// callers exactly like `Some(true)`, because a clone whose depth is unknown
/// cannot be trusted to hold the merge-base history `merge-tree` needs.
pub(super) fn is_shallow(repo: &Path) -> Option<bool> {
    match git(repo, &["rev-parse", "--is-shallow-repository"])?.as_str() {
        "false" => Some(false),
        "true" => Some(true),
        // `--is-shallow-repository` predates every git that has
        // `merge-tree --write-tree`, so an unrecognized answer means something
        // else is wrong. Unknown depth is not "full".
        _ => None,
    }
}

/// Is `sha` present locally AS A COMMIT? `cat-file -e <sha>^{commit}` is the
/// narrow question: a SHA that resolves to a blob or tag, or to nothing at
/// all, answers `false`.
pub(super) fn has_commit(repo: &Path, sha: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `git rev-parse --verify <rev>` — resolve a rev to a full 40-hex OID.
pub(super) fn rev_parse(repo: &Path, rev: &str) -> Option<String> {
    let oid = git(repo, &["rev-parse", "--verify", "--quiet", rev])?;
    if oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(oid)
    } else {
        None
    }
}

/// The parent OIDs of `sha`, in order. An empty vector (a root commit) is a
/// legitimate `Some`; `None` means git could not be asked.
pub(super) fn parents(repo: &Path, sha: &str) -> Option<Vec<String>> {
    let line = git(repo, &["rev-list", "--parents", "-n", "1", sha])?;
    let mut fields = line.split_whitespace();
    // First field is the commit itself.
    fields.next()?;
    Some(fields.map(str::to_string).collect())
}

/// git >= 2.38, where `git merge-tree --write-tree` exists. Same floor and
/// same probe shape as [`crate::overlap_replay::conflict`].
pub(super) fn supports_merge_tree(repo: &Path) -> bool {
    let Some(out) = git(repo, &["--version"]) else {
        return false;
    };
    out.split_whitespace()
        .nth(2)
        .is_some_and(merge_tree_in_version)
}

pub(super) fn merge_tree_in_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(Ok(major)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    let minor = parts
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    major > 2 || (major == 2 && minor >= 38)
}

/// What a `git merge-tree --write-tree` run produced.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MergeTree {
    /// A clean automatic merge whose result tree is this OID.
    Tree(String),
    /// Textual conflicts: there IS no automatic merge, so the head in front of
    /// us cannot be one. Positive evidence, not an error.
    Conflict,
    /// Could not be computed (git too old, missing object, unparsable output,
    /// any other exit status).
    Failed,
}

/// `git merge-tree --write-tree <a> <b>` — the automatic three-way merge of
/// two commits, with git computing their merge base itself (exactly the check
/// Issue #9416's acceptance criterion names).
///
/// Writes the result tree into `repo`'s object store as a loose object and
/// prints its OID. That write is inert: no ref points at it, nothing reads it
/// but this function, and ordinary gc reclaims it — the index and working tree
/// are untouched (`merge-tree` is plumbing).
pub(super) fn merge_tree(repo: &Path, a: &str, b: &str) -> MergeTree {
    if !supports_merge_tree(repo) {
        return MergeTree::Failed;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-tree", "--write-tree", a, b])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    else {
        return MergeTree::Failed;
    };
    match out.status.code() {
        Some(0) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let Some(oid) = stdout.lines().next().map(str::trim) else {
                return MergeTree::Failed;
            };
            if oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit()) {
                MergeTree::Tree(oid.to_string())
            } else {
                MergeTree::Failed
            }
        }
        Some(1) => MergeTree::Conflict,
        _ => MergeTree::Failed,
    }
}
