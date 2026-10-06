//! `fetch_job_log` against stub `gh` binaries (#9057): a current `gh` that
//! refuses escape-bearing output without the flag, and one predating it.
//!
//! Plus `fetch_check_runs`' pagination and its fail-closed short-read contract
//! (#8987, the daemon-side half of #8895): it used to pass `per_page=100`
//! without `--paginate` and drop `total_count` from the `--jq` projection, so a
//! repo growing past one page silently handed the freshness guard a subset with
//! nothing to detect the shortfall against.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use super::{fetch_check_runs, fetch_job_log};
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

#[test]
fn a_current_gh_is_asked_to_allow_the_logs_escape_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        &format!("printf '\\033[36m%s\\033[0m\\n' '{LOG}'; exit 0"),
        "echo 'the response contains terminal escape sequences; pass --allow-escape-sequences to output it anyway' >&2; exit 1",
    );
    let log = fetch_job_log(gh.to_str().unwrap(), "o/r", 7).unwrap();
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
    assert_eq!(
        fetch_job_log(gh.to_str().unwrap(), "o/r", 7)
            .unwrap()
            .trim(),
        LOG
    );
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
    let error = fetch_job_log(gh.to_str().unwrap(), "o/r", 7).unwrap_err();
    assert!(error.contains("404"), "{error}");
}

// ---------------------------------------------------------------------------
// fetch_check_runs: pagination + the fail-closed short read (#8987)
// ---------------------------------------------------------------------------

/// A `gh` stub that records its argv to `<dir>/argv` and prints `body`
/// verbatim. `--jq` is not evaluated, so `body` is whatever the projection
/// would already have produced: one `{total_count, check_runs}` object per
/// page, which is exactly how real `gh --paginate --jq` streams a multi-page
/// read (the filter runs per page; `--slurp` is refused alongside `--jq`).
fn gh_stub(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("gh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {argv}\ncat <<'PAYLOAD'\n{body}\nPAYLOAD\n",
            argv = dir.join("argv").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// One projected page: `n` rows named `check-<offset..>`, reporting `total`.
fn page(total: usize, n: usize, offset: usize) -> String {
    let rows: Vec<String> = (offset..offset + n)
        .map(|i| {
            format!(
                r#"{{"name":"check-{i}","status":"completed","conclusion":"success","started_at":"2026-09-01T00:00:0{}Z","app":"github-actions","details_url":"https://github.com/o/r/actions/runs/5/job/{i}","id":{i}}}"#,
                i % 10
            )
        })
        .collect();
    format!(r#"{{"total_count":{total},"check_runs":[{}]}}"#, rows.join(","))
}

fn argv_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("argv")).unwrap_or_default()
}

#[test]
fn every_page_of_a_multi_page_check_runs_read_is_folded_in() {
    let dir = tempfile::tempdir().unwrap();
    // 250 check-runs as 100 + 100 + 50 — the pre-fix fetch kept only the first.
    let body = format!("{}\n{}\n{}", page(250, 100, 0), page(250, 100, 100), page(250, 50, 200));
    let gh = gh_stub(dir.path(), &body);
    let runs = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef").unwrap();
    assert_eq!(runs.len(), 250, "every page's rows must be folded in");
    let unique: std::collections::BTreeSet<&str> = runs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(unique.len(), 250, "no page dropped or double-counted");
}

#[test]
fn the_check_runs_request_asks_for_pagination_not_just_a_bigger_page() {
    let dir = tempfile::tempdir().unwrap();
    let gh = gh_stub(dir.path(), &page(3, 3, 0));
    fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef").unwrap();
    let argv = argv_of(dir.path());
    assert!(argv.contains("per_page=100"), "{argv}");
    assert!(argv.contains("--paginate"), "per_page alone only moves the cap: {argv}");
    assert!(
        argv.contains("total_count"),
        "the projection must keep total_count so a short read is detectable: {argv}"
    );
}

#[test]
fn a_short_check_runs_read_fails_closed_instead_of_returning_a_subset() {
    let dir = tempfile::tempdir().unwrap();
    // The forge says 250 exist; pagination delivered 200.
    let body = format!("{}\n{}", page(250, 100, 0), page(250, 100, 100));
    let gh = gh_stub(dir.path(), &body);
    let error = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef")
        .expect_err("a subset must be an Err, never a partial Vec<CheckRun>");
    assert!(error.contains("200"), "{error}");
    assert!(error.contains("250"), "{error}");
}

#[test]
fn a_single_page_short_read_fails_closed_too() {
    let dir = tempfile::tempdir().unwrap();
    // The #8895 shape verbatim: total_count 39, 30 rows on the wire.
    let gh = gh_stub(dir.path(), &page(39, 30, 0));
    let error = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef").unwrap_err();
    assert!(error.contains("30 of 39"), "{error}");
}

#[test]
fn a_complete_read_is_not_classified_as_short() {
    let dir = tempfile::tempdir().unwrap();
    let gh = gh_stub(dir.path(), &page(2, 2, 0));
    let runs = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef").unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].actions_job_id, Some(0), "projection still parsed");
}

#[test]
fn an_over_reported_total_count_is_not_a_short_read() {
    let dir = tempfile::tempdir().unwrap();
    // A stale/under-reporting counter: MORE rows than total_count claims. Not a
    // shortfall, so every row is kept.
    let gh = gh_stub(dir.path(), &page(3, 5, 0));
    let runs = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef").unwrap();
    assert_eq!(runs.len(), 5);
}

#[test]
fn a_genuinely_empty_rollup_is_a_complete_read_of_zero_rows() {
    let dir = tempfile::tempdir().unwrap();
    let gh = gh_stub(dir.path(), &page(0, 0, 0));
    let runs = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef").unwrap();
    assert!(runs.is_empty(), "a repo with no CI must still read cleanly");
}

#[test]
fn a_gh_that_exits_zero_printing_nothing_is_not_read_as_zero_checks() {
    let dir = tempfile::tempdir().unwrap();
    let gh = gh_stub(dir.path(), "");
    let error = fetch_check_runs(gh.to_str().unwrap(), "o/r", "deadbeef")
        .expect_err("an empty read is a degraded read, not a commit without checks");
    assert!(error.contains("no pages"), "{error}");
}

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
    path
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
    let gh = pr_gh_stub(dir.path(), &pr_entry(line_of("      # component: File Size Ratchet") + 2));
    assert!(
        pr_ci_scope(gh.to_str().unwrap(), "o/r", "9065", "deadbeef", &[pr_file(CI_WORKFLOW)])
            .affects("File Size Ratchet"),
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
