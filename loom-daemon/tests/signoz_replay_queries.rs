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
//!
//! Queries 6-7 (agreement with the forge, #11128) run over their own fixture
//! (`fixtures/signoz_replay/agreement.sql`): the 24 h report query returns one
//! row per host, a covered host's stale view is a disagreement run measured
//! in covered instants, and an uncovered host is `unknown`, never a
//! disagreement.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// The pin the other SigNoz SQL proofs and the trial's telemetry store use.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const QUERIES: &str = include_str!("../../defaults/observability/signoz/replay-queries.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_replay/fleet_state.sql");
const AGREEMENT_FIXTURE: &str = include_str!("fixtures/signoz_replay/agreement.sql");
const T: &str = "2026-10-04 13:00:00";
const WINDOW: &str = "3900";

/// Queries 1-3 read the instant t only (span 0).
const AT_T: [(&str, &str); 5] = [
    ("t", T),
    ("window", WINDOW),
    ("repo", ""),
    ("span", "0"),
    ("step", "300"),
];

const PREFIX_BEGIN: &str = "-- >>> replay-prefix";
const PREFIX_END: &str = "-- <<< replay-prefix";
const AGREEMENT_BEGIN: &str = "-- >>> agreement-prefix";
const AGREEMENT_END: &str = "-- <<< agreement-prefix";

type Row = BTreeMap<String, serde_json::Value>;

fn clickhouse(script: &str, params: &[(&str, &str)]) -> String {
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
    assert_eq!(
        all.len(),
        10,
        "replay-queries.sql is documented as query 0, 1-3, 4a-4c, 5 and 6-7"
    );
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

/// Queries 6 and 7 as a reader runs them: the prefix, then (7 only) the
/// agreement block, which query 6 already begins with.
fn queries_6_and_7(queries: &str) -> [String; 2] {
    let pre = prefix(queries);
    let begin = queries
        .find(AGREEMENT_BEGIN)
        .expect("agreement begin marker")
        + AGREEMENT_BEGIN.len();
    let end = queries.find(AGREEMENT_END).expect("agreement end marker");
    let block = strip_comments(&queries[begin..end]).trim().to_owned();
    let st = statements(queries);
    assert!(
        st[8].starts_with(&block),
        "query 6 must begin with the agreement block verbatim"
    );
    [
        format!("{pre}\n{}", st[8]),
        format!("{pre}\n{block}\n{}", st[9]),
    ]
}

/// Runs `queries` after the fixture, one result set per query.
fn run(queries: &[String]) -> Vec<Vec<Row>> {
    run_on(FIXTURE, queries, &AT_T)
}

fn run_on(fixture: &str, queries: &[String], params: &[(&str, &str)]) -> Vec<Vec<Row>> {
    let mut script = String::from(fixture);
    for (index, statement) in queries.iter().enumerate() {
        script.push_str(&format!("\nSELECT {index} AS loom_section_marker;\n{statement};\n"));
    }
    let mut sections: Vec<Vec<Row>> = Vec::new();
    for line in clickhouse(&script, params).lines() {
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
    let gate = "WHERE (t, emitter, anchor_as_of) IN (SELECT t, emitter, base FROM status WHERE state = 'complete')";
    assert!(QUERIES.contains(gate), "the committed gate moved; update this counterfactual");
    let mutated = QUERIES.replace(
        gate,
        "WHERE (t, emitter, anchor_as_of) IN (SELECT t, emitter, base FROM status)",
    );
    let [state, _, _] = queries_1_to_3(&mutated);
    let rows = &run(&[state])[0];
    let got = issues(rows);
    for leaked in [7, 8, 20, 41] {
        assert!(got.contains(&leaked), "issue {leaked} should leak without the gate: {got:?}");
    }
}

/// One query 7 row: emitter, coverage, then the counts.
type ReportRow<'a> = (&'a str, &'a str, u64, u64, u64, u64, u64, u64, u64, u64);

/// The 24 h agreement report (query 7) and the disagreement runs (query 6),
/// over the agreement fixture's hour of instants (span 3600 here; the daily
/// report binds 86400).
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_agreement_report_has_one_row_per_host_and_only_covered_hosts_disagree() {
    let params = [
        ("t", T),
        ("window", WINDOW),
        ("repo", ""),
        ("span", "3600"),
        ("step", "300"),
        ("threshold", "600"),
    ];
    let sections = run_on(AGREEMENT_FIXTURE, &queries_6_and_7(QUERIES), &params);
    let (runs, report) = (&sections[0], &sections[1]);

    // emitter, coverage, covered/13, compared, agreeing, disagreeing,
    // not_comparable, longest, over threshold, incomplete anchors.
    let expected: [ReportRow; 7] = [
        ("h-agree", "covered", 13, 52, 52, 0, 26, 0, 0, 0),
        ("h-chunk", "unknown", 0, 0, 0, 0, 0, 0, 0, 2),
        ("h-drift", "covered", 13, 13, 9, 4, 0, 1200, 1, 0),
        ("h-gap", "unknown", 0, 0, 0, 0, 0, 0, 0, 0),
        ("h-lag", "covered", 13, 13, 12, 1, 0, 300, 0, 0),
        ("h-silent", "unknown", 0, 0, 0, 0, 0, 0, 0, 0),
        ("h-stuck", "covered", 13, 13, 4, 9, 0, 2700, 1, 0),
    ];
    assert_eq!(report.len(), expected.len(), "{report:?}");
    for (row, want) in report.iter().zip(expected) {
        let got = (
            text(row, "emitter"),
            text(row, "coverage"),
            num(row, "covered_samples"),
            num(row, "compared"),
            num(row, "agreeing"),
            num(row, "disagreeing"),
            num(row, "not_comparable"),
            num(row, "longest_disagreement_sec"),
            num(row, "disagreements_over_threshold"),
            num(row, "incomplete_anchors"),
        );
        assert_eq!(got, want, "{row:?}");
        assert_eq!(num(row, "samples"), 13, "{row:?}");
    }
    let states = |e: &str| host(report, e)["uncovered_states"].clone();
    assert_eq!(states("h-gap"), serde_json::json!(["broken_chain"]));
    assert_eq!(states("h-chunk"), serde_json::json!(["incomplete_anchor"]));
    assert_eq!(states("h-silent"), serde_json::json!(["no_anchor"]));

    // Query 6: the stuck covered host fails; the host whose item the forge
    // moved through two stages mid-disagreement is one run and fails; the
    // short lag does not; the uncovered host holding the same stale row is
    // absent.
    let got: Vec<_> = runs
        .iter()
        .map(|r| {
            (
                text(r, "emitter"),
                text(r, "repo"),
                num(r, "issue"),
                text(r, "host_stage"),
                text(r, "forge_stage"),
                num(r, "disagree_sec"),
                num(r, "over_threshold"),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("h-stuck", "rjwalters/other", 5, "review_wait", "merge_wait", 2700, 1),
            ("h-drift", "rjwalters/drift", 4, "review_wait", "doctor", 1200, 1),
            ("h-lag", "rjwalters/loom", 3, "ready_wait", "building", 300, 0),
        ]
    );
    assert_eq!(text(&runs[0], "first_at"), "2026-10-04 12:20:00.000");
    assert_eq!(text(&runs[0], "forge_since"), "2026-10-04 12:19:00.000");
    assert_eq!(runs[0]["forge_stages"], serde_json::json!(["merge_wait"]));
    // The drift run spans both forge stages: one run, the latest pair reported.
    assert_eq!(text(&runs[1], "first_at"), "2026-10-04 12:25:00.000");
    assert_eq!(text(&runs[1], "last_at"), "2026-10-04 12:40:00.000");
    assert_eq!(text(&runs[1], "forge_since"), "2026-10-04 12:34:00.000");
    assert_eq!(runs[1]["forge_stages"], serde_json::json!(["doctor", "merge_wait"]));
}
