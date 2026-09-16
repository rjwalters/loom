//! "Has this branch landed on the default branch?" — the daemon-side half of
//! the shared primitive introduced in #7812 (`defaults/scripts/lib/branch-landed.sh`).
//!
//! Four tools used to answer this question four different ways, and every
//! reachability-based answer is wrong under a squash merge (the squash commit
//! is a brand-new commit with no parent link to the branch) and equally wrong
//! under GitHub's rebase merge, which rewrites every commit SHA. This module
//! is `clean --aggressive`'s copy of the same ladder the shell library uses:
//!
//! 1. **Ancestry** — `git merge-base --is-ancestor <head> origin/main`. Only
//!    ever proves `landed`; its falsity proves nothing.
//! 2. **Forge** — a MERGED pull request for the branch (the squash/rebase-proof
//!    answer). A probe failure is `Unknown`, never "not merged".
//! 3. **Tree equality** — `git merge-tree --write-tree origin/main <head>`
//!    compared against `origin/main^{tree}`. Equal trees mean merging the
//!    branch would change nothing, i.e. the default branch already contains
//!    every change it carries — independent of SHAs, and entirely offline.
//!
//! The answer is deliberately **three-way** ([`Landed`]): `Unknown` is a real
//! state that must never be coerced into a boolean at this boundary. Coercing
//! it to "landed" reaps a worktree holding unmerged work (data loss); coercing
//! it to "not landed" resurrects the pre-#4889 "can never clean up a
//! squash-merged branch" bug. [`super::aggressive::evaluate_aggressive_candidate`]
//! therefore carries it through its decision ladder as its own `Keep` arm.
//!
//! (The [`Landed::Reachable`] / [`Landed::Rewritten`] split exists only so the
//! decision tree can keep reporting the two distinct removal reasons
//! `reachable_from_origin_main` and `pr_merged` it has always reported —
//! both mean "landed", and [`Landed::is_landed`] is what every decision uses.)

use std::path::Path;
use std::process::Command;

use super::clean;

/// Three-way answer to "has this branch landed?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// Landed: HEAD is reachable from `origin/main` (merge commit, or a
    /// fast-forward). The cheapest and most local proof.
    Reachable,
    /// Landed under rewritten SHAs: a merged PR, or a tree-equality match.
    /// This is the squash-merge and rebase-merge case, where the branch's own
    /// commits are never reachable from the default branch.
    Rewritten,
    /// Provably NOT landed: the branch carries content `origin/main` does not
    /// have (tree comparison says so), or the forge says no PR ever merged it
    /// and no local check could prove otherwise.
    NotLanded,
    /// Could not be determined — the forge probe failed AND the tree
    /// comparison was unavailable. **Never** reap on this.
    Unknown,
}

impl Landed {
    /// Whether the default branch already contains everything the branch has.
    /// `Unknown` is deliberately false here — the fail-closed direction.
    #[must_use]
    pub fn is_landed(self) -> bool {
        matches!(self, Landed::Reachable | Landed::Rewritten)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Landed::Reachable => "landed(reachable)",
            Landed::Rewritten => "landed(rewritten)",
            Landed::NotLanded => "not-landed",
            Landed::Unknown => "unknown",
        }
    }
}

/// Answer [`Landed`] for one worktree's HEAD / branch / issue number.
///
/// `issue_num` is the forge probe's key (`feature/issue-<n>`); `None` skips
/// straight past the forge, exactly as the pre-#7812 code did for a worktree
/// with no `issue-N` branch. Ordering is load-bearing: the two cheap local
/// checks bracket the single (rate-limited) forge round-trip, which is only
/// made when ancestry already failed.
#[must_use]
pub fn probe(repo_root: &Path, head_sha: Option<&str>, issue_num: Option<u32>) -> Landed {
    if head_sha.is_some_and(|h| is_ancestor_of_origin_main(repo_root, h)) {
        return Landed::Reachable;
    }

    // Rung 2: the forge. `Unknown` here is "could not ask", not "not merged".
    let mut forge_answered_negative = false;
    if let Some(n) = issue_num {
        match pr_merged_status(repo_root, n) {
            clean::PrStatus::Merged { .. } => return Landed::Rewritten,
            clean::PrStatus::Unknown => {}
            _ => forge_answered_negative = true,
        }
    }

    // Rung 3: tree equality — the SHA-independent proof, fully offline.
    match head_sha.and_then(|h| tree_equals_origin_main(repo_root, h)) {
        Some(true) => Landed::Rewritten,
        Some(false) => Landed::NotLanded,
        // Nothing could answer. A definitive forge negative still stands;
        // otherwise this is a genuine `Unknown` and must fail closed.
        None => {
            if forge_answered_negative {
                Landed::NotLanded
            } else {
                Landed::Unknown
            }
        }
    }
}

/// Whether `issue_num`'s branch has a **merged** PR (including squash-merged).
///
/// Reuses `clean.rs`'s shared PR probe (#5177) rather than building a second
/// squash-detection path. REST first — the daemon-side reaper's rationale
/// applies here too: `gh pr list` goes through the routinely-exhausted GraphQL
/// quota, while `gh api .../pulls` uses the separate, less-contended REST
/// pool — falling back to the GraphQL-backed probe only when REST cannot
/// answer. The [`clean::PrStatus`] is returned unflattened so a probe failure
/// stays distinguishable from "checked, and it never merged" (#7812).
fn pr_merged_status(repo_root: &Path, issue_num: u32) -> clean::PrStatus {
    match clean::repo_owner_rest(repo_root)
        .map(|owner| clean::check_pr_merged_rest(repo_root, &owner, issue_num))
    {
        Some(clean::PrStatus::Unknown) | None => clean::check_pr_merged(repo_root, issue_num),
        Some(status) => status,
    }
}

/// `git merge-base --is-ancestor <head_sha> origin/main`.
///
/// Demoted to an internal rung of [`probe`] in #7812: on its own it is exactly
/// the heuristic that made `clean --aggressive` refuse to reap every
/// squash-merged worktree (#5189).
pub(crate) fn is_ancestor_of_origin_main(repo_root: &Path, head_sha: &str) -> bool {
    if head_sha.is_empty() {
        return false;
    }
    Command::new("git")
        .args(["merge-base", "--is-ancestor", head_sha, "origin/main"])
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success())
}

/// Tree-equality probe: does merging `head_sha` into `origin/main` produce
/// `origin/main`'s own tree?
///
/// `Some(true)` — landed (squash, rebase, or merge commit; SHA-independent).
/// `Some(false)` — the branch carries content `origin/main` does not have,
/// including the conflict case (exit 1), which is a definitive divergence.
/// `None` — the comparison could not be made (git < 2.38, so no
/// `--write-tree`; unreadable objects; unrelated histories). The caller must
/// treat `None` as "no answer", never as a negative.
fn tree_equals_origin_main(repo_root: &Path, head_sha: &str) -> Option<bool> {
    if head_sha.is_empty() {
        return None;
    }
    let out = Command::new("git")
        .args(["merge-tree", "--write-tree", "origin/main", head_sha])
        .current_dir(repo_root)
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => {
            let merged_tree = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()?
                .trim()
                .to_string();
            if merged_tree.is_empty() {
                return None;
            }
            let base = Command::new("git")
                .args(["rev-parse", "--verify", "-q", "origin/main^{tree}"])
                .current_dir(repo_root)
                .output()
                .ok()?;
            if !base.status.success() {
                return None;
            }
            let base_tree = String::from_utf8_lossy(&base.stdout).trim().to_string();
            if base_tree.is_empty() {
                return None;
            }
            Some(merged_tree == base_tree)
        }
        // Exit 1 is "the merge conflicts" — content that collides with
        // origin/main definitively has not landed.
        Some(1) => Some(false),
        // Exit 128 (bad args / unknown revision / a pre-2.38 git that has no
        // `--write-tree` at all) or anything else: no answer.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_landed_is_true_for_both_landed_variants() {
        assert!(Landed::Reachable.is_landed());
        assert!(Landed::Rewritten.is_landed());
    }

    /// The whole point of the three-way answer: `Unknown` must never read as
    /// landed, and must stay distinguishable from `NotLanded` (which `--force`
    /// may legitimately override, while `Unknown` may not).
    #[test]
    fn unknown_is_not_landed_and_is_distinct_from_not_landed() {
        assert!(!Landed::Unknown.is_landed());
        assert!(!Landed::NotLanded.is_landed());
        assert_ne!(Landed::Unknown, Landed::NotLanded);
    }

    #[test]
    fn as_str_labels_every_variant_distinctly() {
        let labels = [
            Landed::Reachable.as_str(),
            Landed::Rewritten.as_str(),
            Landed::NotLanded.as_str(),
            Landed::Unknown.as_str(),
        ];
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert_ne!(a, b, "labels must be distinguishable in logs");
            }
        }
    }

    #[test]
    fn empty_head_sha_is_never_reachable_or_tree_equal() {
        let repo = std::path::Path::new("/nonexistent-repo-for-unit-test");
        assert!(!is_ancestor_of_origin_main(repo, ""));
        assert_eq!(tree_equals_origin_main(repo, ""), None);
    }

    /// A repo that does not exist cannot answer either local check, and the
    /// forge is not consulted without an issue number — so the verdict is
    /// `Unknown`, not a silent "not landed" that `--force` could then reap.
    #[test]
    fn unresolvable_repo_with_no_issue_number_is_unknown() {
        let repo = std::path::Path::new("/nonexistent-repo-for-unit-test");
        assert_eq!(probe(repo, Some("deadbeef"), None), Landed::Unknown);
    }
}
