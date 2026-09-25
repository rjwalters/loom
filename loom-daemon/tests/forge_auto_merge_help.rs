//! `loom-daemon forge auto-merge --help` must carry its operator-only safety
//! caveat (#8427).
//!
//! #8410 removed the last Loom caller of this verb: an armed server-side merge
//! is gated only by the branch ruleset's REQUIRED checks and never re-reads the
//! `loom:pr` label or the non-required suites (PR #8220 merged over a
//! `loom:verdict-stale` revocation that way). #8427 kept the verb — as a CLI
//! compatibility surface for installed pre-#8410 `merge-pr.sh` copies and as a
//! deliberate human escape hatch — on the condition that its help text spells
//! the hazard out. This pins that condition so a doc-comment edit cannot
//! silently drop it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

fn help(flag: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["forge", "auto-merge", flag])
        .output()
        .expect("run loom-daemon forge auto-merge --help");
    assert!(out.status.success(), "{flag} exited {:?}", out.status);
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn short_help_flags_operator_only() {
    let h = help("-h");
    assert!(h.contains("OPERATOR-ONLY"), "short help lost the operator-only flag:\n{h}");
    assert!(h.contains("Not a Loom merge path"), "short help:\n{h}");
}

#[test]
fn long_help_carries_the_safety_caveat() {
    let h = help("--help");
    for needle in [
        "OPERATOR-ONLY",
        "SAFETY CAVEAT",
        "REQUIRED checks",
        "loom:pr",
        "non-required test suite",
        "merge-pr.sh --auto",
    ] {
        assert!(h.contains(needle), "--help lost `{needle}`:\n{h}");
    }
}
