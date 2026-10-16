//! Regression test (#12137): the `--auto` wait in `merge-pr.sh` polls the
//! check state repeatedly, and a check can go pending -> failed between two
//! polls. The rule, shared with Champion criterion 6, is **required-only**:
//! a failing *non-required* check never blocks the merge (it is named in the
//! log line instead), a failing *required* check always refuses — including
//! one that was pending on the previous poll.
//!
//! `merge-pr.sh` keeps no memory of earlier polls, so each poll is one
//! independent `loom-daemon merge-pr checks-failure` call; this drives the
//! real binary through the two-poll sequences.

use std::io::Write;
use std::process::{Command, Stdio};

/// One poll: the verdict line and exit code for (failing, required, pending).
fn poll(failing: &str, required: &str, pending: &str) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["merge-pr", "checks-failure", "--pr", "4242"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("loom-daemon spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{failing}\0{required}\0{pending}\0").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

#[test]
fn non_required_check_pending_then_failed_proceeds() {
    // Poll 1: the non-required check is still running (and another failed
    // non-required one is already red) -> keep waiting.
    let (rc, verdict) = poll("Lint", "CI", "Docs");
    assert_eq!((rc, verdict.as_str()), (Some(0), "LOOM-CHECK-FAILURE-PENDING"));
    // Poll 2: it finished red, nothing pending -> proceed over it.
    let (rc, verdict) = poll("Lint\nDocs", "CI", "");
    assert_eq!((rc, verdict.as_str()), (Some(0), "LOOM-CHECK-FAILURE-PROCEED"));
}

#[test]
fn required_check_pending_then_failed_is_refused() {
    // Poll 1: the required check is pending, nothing failing -> not this
    // classifier's concern yet; with a non-required failure it keeps waiting.
    let (rc, verdict) = poll("Lint", "CI", "CI");
    assert_eq!((rc, verdict.as_str()), (Some(0), "LOOM-CHECK-FAILURE-PENDING"));
    // Poll 2: the required check went red -> refuse, naming it.
    let (rc, verdict) = poll("CI\nLint", "CI", "");
    assert_eq!(rc, Some(1));
    assert_eq!(verdict, "LOOM-CHECK-FAILURE-REQUIRED\tCI");
}
