//! The in-flight table's `REPO` column (#4698): five different managed
//! repos' small issue numbers (each repo's own `#3`/`#4`/`#6`) used to
//! look like duplicate same-issue dispatches with no way to disambiguate
//! them from `loom-daemon status` output alone, even though `SweepInfo`
//! already carried the owning workspace root (`repo`, Issue #3929).
use super::{format_repo_column, render_in_flight_table};
use crate::cli::status::sample_report::sample_report;
use chrono::Utc;
use loom_daemon::types::{SweepInfo, SweepKind, SweepState};
use std::path::PathBuf;

/// A minimal in-flight entry for a given issue/repo pair, mirroring the
/// `mk()` helper pattern used by `sweep_registry.rs`'s own tests.
fn mk(issue: u32, repo: Option<&str>) -> SweepInfo {
    SweepInfo {
        pgid: None,
        sweep_id: format!("s{issue}"),
        kind: SweepKind::Issue(issue),
        pid: 4242,
        token_name: "agent-1.token".to_string(),
        runtime: "claude".to_string(),
        runtime_source: None,
        log_path: PathBuf::from(format!(".loom/logs/sweep-issue-{issue}.log")),
        idempotency_key: None,
        started_at: Utc::now(),
        state: SweepState::Running,
        latest_phase: Some("builder".to_string()),
        pr_number: None,
        model: None,
        effort: None,
        depends_on: None,
        repo: repo.map(str::to_string),
        overflow: false,
    }
}

#[test]
fn format_repo_column_takes_path_basename() {
    assert_eq!(format_repo_column(Some("/repos/loom")), "loom");
    assert_eq!(format_repo_column(Some("/home/user/gf180-canary-3")), "gf180-canary-3");
}

#[test]
fn format_repo_column_leaves_bare_value_as_is() {
    // A bare `owner/repo` slug (e.g. from `loom-daemon/src/ipc.rs`'s
    // `"rjwalters/vibesql"`) has no meaningful "directory" component
    // beyond its own last segment — `Path::file_name` already reduces it
    // to `vibesql`, matching the workspace-root basename convention.
    assert_eq!(format_repo_column(Some("rjwalters/vibesql")), "vibesql");
    assert_eq!(format_repo_column(Some("loom")), "loom");
}

#[test]
fn format_repo_column_falls_back_to_dash_when_none() {
    assert_eq!(format_repo_column(None), "-");
}

#[test]
fn overflow_sweep_is_marked_in_the_table() {
    // #9244: an over-limit starred sweep must be visible in `status`.
    let mut report = sample_report();
    let mut over = mk(5, Some("/repos/loom"));
    over.overflow = true;
    report.in_flight = vec![mk(3, Some("/repos/loom")), over];
    let table = render_in_flight_table(&report);
    assert_eq!(table.matches("[overflow]").count(), 1, "{table}");
    assert!(
        table
            .lines()
            .any(|l| l.contains("#5") && l.contains("[overflow]")),
        "{table}"
    );
}

#[test]
fn table_header_advertises_repo_column() {
    let mut report = sample_report();
    report.in_flight = vec![mk(3, Some("/repos/loom"))];
    let table = render_in_flight_table(&report);
    assert!(
        table.lines().next().unwrap().contains("REPO"),
        "expected REPO in the table header, got: {table}"
    );
}

#[test]
fn distinguishes_same_issue_number_across_repos() {
    // The #4698 scenario: two different repos each dispatching their own
    // issue #3 must no longer look like the same duplicate dispatch.
    let mut report = sample_report();
    report.in_flight = vec![
        mk(3, Some("/repos/loom")),
        mk(3, Some("/repos/gf180-canary-2")),
    ];
    let table = render_in_flight_table(&report);
    assert!(table.contains("loom"), "expected repo basename 'loom' in: {table}");
    assert!(
        table.contains("gf180-canary-2"),
        "expected repo basename 'gf180-canary-2' in: {table}"
    );
}

#[test]
fn missing_repo_renders_dash_without_panicking_or_misaligning() {
    let mut report = sample_report();
    report.in_flight = vec![mk(4, None)];
    let table = render_in_flight_table(&report);
    let row = table.lines().nth(2).expect("header + separator + row");
    assert!(row.contains(" - "), "expected a lone '-' placeholder in row: {row}");
}

#[test]
fn single_repo_fleet_stays_readable() {
    // AC3: with only one managed repo, every row just repeats the same
    // basename — no conditional collapsing needed, alignment still holds.
    let mut report = sample_report();
    report.in_flight = vec![mk(1, Some("/repos/loom")), mk(2, Some("/repos/loom"))];
    let table = render_in_flight_table(&report);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 4, "header + separator + 2 rows, got: {table}");
    for row in &lines[2..] {
        assert!(row.contains("loom"), "expected 'loom' in row: {row}");
    }
}
