//! CLI contract for `loom-daemon champion-hold classify` on a stacked PR
//! (#12761).
//!
//! A stacked child targets its parent's `feature/issue-<P>` branch. Champion
//! merges only on `state=clear`, so an approved child based on a Loom stack
//! branch must classify as something else, whether its parent is open, merged
//! with the branch kept, or closed unmerged, and even in a repo too small for
//! the merge sequencer's `StackedBase` edge (it runs only above two open PRs).
//! These tests drive the real binary against a mock `gh` on `LOOM_GH_BIN` that
//! answers the two REST reads: labels and the PR. The parent is never looked
//! up, and the mock fails any listing read so a regression would show.
//!
//! | case | stdout | exit |
//! |---|---|---|
//! | approved child of a Loom stack branch (parent open, landed or closed) | `state=stacked-on-loom-branch … detail=feature/issue-1` | 0 |
//! | approved PR on the default branch, `release/1.2` or `feature/issue-1-extra` | `state=clear` | 0 |
//! | approved PR, its pull object unreadable | (nothing) | 3 |

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A mock `gh` answering `api <path>` by path, from env vars. Every call is
/// appended to `$MOCK_LOG`, so a test can assert what was (not) read.
fn write_mock_gh(dir: &Path) -> PathBuf {
    let path = dir.join("gh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_LOG"
[ "$1" = api ] || exit 1
case "$2" in
  */issues/*/labels*) printf '%s' "$MOCK_LABELS" ;;
  */pulls\?*) exit 1 ;;
  */pulls/*)
    [ "${MOCK_PULL_RC:-0}" = 0 ] || exit "$MOCK_PULL_RC"
    printf '%s' "$MOCK_PULL" ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// The REST pull object of PR #2, based on `base`.
fn pull(base: &str) -> String {
    format!(
        r#"{{"number":2,"state":"open","head":{{"ref":"feature/issue-2"}},"base":{{"ref":"{base}","label":"acme:{base}","repo":{{"owner":{{"login":"acme"}}}}}}}}"#
    )
}

/// Run `champion-hold classify --pr 2 --repo acme/widgets`; returns the
/// output and the mock's call log.
fn run(env: &[(&str, &str)]) -> (Output, String) {
    let dir = tempfile::tempdir().unwrap();
    let gh = write_mock_gh(dir.path());
    let log = dir.path().join("gh.log");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args([
        "champion-hold",
        "classify",
        "--pr",
        "2",
        "--repo",
        "acme/widgets",
    ])
    .current_dir(dir.path())
    .env("LOOM_GH_BIN", &gh)
    .env("MOCK_LOG", &log)
    .env("MOCK_LABELS", r#"[{"name":"loom:pr"}]"#)
    .env("LOOM_FORGE_TYPE", "github")
    .env("LOOM_CONFIG_DEFAULTS_FILE", "")
    .env("LOOM_WORKSPACES_PATH", dir.path().join("workspaces.json"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    (out, std::fs::read_to_string(&log).unwrap_or_default())
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const STACKED: &str = "LOOM-CHAMPION-HOLD state=stacked-on-loom-branch pr=2 detail=feature/issue-1";

/// The REST reads the mock saw, besides the labels read.
fn pull_reads(log: &str) -> Vec<&str> {
    log.lines().filter(|l| l.contains("/pulls")).collect()
}

#[test]
fn an_approved_child_of_an_open_parent_is_not_merged() {
    let child = pull("feature/issue-1");
    let (out, log) = run(&[("MOCK_PULL", &child)]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout(&out), STACKED);
    // Champion's merge step requires `state=clear`; this is not it.
    assert!(!stdout(&out).contains("state=clear"));
    assert_eq!(
        pull_reads(&log),
        vec!["api repos/acme/widgets/pulls/2"],
        "one read of the PR, no parent lookup: {log}"
    );
}

#[test]
fn a_default_or_non_loom_base_is_clear() {
    for base in ["main", "release/1.2", "feature/issue-1-extra"] {
        let pr = pull(base);
        let (out, log) = run(&[("MOCK_PULL", &pr)]);
        assert_eq!(out.status.code(), Some(0), "{base}");
        assert_eq!(stdout(&out), "LOOM-CHAMPION-HOLD state=clear pr=2 detail=-", "{base}");
        assert!(!log.contains("pulls?"), "no parent lookup for {base}: {log}");
    }
}

#[test]
fn a_child_whose_parent_landed_is_not_clear() {
    // A merged parent whose branch survived: the base is unchanged, so the
    // child is skipped until the reconcile retargets it to the default branch.
    let child = pull("feature/issue-1");
    let (out, _) = run(&[("MOCK_PULL", &child)]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), STACKED);
}

#[test]
fn a_child_whose_parent_closed_unmerged_is_not_clear() {
    // The parent was closed and its branch kept (#12760 only labels the
    // orphan in a merge-mode repo): the child must not merge into it.
    let child = pull("feature/harness-ops-7");
    let (out, _) = run(&[("MOCK_PULL", &child)]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out),
        "LOOM-CHAMPION-HOLD state=stacked-on-loom-branch pr=2 detail=feature/harness-ops-7"
    );
}

#[test]
fn an_unreadable_base_fails_safe() {
    let (out, _) = run(&[("MOCK_PULL_RC", "1")]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "", "a failed read never prints a state");
    let (out, _) = run(&[("MOCK_PULL", "not json")]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "");
    let (out, _) = run(&[("MOCK_PULL", r#"{"number":2,"base":{"ref":""}}"#)]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "");
}

#[test]
fn a_held_pr_is_not_read_for_its_base() {
    // A non-clear class already keeps Champion from merging: no extra read.
    let (out, log) =
        run(&[("MOCK_LABELS", r#"[{"name":"loom:pr"},{"name":"loom:changes-requested"}]"#)]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out),
        "LOOM-CHAMPION-HOLD state=contradiction pr=2 detail=loom:changes-requested"
    );
    assert!(!log.contains("pulls"), "{log}");
}
