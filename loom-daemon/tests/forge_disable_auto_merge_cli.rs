//! `loom-daemon forge disable-auto-merge` must exist, be discoverable from
//! `forge --help`, and carry its "why" (#8900).
//!
//! The verb is the disarm half of the pair whose *arm* half (`forge
//! auto-merge`) is operator-only. Two properties are pinned here because both
//! are load-bearing and a doc-comment edit could silently drop either:
//!
//! 1. It is **not** marked operator-only. Disarming can only turn a queued
//!    merge off, so gating it would be actively harmful: the verdict-invalidation
//!    paths that need it (`verdict-staleness-guard.sh --clear`,
//!    `claim_reconciliation`'s `invalidate_verdict()`) are automated.
//! 2. Its help explains the hazard it closes — an armed auto-merge is gated only
//!    by the ruleset's REQUIRED checks and merges the new, unreviewed head
//!    regardless of the label flip (#8694 / #8847 / #8843, 2026-09-25).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(args)
        .output()
        .expect("run loom-daemon");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn the_verb_is_listed_under_forge_help() {
    let (ok, h) = run(&["forge", "--help"]);
    assert!(ok, "`forge --help` failed");
    assert!(
        h.contains("disable-auto-merge"),
        "`forge --help` does not list disable-auto-merge:\n{h}"
    );
}

#[test]
fn long_help_explains_why_and_the_exit_codes() {
    let (ok, h) = run(&["forge", "disable-auto-merge", "--help"]);
    assert!(ok, "`disable-auto-merge --help` failed");
    for needle in [
        // The hazard, so a future reader cannot mistake this for a convenience
        // wrapper and "simplify" it away.
        "REQUIRED checks",
        "unreviewed",
        "#8900",
        // The exit-code contract callers key on.
        "DISARMED=1",
        "DISARMED=0",
    ] {
        assert!(h.contains(needle), "--help lost `{needle}`:\n{h}");
    }
}

/// `verdict-staleness-guard.sh --clear` invokes exactly
/// `forge disable-auto-merge <pr> --audit-comment --hold "$HOLD_LABEL"`, so both
/// flags must exist and be documented. The guard delegating instead of mirroring
/// the mutation inline is the PR #8990 review outcome — dropping either flag
/// silently breaks that shell call site, which no Rust test would otherwise
/// notice.
#[test]
fn the_shell_guards_flags_exist_and_are_documented() {
    let (ok, h) = run(&["forge", "disable-auto-merge", "--help"]);
    assert!(ok, "`disable-auto-merge --help` failed");
    for needle in ["--audit-comment", "--hold <LABEL>"] {
        assert!(h.contains(needle), "--help lost `{needle}`:\n{h}");
    }
    // The flags must be accepted together on the real parser, not merely listed.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "forge",
            "disable-auto-merge",
            "1",
            "--audit-comment",
            "--hold",
            "",
        ])
        .env("PATH", "/nonexistent-so-gh-cannot-be-found")
        .output()
        .expect("run loom-daemon");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unexpected argument") && !stderr.contains("unrecognized"),
        "the guard's exact invocation must parse:\n{stderr}"
    );
}

/// The inverse of `forge auto-merge`'s own pinned property (#8427): the ARM is
/// operator-only, the DISARM must NOT be — gating a safety-increasing verb
/// would keep the automated verdict-invalidation paths from calling it.
#[test]
fn the_disarm_is_not_marked_operator_only() {
    let (_, disable) = run(&["forge", "disable-auto-merge", "--help"]);
    assert!(
        !disable.contains("OPERATOR-ONLY"),
        "the disarm must not be operator-only — it can only turn a queued merge OFF:\n{disable}"
    );
    // Sanity: the arm still is, so this test is a real distinction rather than
    // an assertion that nothing anywhere says OPERATOR-ONLY.
    let (_, arm) = run(&["forge", "auto-merge", "--help"]);
    assert!(
        arm.contains("OPERATOR-ONLY"),
        "`forge auto-merge` must stay operator-only (#8427):\n{arm}"
    );
}
