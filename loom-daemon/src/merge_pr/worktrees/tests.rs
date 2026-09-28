//! Unit tests for the porcelain parsers.
//!
//! The differential (`tests/merge_pr_worktrees_differential.rs`) proves these
//! agree with the retired `awk`. These name the individual *properties* the
//! three shipped defects were about, so a future reader can see what each one
//! was without reconstructing it from a corpus entry.

use super::*;

/// The normal shape: every stanza terminated by a blank record.
const THREE_STANZAS: &str = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/wt-a
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature/issue-42

worktree /repo/wt-b
HEAD 3333333333333333333333333333333333333333
branch refs/heads/other

";

/// The matching stanza is LAST with no terminating blank record — only the
/// `END` arm can catch it.
const LAST_NO_BLANK: &str = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/wt-a
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature/issue-42";

#[test]
fn primary_is_the_first_worktree_record() {
    assert_eq!(primary_path(THREE_STANZAS), Some("/repo/main"));
    assert_eq!(primary_path(LAST_NO_BLANK), Some("/repo/main"));
}

/// The caller MUST be able to tell this from "the target is not the primary".
#[test]
fn primary_is_none_when_there_is_no_porcelain() {
    assert_eq!(primary_path(""), None);
    assert_eq!(primary_path("\n"), None);
    assert_eq!(primary_path("fatal: not a git repository\n"), None);
}

#[test]
fn branch_for_path_strips_refs_heads() {
    assert_eq!(
        branch_for_path(THREE_STANZAS, "/repo/wt-a").as_deref(),
        Some("feature/issue-42")
    );
    assert_eq!(
        branch_for_path(LAST_NO_BLANK, "/repo/wt-a").as_deref(),
        Some("feature/issue-42")
    );
    assert_eq!(branch_for_path(THREE_STANZAS, "/repo/nope"), None);
}

#[test]
fn find_by_branch_returns_the_path() {
    assert_eq!(find_by_branch(THREE_STANZAS, "feature/issue-42"), Some("/repo/wt-a"));
    assert_eq!(find_by_branch(LAST_NO_BLANK, "feature/issue-42"), Some("/repo/wt-a"));
    assert_eq!(find_by_branch(THREE_STANZAS, "no/such/branch"), None);
}

/// #3671: the answer is ONE value. The `awk` this replaces printed the match
/// twice (blank-line rule, then `END` with its condition still true), and the
/// callers spliced the result straight into a shell string — so the operator
/// was shown a `/path\n/path` that exists nowhere. A single `Option` makes the
/// shape unrepresentable; this pins that it is also the RIGHT single value on
/// the fixture that used to double.
#[test]
fn a_mid_list_match_yields_exactly_one_answer() {
    let hit = find_by_branch(THREE_STANZAS, "feature/issue-42").expect("match");
    assert!(!hit.contains('\n'), "the answer must be one path: {hit:?}");
    assert_eq!(hit, "/repo/wt-a");

    let br = branch_for_path(THREE_STANZAS, "/repo/wt-a").expect("match");
    assert!(!br.contains('\n'), "the answer must be one branch: {br:?}");
}

/// #3717: `$2` truncated a path at its first space, so the primary-worktree
/// guard compared a prefix and did not fire.
#[test]
fn paths_containing_spaces_survive_intact() {
    let porcelain = "\
worktree /Users/x/My Repos/loom
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /Users/x/My Repos/loom/.loom/worktrees/issue-7
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature/issue-7

";
    assert_eq!(primary_path(porcelain), Some("/Users/x/My Repos/loom"));
    assert_eq!(
        find_by_branch(porcelain, "feature/issue-7"),
        Some("/Users/x/My Repos/loom/.loom/worktrees/issue-7")
    );
    assert_eq!(branch_for_path(porcelain, "/Users/x/My Repos/loom").as_deref(), Some("main"));
}

/// A detached or bare entry carries no `branch` record. `branch_for_path` must
/// report nothing rather than inheriting the previous stanza's branch — that
/// would name a branch for deletion that is checked out somewhere else.
#[test]
fn detached_and_bare_entries_have_no_branch() {
    let porcelain = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/detached
HEAD 2222222222222222222222222222222222222222
detached

worktree /repo/bare
bare

";
    assert_eq!(branch_for_path(porcelain, "/repo/detached"), None);
    assert_eq!(branch_for_path(porcelain, "/repo/bare"), None);
    assert_eq!(find_by_branch(porcelain, "main"), Some("/repo/main"));
}

/// `find_by_branch` compares fully-qualified refs, so a short name cannot
/// match a tag or a remote-tracking ref that shares its text.
#[test]
fn find_by_branch_matches_only_refs_heads() {
    let porcelain = "\
worktree /repo/wt
HEAD 1111111111111111111111111111111111111111
branch refs/remotes/origin/main

";
    assert_eq!(find_by_branch(porcelain, "main"), None);
}

/// `awk` with the default `RS` does not strip `\r`, and `\r` is not a
/// `<blank>` so field splitting does not drop it either. A port built on
/// [`str::lines`] / [`str::split_ascii_whitespace`] would strip it in both
/// places and then disagree with the shell about the branch name in exactly
/// the comparison that authorises `git branch -D`.
#[test]
fn a_trailing_cr_is_part_of_the_record_as_in_awk() {
    let porcelain = "worktree /repo/wt\r\nbranch refs/heads/main\r\n\r\n";
    assert_eq!(primary_path(porcelain), Some("/repo/wt\r"));
    // The ref keeps its `\r`, so the short name does NOT match.
    assert_eq!(find_by_branch(porcelain, "main"), None);
    assert_eq!(find_by_branch(porcelain, "main\r"), Some("/repo/wt\r"));
    assert_eq!(branch_for_path(porcelain, "/repo/wt\r").as_deref(), Some("main\r"));
}

/// An input whose records are all blank exercises the `("", "")` state the
/// `awk` port inherits from uninitialised scalars: `branch_for_path` must not
/// answer for the empty path.
#[test]
fn blank_input_does_not_match_the_empty_path() {
    assert_eq!(branch_for_path("\n\n\n", ""), None);
    assert_eq!(find_by_branch("\n\n\n", ""), None);
}
