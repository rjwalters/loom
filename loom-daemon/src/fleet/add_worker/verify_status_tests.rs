//! #9064: the rendered verify script treats `loom-daemon status` exit 5
//! (autonomy mismatch) as reachable, retries other nonzero codes bounded,
//! and still runs the ranking + workspace-registration checks afterward.
//!
//! A sibling of `tests.rs`, which is over the file-size ratchet's threshold
//! (`.loom/docs/file-size-policy.md`).

use super::tests::{which_bash, write_executable};
use super::*;

/// Run the rendered verify script against a stub `loom-daemon` whose
/// `status` exit codes follow `codes` (last code repeats). Returns
/// (success, stdout+stderr, status-call count). `ranking`/`registered`
/// toggle the later checks (#9064).
fn run_verify_with_status_codes(
    codes: &[i32],
    ranking: bool,
    registered: bool,
) -> Option<(bool, String, u32)> {
    let bash = which_bash()?;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let local_bin = home.join(".local/bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    let codes_str = codes
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let ws = if registered {
        r#"{"workspaces":["anvil"]}"#
    } else {
        r#"{"workspaces":[]}"#
    };
    write_executable(
        &local_bin.join("loom-daemon"),
        &format!(
            r#"#!/bin/sh
if [ "$1" = "status" ]; then
  COUNTER_FILE="$STUB_STATE_DIR/status-calls"
  n=0
  [ -f "$COUNTER_FILE" ] && n="$(cat "$COUNTER_FILE")"
  n=$((n + 1))
  echo "$n" > "$COUNTER_FILE"
  i=0
  rc=0
  for c in {codes_str}; do
    i=$((i + 1))
    rc=$c
    [ "$i" -ge "$n" ] && break
  done
  exit "$rc"
fi
if [ "$1" = "workspace" ]; then
  echo '{ws}'
  exit 0
fi
exit 0
"#
        ),
    );
    // Fake sleep: no real delay.
    let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()])
        .replace("sleep 2", "true");
    std::fs::create_dir_all(home.join("loom-workspaces/anvil")).unwrap();
    if ranking {
        std::fs::create_dir_all(home.join(".loom/tokens")).unwrap();
        std::fs::write(home.join(".loom/tokens/.ranking"), "x").unwrap();
    }
    let out = Command::new(bash)
        .arg("-c")
        .arg(&script)
        .env("HOME", &home)
        .env("STUB_STATE_DIR", dir.path())
        .output()
        .unwrap();
    let calls = std::fs::read_to_string(dir.path().join("status-calls"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some((out.status.success(), text, calls))
}

#[test]
fn verify_accepts_status_exit_0_immediately() {
    let Some((ok, text, calls)) = run_verify_with_status_codes(&[0], true, true) else {
        return;
    };
    assert!(ok, "{text}");
    assert_eq!(calls, 1);
    assert!(!text.contains("autonomy mismatch"), "{text}");
}

#[test]
fn verify_accepts_status_exit_5_with_warning() {
    let Some((ok, text, calls)) = run_verify_with_status_codes(&[5], true, true) else {
        return;
    };
    assert!(ok, "exit 5 must count as reachable:\n{text}");
    assert_eq!(calls, 1);
    assert!(text.contains("autonomy mismatch"), "{text}");
}

#[test]
fn verify_recovers_from_transient_failure_then_exit_5() {
    let Some((ok, text, calls)) = run_verify_with_status_codes(&[1, 2, 5], true, true) else {
        return;
    };
    assert!(ok, "{text}");
    assert_eq!(calls, 3);
    assert!(text.contains("autonomy mismatch"), "{text}");
}

#[test]
fn verify_persistent_nonreachable_status_fails_bounded_with_last_exit_code() {
    let Some((ok, text, calls)) = run_verify_with_status_codes(&[1, 1, 7], true, true) else {
        return;
    };
    assert!(!ok, "{text}");
    assert_eq!(calls, 15, "retry must stay bounded at 15 attempts");
    assert!(text.contains("last status exit code: 7"), "{text}");
}

#[test]
fn verify_exit_5_does_not_bypass_missing_ranking() {
    let Some((ok, text, _)) = run_verify_with_status_codes(&[5], false, true) else {
        return;
    };
    assert!(!ok, "{text}");
    assert!(text.contains("no token ranking found"), "{text}");
}

#[test]
fn verify_exit_5_does_not_bypass_unregistered_workspace() {
    let Some((ok, text, _)) = run_verify_with_status_codes(&[5], true, false) else {
        return;
    };
    assert!(!ok, "{text}");
    assert!(text.contains("workspace anvil not registered"), "{text}");
}
