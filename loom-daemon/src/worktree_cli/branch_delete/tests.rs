//! Unit tests for the squash-aware branch-delete rule.

use super::*;

/// `merge-pr.sh`'s text, or `None` outside a full checkout.
///
/// #8191 replaced `_maybe_delete_local_branch` with a thin call into `loom-daemon
/// merge-pr delete-branch`, and the orchestrator slice then moved that call
/// in-process (`merge-pr post-merge-cleanup` calls [`maybe_delete_local_branch`]
/// directly) — this module IS the implementation, so there is no second copy of
/// the rule's text to drift out of sync with. What the shell must still get
/// right is the WIRING: it holds no branch-delete logic of its own, per
/// `shell_delegates_rather_than_reimplementing` below.
fn shell_twin() -> Option<String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/merge-pr.sh");
    std::fs::read_to_string(&script).ok()
}

// ---------------------------------------------------------------------------
// The anti-drift guarantee
// ---------------------------------------------------------------------------

/// `merge-pr.sh` must reach the local-branch delete only through `loom-daemon
/// merge-pr post-merge-cleanup` — whose orchestrator calls THIS module — rather
/// than growing a second, independent implementation. Prior to #8191 this test
/// asserted every operator-visible message string existed verbatim in both
/// copies; now there is exactly one copy (this file), and what could drift
/// instead is the shell quietly regaining its own logic: a `git branch -d`/`-D`
/// or a `_maybe_delete_local_branch` function reappearing there would be
/// exactly that regression.
#[test]
fn shell_delegates_rather_than_reimplementing() {
    let Some(sh) = shell_twin() else {
        return;
    };
    assert!(
        sh.contains("merge-pr post-merge-cleanup"),
        "merge-pr.sh no longer calls 'loom-daemon merge-pr post-merge-cleanup' — has the \
         cleanup regrown its own copy of the branch_delete.rs rule?"
    );
    assert!(
        !sh.contains("_maybe_delete_local_branch() {"),
        "merge-pr.sh defines _maybe_delete_local_branch again — that is the pre-#8191 \
         inline implementation this module replaced"
    );
    for forbidden in [
        "git -C \"$REPO_ROOT\" branch -D",
        "branch -d \"$branch\"",
        "branch -D \"$branch\"",
    ] {
        assert!(
            !sh.contains(forbidden),
            "merge-pr.sh deletes a local branch itself ({forbidden}); that decision lives in \
             branch_delete.rs"
        );
    }
}

/// The escalation criterion, stated as a test so it cannot be relaxed by
/// accident: `-D` requires a `landed` verdict, and `-d` failing is NOT one.
#[test]
fn only_a_landed_verdict_may_escalate_to_force_delete() {
    // On the Rust side: exactly one place chooses `-D` for the first
    // attempt, and it is the `Verdict::Landed` arm.
    let me = include_str!("../branch_delete.rs");
    assert_eq!(
        me.matches("        (\n            \"-D\",").count() + me.matches("(\"-D\",").count(),
        1,
        "exactly one force-delete escalation may exist, gated on Verdict::Landed"
    );
    assert!(me.contains("if landed.verdict == Verdict::Landed {"));
}

// ---------------------------------------------------------------------------
// The "checked out somewhere" classifier
// ---------------------------------------------------------------------------

#[test]
fn checked_out_refusals_are_recognised_case_insensitively() {
    for msg in [
        "error: Cannot delete branch 'x' checked out at '/repo'",
        "error: branch 'x' is currently checked out",
        "fatal: 'x' is already used by worktree at '/repo/.loom/worktrees/issue-1'",
        "ERROR: CHECKED OUT AT '/repo'",
    ] {
        assert!(is_checked_out_refusal(msg), "must classify: {msg:?}");
    }
}

/// A genuine "not fully merged" refusal must NOT be mistaken for a
/// checked-out one: the two get opposite messages, and #4100 AC #4 is that an
/// operator can tell them apart.
#[test]
fn an_unmerged_refusal_is_not_a_checked_out_refusal() {
    for msg in [
        "error: The branch 'x' is not fully merged.",
        "error: branch 'x' not found.",
        "",
    ] {
        assert!(!is_checked_out_refusal(msg), "must not classify: {msg:?}");
    }
}

// ---------------------------------------------------------------------------
// Porcelain parsing
// ---------------------------------------------------------------------------

/// `git worktree list --porcelain` paths may contain spaces. Everything after
/// the literal `worktree ` prefix is the path — never field 2 of a split.
/// This is #7858's class in read-only form, and getting it wrong here would
/// mis-identify which worktree holds a branch immediately before a
/// `git branch -D`.
#[test]
fn worktree_entries_keeps_spaces_in_paths() {
    let entries = parse_worktree_porcelain(
        "worktree /tmp/my repo/wt one\nHEAD abc\nbranch refs/heads/feature/issue-1\n\n\
         worktree /tmp/other\nHEAD def\ndetached\n",
    );
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, PathBuf::from("/tmp/my repo/wt one"));
    assert_eq!(entries[0].1.as_deref(), Some("refs/heads/feature/issue-1"));
    assert_eq!(entries[1].0, PathBuf::from("/tmp/other"));
    assert_eq!(entries[1].1, None, "a detached worktree has no branch");
}

/// The primary checkout is the FIRST entry, whatever comes after it — the
/// property `_primary_worktree_path`'s `exit` after the first match encodes,
/// and what decides whether an operator gets the two-step primary-checkout
/// remediation or the generic one.
#[test]
fn the_first_porcelain_entry_is_the_primary_checkout() {
    let entries = parse_worktree_porcelain(
        "worktree /repo\nHEAD a\nbranch refs/heads/main\n\n\
         worktree /repo/.loom/worktrees/issue-9\nHEAD b\nbranch refs/heads/feature/issue-9\n",
    );
    assert_eq!(entries[0].0, PathBuf::from("/repo"));
}
