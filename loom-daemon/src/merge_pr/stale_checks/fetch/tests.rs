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
