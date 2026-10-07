//! Differential test: `loom-daemon merge-pr usage` must print the bytes the
//! retired `merge-pr.sh` `show_help` heredoc printed (#8191 slice), and
//! `merge-pr.sh --help` must still print exactly those bytes through the new
//! call site.
//!
//! # Why a differential for text
//!
//! The help text carries the exit-code table role prompts and
//! `champion-pr-merge.md` branch on. `test-merge-pr-help.sh` pins a handful
//! of substrings and the exit code; it does not pin the whole text against a
//! frozen oracle, which is this file's job. There is no input alphabet to
//! sweep — the retired function took no arguments and read no state — so the
//! corpus is the single empty invocation, run once per side.
//!
//! # Rules from `defaults/docs/verification-recipes.md` §6 this obeys
//!
//! - **Compare against a FROZEN copy**, never the live script: the oracle is
//!   `tests/fixtures/merge-pr-usage-retired.sh`, whose header says what is
//!   verbatim (all of `show_help`) and what is not (nothing).
//! - **Exercise the refusal through its real caller**: the fallback tests run
//!   the real `merge-pr.sh --help` with an unusable `LOOM_DAEMON_BIN`, rather
//!   than asserting on a function in isolation.
//! - **Pin `LC_ALL=C`**, as the sibling differentials do.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use loom_daemon::merge_pr::usage::{render, USAGE, USAGE_SENTINEL};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/merge-pr-usage-retired.sh")
}

fn merge_pr_script() -> PathBuf {
    manifest().join("../defaults/scripts/merge-pr.sh")
}

/// The frozen `show_help`'s stdout.
fn frozen_show_help() -> String {
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg("set -euo pipefail; source \"$1\"; show_help")
        .arg("bash")
        .arg(fixture())
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "frozen show_help failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 help text")
}

/// `merge-pr.sh <args>` run from an empty temp dir (outside any git repo,
/// as `test-merge-pr-help.sh` also does). `self_bin` is `LOOM_DAEMON_SELF_BIN`
/// (unset when `None`); `LOOM_DAEMON_BIN` is always the REAL build, so a
/// pinned-but-unusable `self_bin` proves the pin is never swapped for it.
fn run_script(self_bin: Option<&str>, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new("bash");
    cmd.env("LC_ALL", "C")
        .env("LOOM_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
        .env_remove("LOOM_DAEMON_SELF_BIN");
    if let Some(bin) = self_bin {
        cmd.env("LOOM_DAEMON_SELF_BIN", bin);
    }
    cmd.current_dir(dir.path())
        .arg(merge_pr_script())
        .args(args)
        .output()
        .expect("run merge-pr.sh")
}

#[test]
fn port_matches_frozen_show_help_byte_for_byte() {
    assert_eq!(USAGE, frozen_show_help());
}

#[test]
fn verb_prints_sentinel_then_frozen_text() {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .env("LC_ALL", "C")
        .args(["merge-pr", "usage"])
        .output()
        .expect("run loom-daemon");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(stdout, render());
    assert_eq!(stdout, format!("{USAGE_SENTINEL}\n{}", frozen_show_help()));
}

#[test]
fn script_help_through_the_port_is_unchanged() {
    let want = frozen_show_help();
    // Through the LOOM_DAEMON_SELF_BIN seam, and through LOOM_DAEMON_BIN
    // alone when no self pin is set.
    for self_bin in [Some(env!("CARGO_BIN_EXE_loom-daemon")), None] {
        for flag in ["--help", "-h"] {
            let out = run_script(self_bin, &[flag, "999"]);
            assert_eq!(out.status.code(), Some(0), "{flag} exit code");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                want,
                "{flag} (self pin {self_bin:?}) stdout differs from the retired show_help"
            );
        }
    }
}

#[test]
fn script_help_falls_back_open_when_the_verb_cannot_answer() {
    // A stub that answers exit 0 with off-protocol output stands in for an
    // unrelated binary pinned at LOOM_DAEMON_SELF_BIN; its text must never be
    // printed as merge-pr.sh's help.
    let dir = tempfile::tempdir().expect("tempdir");
    let stub = dir.path().join("loom-daemon");
    std::fs::write(&stub, "#!/bin/sh\necho 'not the usage text'\n").expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // Older daemon (clap exit 2) is modelled by a binary that exits 2.
    let old = dir.path().join("old-daemon");
    std::fs::write(&old, "#!/bin/sh\necho 'error: unrecognized subcommand' >&2\nexit 2\n")
        .expect("write old");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let missing = dir.path().join("no-such-daemon");
    let not_exec = dir.path().join("not-exec");
    std::fs::write(&not_exec, "#!/bin/sh\nexit 0\n").expect("write not-exec");

    for bin in [&stub, &old, &missing, &not_exec] {
        let out = run_script(Some(bin.to_str().expect("utf-8 path")), &["--help"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{bin:?}: help must exit 0");
        assert!(
            stdout.starts_with("Usage: ./.loom/scripts/merge-pr.sh <pr-number>"),
            "{bin:?}: fallback usage missing: {stdout:?}"
        );
        assert!(!stdout.contains("not the usage text"), "{bin:?}: stub output leaked");
        assert!(!stdout.contains(USAGE_SENTINEL), "{bin:?}: sentinel leaked");
        assert!(
            stderr.contains("merge-pr usage"),
            "{bin:?}: no pointer to the verb on stderr: {stderr:?}"
        );
    }
}
