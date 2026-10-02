//! Equivalence kind 2 (Issue #9416): the new head is **exactly the clean
//! automatic merge of the PR's base into the reviewed head**.
//!
//! This is the shape the fleet's own `gh pr update-branch` produces, and the
//! shape four of the six moved heads in #9416's 16-PR audit had.
//!
//! # The four conditions, all required
//!
//! 1. `head` has **exactly two** parents. A single-parent head is not a merge;
//!    an octopus merge is not "the base merged in", and its automatic result is
//!    not what `merge-tree <a> <b>` computes. Either answers [`Evidence::Refuted`].
//! 2. `parents[0] == reviewed`. The merge must have been made **on** the
//!    reviewed head, not the other way round.
//! 3. `parents[1]` descends from nothing in particular — but it must be a commit
//!    **on the PR's base branch**, confirmed against the forge
//!    ([`super::descends_from`]). Without this condition the kind would carry a
//!    verdict across a clean merge of an *arbitrary* branch, whose content
//!    nobody reviewed. That is the hazard, not an edge case.
//! 4. `git merge-tree --write-tree <reviewed> <parents[1]>` succeeds cleanly and
//!    its result tree equals `head^{tree}`. This is what rules out hand edits
//!    and conflict resolution: a conflicted merge, or one whose author touched
//!    anything while resolving, produces a different tree.
//!
//! # Why local git, and why an absent object is cheap
//!
//! `merge-tree` has no forge equivalent, so this kind needs the objects — and
//! it never fetches them (see [`super::git_objects`]). That costs nothing,
//! because the *rebase* kind ([`super::patch_identity`]) is computed entirely by
//! the forge and proves the same thing for most clean merges: merging the base
//! in leaves the PR's own merge-base-relative patch untouched. So on a host
//! whose object store lacks the head, this kind steps aside and the next one
//! answers.
//!
//! The one case only THIS kind can carry is the overlap: when the base changed a
//! file the PR also touches, the PR's patch text legitimately differs (different
//! context, different index lines) and `patch_identity` refutes — but the head
//! is still, provably, the automatic merge with no hand edits, which is exactly
//! the operator's row 2.
//!
//! # Fail closed
//!
//! A shallow clone, a cwd that is not a git repository, an object that is not
//! present locally, a git predating `merge-tree --write-tree`, a
//! `merge-tree` conflict, or an unreadable tree all answer
//! [`Evidence::Indeterminate`] (a `merge-tree` **conflict** answers `Refuted`:
//! that is positive evidence there is no clean automatic merge). Both are
//! treated identically by [`super::detect`] — invalidate as before.

use std::path::Path;

use super::git_objects::{self, MergeTree};
use super::Evidence;

/// Is `head` exactly the clean automatic merge of `base_ref` into `reviewed`?
///
/// `repo` is the local repository to use for the git half; `cwd` is what the
/// `gh` half resolves `{owner}/{repo}` against (the two are the same path on
/// the daemon pass, and `cwd` is `None` on the CLI path, where `gh` inherits
/// the process cwd).
pub fn evidence(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: &Path,
    reviewed: &str,
    head: &str,
    base_ref: &str,
) -> Evidence {
    if !super::is_sha(reviewed) || !super::is_sha(head) || !super::is_safe_ref(base_ref) {
        return Evidence::Indeterminate;
    }
    if !git_objects::is_git_repo(repo) {
        return Evidence::Indeterminate;
    }
    // A shallow clone can hold `head` and still be missing the merge-base
    // history `merge-tree` needs, and would then produce a *different* tree
    // rather than an error. Unknown depth is treated as shallow.
    if git_objects::is_shallow(repo) != Some(false) {
        return Evidence::Indeterminate;
    }

    // The objects must already be here: this function never fetches (see
    // git_objects' module doc). An absent head is Indeterminate, and
    // `super::patch_identity` — which asks the forge, not the object store —
    // goes on to answer the same shape anyway.
    if !git_objects::has_commit(repo, head) {
        return Evidence::Indeterminate;
    }
    let Some(head_oid) = git_objects::rev_parse(repo, &format!("{head}^{{commit}}")) else {
        return Evidence::Indeterminate;
    };

    // Condition 1: exactly two parents.
    let Some(parents) = git_objects::parents(repo, &head_oid) else {
        return Evidence::Indeterminate;
    };
    let [first, second] = parents.as_slice() else {
        return Evidence::Refuted;
    };

    // Condition 2: the first parent IS the reviewed head. Both sides are
    // canonicalized through rev-parse so an abbreviated marker SHA still
    // compares correctly — and so a SHA this repo does not have fails closed
    // rather than comparing unequal by accident.
    let Some(reviewed_oid) = git_objects::rev_parse(repo, &format!("{reviewed}^{{commit}}")) else {
        return Evidence::Indeterminate;
    };
    if *first != reviewed_oid {
        return Evidence::Refuted;
    }

    // Condition 3: the merged parent is base-branch content.
    match super::descends_from(gh_bin, cwd, second, base_ref) {
        Some(true) => {}
        Some(false) => return Evidence::Refuted,
        None => return Evidence::Indeterminate,
    }

    // Condition 4: the head's tree IS the automatic merge's tree.
    let expected = match git_objects::merge_tree(repo, &reviewed_oid, second) {
        MergeTree::Tree(oid) => oid,
        // No clean automatic merge exists, so this head cannot be one.
        MergeTree::Conflict => return Evidence::Refuted,
        MergeTree::Failed => return Evidence::Indeterminate,
    };
    let Some(head_tree) = git_objects::rev_parse(repo, &format!("{head_oid}^{{tree}}")) else {
        return Evidence::Indeterminate;
    };
    if head_tree == expected {
        Evidence::Proven
    } else {
        Evidence::Refuted
    }
}
