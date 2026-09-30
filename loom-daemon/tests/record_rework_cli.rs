//! CLI + wiring contract for `loom-daemon record-rework` (Issue #9444) — the
//! first **writer** of the rework-event marker protocol.
//!
//! `rework_events` shipped reader-first: `read_rework_events`, the
//! substantive/environmental table, the D1 rollup columns and SF5's rework
//! split all landed, and nothing anywhere appended to
//! `.loom/logs/sweep-rework-events.jsonl`. So the field was permanently
//! absent and every query over it answered "this fleet re-does no work" from a
//! fleet that does. Nothing failed; the number was just always zero. That is
//! the failure mode this suite exists to keep closed, which is why it checks
//! two different things:
//!
//! 1. **The command's own contract** — it writes a marker the reader accepts,
//!    and it never fails the operation it describes (always exit 0, whatever
//!    goes wrong), because its callers are merge and doctor paths where a
//!    telemetry refusal must not become an operational one. The one exception
//!    is deliberate: an unknown `--kind`/`--classification` is a clap argument
//!    error (exit 2), since a typo that widens a low-cardinality vocabulary
//!    poisons every downstream rollup silently.
//!
//! 2. **That `merge-pr.sh` actually calls it.** A writer nothing invokes is
//!    the same always-zero as no writer at all, and the wiring is two appended
//!    fragments on pre-existing lines (the script is frozen by the file-size
//!    ratchet), which is exactly the shape a later edit removes without
//!    noticing. These assertions read the committed script.
//!
//! No forge, network, Docker or credential: the marker file is a local append
//! under a temp directory.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Output;

const MERGE_PR: &str = include_str!("../../defaults/scripts/merge-pr.sh");

/// A directory that [`loom_daemon::repo_root::find_repo_root`] accepts as a
/// workspace root: `.git` plus `.loom`.
fn workspace(dir: &Path) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
}

fn marker_file(root: &Path) -> PathBuf {
    root.join(".loom")
        .join("logs")
        .join("sweep-rework-events.jsonl")
}

fn run_in(cwd: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("record-rework")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn markers(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(marker_file(root))
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        })
        .unwrap_or_default()
}

/// The happy path: one marker, classified by the table, under the root the
/// outcome journal reads.
#[test]
fn records_a_marker_the_outcome_journal_will_find() {
    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let out = run_in(
        dir.path(),
        &[
            "--kind",
            "merge_conflict",
            "--issue",
            "9444",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--reason",
            "git merge-tree reports conflicts",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["recorded"], 1, "{summary}");
    assert_eq!(summary["issue"], 9444);
    assert_eq!(summary["classification"], "environmental");

    let rows = markers(dir.path());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["kind"], "merge_conflict");
    assert_eq!(rows[0]["issue"], 9444);
    assert_eq!(rows[0]["classification"], "environmental");
    assert_eq!(rows[0]["reason"], "git merge-tree reports conflicts");
    chrono::DateTime::parse_from_rfc3339(rows[0]["at"].as_str().unwrap()).unwrap();
}

/// `--branch` is how the merge path names the issue: it has a PR branch in
/// hand, not an issue number.
#[test]
fn a_feature_branch_names_the_issue_and_anything_else_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let root = dir.path().to_str().unwrap();

    let out = run_in(
        dir.path(),
        &[
            "--kind",
            "rebase",
            "--branch",
            "feature/issue-9444",
            "--repo-root",
            root,
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(markers(dir.path())[0]["issue"], 9444);

    // A fork/ad-hoc branch: no issue is derivable, so nothing is recorded.
    // Guessing here would charge a rework to whatever number happened to be
    // in the string, which is worse than an absent event.
    let out = run_in(
        dir.path(),
        &[
            "--kind",
            "rebase",
            "--branch",
            "patch-1",
            "--repo-root",
            root,
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["recorded"], 0, "{summary}");
    assert!(summary["skipped"].is_string(), "{summary}");
    assert_eq!(markers(dir.path()).len(), 1, "no second marker was written");
}

/// Markers accumulate: one merge can sync a moved base more than once, and a
/// later sweep's marker must not replace an earlier one.
#[test]
fn markers_append_rather_than_replace() {
    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let root = dir.path().to_str().unwrap();
    for kind in ["rebase", "rebase", "merge_conflict"] {
        let out = run_in(dir.path(), &["--kind", kind, "--issue", "42", "--repo-root", root]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }
    let rows = markers(dir.path());
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[2]["kind"], "merge_conflict");
}

/// The root is discovered from the working directory when `--repo-root` is
/// absent, so a call from inside a worktree still lands in the shared file the
/// outcome journal samples.
#[test]
fn the_workspace_root_is_discovered_from_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let nested = dir.path().join("loom-daemon").join("src");
    std::fs::create_dir_all(&nested).unwrap();

    let out = run_in(&nested, &["--kind", "ci_rerun", "--issue", "7"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(markers(dir.path()).len(), 1, "the marker went to the repo root");
}

/// Outside any Loom repository there is no root to write under. That is a
/// skip, not a failure — the caller is mid-merge.
#[test]
fn no_resolvable_workspace_root_skips_and_still_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_in(dir.path(), &["--kind", "rebase", "--issue", "7"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["recorded"], 0, "{summary}");
    assert!(summary["skipped"].is_string(), "{summary}");
}

/// An unwritable log directory is the realistic I/O failure. Reported, never
/// fatal — this runs on the path that merges PRs.
#[cfg(unix)]
#[test]
fn an_unwritable_log_directory_reports_and_still_exits_zero() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let logs = dir.path().join(".loom").join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o500)).unwrap();

    let out = run_in(
        dir.path(),
        &[
            "--kind",
            "rebase",
            "--issue",
            "7",
            "--repo-root",
            dir.path().to_str().unwrap(),
        ],
    );
    // Restore before asserting so the temp dir can be cleaned up either way.
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["recorded"], 0, "{summary}");
    assert!(summary["error"].is_string(), "{summary}");
}

/// The vocabulary is closed at the CLI. A typo must not reach the marker file
/// — the reader would classify it by the catch-all and every rollup would
/// carry a one-off `kind` nobody buckets on.
#[test]
fn an_unknown_kind_or_classification_is_an_argument_error() {
    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let root = dir.path().to_str().unwrap();

    for args in [
        vec!["--kind", "rebased", "--issue", "7", "--repo-root", root],
        vec![
            "--kind",
            "rebase",
            "--classification",
            "env",
            "--issue",
            "7",
            "--repo-root",
            root,
        ],
    ] {
        let out = run_in(dir.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{args:?} -> {out:?}");
    }
    assert!(markers(dir.path()).is_empty(), "nothing was written");
}

/// `--classification` overrides the table for a caller that knows better.
#[test]
fn an_explicit_classification_overrides_the_table() {
    let dir = tempfile::tempdir().unwrap();
    workspace(dir.path());
    let out = run_in(
        dir.path(),
        &[
            "--kind",
            "merge_conflict",
            "--classification",
            "substantive",
            "--issue",
            "7",
            "--repo-root",
            dir.path().to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(markers(dir.path())[0]["classification"], "substantive");
}

// ---------------------------------------------------------------------------
// merge-pr.sh wiring. A writer nothing calls measures nothing.
// ---------------------------------------------------------------------------

/// Both emission sites exist, with the kinds they are supposed to carry.
#[test]
fn merge_pr_emits_a_rebase_marker_when_it_syncs_a_moved_base() {
    let site = MERGE_PR
        .lines()
        .find(|line| line.contains("record-rework") && line.contains("--kind rebase"))
        .expect("merge-pr.sh no longer marks the base-sync rebase (#9444)");
    assert!(
        site.contains("_refresh_precondition_sha"),
        "the rebase marker moved off the base-modified retry arm: {site}"
    );
    assert!(
        site.contains("--branch \"$PR_BRANCH\"") && site.contains("--repo-root \"$REPO_ROOT\""),
        "the marker must name the issue's branch and the shared workspace root: {site}"
    );
}

#[test]
fn merge_pr_emits_a_merge_conflict_marker_only_on_a_corroborated_conflict() {
    let site = MERGE_PR
        .lines()
        .find(|line| line.contains("record-rework") && line.contains("--kind merge_conflict"))
        .expect("merge-pr.sh no longer marks the corroborated conflict (#9444)");
    assert!(
        site.contains("refuse-conflict)"),
        "the conflict marker must stay on the CORROBORATED arm — the stale/unknown \
         arm is 'nobody could tell', not 'this branch conflicts': {site}"
    );
    // The `*` fallback arm must not gain one.
    assert_eq!(
        MERGE_PR.matches("--kind merge_conflict").count(),
        1,
        "exactly one conflict emission site is wired"
    );
}

/// Every call is isolated. A marker may never fail a merge, and a daemon
/// predating the verb must be indistinguishable from one that recorded
/// nothing.
#[test]
fn every_emission_site_is_failure_isolated_and_declared() {
    let sites: Vec<&str> = MERGE_PR
        .lines()
        .filter(|line| line.contains("record-rework") && !line.trim_start().starts_with('#'))
        .collect();
    assert_eq!(sites.len(), 2, "expected exactly the two wired sites: {sites:?}");
    for site in &sites {
        assert!(
            site.contains(">/dev/null 2>&1 || true"),
            "a rework marker must never fail the merge it describes: {site}"
        );
        assert!(
            site.contains("${LOOM_DAEMON_BIN:-loom-daemon}"),
            "the call must honour LOOM_DAEMON_BIN like every other daemon call \
             in this script: {site}"
        );
    }
    assert!(
        MERGE_PR.contains("# requires-daemon: record-rework optional"),
        "#8285: a new daemon dependency declares its floor, and this one \
         degrades rather than refusing"
    );
}
