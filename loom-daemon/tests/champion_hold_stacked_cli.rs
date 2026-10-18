//! CLI contract for `loom-daemon champion-hold classify` on a stacked PR
//! (#12761).
//!
//! A stacked child targets its parent's `feature/issue-<P>` branch. Champion
//! merges only on `state=clear`, so an approved child of a still-open parent
//! must classify as something else, even in a repo too small for the merge
//! sequencer's `StackedBase` edge (it runs only above two open PRs). These
//! tests drive the real binary against a mock `gh` on `LOOM_GH_BIN` that
//! answers the three REST reads: labels, the PR, and the open-PR listing for
//! its base.
//!
//! | case | stdout | exit |
//! |---|---|---|
//! | approved child, parent PR open (a two-PR repo) | `state=stacked-on-open-parent … detail=feature/issue-1` | 0 |
//! | approved PR on the default branch | `state=clear` | 0 |
//! | approved child, parent landed, branch survives | `state=clear` | 0 |
//! | approved child, parent listing unreadable | (nothing) | 3 |

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
  */pulls\?*)
    [ "${MOCK_LIST_RC:-0}" = 0 ] || exit "$MOCK_LIST_RC"
    printf '%s' "$MOCK_LIST" ;;
  */pulls/*) printf '%s' "$MOCK_PULL" ;;
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

/// The parent, PR #1, open on `feature/issue-1`.
const PARENT_OPEN: &str = r#"[{"number":1,"state":"open","head":{"ref":"feature/issue-1"}}]"#;

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

#[test]
fn an_approved_child_of_an_open_parent_is_not_merged() {
    let child = pull("feature/issue-1");
    let (out, log) = run(&[("MOCK_PULL", &child), ("MOCK_LIST", PARENT_OPEN)]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        stdout(&out),
        "LOOM-CHAMPION-HOLD state=stacked-on-open-parent pr=2 detail=feature/issue-1"
    );
    // Champion's merge step requires `state=clear`; this is not it.
    assert!(!stdout(&out).contains("state=clear"));
    assert!(
        log.contains(
            "api repos/acme/widgets/pulls?state=open&head=acme:feature/issue-1&per_page=100"
        ),
        "the parent lookup is filtered to the base's head: {log}"
    );
}

#[test]
fn a_default_branch_pr_is_clear_and_lists_nothing() {
    let main = pull("main");
    let (out, log) = run(&[("MOCK_PULL", &main), ("MOCK_LIST", PARENT_OPEN)]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "LOOM-CHAMPION-HOLD state=clear pr=2 detail=-");
    assert!(!log.contains("pulls?"), "no parent lookup for an unstacked base: {log}");
}

#[test]
fn a_child_whose_parent_landed_is_clear() {
    let child = pull("feature/issue-1");
    let (out, _) = run(&[("MOCK_PULL", &child), ("MOCK_LIST", "[]")]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "LOOM-CHAMPION-HOLD state=clear pr=2 detail=-");
}

#[test]
fn an_unreadable_parent_fails_safe() {
    let child = pull("feature/issue-1");
    let (out, _) = run(&[("MOCK_PULL", &child), ("MOCK_LIST_RC", "1")]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "", "a failed read never prints a state");
    let (out, _) = run(&[("MOCK_PULL", "not json")]);
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
