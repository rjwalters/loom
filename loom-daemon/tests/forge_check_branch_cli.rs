//! CLI contract for `loom-daemon forge check-branch <issue> [--branch NAME]
//! [--closed-pr-head]` (#9453 Phase 4, #10027).
//!
//! `sweep-lease-fence.sh` maps this command's exit codes onto its own abort
//! codes, so the codes are pinned here end to end through the real binary:
//!
//! | Exit | Meaning |
//! |---|---|
//! | `0` | branch exists — BRANCH_COLLISION |
//! | `1` | verified absent |
//! | `2` | unsafe `--branch` operand (refused before any `git` runs) |
//! | `6` | `--closed-pr-head` only: a closed-unmerged PR's preserved head, and the issue has no open linked PR |
//!
//! `git` is a fake on `LOOM_GIT_BIN`; the forge is a fake `gh` that is BOTH on
//! a minimal `PATH` (the closed-PR `pr list` probe resolves `gh` off `PATH`,
//! exactly like `worktree.sh`'s closed-unmerged arm) and on `LOOM_GH_BIN` (the
//! open-linked-PR union reads that seam).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write_exec(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// A fake `git` whose `ls-remote` logs its argv and reports `MOCK_LS_REMOTE`.
const FAKE_GIT: &str = r#"#!/bin/sh
case "$1" in
  ls-remote)
    printf '%s\n' "$@" > "$MOCK_DIR/ls-remote.args"
    [ -n "${MOCK_LS_REMOTE:-}" ] && printf '%s\n' "$MOCK_LS_REMOTE"
    exit 0
    ;;
  *) exit 1 ;;
esac
"#;

/// A fake `gh`: `pr list` (closed-PR probe), `repo view` + `api graphql` +
/// `api …/timeline` (the open-linked-PR union).
const FAKE_GH: &str = r#"#!/bin/sh
case "$1" in
  pr) printf '%s' "${MOCK_PR_LIST:-[]}" ;;
  repo) printf '%s\n' "rjwalters/loom" ;;
  api)
    case "$2" in
      graphql) printf '%s' "${MOCK_GRAPHQL_OUT:-}" ;;
      *) printf '%s' "${MOCK_TIMELINE_OUT:-}" ;;
    esac
    ;;
  *) exit 1 ;;
esac
"#;

fn graphql_nodes(nodes: &str) -> String {
    format!(
        "{{\"data\":{{\"repository\":{{\"issue\":{{\
         \"closedByPullRequestsReferences\":{{\"nodes\":[{nodes}]}}}}}}}}}}"
    )
}

struct Run {
    out: Output,
    ls_remote_args: String,
}

fn run(args: &[&str], env: &[(&str, &str)]) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let git = write_exec(&bin, "fake-git", FAKE_GIT);
    let gh = write_exec(&bin, "gh", FAKE_GH);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["forge", "check-branch"])
        .args(args)
        .current_dir(dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("MOCK_DIR", dir.path())
        .env("LOOM_GIT_BIN", &git)
        .env("LOOM_GH_BIN", &gh)
        .env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
        .env("LOOM_FORGE_TYPE", "github")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_WORKSPACES_PATH", dir.path().join("workspaces.json"))
        .env_remove("LOOM_BRANCH_LANDED_OFFLINE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let ls_remote_args =
        std::fs::read_to_string(dir.path().join("ls-remote.args")).unwrap_or_default();
    Run {
        out,
        ls_remote_args,
    }
}

const EXISTS: &str = "aef1c2d\trefs/heads/feature/issue-42";

#[test]
fn default_branch_is_feature_issue_n_and_absence_is_exit_one() {
    let r = run(&["42"], &[]);
    assert_eq!(r.out.status.code(), Some(1), "{:?}", r.out);
    assert!(r.ls_remote_args.contains("refs/heads/feature/issue-42"), "{}", r.ls_remote_args);
}

#[test]
fn branch_flag_probes_the_requested_branch() {
    let r = run(&["42", "--branch", "topic/mining-log"], &[]);
    assert_eq!(r.out.status.code(), Some(1));
    assert!(r.ls_remote_args.contains("refs/heads/topic/mining-log"), "{}", r.ls_remote_args);
    assert!(!r.ls_remote_args.contains("feature/issue-42"), "{}", r.ls_remote_args);
}

#[test]
fn an_unsafe_branch_operand_is_refused_with_exit_two_before_git_runs() {
    let r = run(&["42", "--branch=--upload-pack=/tmp/x"], &[]);
    assert_eq!(r.out.status.code(), Some(2));
    assert!(r.ls_remote_args.is_empty(), "git must not run: {}", r.ls_remote_args);
}

#[test]
fn an_existing_branch_without_the_flag_is_exit_zero() {
    let r = run(&["42"], &[("MOCK_LS_REMOTE", EXISTS)]);
    assert_eq!(r.out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&r.out.stderr).contains("BRANCH_COLLISION"));
}

#[test]
fn closed_pr_head_with_no_open_linked_pr_is_exit_six_with_the_pr_number() {
    let r = run(
        &["42", "--closed-pr-head"],
        &[
            ("MOCK_LS_REMOTE", EXISTS),
            (
                "MOCK_PR_LIST",
                r#"[{"number":721,"state":"CLOSED","mergedAt":null,"headRefOid":"aef1c2d","url":""}]"#,
            ),
            ("MOCK_GRAPHQL_OUT", &graphql_nodes("")),
            ("MOCK_TIMELINE_OUT", ""),
        ],
    );
    let stderr = String::from_utf8_lossy(&r.out.stderr);
    assert_eq!(r.out.status.code(), Some(6), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&r.out.stdout).trim(), "721");
}

/// The #9447 shape: a racing claimant's fresh push has no PR at all yet.
#[test]
fn a_fresh_push_with_no_pr_stays_a_collision_under_the_flag() {
    let r = run(
        &["42", "--closed-pr-head"],
        &[("MOCK_LS_REMOTE", EXISTS), ("MOCK_PR_LIST", "[]")],
    );
    assert_eq!(r.out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&r.out.stderr).contains("BRANCH_COLLISION"));
}

#[test]
fn closed_pr_head_but_an_open_linked_pr_stays_a_collision() {
    let r = run(
        &["42", "--closed-pr-head"],
        &[
            ("MOCK_LS_REMOTE", EXISTS),
            (
                "MOCK_PR_LIST",
                r#"[{"number":721,"state":"CLOSED","mergedAt":null,"headRefOid":"aef1c2d","url":""}]"#,
            ),
            ("MOCK_GRAPHQL_OUT", &graphql_nodes(r#"{"number":900,"state":"OPEN"}"#)),
        ],
    );
    let stderr = String::from_utf8_lossy(&r.out.stderr);
    assert_eq!(r.out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("open linked PR #900"), "stderr: {stderr}");
}
