//! Branch/worktree naming conventions.
//!
//! Rust port of `loom_tools.common.paths.NamingConventions` — the small pure
//! string-formatting helpers `clean`/`orphan_recovery` use to translate
//! between issue numbers, branch names (`feature/issue-<N>`), and worktree
//! directory names (`issue-<N>`).

/// Prefix for feature branches (`feature/issue-<N>`).
pub const BRANCH_PREFIX: &str = "feature/issue-";
/// Prefix for worktree directory names (`issue-<N>`).
pub const WORKTREE_PREFIX: &str = "issue-";

#[must_use]
pub fn branch_name(issue: u32) -> String {
    format!("{BRANCH_PREFIX}{issue}")
}

#[must_use]
pub fn worktree_name(issue: u32) -> String {
    format!("{WORKTREE_PREFIX}{issue}")
}

#[must_use]
pub fn issue_from_branch(branch: &str) -> Option<u32> {
    branch.strip_prefix(BRANCH_PREFIX)?.parse().ok()
}

#[must_use]
pub fn issue_from_worktree(worktree_name: &str) -> Option<u32> {
    worktree_name.strip_prefix(WORKTREE_PREFIX)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_branch_name() {
        assert_eq!(branch_name(42), "feature/issue-42");
        assert_eq!(issue_from_branch("feature/issue-42"), Some(42));
    }

    #[test]
    fn round_trips_worktree_name() {
        assert_eq!(worktree_name(42), "issue-42");
        assert_eq!(issue_from_worktree("issue-42"), Some(42));
    }

    #[test]
    fn rejects_non_matching_names() {
        assert_eq!(issue_from_branch("main"), None);
        assert_eq!(issue_from_branch("feature/issue-abc"), None);
        assert_eq!(issue_from_worktree("pr-42"), None);
        assert_eq!(issue_from_worktree("issue-"), None);
    }
}
