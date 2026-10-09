//! `telemetry-replay` against a fixture store (#11127): the exact queries and
//! parameters the command sends, executed by the pinned ClickHouse over R3's
//! fixture (`fixtures/signoz_replay/fleet_state.sql`), parsed and assembled by
//! the command's own reader. Requires Docker; never converts missing Docker to
//! a pass.
//!
//! `signoz_replay_queries.rs` proves the committed SQL; this proves the reader
//! runs that SQL (not a copy), binds `t` / `window` / `repo` as the SQL
//! expects, and reports what it returns:
//!
//! - a record knowable at or after `t` is excluded even when its event time is
//!   before `t` (`h-late`'s removal of 50);
//! - a broken `prev_as_of` chain makes that host `unknown`, not a stale state
//!   (`h-gap`), as R3's SQL decides;
//! - an anchor missing a chunk is not a base (`h-chunk`);
//! - an uncovered host is reported `unknown`, not empty (`h-silent`);
//! - two hosts' rows for one item resolve to the row naming the holding host
//!   (issue 60).
//!
//! `--check` (#11128) the same way, over the agreement fixture
//! (`fixtures/signoz_replay/agreement.sql`): the command's own queries 6-7 and
//! parameters, its reader and its exit code. The recorded export the unit
//! tests read (`check_export.jsonl`) must equal this live run.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

use chrono::{TimeZone, Utc};
use loom_daemon::telemetry_replay::check::{self, CheckParams, EXIT_AGREE, EXIT_DISAGREE};
use loom_daemon::telemetry_replay::{
    assemble, render, state_and_coverage_queries, Replay, ReplayParams, REPLAY_QUERIES,
};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";
const FIXTURE: &str = include_str!("fixtures/signoz_replay/fleet_state.sql");
const AGREEMENT_FIXTURE: &str = include_str!("fixtures/signoz_replay/agreement.sql");
const CHECK_EXPORT: &str = include_str!("fixtures/signoz_replay/check_export.jsonl");

fn run_query(params: &ReplayParams, sql: &str) -> String {
    run_on(FIXTURE, &params.params(), sql)
}

fn run_on(fixture: &str, params: &[(String, String)], sql: &str) -> String {
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "-i",
        "--entrypoint",
        "clickhouse",
        CLICKHOUSE_IMAGE,
        "local",
        "--multiquery",
        "--format=JSONEachRow",
    ]
    .map(str::to_string)
    .to_vec();
    args.extend(params.iter().map(|(k, v)| format!("--param_{k}={v}")));
    let mut child = Command::new("docker")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    let script = format!("{fixture}\n{sql};\n");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "clickhouse rejected the replay query:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn replay(repo: &str) -> Replay {
    let params = ReplayParams {
        as_of: Utc.with_ymd_and_hms(2026, 10, 4, 13, 0, 0).unwrap(),
        window_sec: 3900,
        repo: repo.to_string(),
    };
    let (state, coverage) = state_and_coverage_queries(REPLAY_QUERIES).unwrap();
    assemble(&params, &run_query(&params, &state), &run_query(&params, &coverage)).unwrap()
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn telemetry_replay_reports_fixture_state_with_uncovered_hosts_unknown() {
    let r = replay("");
    let issues: Vec<u64> = r.state.iter().map(|s| s.issue).collect();
    assert_eq!(issues, [3, 30, 31, 32, 50, 60, 98], "{r:#?}");

    let item = |n: u64| r.state.iter().find(|s| s.issue == n).unwrap();
    // Knowable-at decides membership: 50's removal (event 12:55) is knowable
    // only at 13:00:05, after t, so 50 is still live.
    assert_eq!(item(50).stage, "sweep_builder");
    // Two hosts report 60; the row that names the holding host wins.
    assert_eq!(item(60).host, "h-readd");
    assert_eq!(item(60).stage, "review_wait");
    assert_eq!(item(60).reporting_hosts, 2);
    // Nothing from a host whose chain is broken (7, 8) or whose anchor is
    // missing a chunk (20).
    for gone in [7, 8, 20] {
        assert!(!issues.contains(&gone), "issue {gone} leaked: {issues:?}");
    }

    let host = |e: &str| r.hosts.iter().find(|h| h.emitter == e).unwrap();
    for (emitter, verdict, state) in [
        ("h-readd", "covered", "complete"),
        ("h-late", "covered", "complete"),
        ("h-gap", "unknown", "broken_chain"),
        ("h-chunk", "unknown", "incomplete_anchor"),
        ("h-dchunk", "unknown", "incomplete_delta"),
        ("h-lostanchor", "unknown", "missing_anchor"),
        ("h-silent", "unknown", "no_anchor"),
    ] {
        let h = host(emitter);
        assert_eq!((h.verdict(), h.state.as_str()), (verdict, state), "{h:?}");
    }
    assert_eq!(r.hosts.len(), 9, "{:#?}", r.hosts);
    let text = render(&r);
    assert!(text.contains("hosts: 9 (4 covered)"), "{text}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn telemetry_replay_repo_scope_binds_through_to_the_state_query() {
    assert!(replay("other/repo").state.is_empty());
    assert_eq!(replay("rjwalters/loom").state.len(), 7);
}

fn check_params(repo: &str, threshold_sec: u32) -> CheckParams {
    CheckParams {
        replay: ReplayParams {
            as_of: Utc.with_ymd_and_hms(2026, 10, 4, 13, 0, 0).unwrap(),
            window_sec: 3900,
            repo: repo.to_string(),
        },
        span_sec: 3600,
        step_sec: 300,
        threshold_sec,
    }
}

fn run_check(params: &CheckParams) -> check::Check {
    let (runs_sql, report_sql) = check::check_and_report_queries(REPLAY_QUERIES).unwrap();
    let bound = params.params();
    check::assemble(
        params,
        &run_on(AGREEMENT_FIXTURE, &bound, &runs_sql),
        &run_on(AGREEMENT_FIXTURE, &bound, &report_sql),
    )
    .unwrap()
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn telemetry_replay_check_fails_on_the_covered_host_past_the_threshold_only() {
    let params = check_params("", 600);
    let c = run_check(&params);
    assert_eq!(c.exit_code(), EXIT_DISAGREE);
    let failures: Vec<_> = c
        .failures()
        .map(|d| (d.emitter.as_str(), d.issue, d.disagree_sec))
        .collect();
    assert_eq!(failures, [("h-stuck", 5, 2700)]);
    // The unit tests' recorded export is this run, not a hand-written copy.
    assert_eq!(c, check::assemble_export(&params, CHECK_EXPORT).unwrap());
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn telemetry_replay_check_agrees_when_only_uncovered_or_short_disagreements_remain() {
    // Scoped to rjwalters/loom: h-gap's identical stale row is unknown, and
    // h-lag's 300 s lag is under the threshold.
    let c = run_check(&check_params("rjwalters/loom", 600));
    assert_eq!(c.exit_code(), EXIT_AGREE, "{c:#?}");
    let gap = c.hosts.iter().find(|h| h.emitter == "h-gap").unwrap();
    assert_eq!(gap.coverage, "unknown");
    assert!(c.disagreements.iter().all(|d| d.emitter == "h-lag"));
    // --threshold raises the bar: 2700 s no longer fails at 3000 s.
    assert_eq!(run_check(&check_params("", 3000)).exit_code(), EXIT_AGREE);
}
