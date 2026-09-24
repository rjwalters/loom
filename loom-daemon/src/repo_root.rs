//! Single-source repository-root resolution for every `loom-daemon` entry
//! point (issue #5140).
//!
//! Three copies of "walk up looking for the repo root" used to live in this
//! crate ([`crate::script_helpers`], [`crate::agent_session`], and
//! `worktree_ops::repo`), and they did not agree: the `worktree_ops` copy
//! accepted **any** ancestor containing a `.loom/` directory, with no `.git`
//! check. That is not a hypothetical mismatch — every fleet host that runs
//! `loom-daemon tokens bootstrap` has a machine-level `~/.loom/` (the token
//! pool), so `loom-daemon recover-orphans` invoked from `$HOME` resolved
//! `$HOME` as the "repo root" on the very first iteration and then ran
//! `gh issue list` there, which failed with `fatal: not a git repository`.
//!
//! The canonical rule implemented here (the one the other two copies already
//! used, and the one `loom_tools.common.repo.find_repo_root` used before the
//! Rust port):
//!
//! - A candidate ancestor qualifies only when it contains a `.git` entry.
//! - When `.git` is a **file** (a linked worktree's `gitdir:` pointer) the
//!   pointer is followed back to the *main* checkout, so a builder inside
//!   `.loom/worktrees/issue-N` resolves to the shared root.
//! - The resolved root must also contain a `.loom/` directory.
//!
//! Both `.git` and `.loom` are required: `.git` alone is any git checkout,
//! `.loom` alone is machine-level daemon state such as `~/.loom/tokens`.
//!
//! That `gitdir:` dereference is correct for *shared state* and wrong for
//! *file content*, so this module exposes a second resolver,
//! [`find_worktree_root`], which stops at the invoking working tree instead
//! (issue #8499). Pick by what the value is used for, not by convenience:
//! config/cache/token lookups take [`find_repo_root`]; "does this path exist,
//! and what does it say?" takes [`find_worktree_root`].

use std::path::{Path, PathBuf};

/// Walk up from `start` looking for the enclosing Loom repository root.
///
/// Returns `None` outside any Loom repository. Callers degrade to an empty
/// config or an explicit exit code rather than panicking — that degradation is
/// what keeps `resolve-model.sh` working outside a Loom repo (issue #4060
/// contract).
#[must_use]
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = normalized_start(start)?;
    loop {
        let git_path = current.join(".git");
        if git_path.exists() {
            let root = resolve_git_root(&current, &git_path);
            if root.join(".loom").is_dir() {
                return Some(root);
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

/// [`find_repo_root`] rooted at the process working directory.
#[must_use]
pub fn find_repo_root_from_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    find_repo_root(&cwd)
}

/// Walk up from `start` looking for the enclosing *working tree* — the same
/// walk [`find_repo_root`] performs, minus the `gitdir:` dereference.
///
/// The two answer different questions and a caller must pick deliberately
/// (issue #8499):
///
/// - [`find_repo_root`] answers **"where is the shared state for this
///   clone?"** — `.loom/config.json`, the `gh` read cache, the token pool.
///   There is exactly one per clone, so following a linked worktree's `.git`
///   pointer file back to the main checkout is the *right* answer there.
/// - This function answers **"which tree am I reasoning about?"** — file
///   content, which differs per worktree by construction. Loom's whole model
///   is that the primary clone sits on `main` while N worktrees run ahead of
///   it, so resolving content against the main checkout reports a file the
///   caller can see as missing (and a file the caller *cannot* see as
///   present). `premise-check`'s citation resolution hit exactly that: a
///   `RECORD-MALFORMED` (exit 12) for a citation that was correct in the
///   worktree it was written in.
///
/// Returns `None` outside any Loom checkout; the caller decides the fallback.
#[must_use]
pub fn find_worktree_root(start: &Path) -> Option<PathBuf> {
    let mut current = normalized_start(start)?;
    loop {
        // No `resolve_git_root` here: a linked worktree's `.git` pointer file
        // marks *this* directory as a working tree, which is the whole point.
        if current.join(".git").exists() && current.join(".loom").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// [`find_worktree_root`] rooted at the process working directory.
#[must_use]
pub fn find_worktree_root_from_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    find_worktree_root(&cwd)
}

/// Absolutize `start`, then canonicalize best-effort — a non-existent start
/// path still walks up.
fn normalized_start(start: &Path) -> Option<PathBuf> {
    let current: PathBuf = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    Some(current.canonicalize().unwrap_or(current))
}

/// Resolve a `--workspace` CLI argument (default `.`) to the enclosing Loom
/// repository root.
///
/// Unlike [`find_repo_root`] this is the *entry-point* form: it fails with an
/// operator-facing message naming both the directory it searched from and what
/// it required, so a run from the wrong directory is never mistaken for a
/// forge or git outage (issue #5140).
pub fn resolve_repo_root(workspace: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(workspace);
    let start = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    let start = start.canonicalize().unwrap_or(start);
    find_repo_root(&start).ok_or_else(|| {
        anyhow::anyhow!(
            "not inside a Loom repository: no ancestor of {} contains both a .git entry and a \
             .loom/ directory. Run this command from inside a Loom checkout, or pass \
             --workspace <path-to-checkout>. (A machine-level ~/.loom/ — e.g. the token pool — \
             is not a repository.)",
            start.display()
        )
    })
}

/// Resolve the main repo root for a candidate directory, following a linked
/// worktree's `.git` `gitdir:` pointer file when present. Mirrors
/// `loom_tools.common.repo._resolve_git_root`.
fn resolve_git_root(candidate: &Path, git_path: &Path) -> PathBuf {
    if git_path.is_dir() {
        return candidate.to_path_buf();
    }
    let Ok(text) = std::fs::read_to_string(git_path) else {
        return candidate.to_path_buf();
    };
    let Some(rest) = text.trim().strip_prefix("gitdir:") else {
        return candidate.to_path_buf();
    };
    let gitdir = Path::new(rest.trim());
    let joined = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        candidate.join(gitdir)
    };
    let mut p = joined.canonicalize().unwrap_or(joined);
    // Walk up from e.g. /repo/.git/worktrees/issue-42 to /repo/.git.
    while p.file_name().is_some_and(|n| n != ".git") {
        if !p.pop() {
            return candidate.to_path_buf();
        }
    }
    if p.file_name().is_some_and(|n| n == ".git") {
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }
    candidate.to_path_buf()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_repo(root: &Path) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".loom")).unwrap();
    }

    #[test]
    fn finds_repo_root_at_start() {
        let dir = tempdir().unwrap();
        make_repo(dir.path());
        assert_eq!(find_repo_root(dir.path()), Some(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn finds_repo_root_in_ancestor() {
        let dir = tempdir().unwrap();
        make_repo(dir.path());
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_repo_root(&nested), Some(dir.path().canonicalize().unwrap()));
    }

    /// The #5140 regression: a directory holding machine-level daemon state
    /// (`~/.loom/tokens`) but no `.git` is NOT a repository root.
    #[test]
    fn loom_dir_without_git_is_not_a_repo_root() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom").join("tokens")).unwrap();
        assert_eq!(find_repo_root(dir.path()), None);
    }

    /// ...and the walk must not stop there either — a real checkout above a
    /// `~/.loom`-shaped directory still wins.
    #[test]
    fn walk_continues_past_a_bare_loom_dir() {
        let dir = tempdir().unwrap();
        make_repo(dir.path());
        let home_like = dir.path().join("home").join("worker");
        std::fs::create_dir_all(home_like.join(".loom").join("tokens")).unwrap();
        assert_eq!(find_repo_root(&home_like), Some(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn git_without_loom_is_not_a_repo_root() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(find_repo_root(dir.path()), None);
    }

    /// A linked worktree whose `.git` is a `gitdir:` pointer file, laid out
    /// the way `worktree.sh` lays one out. Returns `(main, worktree)`.
    fn make_linked_worktree(dir: &Path) -> (PathBuf, PathBuf) {
        let main = dir.join("repo");
        make_repo(&main);
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("issue-42")).unwrap();
        let worktree = dir.join("wt");
        make_repo(&worktree);
        std::fs::remove_dir_all(worktree.join(".git")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git")
                    .join("worktrees")
                    .join("issue-42")
                    .display()
            ),
        )
        .unwrap();
        (main, worktree)
    }

    #[test]
    fn resolves_worktree_gitlink_to_main_checkout() {
        let dir = tempdir().unwrap();
        let (main, worktree) = make_linked_worktree(dir.path());
        assert_eq!(find_repo_root(&worktree), Some(main.canonicalize().unwrap()));
    }

    /// The #8499 regression: the *content* resolver must stay in the linked
    /// worktree. Following the gitlink here reports the main checkout's files,
    /// which is a different commit by construction.
    #[test]
    fn worktree_root_stays_in_the_linked_worktree() {
        let dir = tempdir().unwrap();
        let (main, worktree) = make_linked_worktree(dir.path());
        assert_eq!(find_worktree_root(&worktree), Some(worktree.canonicalize().unwrap()));
        // ...and the two resolvers genuinely disagree, which is the point.
        assert_eq!(find_repo_root(&worktree), Some(main.canonicalize().unwrap()));
    }

    #[test]
    fn worktree_root_finds_the_tree_from_a_nested_directory() {
        let dir = tempdir().unwrap();
        let (_main, worktree) = make_linked_worktree(dir.path());
        let nested = worktree.join("loom-daemon").join("src");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_worktree_root(&nested), Some(worktree.canonicalize().unwrap()));
    }

    /// In the primary clone the two resolvers agree — there is no gitlink to
    /// dereference, so nothing about the ordinary case changes.
    #[test]
    fn worktree_root_matches_repo_root_in_the_primary_clone() {
        let dir = tempdir().unwrap();
        make_repo(dir.path());
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(find_worktree_root(dir.path()), Some(root.clone()));
        assert_eq!(find_repo_root(dir.path()), Some(root));
    }

    #[test]
    fn worktree_root_requires_both_markers() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom").join("tokens")).unwrap();
        assert_eq!(find_worktree_root(dir.path()), None);
        let other = tempdir().unwrap();
        std::fs::create_dir_all(other.path().join(".git")).unwrap();
        assert_eq!(find_worktree_root(other.path()), None);
    }

    #[test]
    fn resolve_repo_root_error_names_the_searched_directory() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let err = resolve_repo_root(dir.path().to_str().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not inside a Loom repository"), "{msg}");
        assert!(msg.contains("--workspace"), "{msg}");
    }
}
