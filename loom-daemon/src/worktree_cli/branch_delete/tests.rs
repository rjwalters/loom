//! Unit tests for the squash-aware branch-delete rule.

use super::*;

/// `merge-pr.sh`'s `_maybe_delete_local_branch` body, or `None` outside a full
/// checkout.
fn shell_twin_body() -> Option<String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/merge-pr.sh");
    let sh = std::fs::read_to_string(&script).ok()?;
    let start = sh
        .find("_maybe_delete_local_branch() {")
        .expect("merge-pr.sh still defines _maybe_delete_local_branch");
    let body = &sh[start..];
    let end = body.find("\n}\n").expect("the function has a terminator");
    Some(body[..end].to_string())
}

// ---------------------------------------------------------------------------
// The anti-drift guarantee
// ---------------------------------------------------------------------------

/// Every operator-visible string this module emits must still exist verbatim
/// in `merge-pr.sh`'s `_maybe_delete_local_branch`.
///
/// This is the whole reason it was safe to stop `eval`-ing that function out
/// of the live script: the shell's contraption existed to avoid a second
/// implementation, and a second implementation is only acceptable while
/// something mechanical keeps the two aligned. Reword either side and this
/// test names the string that moved.
///
/// The check is on the invariant *fragments* — bash's `$branch` and Rust's
/// `{branch}` interpolate differently — which is also exactly what an operator
/// and every grep-based shell assertion actually see.
#[test]
fn messages_match_the_merge_pr_shell_twin() {
    let Some(body) = shell_twin_body() else {
        return;
    };
    for fragment in [
        "does not exist — skipping branch delete",
        "it is the repository's default branch",
        " — safe force-delete)",
        "Could not query the forge for a merged PR on",
        "and kept the conservative 'git branch -d'",
        "Could not determine whether",
        "keeping the conservative 'git branch -d'",
        "may have unpushed commits",
        "it is checked out (current HEAD or another worktree)",
        "it is checked out in the primary repository checkout",
        "To clean it up: git -C",
        "automatically switched to",
        "falling back to manual instructions",
        "checked out at|is currently checked out|used by worktree",
    ] {
        assert!(
            body.contains(fragment),
            "merge-pr.sh's _maybe_delete_local_branch no longer contains {fragment:?} — the Rust \
             port in branch_delete.rs has drifted from the rule it mirrors"
        );
    }
}

/// The escalation criterion, stated as a test so it cannot be relaxed by
/// accident: `-D` requires a `landed` verdict, and `-d` failing is NOT one.
#[test]
fn only_a_landed_verdict_may_escalate_to_force_delete() {
    if let Some(body) = shell_twin_body() {
        assert!(
            body.contains(r#"if [[ "$BRANCH_LANDED_VERDICT" == "landed" ]]; then"#),
            "the shell's escalation gate changed shape; re-derive the Rust arm"
        );
    }
    // And on the Rust side: exactly one place chooses `-D` for the first
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
