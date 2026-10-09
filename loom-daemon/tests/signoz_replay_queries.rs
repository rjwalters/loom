//! Live proof for `replay-queries.sql` queries 1-3 (#11125): fleet state at
//! `t`, host disagreement and coverage, executed verbatim against the pinned
//! ClickHouse the other SigNoz SQL proofs use. Requires Docker; never converts
//! missing Docker to a pass.
//!
//! Each assertion pins one replay-correctness rule (fixture layout:
//! `fixtures/signoz_replay/fleet_state.sql`):
//!
//! - **The last operation wins.** `upsert -> remove -> re-add -> remove` leaves
//!   the issue deleted; a predicate that keeps an issue when *any* removal
//!   predates its last upsert resurrects it. Repeated removals stay removed.
//! - **A lost delta makes the host unknown.** A delta whose `prev_as_of` is not
//!   the record before it means a delta in between was lost; that host
//!   contributes no rows and is not covered.
//! - **Completeness is a prerequisite.** An anchor or delta missing a byte
//!   chunk, or a chain whose anchor never arrived, is not a base.
//! - **Knowable-at decides membership.** A removal inserted after `t` does not
//!   apply at `t`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// The pin the other SigNoz SQL proofs and the trial's telemetry store use.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const QUERIES: &str = include_str!("../../defaults/observability/signoz/replay-queries.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_replay/fleet_state.sql");
const T: &str = "2026-10-04 13:00:00";
const WINDOW: &str = "3900";

const PREFIX_BEGIN: &str = "-- >>> replay-prefix";
const PREFIX_END: &str = "-- <<< replay-prefix";

type Row = BTreeMap<String, serde_json::Value>;

fn clickhouse(script: &str) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "clickhouse",
            &hub_image::resolve(CLICKHOUSE_IMAGE),
            "local",
            "--multiquery",
            &format!("--param_t={T}"),
            &format!("--param_window={WINDOW}"),
            "--param_repo=",
            "--format=JSONEachRow",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "clickhouse rejected the script:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn strip_comments(sql: &str) -> String {
    sql.lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The shared reconstruction prefix, exactly as committed between its markers.
fn prefix(queries: &str) -> String {
    let begin = queries.find(PREFIX_BEGIN).expect("prefix begin marker") + PREFIX_BEGIN.len();
    let end = queries.find(PREFIX_END).expect("prefix end marker");
    strip_comments(&queries[begin..end]).trim().to_owned()
}

/// The committed statements, comments stripped before the split on `;`.
fn statements(queries: &str) -> Vec<String> {
    let all: Vec<String> = strip_comments(queries)
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(all.len(), 8, "replay-queries.sql is documented as query 0, 1-3, 4a-4c and 5");
    all
}

/// Queries 1, 2 and 3 as a reader runs them: query 1 already starts with the
/// prefix; 2 and 3 have it prepended.
fn queries_1_to_3(queries: &str) -> [String; 3] {
    let pre = prefix(queries);
    let st = statements(queries);
    assert!(
        st[1].starts_with(&pre),
        "query 1 must begin with the shared replay prefix verbatim"
    );
    [
        st[1].clone(),
        format!("{pre}\n{}", st[2]),
        format!("{pre}\n{}", st[3]),
    ]
}

/// Runs `queries` after the fixture, one result set per query.
fn run(queries: &[String]) -> Vec<Vec<Row>> {
    let mut script = String::from(FIXTURE);
    for (index, statement) in queries.iter().enumerate() {
        script.push_str(&format!("\nSELECT {index} AS loom_section_marker;\n{statement};\n"));
    }
    let mut sections: Vec<Vec<Row>> = Vec::new();
    for line in clickhouse(&script).lines() {
        let row: Row = serde_json::from_str(line).expect("JSONEachRow line");
        if row.contains_key("loom_section_marker") {
            sections.push(Vec::new());
        } else {
            sections.last_mut().expect("a marker first").push(row);
        }
    }
    assert_eq!(sections.len(), queries.len());
    sections
}

fn num(row: &Row, key: &str) -> u64 {
    let value = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("column {key} is not a number in {row:?}"))
}

fn text<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("column {key} is not a string in {row:?}"))
}

fn issues(rows: &[Row]) -> Vec<u64> {
    rows.iter().map(|r| num(r, "issue")).collect()
}

fn issue(rows: &[Row], n: u64) -> &Row {
    rows.iter()
        .find(|r| num(r, "issue") == n)
        .unwrap_or_else(|| panic!("issue {n} missing from {rows:?}"))
}

fn host<'a>(rows: &'a [Row], emitter: &str) -> &'a Row {
    rows.iter()
        .find(|r| text(r, "emitter") == emitter)
        .unwrap_or_else(|| panic!("no coverage row for {emitter} in {rows:?}"))
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replay_rebuilds_only_reconstructable_state_and_the_last_operation_wins() {
    let sections = run(&queries_1_to_3(QUERIES));
    let (state, spread, coverage) = (&sections[0], &sections[1], &sections[2]);

    // Exactly the issues of complete chains, after their last operation.
    assert_eq!(issues(state), vec![3, 30, 31, 32, 50, 60, 98], "{state:?}");

    // upsert -> remove -> re-add -> remove: deleted (1); re-added and kept (3).
    assert_eq!(text(issue(state, 3), "stage"), "sweep_doctor");
    // A repeated removal stays removed (2); a later change still applies (98).
    assert_eq!(text(issue(state, 98), "stage"), "sweep_judge");
    // A removal knowable only after t does not apply at t.
    assert_eq!(text(issue(state, 50), "stage"), "sweep_builder");
    // Merged across hosts, the row that carries a host wins.
    let merged = issue(state, 60);
    assert_eq!(text(merged, "host"), "h-readd");
    assert_eq!(text(merged, "stage"), "review_wait");
    assert_eq!(num(merged, "reporting_hosts"), 2);

    // Query 2: the only issue two complete hosts report.
    assert_eq!(issues(spread), vec![60], "{spread:?}");
    assert_eq!(num(&spread[0], "distinct_stages"), 2);
    assert_eq!(num(&spread[0], "distinct_holders"), 1);
    assert_eq!(num(&spread[0], "entered_at_spread_sec"), 1800);

    // Query 3: completeness gates coverage.
    for (emitter, expected) in [
        ("h-readd", "complete"),
        ("h-rmtwice", "complete"),
        ("h-chunked-ok", "complete"),
        ("h-late", "complete"),
        ("h-gap", "broken_chain"),
        ("h-chunk", "incomplete_anchor"),
        ("h-dchunk", "incomplete_delta"),
        ("h-lostanchor", "missing_anchor"),
        ("h-silent", "no_anchor"),
    ] {
        let row = host(coverage, emitter);
        assert_eq!(text(row, "state"), expected, "{emitter}: {row:?}");
        assert_eq!(num(row, "covered"), u64::from(expected == "complete"), "{emitter}: {row:?}");
    }
    assert_eq!(coverage.len(), 9, "{coverage:?}");
}

/// The counterfactual: drop the completeness gate from the committed prefix and
/// the lost-delta, missing-chunk and missing-anchor hosts' stale rows come back
/// (issue 7, which the lost delta removed, among them). Proves the fixture
/// actually exercises the gate.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn without_the_completeness_gate_unreconstructable_rows_leak_into_the_state() {
    let gate = "WHERE (emitter, anchor_as_of) IN (SELECT emitter, base FROM status WHERE state = 'complete')";
    assert!(QUERIES.contains(gate), "the committed gate moved; update this counterfactual");
    let mutated = QUERIES
        .replace(gate, "WHERE (emitter, anchor_as_of) IN (SELECT emitter, base FROM status)");
    let [state, _, _] = queries_1_to_3(&mutated);
    let rows = &run(&[state])[0];
    let got = issues(rows);
    for leaked in [7, 8, 20, 41] {
        assert!(got.contains(&leaked), "issue {leaked} should leak without the gate: {got:?}");
    }
}
