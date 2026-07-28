//! Resolve the Loom repository root from a starting directory.
//!
//! Rust port of `loom_tools.common.repo.find_repo_root` (the piece the
//! `worktree_ops` family needs): walk up from a starting directory until a
//! `.loom/` directory is found. Unlike the Python original this does not
//! special-case the `.git` gitlink-file worktree case — every caller in this
//! module runs from the main workspace checkout (the `clean` / `cleanup` /
//! `recover-orphans` CLIs operate on `.loom/worktrees/issue-*`, not from
//! inside one), so a plain "nearest ancestor with a `.loom/` dir" walk is
//! sufficient and matches practical behavior.

use std::path::{Path, PathBuf};

/// Walk up from `start` looking for the nearest ancestor containing a
/// `.loom/` directory. Returns `None` if none is found before the filesystem
/// root.
#[must_use]
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        if current.join(".loom").is_dir() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return None,
        }
    }
}

/// Resolve a `--workspace` CLI argument (default `.`) to an absolute path,
/// then confirm/locate the Loom repo root from there. Mirrors the
/// `find_repo_root()` call every Python entry point in this family makes
/// immediately after `argparse`.
pub fn resolve_repo_root(workspace: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(workspace);
    let start = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    find_repo_root(&start)
        .ok_or_else(|| anyhow::anyhow!("Not in a git repository with .loom directory"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finds_loom_dir_at_start() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        assert_eq!(find_repo_root(dir.path()), Some(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn finds_loom_dir_in_ancestor() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_repo_root(&nested), Some(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn does_not_match_a_directory_lacking_loom() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("x");
        std::fs::create_dir_all(&nested).unwrap();
        // `nested` itself has no `.loom/`, so it must never be returned
        // as-is (the walk must go at least one level up looking for it).
        assert_ne!(find_repo_root(&nested), Some(nested));
    }
}
