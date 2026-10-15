//! `fetch_job_log` against stub `gh` binaries (#9057): a current `gh` that
//! refuses escape-bearing output without the flag, and one predating it.
//!
//! `fetch_check_runs`' pagination and short-read contract (#8987) is pinned in
//! `conditional_tests.rs`, against a stub that speaks `--include` (#11418).
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use super::fetch_job_log;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LOG: &str = "HEAD is now at 8536533 Merge f8f004b17 into c3db84070";

fn stub(dir: &Path, name: &str, with_flag: &str, without_flag: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --allow-escape-sequences ] && {{ {with_flag}; }}; done\n{without_flag}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// `fetch_job_log` against a freshly written stub, waiting out ETXTBSY (#9614).
///
/// Under `cargo test` another test thread can fork while `stub()`'s write fd is
/// open; the child keeps that fd until its own exec, and Linux refuses to exec
/// the script meanwhile ("Text file busy (os error 26)"). Same bounded retry as
/// `gh_invocation/outcome.rs::run_stub`.
fn fetch_stub_log(gh: &Path) -> Result<String, String> {
    let gh = gh.to_str().unwrap();
    for _ in 0..100 {
        match fetch_job_log(gh, "o/r", 7) {
            Err(e) if e.contains("os error 26") => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            out => return out,
        }
    }
    fetch_job_log(gh, "o/r", 7)
}

#[test]
fn a_current_gh_is_asked_to_allow_the_logs_escape_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        &format!("printf '\\033[36m%s\\033[0m\\n' '{LOG}'; exit 0"),
        "echo 'the response contains terminal escape sequences; pass --allow-escape-sequences to output it anyway' >&2; exit 1",
    );
    let log = fetch_stub_log(&gh).unwrap();
    assert!(log.contains(LOG), "{log:?}");
}

#[test]
fn a_gh_without_the_flag_is_retried_plainly() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        "echo 'unknown flag: --allow-escape-sequences' >&2; exit 1",
        &format!("echo '{LOG}'"),
    );
    assert_eq!(fetch_stub_log(&gh).unwrap().trim(), LOG);
}

#[test]
fn any_other_failure_is_reported_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        "echo 'HTTP 404: Not Found' >&2; exit 1",
        &format!("echo '{LOG}'"),
    );
    let error = fetch_stub_log(&gh).unwrap_err();
    assert!(error.contains("404"), "{error}");
}

fn argv_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("argv")).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// pr_ci_scope: the `P`-side ci.yml attribution (#9065)
// ---------------------------------------------------------------------------

use super::{pr_ci_scope, CiScope};
use crate::merge_pr::stale_checks::evidence::ChangedFile;
use crate::merge_pr::stale_checks::inputs::CI_WORKFLOW;

const PR_CI_YML: &str = include_str!("../../../../../.github/workflows/ci.yml");

fn pr_file(path: &str) -> ChangedFile {
    ChangedFile {
        path: path.to_string(),
        status: "modified".to_string(),
        previous_filename: None,
        patch: None,
    }
}

/// A `gh` stub that answers the `pulls/{n}/files` read with `entry` and every
/// `contents/` read with the real `ci.yml`. Both are heredocs, so a payload
/// containing `$`, backticks or `${{ }}` survives verbatim.
fn pr_gh_stub(dir: &Path, entry: &str) -> PathBuf {
    let path = dir.join("gh");
    let files = dir.join("files.json");
    let wf = dir.join("workflow.yml");
    std::fs::write(&files, entry).unwrap();
    std::fs::write(&wf, PR_CI_YML).unwrap();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {argv}\nfor a in \"$@\"; do\n  case \"$a\" in\n    *contents*) cat {wf}; exit 0;; \n  esac\ndone\ncat {files}\n",
            argv = dir.join("argv").display(),
            wf = wf.display(),
            files = files.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_until_execable(&path);
    let _ = std::fs::remove_file(dir.join("argv"));
    path
}

/// Block until the freshly written stub at `path` can be exec'd (#9614).
///
/// `pr_ci_scope` folds an exec failure into `CiScope::Unscoped`, so the
/// ETXTBSY race (see `fetch_stub_log`) cannot be retried at the call site;
/// probe the stub directly instead. The probe's `argv` line is removed by the
/// caller so the tests still see only their own `gh` calls.
fn wait_until_execable(path: &Path) {
    for _ in 0..100 {
        match std::process::Command::new(path).output() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => return,
        }
    }
}

/// The `{status, patch}` object `gh --jq` emits for a `ci.yml` entry that adds
/// one line at `line`.
fn pr_entry(line: usize) -> String {
    let patch = format!("@@ -{line},0 +{line},1 @@\\n+      # touched by this PR\\n");
    format!(r#"{{"status":"modified","patch":"{patch}"}}"#)
}

fn line_of(needle: &str) -> usize {
    PR_CI_YML.lines().position(|l| l == needle).unwrap() + 1
}

#[test]
fn a_pr_that_does_not_touch_ci_yml_costs_no_reads_and_stays_unscoped() {
    let dir = tempfile::tempdir().unwrap();
    let gh = pr_gh_stub(dir.path(), &pr_entry(10));
    let scope = pr_ci_scope(
        gh.to_str().unwrap(),
        "o/r",
        "9065",
        "deadbeef",
        &[pr_file("loom-daemon/src/lib.rs")],
    );
    assert_eq!(scope, CiScope::Unscoped);
    assert!(!dir.path().join("argv").exists(), "no gh call is made at all");
}

#[test]
fn a_prs_ci_yml_edit_is_attributed_against_the_head_workflow() {
    let dir = tempfile::tempdir().unwrap();
    // A block no granular required context runs — only the `CI Result`
    // aggregate (#10444), which needs every job.
    let gh = pr_gh_stub(dir.path(), &pr_entry(line_of("  backend-tests:") + 3));
    assert_eq!(
        pr_ci_scope(gh.to_str().unwrap(), "o/r", "9065", "deadbeef", &[pr_file(CI_WORKFLOW)]),
        CiScope::Scoped(std::iter::once("CI Result".to_string()).collect()),
    );

    // Both reads happened, and the workflow one was pinned to the PR HEAD —
    // the tree `pulls/{n}/files`' patches diff TO, not the base tip.
    let argv = argv_of(dir.path());
    assert!(argv.contains("pulls/9065/files"), "{argv}");
    assert!(argv.contains("contents/.github/workflows/ci.yml?ref=deadbeef"), "{argv}");

    // …and a block that IS a required gate's own definition.
    let dir = tempfile::tempdir().unwrap();
    let gh =
        pr_gh_stub(dir.path(), &pr_entry(line_of("      # component: Markdown Token Ratchet") + 2));
    assert!(
        pr_ci_scope(gh.to_str().unwrap(), "o/r", "9065", "deadbeef", &[pr_file(CI_WORKFLOW)])
            .affects("Markdown Token Ratchet"),
    );
}

#[test]
fn an_unreadable_pr_side_read_keeps_the_whole_file_meaning() {
    let dir = tempfile::tempdir().unwrap();
    for entry in [
        "",                                                        // the endpoint returned no such file
        "not json at all",                                         // an answer that will not parse
        r#"{"status":"modified","patch":null}"#,                   // GitHub suppressed the patch
        r#"{"status":"renamed","patch":"@@ -1,0 +1,1 @@\n+x\n"}"#, // not an in-file edit
    ] {
        let gh = pr_gh_stub(dir.path(), entry);
        assert_eq!(
            pr_ci_scope(gh.to_str().unwrap(), "o/r", "9065", "deadbeef", &[pr_file(CI_WORKFLOW)]),
            CiScope::Unscoped,
            "{entry:?}"
        );
    }
    // A `gh` that cannot run at all is the same answer.
    assert_eq!(
        pr_ci_scope("/nonexistent/gh", "o/r", "9065", "deadbeef", &[pr_file(CI_WORKFLOW)]),
        CiScope::Unscoped,
    );
}
