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
/// Prefix for PR-checkout worktree directory names (`pr-<N>`, created by
/// `.loom/scripts/pr-worktree.sh` for a PR branch that doesn't fit the
/// `feature/issue-<N>` convention — external forks, ad-hoc branch names).
/// Unlike [`WORKTREE_PREFIX`], there is no matching branch-name prefix: a
/// `pr-<N>` worktree's checked-out branch is whatever `gh pr checkout`
/// produced, not a name Loom controls (#5939).
pub const PR_WORKTREE_PREFIX: &str = "pr-";

#[must_use]
pub fn branch_name(issue: u32) -> String {
    format!("{BRANCH_PREFIX}{issue}")
}

#[must_use]
pub fn worktree_name(issue: u32) -> String {
    format!("{WORKTREE_PREFIX}{issue}")
}

/// A non-empty run of ASCII digits and nothing else — `[0-9]+` under an
/// anchored ERE. Rejects a sign (`u32::parse` would accept a leading `+`),
/// an empty string, a trailing newline and non-ASCII decimal digits, any of
/// which would otherwise address a nonexistent issue.
#[must_use]
pub fn is_issue_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse a digits-only issue number ([`is_issue_digits`]); `None` on any other
/// shape or on `u32` overflow.
#[must_use]
pub fn parse_issue_digits(s: &str) -> Option<u32> {
    if is_issue_digits(s) {
        s.parse().ok()
    } else {
        None
    }
}

/// The digit text of a `feature/issue-<digits>` branch, verbatim (leading
/// zeros and over-`u32` runs intact), for callers that interpolate it as text.
#[must_use]
pub fn issue_digits_from_branch(branch: &str) -> Option<&str> {
    let digits = branch.strip_prefix(BRANCH_PREFIX)?;
    is_issue_digits(digits).then_some(digits)
}

#[must_use]
pub fn issue_from_branch(branch: &str) -> Option<u32> {
    parse_issue_digits(branch.strip_prefix(BRANCH_PREFIX)?)
}

#[must_use]
pub fn issue_from_worktree(worktree_name: &str) -> Option<u32> {
    worktree_name.strip_prefix(WORKTREE_PREFIX)?.parse().ok()
}

/// Parse a `pr-<N>` worktree directory name into its PR number, or `None` for
/// anything else (including `issue-<N>` — the two prefixes are disjoint by
/// construction, so a name can match at most one of
/// [`issue_from_worktree`]/[`pr_from_worktree`]).
#[must_use]
pub fn pr_from_worktree(worktree_name: &str) -> Option<u32> {
    worktree_name.strip_prefix(PR_WORKTREE_PREFIX)?.parse().ok()
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
    fn issue_from_branch_accepts_digits_only() {
        for (branch, want) in [
            ("feature/issue-5", Some(5)),
            ("feature/issue-007", Some(7)),
            ("feature/issue-+5", None),
            ("feature/issue--5", None),
            ("feature/issue-", None),
            ("feature/issue-12a", None),
            ("feature/issue-12\n", None),
            ("feature/issue-5-slug", None),
            ("feature/issue-99999999999", None),
            ("feature/issue-١٢", None),
            ("main", None),
        ] {
            assert_eq!(issue_from_branch(branch), want, "branch {branch:?}");
        }
    }

    #[test]
    fn issue_digits_from_branch_keeps_text_verbatim() {
        assert_eq!(issue_digits_from_branch("feature/issue-007"), Some("007"));
        assert_eq!(issue_digits_from_branch("feature/issue-+5"), None);
        assert_eq!(issue_digits_from_branch("feature/issue-"), None);
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

    #[test]
    fn parses_pr_worktree_names() {
        assert_eq!(pr_from_worktree("pr-42"), Some(42));
        assert_eq!(pr_from_worktree("pr-5312"), Some(5312));
    }

    #[test]
    fn pr_from_worktree_rejects_non_matching_names() {
        assert_eq!(pr_from_worktree("issue-42"), None, "issue-* is not pr-*");
        assert_eq!(pr_from_worktree("pr-abc"), None, "non-numeric suffix");
        assert_eq!(pr_from_worktree("pr-"), None, "empty suffix");
        assert_eq!(pr_from_worktree("scratch"), None, "no matching prefix");
    }
}
