//! Unit tests for the pure parts of the reconciliation plan (#8583).
//!
//! The git-fixture regressions — stale local default branch, the silent
//! independent-child-file case, a child that edits parent-introduced content —
//! live in `tests/reconcile_stack_stale_default.rs`, because they need real
//! repositories rather than strings.

use super::*;

#[test]
fn prerequisite_tokens_are_distinct_and_greppable() {
    let all = [
        Prerequisite::Fetch,
        Prerequisite::RemoteTarget,
        Prerequisite::ParentRef,
        Prerequisite::ParentAncestry,
        Prerequisite::DirtyWorktree,
        Prerequisite::ChildBranch,
    ];
    let mut seen = std::collections::BTreeSet::new();
    for p in all {
        assert!(seen.insert(p.token()), "duplicate token: {}", p.token());
        assert!(!p.token().is_empty());
        assert_eq!(p.token(), p.token().to_uppercase());
    }
}

#[test]
fn plan_error_display_names_the_failing_prerequisite() {
    let e = PlanError {
        prerequisite: Prerequisite::Fetch,
        message: "network down".into(),
    };
    assert_eq!(e.to_string(), "[FETCH] network down");
}

#[test]
fn sh_quote_survives_a_quote_in_a_branch_name() {
    assert_eq!(sh_quote("plain"), "'plain'");
    // Not a realistic branch name, but the output is `eval`ed: a value that
    // escapes its quoting is arbitrary shell execution, not a cosmetic bug.
    assert_eq!(sh_quote("it's"), r#"'it'\''s'"#);
}

/// The shell contract: the destination is the pinned COMMIT, and the branch
/// name is not in the mutation-facing fields at all. A regression that let a
/// branch name back into `LOOM_RS_TARGET_COMMIT` would reintroduce #8583
/// without failing any git-level test that happens to have a fresh local
/// checkout.
#[test]
fn render_shell_emits_the_pinned_commit_not_a_branch_name() {
    let plan = Plan {
        default_branch: "main".into(),
        target_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        target_ref: "refs/remotes/origin/main".into(),
        git_dir: std::path::PathBuf::from("/tmp/wt"),
        child_worktree: Some(std::path::PathBuf::from("/tmp/wt")),
        child_branch: "feature/issue-2".into(),
        parent_ref: "refs/loom/parent/feature/issue-1".into(),
        parent_pin_ref: Some("refs/loom/parent/feature/issue-1".into()),
        notices: Vec::new(),
    };
    let rendered = render_shell(&plan);
    assert!(rendered.contains("LOOM_RS_TARGET_COMMIT='0123456789abcdef0123456789abcdef01234567'\n"));
    assert!(rendered.contains("LOOM_RS_GIT_DIR='/tmp/wt'\n"));
    assert!(rendered.contains("LOOM_RS_PARENT_REF='refs/loom/parent/feature/issue-1'\n"));
    assert!(rendered.contains("LOOM_RS_PARENT_PIN_REF='refs/loom/parent/feature/issue-1'\n"));
    for line in rendered.lines() {
        assert!(
            line != "LOOM_RS_TARGET_COMMIT='main'",
            "the destination must never be a branch name"
        );
    }
}

#[test]
fn render_shell_leaves_absent_optionals_empty_rather_than_unset() {
    let plan = Plan {
        default_branch: "trunk".into(),
        target_commit: "deadbeef".into(),
        target_ref: "refs/remotes/origin/trunk".into(),
        git_dir: std::path::PathBuf::from("/repo"),
        child_worktree: None,
        child_branch: "feature/issue-2".into(),
        parent_ref: "feature/issue-1".into(),
        parent_pin_ref: None,
        notices: Vec::new(),
    };
    let rendered = render_shell(&plan);
    // Empty, not missing: the script `eval`s this and then tests the
    // variables, so an unset variable under `set -u` would abort the run.
    assert!(rendered.contains("LOOM_RS_CHILD_WORKTREE=''\n"));
    assert!(rendered.contains("LOOM_RS_PARENT_PIN_REF=''\n"));
    assert!(rendered.contains("LOOM_RS_GIT_DIR='/repo'\n"));
}
