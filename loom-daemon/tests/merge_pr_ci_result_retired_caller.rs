//! The retired `merge-pr ci-result` verb (#12063 slice 1) against the caller
//! that still uses it: an installed `merge-pr.sh` from 0.19.763 up to
//! 0.19.1077, whose plain (non `--auto`) path had no other loom-side CI gate.
//!
//! The frozen `_check_ci_result` (`tests/fixtures/merge-pr-ci-result-caller-retired.sh`)
//! runs against the real binary with its exact argv
//! (`merge-pr ci-result --pr N --repo O/R --head-sha SHA`). The only addition
//! is the hidden `--from-stdin` fixture seam, inserted by a wrapper so the
//! required-check evidence comes from a document rather than the forge. A red
//! required context must refuse, a pending one must hold, and only a green
//! head may reach "the merge".

use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/merge-pr-ci-result-caller-retired.sh")
}

/// Run the frozen caller with `doc` as the required-check evidence:
/// `(exit code, stdout, stderr)`. Stdout ends with `MERGED` iff the gate let
/// the merge through.
fn run_old_caller(doc: &str) -> (i32, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let evidence = dir.path().join("evidence.json");
    std::fs::write(&evidence, doc).expect("write evidence");
    let wrapper = dir.path().join("loom-daemon");
    std::fs::write(
        &wrapper,
        "#!/usr/bin/env bash\nexec \"$REAL_LOOM_DAEMON\" \"$1\" \"$2\" --from-stdin \"${@:3}\" <\"$EVIDENCE\"\n",
    )
    .expect("write wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("chmod wrapper");
    }
    let script = r#"set -euo pipefail
info()    { echo "INFO: $*"; }
warning() { echo "WARN: $*"; }
error()   { echo "ERROR: $*" >&2; exit 1; }
_mp_daemon_roll_hint() { echo "(roll hint)"; }
source "$1"
FORGE_TYPE=github PR_NUMBER=7 REPO_NWO=o/r PR_HEAD_SHA=abc123 AUTO_MERGE=false DRY_RUN=false
LOOM_CI_RESULT_RETRY_DELAY=0
_check_ci_result
echo MERGED
"#;
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .env("LOOM_DAEMON_BIN", &wrapper)
        .env("REAL_LOOM_DAEMON", env!("CARGO_BIN_EXE_loom-daemon"))
        .env("EVIDENCE", &evidence)
        .arg("-c")
        .arg(script)
        .arg("bash")
        .arg(fixture())
        .output()
        .expect("run bash");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn doc(status: &str, conclusion: Option<&str>) -> String {
    let conclusion = conclusion.map_or("null".to_string(), |c| format!("\"{c}\""));
    format!(
        r#"{{"required": ["Rust Tests"], "check_runs": [{{"name": "Rust Tests", "status": "{status}", "conclusion": {conclusion}, "started_at": "2026-10-15T00:00:00Z"}}], "statuses": {{"total_count": 0, "statuses": []}}}}"#
    )
}

#[test]
fn old_caller_refuses_a_red_head() {
    let (code, stdout, stderr) = run_old_caller(&doc("completed", Some("failure")));
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stdout.contains("MERGED"), "merged over red: {stdout}");
    assert!(stderr.contains("Merge blocked: PR #7"), "{stderr}");
    assert!(stderr.contains("`Rust Tests` (failure)"), "{stderr}");
}

#[test]
fn old_caller_holds_a_pending_head() {
    let (code, stdout, stderr) = run_old_caller(&doc("in_progress", None));
    assert_eq!(code, 5, "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stdout.contains("MERGED"), "merged over pending: {stdout}");
}

#[test]
fn old_caller_merges_a_green_head() {
    let (code, stdout, stderr) = run_old_caller(&doc("completed", Some("success")));
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.ends_with("MERGED\n"), "{stdout}");
}
