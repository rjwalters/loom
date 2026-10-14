//! The rebase-then-push paths of Issue #9282 (seven since #10771) all ask
//! `forge rebase-guard` between their version gate and their push.
//!
//! Structural, on the shipped text: a path that loses its call would otherwise
//! be invisible, because the guard's absence fails nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

fn shipped(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Is this line a force-push that runs, as opposed to prose about one?
fn is_push(line: &str) -> bool {
    let t = line.trim_start();
    // The prompts push `--force-with-lease[=pin]`; the scripts push a pinned
    // lease held in a variable (#9487): `git push "$child_lease_arg" ...`.
    (t.contains("push --force-with-lease")
        || t.contains("push \"$child_lease_arg\"")
        || t.contains("push \"$PUSH_LEASE_ARG\""))
        && !t.starts_with('#')
        && !["echo ", "printf ", "err ", "info ", "warn "]
            .iter()
            .any(|p| t.contains(p))
}

/// Walk `text`: after each version-gate invocation, the next force-push must
/// have a `forge rebase-guard --base` call before it. Returns how many
/// gate-then-push paths were found, all of them guarded.
fn guarded_paths(rel: &str, text: &str) -> usize {
    let mut paths = 0;
    let mut awaiting_push = false;
    let mut guarded = false;
    for (n, line) in text.lines().enumerate() {
        if line.contains("-x ") && line.contains("version-check-gate.sh") {
            awaiting_push = true;
            guarded = false;
        } else if awaiting_push && line.contains("forge rebase-guard --base") {
            guarded = true;
        } else if awaiting_push && is_push(line) {
            assert!(guarded, "{rel}:{}: force-push with no rebase-guard call before it", n + 1);
            paths += 1;
            awaiting_push = false;
        }
    }
    paths
}

#[test]
fn all_paths_call_the_guard_before_the_force_push() {
    for (rel, expected) in [
        ("defaults/.claude/commands/loom/judge.md", 3),
        // 2, not 3: the unlabeled-queue block no longer pushes itself; it defers to
        // "PR Has Merge Conflicts" (#10771), so it has no gate-then-push path of its own.
        ("defaults/.claude/commands/loom/doctor.md", 2),
        ("defaults/scripts/rebase-stacked-children.sh", 1),
        ("defaults/scripts/reconcile-stack.sh", 1),
    ] {
        let found = guarded_paths(rel, &shipped(rel));
        assert_eq!(found, expected, "{rel}: guarded rebase-then-push paths");
    }
}

#[test]
fn judge_no_longer_carries_its_own_empty_diff_predicate() {
    let judge = shipped("defaults/.claude/commands/loom/judge.md");
    assert!(!judge.contains("PRE_REBASE_AHEAD"));
    assert!(!judge.contains("git diff --quiet origin/main HEAD"));
}

/// An unanswered guard must be loud at every prompt call site: a daemon that
/// predates the verb prints only clap's usage error.
#[test]
fn every_prompt_call_site_warns_when_the_guard_gives_no_answer() {
    for (rel, expected) in [
        ("defaults/.claude/commands/loom/judge.md", 3),
        ("defaults/.claude/commands/loom/doctor.md", 2),
    ] {
        let text = shipped(rel);
        let calls = text
            .matches("RG=0; loom-daemon forge rebase-guard --base origin/main || RG=$?")
            .count();
        let warnings = text
            .matches("WARNING: rebase-guard gave no answer (rc=$RG)")
            .count();
        assert_eq!(calls, expected, "{rel}: guard calls that capture the exit code");
        assert_eq!(warnings, expected, "{rel}: unanswered warnings");
    }
}

#[test]
fn the_walker_rejects_an_unguarded_push() {
    let unguarded =
        "if [ -x ./version-check-gate.sh ]; then exit 1; fi\ngit push --force-with-lease\n";
    assert!(std::panic::catch_unwind(|| guarded_paths("x", unguarded)).is_err());
    let prose =
        "if [ -x ./version-check-gate.sh ]; then exit 1; fi\n# git push --force-with-lease\n";
    assert_eq!(guarded_paths("x", prose), 0);
}
