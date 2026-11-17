//! The one `git merge-tree --write-tree` availability gate (git >= 2.38).
//!
//! Shared by `overlap_replay::conflict`, `verdict_equivalence::git_objects`
//! and `worktree_cli::branch_landed`, which each used to carry a
//! byte-identical copy. A future floor change belongs here, once.

use std::path::Path;
use std::process::Command;

/// Whether a `git --version` number (`2.39.0`, `2.39.0.windows.1`) is at or
/// above the 2.38 floor where `git merge-tree --write-tree` exists. An
/// unparseable version is not a free pass.
pub(crate) fn merge_tree_in_version(version: &str) -> bool {
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

/// The version token from `git --version` output (`git version 2.43.0`).
pub(crate) fn parse_git_version(output: &str) -> Option<&str> {
    output.split_whitespace().nth(2)
}

/// Probe `git --version` (in `repo` when given) and apply the floor.
pub(crate) fn supports_merge_tree(repo: Option<&Path>) -> bool {
    let mut cmd = Command::new("git");
    if let Some(repo) = repo {
        cmd.arg("-C").arg(repo);
    }
    let Ok(out) = cmd.arg("--version").output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_git_version(&stdout).is_some_and(merge_tree_in_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_tree_version_floor_is_2_38() {
        assert!(merge_tree_in_version("2.38.0"));
        assert!(merge_tree_in_version("2.45.1"));
        assert!(merge_tree_in_version("2.39.0.windows.1"));
        assert!(merge_tree_in_version("3.0.0"));
        assert!(!merge_tree_in_version("2.30.0"));
        assert!(!merge_tree_in_version("2.37.9"));
        assert!(!merge_tree_in_version("1.9"));
        assert!(!merge_tree_in_version("not-a-version"));
        assert!(!merge_tree_in_version(""));
    }

    #[test]
    fn version_token_is_the_third_field() {
        assert_eq!(parse_git_version("git version 2.43.0\n"), Some("2.43.0"));
        assert_eq!(parse_git_version(""), None);
    }
}
