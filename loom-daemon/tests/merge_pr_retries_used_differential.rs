//! Differential test: `loom-daemon merge-pr retries-used` against the retired
//! shell it replaced — the bash `=~ recheck\ \#([0-9]+)` derivation of the
//! telemetry `retries_used` field in `merge-pr.sh`'s mergeability gate.
//!
//! One once-generated corpus (verification-recipes §6) feeds both sides: the
//! real reason strings `mergeable_recheck::decide` produces, plus adversarial
//! texts (several occurrences, a bare `recheck #`, long digit runs, multi-line
//! text, the `[[:space:]]`-style line-vs-whole-string family).

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-retries-used-retired.sh")
}

fn run_shell(reason: &str, configured: &str) -> String {
    let out = Command::new("bash")
        .args([
            "-c",
            "source \"$1\"; _retired_retries_used \"$2\" \"$3\"",
            "driver",
        ])
        .arg(fixture_path())
        .args([reason, configured])
        .output()
        .expect("bash ran the frozen shell side");
    assert!(out.status.success(), "frozen shell failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_rust(reason: &str, configured: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "merge-pr",
            "retries-used",
            "--reason",
            reason,
            "--configured",
            configured,
        ])
        .output()
        .expect("loom-daemon spawned");
    assert!(out.status.success(), "retries-used exited {:?}", out.status.code());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let corpus = [
        "",
        "recheck #",
        "recheck #1",
        "recheck #2 (post-backoff, uncached) now reports mergeable=true",
        "cached mergeable=false was stale; recheck #3 (post-backoff, uncached) now reports mergeable=true",
        "forge reports mergeable=false after 3 recheck(s); base/head ref unavailable for local corroboration",
        "recheck #x recheck #7 recheck #9",
        "recheck # recheck #12",
        "recheck #007z",
        "recheck #99999999999999999999",
        "line one\nrecheck #4\nline three",
        "recheck #\n5",
        "recheck  #5",
        "Recheck #5",
        "xrecheck #6",
        "recheck #5 - and --reason-looking -text",
        "-n",
        "-e recheck #8",
        "recheck #\u{0663}",
        "recheck #4\u{0663}",
        "tab\trecheck #11\ttab",
    ];
    for reason in corpus {
        for configured in ["3", "0", "10", ""] {
            assert_eq!(
                run_shell(reason, configured),
                run_rust(reason, configured),
                "diverged: reason={reason:?} configured={configured:?}"
            );
        }
    }
}
