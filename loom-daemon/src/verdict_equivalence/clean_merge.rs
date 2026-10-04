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
//! # Why local git, and why an absent object is fetched (#10134)
//!
//! `merge-tree` has no forge equivalent, so this kind needs the objects. The
//! *rebase* kind ([`super::patch_identity`]) is computed entirely by the forge
//! and proves the same thing for a clean merge whose base changes do not touch
//! the PR's files — but the case only THIS kind can carry is the overlap: when
//! the base changed a file the PR also touches, the PR's patch text
//! legitimately differs (different context, different index lines) and
//! `patch_identity` refutes, while the head is still, provably, the automatic
//! merge with no hand edits — the operator's row 2.
//!
//! That is precisely the head a host has NOT seen yet: `gh pr update-branch`
//! mints the merge commit on the forge, and a daemon root that has not fetched
//! since does not hold it. Before #10134 that answered `Indeterminate` and the
//! verdict was cleared (2am#2150/#2090/#2173/#2121). [`assess`] therefore brings
//! an absent head in with one bounded fetch ([`git_objects::ensure_commit`]),
//! and when even that fails, says WHY so the fail-closed clear is visible.
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
/// the process cwd). [`assess`] without a PR number, and without the reason.
pub fn evidence(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: &Path,
    reviewed: &str,
    head: &str,
    base_ref: &str,
) -> Evidence {
    assess(gh_bin, cwd, repo, None, reviewed, head, base_ref).0
}

/// [`evidence`], fetching an absent head first (#10134), plus — when the
/// answer is `Indeterminate` because the comparison could not be made at all —
/// a one-line reason a caller can log or post. `pr` adds the PR's
/// `refs/pull/<n>/head` as a fetch fallback.
pub fn assess(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: &Path,
    pr: Option<u32>,
    reviewed: &str,
    head: &str,
    base_ref: &str,
) -> (Evidence, Option<String>) {
    let unavailable = |why: String| (Evidence::Indeterminate, Some(why));
    if !super::is_sha(reviewed) || !super::is_sha(head) || !super::is_safe_ref(base_ref) {
        return unavailable("an argument is not a bare hex SHA / safe ref name".into());
    }
    if !git_objects::is_git_repo(repo) {
        return unavailable(format!("`{}` is not a git repository", repo.display()));
    }
    // A shallow clone can hold `head` and still be missing the merge-base
    // history `merge-tree` needs, and would then produce a *different* tree
    // rather than an error. Unknown depth is treated as shallow.
    if git_objects::is_shallow(repo) != Some(false) {
        return unavailable(format!("`{}` is a shallow clone", repo.display()));
    }
    // Checked before the fetch, so a host that could never compute the answer
    // does not pay for the network round trip.
    if !git_objects::supports_merge_tree(repo) {
        return unavailable("git predates 2.38 (no `merge-tree --write-tree`)".into());
    }

    // #10134: an update-branch merge commit is routinely absent from a clone
    // that has not fetched since the forge minted it. Bring it in (objects
    // only) rather than failing closed on a byte-identical reviewed change.
    if let Err(why) = git_objects::ensure_commit(repo, pr, head) {
        return unavailable(format!(
            "new head {head} is not in the local clone and could not be fetched: {why}"
        ));
    }
    let Some(head_oid) = git_objects::rev_parse(repo, &format!("{head}^{{commit}}")) else {
        return unavailable(format!("could not resolve {head} in the local clone"));
    };

    // Condition 1: exactly two parents.
    let Some(parents) = git_objects::parents(repo, &head_oid) else {
        return unavailable(format!("could not read the parents of {head_oid}"));
    };
    let [first, second] = parents.as_slice() else {
        return (Evidence::Refuted, None);
    };

    // Condition 2: the first parent IS the reviewed head. Both sides are
    // canonicalized through rev-parse so an abbreviated marker SHA still
    // compares correctly — and so a SHA this repo does not have fails closed
    // rather than comparing unequal by accident.
    let Some(reviewed_oid) = git_objects::rev_parse(repo, &format!("{reviewed}^{{commit}}")) else {
        return unavailable(format!("reviewed commit {reviewed} is not in the local clone"));
    };
    if *first != reviewed_oid {
        return (Evidence::Refuted, None);
    }

    // Condition 3: the merged parent is base-branch content.
    match super::descends_from(gh_bin, cwd, second, base_ref) {
        Some(true) => {}
        Some(false) => return (Evidence::Refuted, None),
        None => {
            return unavailable(format!(
                "the forge could not confirm {second} is on base branch `{base_ref}`"
            ))
        }
    }

    // Condition 4: the head's tree IS the automatic merge's tree.
    let expected = match git_objects::merge_tree(repo, &reviewed_oid, second) {
        MergeTree::Tree(oid) => oid,
        // No clean automatic merge exists, so this head cannot be one.
        MergeTree::Conflict => return (Evidence::Refuted, None),
        MergeTree::Failed => return unavailable("`git merge-tree --write-tree` failed".into()),
    };
    let Some(head_tree) = git_objects::rev_parse(repo, &format!("{head_oid}^{{tree}}")) else {
        return unavailable(format!("could not read the tree of {head_oid}"));
    };
    if head_tree == expected {
        (Evidence::Proven, None)
    } else {
        (Evidence::Refuted, None)
    }
}
