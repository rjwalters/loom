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
//! # Through tree-identical commits and earlier clean merges (#10875)
//!
//! The four conditions are applied to the *merge*, which need not be the head
//! itself, and "the reviewed head" in condition 2 means anything that reduces
//! to it. Walking down from the head, a single-parent commit whose tree equals
//! its parent's (a #8248/#8508 re-date push, an empty "re-run CI" commit) is
//! peeled off as a no-op, and a two-parent commit must meet conditions 1, 3 and
//! 4 with a first parent that itself reduces to the reviewed head. So
//! `reviewed → re-date → merge(base) → re-date` proves, where before only the
//! bare `merge(reviewed, base)` did — the shape of #10857's history, on which a
//! released critical-file hold re-armed. Every step is read from git objects
//! (plus the forge's ancestry answer for condition 3), never a commit message;
//! a real commit, a hand edit or a conflict anywhere on the way still refutes,
//! and a walk past its bound is `Indeterminate`. A run of no-ops with no merge
//! at all is the tree kind's proof, so this kind leaves it to that one.
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

    // Resolved up front: the no-op walk below stops when it reaches it. A SHA
    // this repo does not have fails closed rather than comparing unequal by
    // accident, and both sides are canonicalized so an abbreviated marker SHA
    // still compares correctly.
    let Some(reviewed_oid) = git_objects::rev_parse(repo, &format!("{reviewed}^{{commit}}")) else {
        return unavailable(format!("reviewed commit {reviewed} is not in the local clone"));
    };

    let mut budget = Budget {
        noops: MAX_NOOP_COMMITS,
        merges: MAX_MERGES,
    };
    match reduce(gh_bin, cwd, repo, &head_oid, &reviewed_oid, base_ref, &mut budget) {
        // Zero merges on the way down: `head` is a run of tree-identical
        // commits on the reviewed head. That IS equivalent, but it is the
        // tree kind's proof (`forge_tree_unchanged`), not this one's, so this
        // kind does not claim it under the wrong name.
        Ok(0) => (Evidence::Refuted, None),
        Ok(_) => (Evidence::Proven, None),
        Err(evidence) => evidence,
    }
}

/// How far [`reduce`] may walk. Generous for any real PR (a re-date push is
/// one commit; `gh pr update-branch` is one merge) and small enough that a
/// pathological history costs a bounded number of local git calls and forge
/// ancestry checks. Exhausting either answers `Indeterminate` — never assumed.
const MAX_NOOP_COMMITS: usize = 64;
const MAX_MERGES: usize = 8;

struct Budget {
    noops: usize,
    merges: usize,
}

/// Is `commit`'s content exactly `reviewed`'s change with nothing added but
/// clean automatic merges of the base (#10875)? `Ok(n)` — yes, through `n`
/// such merges; `Err` — the [`assess`] answer to return instead.
///
/// Two moves, each re-derived from the repository and nothing else:
///
/// - **a tree-identical no-op** — a single-parent commit whose tree equals its
///   parent's (a #8248/#8508 re-date push, a "re-run CI" empty commit) is
///   peeled off. It changes no content, whatever its message says; its message
///   is never read.
/// - **a clean merge of the base** — a two-parent commit whose second parent is
///   on the PR's base branch (asked of the forge), whose tree equals
///   `git merge-tree --write-tree <first> <second>`, and whose first parent
///   itself reduces to `reviewed`.
///
/// Before #10875 only the bare shape `head = merge(reviewed, base)` was
/// accepted, so the fleet's own routine history — a re-date stacked on an
/// update-branch merge, or a merge made on top of an earlier re-date — could
/// never be proven, and a released critical-file hold re-armed on it. Anything
/// else on the way down (a real commit, a hand-edited or conflicted merge, a
/// merge of a non-base branch, an octopus) still refutes.
fn reduce(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: &Path,
    commit: &str,
    reviewed_oid: &str,
    base_ref: &str,
    budget: &mut Budget,
) -> Result<usize, (Evidence, Option<String>)> {
    let unavailable = |why: String| Err((Evidence::Indeterminate, Some(why)));
    let mut cur = commit.to_string();
    loop {
        if cur == reviewed_oid {
            return Ok(0);
        }
        let Some(parents) = git_objects::parents(repo, &cur) else {
            return unavailable(format!("could not read the parents of {cur}"));
        };
        match parents.as_slice() {
            [parent] => {
                // A single-parent commit carries no change only when its tree
                // is its parent's; otherwise it is a real commit.
                let (Some(tree), Some(parent_tree)) = (
                    git_objects::rev_parse(repo, &format!("{cur}^{{tree}}")),
                    git_objects::rev_parse(repo, &format!("{parent}^{{tree}}")),
                ) else {
                    return unavailable(format!(
                        "could not read the trees of {cur} and its parent"
                    ));
                };
                if tree != parent_tree {
                    return Err((Evidence::Refuted, None));
                }
                if budget.noops == 0 {
                    return unavailable(format!(
                        "more than {MAX_NOOP_COMMITS} tree-identical commits between the \
                         reviewed head and the new head"
                    ));
                }
                budget.noops -= 1;
                cur = parent.clone();
            }
            [first, second] => {
                if budget.merges == 0 {
                    return unavailable(format!(
                        "more than {MAX_MERGES} merges between the reviewed head and the new head"
                    ));
                }
                budget.merges -= 1;
                // The merge must have been made ON the reviewed change: its
                // first parent reduces to it (the pre-#10875 rule required
                // equality, which this includes).
                let below = reduce(gh_bin, cwd, repo, first, reviewed_oid, base_ref, budget)?;

                // The merged parent is base-branch content.
                match super::descends_from(gh_bin, cwd, second, base_ref) {
                    Some(true) => {}
                    Some(false) => return Err((Evidence::Refuted, None)),
                    None => {
                        return unavailable(format!(
                            "the forge could not confirm {second} is on base branch `{base_ref}`"
                        ))
                    }
                }

                // The merge's tree IS the automatic merge's tree.
                let expected = match git_objects::merge_tree(repo, first, second) {
                    MergeTree::Tree(oid) => oid,
                    // No clean automatic merge exists, so this cannot be one.
                    MergeTree::Conflict => return Err((Evidence::Refuted, None)),
                    MergeTree::Failed => {
                        return unavailable("`git merge-tree --write-tree` failed".into())
                    }
                };
                let Some(tree) = git_objects::rev_parse(repo, &format!("{cur}^{{tree}}")) else {
                    return unavailable(format!("could not read the tree of {cur}"));
                };
                return if tree == expected {
                    Ok(below + 1)
                } else {
                    Err((Evidence::Refuted, None))
                };
            }
            // A root commit or an octopus merge is not "the base merged in".
            _ => return Err((Evidence::Refuted, None)),
        }
    }
}
