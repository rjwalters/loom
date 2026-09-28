//! Differential test: the Rust port of `merge-pr.sh`'s failing-check overlap
//! classification must agree with the shell, byte for byte, on a shared
//! corpus (`defaults/docs/verification-recipes.md` §6 — the #8191 slice that
//! moved the `comm -12` block inside `_wait_for_checks_then_sync_merge` into
//! `loom_daemon::merge_pr::checks_failure`).
//!
//! # The corpus is fed to both sides
//!
//! The shell side runs the REAL frozen block, sourced from
//! `tests/fixtures/merge-pr-checks-failure-retired.sh`, with `error`/`info`
//! stubs that report which way it went (and `error`'s full message). The Rust
//! side runs the REAL `loom-daemon merge-pr checks-failure` binary with the
//! same NUL-framed stdin record the live `merge-pr.sh` sends, and renders its
//! sentinel into the same message the live shell's `error` call builds —
//! so the frame parsing, the kernel, and the sentinel protocol are all under
//! test, not just [`loom_daemon::merge_pr::checks_failure::classify`].
//!
//! # Collation
//!
//! Both sides run under `LC_ALL=C`: `sort -u`'s collation only changes the
//! ORDER of names inside the refusal message (never which verdict fires —
//! `comm` reads both inputs under the same locale), and under `C` that order
//! is byte order, which is what the port's `BTreeSet<&str>` produces.
//!
//! # Coverage floor
//!
//! The test fails unless all three verdicts were reached, and a refusal naming
//! at least two required checks was seen (the `tr '\n' ' '` join).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-checks-failure-retired.sh")
}

const PR: &str = "4242";

/// Run the frozen block: `REQUIRED\t<message>`, `PROCEED`, or `PENDING`.
fn run_frozen_shell(failing: &str, required: &str, pending: &str) -> String {
    let driver = r#"set -euo pipefail
error() { printf 'REQUIRED\t%s\n' "$1"; exit 1; }
info() { echo "PROCEED"; }
source "$1"
_retired_classify_failing_checks "$2" "$3" "$4" "$5"
"#;
    let out = Command::new("bash")
        .args(["-c", driver, "driver"])
        .arg(fixture_path())
        .args([failing, required, pending, PR])
        .env("LC_ALL", "C")
        .output()
        .expect("bash ran the frozen shell side");
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    // `error` exits 1; every other path returns 0.
    let want_rc = i32::from(stdout.starts_with("REQUIRED"));
    assert_eq!(
        out.status.code(),
        Some(want_rc),
        "frozen shell side: unexpected exit for {stdout:?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// Run the ported subcommand exactly as the live `merge-pr.sh` does and map
/// its sentinel onto the retired block's observable outcome.
fn run_rust_cli(failing: &str, required: &str, pending: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["merge-pr", "checks-failure", "--pr", PR])
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
    let rc = out.status.code();
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    match (rc, stdout.as_str()) {
        (Some(1), s) if s.starts_with("LOOM-CHECK-FAILURE-REQUIRED\t") => {
            let names = &s["LOOM-CHECK-FAILURE-REQUIRED\t".len()..];
            // The live merge-pr.sh `error` line, byte for byte.
            format!(
                "REQUIRED\tCannot merge PR #{PR}: a required status check has failed ({names}). Fix the check and re-run the merge."
            )
        }
        (Some(0), "LOOM-CHECK-FAILURE-PROCEED") => "PROCEED".to_string(),
        (Some(0), "LOOM-CHECK-FAILURE-PENDING") => "PENDING".to_string(),
        other => panic!(
            "unrecognized checks-failure result {other:?}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    // (failing, required, pending) — newline-separated lists exactly as the
    // live shell's jq/`forge_get_required_status_check_contexts` produce them.
    let cases: &[(&str, &str, &str)] = &[
        // informational only, nothing pending → proceed
        ("Lint", "CI", ""),
        ("Lint\nFormat", "CI\nBuild", ""),
        ("Lint", "", ""),
        // informational only, something pending → keep waiting
        ("Lint", "CI", "CI"),
        ("Lint", "", "Deploy\nDocs"),
        // a whitespace-only pending name still means "not empty" to `-z`
        ("Lint", "CI", " "),
        // required failure → refuse, regardless of pending
        ("CI", "CI", ""),
        ("CI", "CI", "Other"),
        ("CI\nLint", "CI", ""),
        // multi-name overlap: sorted, de-duplicated, space-joined
        ("Zeta\nAlpha\nLint", "Alpha\nZeta", ""),
        ("b\nB\na\nA", "a\nA\nB\nb\nC", "x"),
        // exact line match only — no substring, no case folding, no trimming
        ("CI-lint", "CI", ""),
        ("ci", "CI", ""),
        (" CI", "CI", ""),
        ("CI ", "CI", "p"),
        // names with spaces / punctuation (workflow job names)
        ("build (ubuntu-latest, stable)", "build (ubuntu-latest, stable)", ""),
        ("build (macos)", "build (ubuntu-latest, stable)", ""),
        // a blank line in either list never manufactures an overlap
        ("\nLint", "\nCI", ""),
        ("Lint\n", "CI\n\n", "\n"),
    ];

    let mut reached = [false; 3];
    let mut saw_multi_name_refusal = false;
    for (failing, required, pending) in cases {
        let shell_out = run_frozen_shell(failing, required, pending);
        let rust_out = run_rust_cli(failing, required, pending);
        assert_eq!(
            shell_out, rust_out,
            "divergence on failing={failing:?} required={required:?} pending={pending:?}"
        );
        match rust_out.as_str() {
            s if s.starts_with("REQUIRED") => {
                reached[0] = true;
                if s.contains("(Alpha Zeta)") {
                    saw_multi_name_refusal = true;
                }
            }
            "PROCEED" => reached[1] = true,
            "PENDING" => reached[2] = true,
            _ => unreachable!(),
        }
    }
    assert!(
        reached.iter().all(|r| *r),
        "corpus did not reach every verdict (REQUIRED, PROCEED, PENDING): {reached:?}"
    );
    assert!(saw_multi_name_refusal, "corpus never exercised a multi-name refusal message");
}
