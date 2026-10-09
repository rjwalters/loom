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
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::io::Write;
use std::process::{Command, Stdio};

use chrono::{TimeZone, Utc};
use loom_daemon::telemetry_replay::{
    assemble, render, state_and_coverage_queries, Replay, ReplayParams, REPLAY_QUERIES,
};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";
const FIXTURE: &str = include_str!("fixtures/signoz_replay/fleet_state.sql");

fn run_query(params: &ReplayParams, sql: &str) -> String {
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "-i",
        "--entrypoint",
        "clickhouse",
        &hub_image::resolve(CLICKHOUSE_IMAGE),
        "local",
        "--multiquery",
        "--format=JSONEachRow",
    ]
    .map(str::to_string)
    .to_vec();
    args.extend(
        params
            .params()
            .into_iter()
            .map(|(k, v)| format!("--param_{k}={v}")),
    );
    let mut child = Command::new("docker")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    let script = format!("{FIXTURE}\n{sql};\n");
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
