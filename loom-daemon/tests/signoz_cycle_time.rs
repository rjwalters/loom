//! Live proof for the SigNoz half of the cycle-time analytics seam (Issue
//! #8528 scope item 4; Issue #8665): requires Docker, never converts missing
//! Docker to a pass. The static half is `cycle_time_artifacts.rs`, which
//! checks that `signoz/cycle-time-extract.sql` and
//! `clickstack/cycle-time-extract.sql` agree on the `raw_ship_outcome` column
//! list; it cannot see whether either view actually produces correct numbers
//! against a real engine.
//!
//! `signoz/cycle-time-extract.sql` carried this header for its entire life
//! until now:
//!
//! > STATUS: contract-checked in CI, NOT yet executed against a live SigNoz
//! > deployment — the ClickStack side is the live-verified one
//! > (`loom-daemon/tests/cycle_time_clickhouse.rs`).
//!
//! This closes that gap using the same technique `signoz_usage_queries.rs`
//! already established for this issue's other SigNoz artifact: no full
//! SigNoz deployment (the multi-container render is exercised by
//! `signoz_deployment_contract.rs` and the trial itself, not by this test),
//! just `clickhouse local` in the same pinned image the trial's telemetry
//! store runs, seeded with rows shaped exactly like SigNoz's real
//! `distributed_logs_v2` schema.
//!
//! What makes this more than a schema-shape check: the seven rows in
//! `fixtures/signoz_cycle_time/fixture.sql` are the SAME seven `sweep.outcome`
//! envelopes as the ClickStack live proof's fixture
//! (`fixtures/cycle_time/envelopes.jsonl.tmpl`), by hand-translating them
//! through the real mapper's own attribute-type decisions (`kv_int` vs
//! `kv_string` call sites in `otlp/mapping.rs` / `otlp/mapping/metadata.rs`).
//! Feeding the identical fixture through `signoz/cycle-time-extract.sql`,
//! the shared `cycle-time-rollup.sql` and the shared `cycle-time-queries.sql`
//! and comparing the CT1-CT8 answers below against the values
//! `cycle_time_clickhouse.rs` already asserts for ClickStack is therefore a
//! same-workload cross-backend comparison, not just two independent proofs
//! that happen to both pass.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`),
/// `cycle_time_clickhouse.rs` and `signoz_usage_queries.rs`, so no proof in
/// this repo can drift from the deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const EXTRACT: &str = include_str!("../../defaults/observability/signoz/cycle-time-extract.sql");
const ROLLUP: &str = include_str!("../../defaults/observability/cycle-time-rollup.sql");
const QUERIES: &str = include_str!("../../defaults/observability/cycle-time-queries.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_cycle_time/fixture.sql");

/// Wide enough to hold all seven canonical ships (2026-09-20 through
/// 2026-09-26) and to exclude `ship-fallback-check`, whose timestamp is
/// deliberately in the year 2200 so it can never perturb the CT1-CT8
/// comparison below (see the fixture file's own header for why).
const SINCE: &str = "2000-01-01 00:00:00";
const UNTIL: &str = "2100-01-01 00:00:00";

/// One result row: column name -> JSON value, as `JSONEachRow` renders it.
type Row = BTreeMap<String, serde_json::Value>;

/// Runs `script` through `clickhouse local` in the pinned image and returns
/// stdout. Panics with the engine's own stderr on failure — a query that does
/// not parse must fail this test, not be silently skipped.
fn clickhouse(script: &str, format: &str) -> String {
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
            &format!("--param_since={SINCE}"),
            &format!("--param_until={UNTIL}"),
            "--param_top_n=10",
            &format!("--format={format}"),
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

/// The committed file's statements. Line comments are stripped **before**
/// the split on `;`, mirroring `signoz_usage_queries.rs`'s `statements()`: no
/// `;` or `--` occurs inside a string literal in this artifact, so what
/// remains is each statement's exact committed text. The strict verbatim
/// proof is the whole-file run in the test below, which executes the bytes
/// as committed, comments included; this is only used to attribute each
/// section's output to its question.
fn statements(sql: &str) -> Vec<String> {
    let code = sql
        .lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n");
    code.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Every CT question in `cycle-time-queries.sql`, executed in one session
/// after the fixture/extract/rollup, each preceded by a marker row so the
/// concatenated `JSONEachRow` output can be attributed to CT1..CT8. The
/// questions themselves are verbatim; only the marker `SELECT`s are added.
fn sections() -> Vec<Vec<Row>> {
    let committed = statements(QUERIES);
    let mut script = format!("{FIXTURE}\n{EXTRACT}\n{ROLLUP}\n");
    for (index, statement) in committed.iter().enumerate() {
        script.push_str(&format!("\nSELECT {index} AS loom_section_marker;\n"));
        script.push_str(statement);
        script.push_str(";\n");
    }
    let mut sections: Vec<Vec<Row>> = Vec::new();
    for line in clickhouse(&script, "JSONEachRow").lines() {
        let row: Row = serde_json::from_str(line).expect("JSONEachRow line");
        if row.contains_key("loom_section_marker") {
            sections.push(Vec::new());
        } else {
            sections
                .last_mut()
                .expect("a marker precedes every result row")
                .push(row);
        }
    }
    assert_eq!(
        sections.len(),
        committed.len(),
        "cycle-time-queries.sql is documented as CT1 through CT8"
    );
    sections
}

fn num(row: &Row, key: &str) -> i64 {
    let value = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("column {key} is not an integer in {row:?}"))
}

fn text<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"))
        .as_str()
        .unwrap_or_else(|| panic!("column {key} is not a string in {row:?}"))
}

fn is_null(row: &Row, key: &str) -> bool {
    row.get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"))
        .is_null()
}

fn by<'a>(rows: &'a [Row], key: &str, value: &str) -> &'a Row {
    rows.iter()
        .find(|row| text(row, key) == value)
        .unwrap_or_else(|| panic!("no row with {key} = {value} in {rows:?}"))
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_cycle_time_artifacts_answer_the_canonical_questions_on_signoz_schema() {
    // ---- the whole pipeline runs verbatim, as one pass, exactly as documented
    let verbatim = format!("{FIXTURE}\n{EXTRACT}\n{ROLLUP}\n{QUERIES}");
    clickhouse(&verbatim, "TSV");

    let sections = sections();

    // ---- CT1: the headline question -----------------------------------
    let ct1 = &sections[0];
    let headline: Vec<(String, i64, bool)> = ct1
        .iter()
        .map(|row| {
            (
                text(row, "sweep_id").to_owned(),
                num(row, "total_duration_sec"),
                is_null(row, "dominant_phase"),
            )
        })
        .collect();
    assert_eq!(
        headline,
        vec![
            ("ship-beta-103".into(), 5400, true),
            ("ship-alpha-101".into(), 3600, false),
            ("ship-alpha-102".into(), 2960, false),
            ("ship-alpha-106".into(), 480, false),
            ("ship-alpha-105".into(), 265, false),
        ],
        "CT1 no longer answers 'the slowest ships, and which phase dominated each' \
         identically to the ClickStack live proof over the same seven envelopes"
    );
    assert!(
        !ct1.iter()
            .any(|row| text(row, "sweep_id") == "ship-beta-104"),
        "CT1 included a failed sweep among the ships"
    );
    let dominant =
        |sweep_id: &str| text(by(ct1, "sweep_id", sweep_id), "dominant_phase").to_owned();
    assert_eq!(dominant("ship-alpha-101"), "builder");
    assert_eq!(
        dominant("ship-alpha-102"),
        "judge",
        "800s + 700s of judge across the repair loop must outrank the single 900s builder"
    );

    // ---- CT2: per-phase totals ------------------------------------------
    let ct2 = &sections[1];
    assert_eq!(text(&ct2[0], "phase"), "builder", "CT2 no longer ranks phases by total time");
    assert_eq!(
        by(ct2, "phase", "judge")
            .get("total_sec")
            .and_then(serde_json::Value::as_i64),
        Some(1910),
        "CT2's per-phase totals changed"
    );

    // ---- CT3: per-repo success rate ---------------------------------------
    let ct3 = &sections[2];
    assert_eq!(
        num(by(ct3, "repo", "synthetic/beta"), "success_pct"),
        50,
        "CT3 no longer reports the per-repo success rate"
    );

    // ---- CT4: execution configuration -------------------------------------
    let ct4 = &sections[3];
    assert!(
        ct4.iter().any(|row| is_null(row, "runtime")),
        "CT4 collapsed an unreported runtime into a bucket instead of leaving it NULL"
    );

    // ---- CT5: repair cost --------------------------------------------------
    let ct5 = &sections[4];
    let repaired = ct5
        .iter()
        .find(|row| num(row, "doctor_engaged") == 1)
        .expect("CT5 no longer separates repaired ships");
    assert_eq!(num(repaired, "ships"), 1, "CT5 counted the wrong number of repaired ships");
    assert_eq!(num(repaired, "avg_doctor_sec"), 400, "CT5 lost the doctor-phase seconds");

    // ---- CT6: fleet trend, by week ------------------------------------------
    let ct6 = &sections[5];
    let weeks: i64 = ct6.iter().map(|row| num(row, "ships")).sum();
    assert_eq!(weeks, 6, "CT6 lost ships while bucketing them by week");

    // ---- CT7: coverage -------------------------------------------------------
    let ct7 = &sections[6][0].clone();
    assert_eq!(num(ct7, "ships"), 6);
    assert_eq!(num(ct7, "without_phase_breakdown"), 1);
    assert_eq!(num(ct7, "without_runtime"), 1);
    assert_eq!(num(ct7, "without_provider"), 1);
    assert_eq!(num(ct7, "without_model"), 1);
    assert_eq!(num(ct7, "without_effort"), 2);
    assert_eq!(num(ct7, "without_pr_number"), 1);
    assert_eq!(
        num(ct7, "without_doctor_cycles"),
        2,
        "CT7's coverage counts changed; an absent measurement may have become a zero"
    );
    assert_eq!(num(ct7, "without_positive_total"), 0);

    // ---- CT8: rollup fidelity -----------------------------------------------
    let ct8 = &sections[7][0].clone();
    assert_eq!(num(ct8, "raw_ships"), 6);
    assert_eq!(num(ct8, "rolled_up_ships"), 6);
    assert_eq!(num(ct8, "missing_from_rollup"), 0);
    assert_eq!(num(ct8, "rollup_beyond_raw"), 0);
    assert_eq!(
        num(ct8, "mismatched_totals"),
        0,
        "CT8 reports drift between the raw logs and the rollup"
    );
}

/// The two deliberate differences `signoz/cycle-time-extract.sql` documents
/// from the ClickStack view — which map an integer attribute lands in is a
/// property of the pinned ingester, not a fixture choice, so every numeric
/// read falls back from `attributes_number` to `attributes_string` — proven
/// here rather than assumed. `ship-fallback-check` carries `loom.issue` and
/// `loom.total_duration_sec` ONLY in `attributes_string`, and carries neither
/// `loom.pr_number` nor `loom.doctor_cycles` in either map at all.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn extraction_falls_back_to_the_string_map_and_preserves_true_absence() {
    let probe = format!(
        "{FIXTURE}\n{EXTRACT}\n\
         SELECT issue, total_duration_sec, pr_number, doctor_cycles\n\
         FROM loom_analytics.raw_ship_outcome\n\
         WHERE sweep_id = 'ship-fallback-check';\n"
    );
    let output = clickhouse(&probe, "JSONEachRow");
    let row: Row = serde_json::from_str(output.trim()).expect("one JSONEachRow line");
    assert_eq!(
        num(&row, "issue"),
        900,
        "loom.issue was seeded only in attributes_string; the fallback must resolve it"
    );
    assert_eq!(
        num(&row, "total_duration_sec"),
        999,
        "loom.total_duration_sec was seeded only in attributes_string; the fallback must \
         resolve it, not the ifNull(...,0) default a missing value would take"
    );
    assert!(
        is_null(&row, "pr_number"),
        "loom.pr_number is absent from both maps and must stay NULL, never 0"
    );
    assert!(
        is_null(&row, "doctor_cycles"),
        "loom.doctor_cycles is absent from both maps and must stay NULL, never 0"
    );
}
