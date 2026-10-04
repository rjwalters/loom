//! Live proof for `eta-queries.sql` Q4-Q7 (#10233): the late-surprise,
//! stability, convergence and answer-rate views, executed against the same
//! pinned ClickHouse as `signoz_eta_queries.rs` (which proves section 0 and
//! Q1-Q3 and runs the whole file verbatim). Requires Docker; never converts
//! missing Docker to a pass.
//!
//! Each view exists to stop one specific plausible-but-wrong reading, and each
//! has the mutation of the **committed SQL** that reinstates it run as a
//! counterfactual:
//!
//! - **Q4, the common decidable subset.** Without it, a heuristic that refuses
//!   the cases its sibling was late on reads as the better one.
//! - **Q5, the landing instant.** Measured on remaining seconds, a perfectly
//!   steady ETA looks like it drifts and a drifting one looks steady.
//! - **Q5, no transition.** Counting a stage change as drift charges a
//!   heuristic for reacting to news.
//! - **Q6, scored outcomes only.** A censored outcome's lead is only a lower
//!   bound; admitted, it invents a `gt_24h` convergence row.
//! - **Q7, time weighting.** A refusal is emitted once and never refreshed;
//!   counting rows inflates the answer rate from 0.5 to 0.75.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// The pin `signoz_eta_queries.rs` and the trial's telemetry store use.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const QUERIES: &str = include_str!("../../defaults/observability/signoz/eta-queries.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_eta/accuracy_views.sql");
const SINCE: &str = "2026-09-01 00:00:00";

type Row = BTreeMap<String, serde_json::Value>;

fn clickhouse(script: &str) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "clickhouse",
            CLICKHOUSE_IMAGE,
            "local",
            "--multiquery",
            &format!("--param_since={SINCE}"),
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

/// The committed statements, comments stripped before the split on `;` (the
/// prose uses semicolons), exactly as `signoz_eta_queries.rs` does.
fn statements() -> Vec<String> {
    let code = QUERIES
        .lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n");
    let all: Vec<String> = code
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(all.len(), 8, "eta-queries.sql is documented as section 0 plus Q1-Q7");
    all
}

/// The committed `Qn`.
fn q(n: usize) -> String {
    statements()[n].clone()
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

fn num(row: &Row, key: &str) -> f64 {
    let value = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("column {key} is not a number in {row:?}"))
}

fn text<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("column {key} is not a string in {row:?}"))
}

/// The row of `heuristic`. Panics when absent.
fn row<'a>(rows: &'a [Row], heuristic: &str) -> &'a Row {
    rows.iter()
        .find(|r| text(r, "heuristic") == heuristic)
        .unwrap_or_else(|| panic!("no row for {heuristic} in {rows:?}"))
}

fn bucket<'a>(rows: &'a [Row], heuristic: &str, lead: &str) -> Option<&'a Row> {
    rows.iter()
        .find(|r| text(r, "heuristic") == heuristic && text(r, "lead_bucket") == lead)
}

/// Asserts `needle` is in the committed text before replacing it, so a
/// counterfactual cannot silently become a no-op after the artifact changes.
fn mutate(statement: &str, needle: &str, replacement: &str) -> String {
    assert!(
        statement.contains(needle),
        "the committed query no longer contains `{needle}`; re-point this \
         counterfactual:\n{statement}"
    );
    statement.replace(needle, replacement)
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn late_surprise_is_read_on_the_common_decidable_subset() {
    let committed = q(4);
    // An instant counts when ANY heuristic decided it, instead of all.
    let naive = mutate(&committed, "HAVING min(has_p90) = 1", "HAVING max(has_p90) = 1");
    let out = run(&[committed, naive]);

    let (a, b) = (row(&out[0], "late-a"), row(&out[0], "late-b"));
    assert_eq!(num(a, "decided"), 10.0, "i 10-11 drop out: late-b refused them");
    assert_eq!(num(a, "late_surprise_rate"), 0.1);
    assert_eq!(num(b, "decided"), 10.0);
    assert_eq!(num(b, "late_surprise_rate"), 0.2);
    assert_eq!(
        num(a, "censored"),
        1.0,
        "the expired-unresolved estimate is a decided late surprise, counted"
    );

    let (a, b) = (row(&out[1], "late-a"), row(&out[1], "late-b"));
    assert_eq!(num(a, "decided"), 12.0);
    assert_eq!(
        num(a, "late_surprise_rate"),
        0.25,
        "without the common subset, late-a is charged for the two cases late-b \
         declined to answer"
    );
    assert!(
        num(b, "late_surprise_rate") < num(a, "late_surprise_rate"),
        "and the heuristic that refused the hard cases reads as the better one"
    );
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn stability_is_measured_on_the_landing_instant_between_unchanged_emissions() {
    let committed = q(5);
    let remaining = mutate(
        &committed,
        "as_of_sec + toInt64(attributes_number['loom.eta.p50_sec']) AS landing",
        "toInt64(attributes_number['loom.eta.p50_sec']) AS landing",
    );
    let with_transitions = mutate(&committed, " AND stage = prev_stage", "");
    let out = run(&[committed, remaining, with_transitions]);

    let (stable, drifty) = (row(&out[0], "stable-v1"), row(&out[0], "drifty-v1"));
    assert_eq!(num(stable, "steps"), 3.0, "the k=4 stage change is not a drift step");
    assert_eq!(num(stable, "median_shift_sec"), 0.0, "its instant never moved");
    assert_eq!(num(drifty, "median_shift_sec"), 300.0);
    assert_eq!(num(drifty, "max_shift_sec"), 300.0);

    let (stable, drifty) = (row(&out[1], "stable-v1"), row(&out[1], "drifty-v1"));
    assert_eq!(
        (num(stable, "median_shift_sec"), num(drifty, "median_shift_sec")),
        (300.0, 0.0),
        "on remaining seconds the ranking inverts: the steady ETA looks jumpy \
         and the sliding one looks steady"
    );

    let stable = row(&out[2], "stable-v1");
    assert_eq!(num(stable, "steps"), 4.0);
    assert_eq!(
        num(stable, "max_shift_sec"),
        1400.0,
        "counting the transition charges the heuristic for reacting to news"
    );
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn convergence_reads_widths_by_actual_lead_over_scored_outcomes_only() {
    let committed = q(6);
    let lax = mutate(
        &committed,
        "      AND mapContains(attributes_number, 'loom.eta.error_sec')\n",
        "",
    );
    let out = run(&[committed, lax]);

    let near = bucket(&out[0], "conv-v1", "lt_15m").expect("lt_15m row");
    assert_eq!(num(near, "scored"), 2.0);
    assert_eq!(num(near, "median_p25_p75_sec"), 300.0);
    assert_eq!(num(near, "median_p25_p90_sec"), 500.0);
    let far = bucket(&out[0], "conv-v1", "1h_4h").expect("1h_4h row");
    assert_eq!(num(far, "scored"), 3.0);
    assert_eq!(num(far, "median_p25_p75_sec"), 3000.0);
    assert_eq!(
        num(far, "with_p90"),
        2.0,
        "the pre-#10211 row has no p90 and is left out of that width only"
    );
    assert_eq!(num(far, "median_p25_p90_sec"), 5000.0);
    assert!(
        bucket(&out[0], "conv-v1", "gt_24h").is_none(),
        "the censored outcome's lead is a lower bound, not a lead: {:?}",
        out[0]
    );

    assert!(
        bucket(&out[1], "conv-v1", "gt_24h").is_some(),
        "admitted, it invents a convergence row 40 days out: {:?}",
        out[1]
    );
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_answer_rate_is_time_weighted_not_row_counted() {
    let committed = q(7);
    let by_rows = mutate(&committed, "next_t - t AS stood_sec", "1 AS stood_sec");
    let out = run(&[committed, by_rows]);

    let honest = row(&out[0], "answer-v1");
    assert_eq!(num(honest, "states"), 4.0, "issue 702's open tail is left out");
    assert_eq!(num(honest, "total_sec"), 1800.0);
    assert_eq!(num(honest, "answered_sec"), 900.0);
    assert_eq!(
        num(honest, "answer_rate"),
        0.5,
        "one subject answered for 900 s, the other refused for 900 s"
    );

    let inflated = row(&out[1], "answer-v1");
    assert_eq!(
        num(inflated, "answer_rate"),
        0.75,
        "counted by rows, the two refreshes of the answer outvote the refusal \
         that was never re-emitted"
    );
}
