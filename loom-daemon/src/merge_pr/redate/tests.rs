//! Tests for the re-date remedy (#8508).
//!
//! The pure decision logic (`decide`, `commit_message`) is exercised directly.
//! [`redate_with`] additionally gets integration-style coverage against a stub
//! `gh` script — unlike `stale_checks::fetch` / `head_sync::fetch`, this
//! module WRITES to the forge (creates a commit, moves a ref), so getting its
//! call sequence and outcome mapping wrong is a real-commit-producing bug, not
//! just a stale-read bug; the extra coverage is worth the stub. Injected as a
//! plain function argument (`redate_with(gh_path, ...)`), never a
//! `LOOM_GH_BIN` env var — that would race across `cargo test`'s parallel
//! threads in one process.

use super::*;
use std::fs;
use std::io::Write;

// --- Pure logic ---------------------------------------------------------

#[test]
fn decide_proceeds_when_current_matches_expected() {
    assert_eq!(decide("abc123", "abc123"), PushDecision::Proceed);
}

#[test]
fn decide_refuses_when_the_branch_already_moved() {
    assert_eq!(
        decide("abc123", "def456"),
        PushDecision::HeadMoved {
            current: "def456".to_string()
        }
    );
}

#[test]
fn decide_is_case_sensitive_on_sha() {
    // A SHA compare must never be lossy: different case is a different
    // string, and there is no forge API where that arises legitimately, but
    // the gate must not silently treat it as a match either.
    assert_eq!(
        decide("ABC123", "abc123"),
        PushDecision::HeadMoved {
            current: "abc123".to_string()
        }
    );
}

#[test]
fn commit_message_names_the_pr_and_the_guard() {
    let msg = commit_message("8493");
    for needle in ["#8493", "#8248", "actions:write", "#8508"] {
        assert!(msg.contains(needle), "commit message should mention {needle:?}, got: {msg}");
    }
}

#[test]
fn commit_message_is_deterministic() {
    assert_eq!(commit_message("1"), commit_message("1"));
    assert_ne!(commit_message("1"), commit_message("2"));
}

#[test]
fn redate_outcome_head_moved_is_not_a_pushed_variant() {
    // Guards against a match-arm regression collapsing the two variants —
    // callers branch on this distinction (HeadMoved = re-queue silently,
    // Pushed = report + wait for CI), same shape as merge-pr.sh's #5579
    // exit-3 vs exit-1 split.
    let moved = RedateOutcome::HeadMoved {
        current: "abc".to_string(),
    };
    let pushed = RedateOutcome::Pushed {
        new_sha: "abc".to_string(),
    };
    assert_ne!(moved, pushed);
}

// --- Stubbed forge I/O ---------------------------------------------------

/// Writes an executable fake `gh` to `dir` that answers the four calls
/// [`redate_with`] makes, in order, from canned values — and appends every
/// invocation's argv (sentinel-terminated, see [`split_stub_calls`]) to
/// `dir/argv.log` for assertions. The ref-heads path is reused for BOTH the
/// read (no `-X`) and the write (`-X PATCH`); the case arms tell them apart
/// by scanning for `-X PATCH` anywhere in argv, mirroring this repo's other
/// stubs (`test-merge-pr-stale-required-checks.sh`).
fn write_stub_gh(
    dir: &std::path::Path,
    current_sha: &str,
    tree_sha: &str,
    new_sha: &str,
    fail_at: &str,
) -> std::path::PathBuf {
    let path = dir.join("gh");
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
shift # drop leading "api"
# One record per call, terminated by a sentinel line rather than a bare
# newline — the create-commit call's `message=` argument embeds real
# newlines, so a plain newline-per-record log would miscount calls.
{{ printf '%s\n<<<REDATE-STUB-CALL-END>>>\n' "$*"; }} >> "{dir}/argv.log"

# The path arg can be ANYWHERE in argv — real `gh api -X PATCH <path> ...`
# puts `-X PATCH` BEFORE it — so find it by shape rather than assuming $1.
PATH_ARG=""
IS_PATCH=0
prev=""
for arg in "$@"; do
  if [ "$prev" = "-X" ] && [ "$arg" = "PATCH" ]; then
    IS_PATCH=1
  fi
  case "$arg" in
    repos/*) PATH_ARG="$arg" ;;
  esac
  prev="$arg"
done

case "$PATH_ARG" in
  */git/refs/heads/*)
    if [ "$IS_PATCH" = 1 ]; then
      [ "{fail_at}" = "patch" ] && exit 1
      exit 0
    else
      [ "{fail_at}" = "ref" ] && exit 1
      echo "{current_sha}"
    fi
    ;;
  */git/commits/*)
    [ "{fail_at}" = "tree" ] && exit 1
    echo "{tree_sha}"
    ;;
  */git/commits)
    [ "{fail_at}" = "create" ] && exit 1
    echo "{new_sha}"
    ;;
  *)
    echo "stub gh: unexpected path '$PATH_ARG' in args: $*" >&2
    exit 2
    ;;
esac
"#,
        dir = dir.display(),
    );
    let mut f = fs::File::create(&path).expect("write stub gh");
    f.write_all(script.as_bytes())
        .expect("write stub gh contents");
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub gh");
    }
    path
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-redate-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Split `argv.log` into one entry per recorded call. NOT `.lines()`: the
/// create-commit call's `message=` argument embeds real newlines (the commit
/// message is multi-line), so the stub terminates each record with an
/// explicit sentinel instead of relying on one call == one line.
fn split_stub_calls(argv_log: &str) -> Vec<&str> {
    argv_log
        .split("<<<REDATE-STUB-CALL-END>>>\n")
        .map(str::trim_end)
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn redate_with_pushes_a_tree_identical_commit_when_head_matches() {
    let dir = tmp_dir("happy-path");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "");

    let outcome = redate_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42");
    assert_eq!(
        outcome,
        RedateOutcome::Pushed {
            new_sha: "newsha22".to_string()
        }
    );

    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let calls = split_stub_calls(&argv);
    assert_eq!(calls.len(), 4, "expected exactly 4 gh api calls, got: {argv}");
    assert!(
        calls[0].starts_with("repos/o/r/git/refs/heads/feature/x"),
        "call 1 reads the ref: {}",
        calls[0]
    );
    assert!(
        calls[1].starts_with("repos/o/r/git/commits/abc0000"),
        "call 2 reads the tree: {}",
        calls[1]
    );
    assert!(
        calls[2].starts_with("repos/o/r/git/commits "),
        "call 3 creates the commit: {}",
        calls[2]
    );
    assert!(
        calls[2].contains("parents[]=abc0000"),
        "new commit's parent is the current head: {}",
        calls[2]
    );
    assert!(calls[3].contains("-X PATCH"), "call 4 patches the ref: {}", calls[3]);
    assert!(
        calls[3].contains("sha=newsha22"),
        "ref is patched to the new commit: {}",
        calls[3]
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn redate_with_refuses_to_push_when_the_branch_already_moved() {
    let dir = tmp_dir("head-moved");
    // Stub reports "def9999" as the live ref; caller expected "abc0000".
    let gh = write_stub_gh(&dir, "def9999", "unused", "unused", "");

    let outcome = redate_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42");
    assert_eq!(
        outcome,
        RedateOutcome::HeadMoved {
            current: "def9999".to_string()
        }
    );

    // Only the ref read happened — no commit/tree/patch calls once the head
    // mismatch is detected.
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert_eq!(split_stub_calls(&argv).len(), 1, "must stop after the ref read: {argv}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn redate_with_fails_when_the_ref_update_errors() {
    let dir = tmp_dir("patch-fails");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "patch");

    let outcome = redate_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42");
    match outcome {
        RedateOutcome::Failed(msg) => {
            assert!(msg.contains("newsha22"), "failure should name the dangling commit: {msg}");
            assert!(
                msg.contains("dangling"),
                "failure should explain the commit is orphaned: {msg}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn redate_with_fails_when_the_commit_cannot_be_created() {
    let dir = tmp_dir("create-fails");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "create");

    let outcome = redate_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42");
    assert!(matches!(outcome, RedateOutcome::Failed(_)), "expected Failed, got {outcome:?}");

    let _ = fs::remove_dir_all(&dir);
}
