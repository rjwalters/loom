//! Proof for the saved SigNoz alert rule `signoz/alerts/eta-not-emitted.json`
//! (#10898): the critical alert for the 2026-10-07/08 incident, where the ETA
//! authority covered two repos for ~31 h without emitting one `eta.estimate`
//! and nothing noticed.
//!
//! Two groups, mirroring `signoz_queue_starvation_alert.rs`:
//!
//! - Static tests read the committed JSON and run in ordinary CI.
//! - The engine tests execute the embedded query on `clickhouse local` in the
//!   pinned image (Docker) and apply the committed `op` / `target` /
//!   `matchType`; `#[ignore]`d for CI's explicit `--ignored` invocation.
//!
//! The property that matters is the one the incident violated: **a missing
//! signal must fire**. The query therefore always returns exactly one row (it
//! aggregates with no GROUP BY), because SigNoz treats "no data point" as not
//! firing, and zero `eta.estimate` logs must read as `value = 1` while
//! review-stage PRs are open, not as an empty result.
//!
//! Not established: no SigNoz rule evaluator ran and no notification (Matrix
//! delivery lives in loom-ui, #1854/#2231) was delivered.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the other engine-level proofs in the SigNoz trial.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const ALERT: &str = include_str!("../../defaults/observability/signoz/alerts/eta-not-emitted.json");
const FIXTURE: &str = include_str!("fixtures/signoz_eta_not_emitted/fixture.sql");

fn rule() -> serde_json::Value {
    serde_json::from_str(ALERT).expect("alerts/eta-not-emitted.json must be valid JSON")
}

fn embedded_query() -> String {
    rule()["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query must be a string")
        .to_owned()
}

// ===========================================================================
// Static, ordinary CI.
// ===========================================================================

#[test]
fn the_rule_is_critical_enabled_and_watches_a_two_hour_window() {
    let rule = rule();
    assert_eq!(rule["labels"]["severity"], "critical");
    assert_eq!(rule["labels"]["loom_signal"], "eta_not_emitted");
    assert_eq!(rule["alertType"], "METRIC_BASED_ALERT");
    assert_eq!(rule["evalWindow"], "2h0m0s");
    assert_eq!(rule["disabled"], false);
    assert_eq!(rule["condition"]["compositeQuery"]["chQueries"]["A"]["disabled"], false);
    assert_eq!(rule["condition"]["compositeQuery"]["queryType"], "clickhouse_sql");
    // value > 0 at least once in the window.
    assert_eq!(rule["condition"]["op"], "1");
    assert_eq!(rule["condition"]["matchType"], "1");
    assert_eq!(rule["condition"]["target"], 0);
}

#[test]
fn the_query_reads_the_signals_the_emitters_produce_and_is_window_bounded() {
    let query = embedded_query();
    for needle in [
        "signoz_logs.logs_v2",
        "loom.eta.estimate_id",
        "loom.eta.outcome",
        "loom.eta.authority",
        "loom.eta.fallback",
        "loom.forge.stage_items",
        "review_requested",
        "{{.start_timestamp_ms}}",
        "{{.end_timestamp_ms}}",
    ] {
        assert!(query.contains(needle), "the embedded query lost {needle:?}");
    }
    // A GROUP BY at the top level would let an empty log set return no row,
    // and a missing signal would then read as healthy.
    let top_level = query.split("FROM (").next().unwrap();
    assert!(!top_level.contains("GROUP BY"), "the outer query must always return one row");
}

#[test]
fn the_rule_re_evaluates_more_often_than_its_window_is_long() {
    let rule = rule();
    assert_eq!(rule["frequency"], "5m0s");
}

// ===========================================================================
// Engine proof (Docker).
// ===========================================================================

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

/// Fixed window of the fixture: 2026-10-08T00:00Z .. 02:00Z.
const START_MS: i64 = 1_791_417_600_000;
const END_MS: i64 = START_MS + 2 * 3_600_000;

/// One `loom.forge.stage_items` gauge series with its latest value in-window.
fn open_prs(fingerprint: u64, state: &str, value: u32) -> String {
    format!(
        "INSERT INTO signoz_metrics.time_series_v4 (metric_name, fingerprint, unix_milli, labels) \
         VALUES ('loom.forge.stage_items', {fingerprint}, {START_MS}, '{{\"state\":\"{state}\"}}');\n\
         INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value) \
         VALUES ('loom.forge.stage_items', {fingerprint}, {at}, {value});\n",
        at = END_MS - 60_000
    )
}

/// One `eta.estimate` log record at `ts_ms`.
fn estimate_log(ts_ms: i64) -> String {
    format!(
        "INSERT INTO signoz_logs.logs_v2 (timestamp, body, attributes_string) VALUES \
         ({}, '{{}}', map('loom.eta.estimate_id', 'e-{ts_ms}'));\n",
        ts_ms * 1_000_000
    )
}

/// An `eta.estimate` from a #10897 fallback host: it covers repos the
/// authority is not declared to cover, and is stamped `loom.eta.fallback`
/// (never `loom.eta.authority`) by `push_authority`.
fn fallback_estimate_log(ts_ms: i64) -> String {
    format!(
        "INSERT INTO signoz_logs.logs_v2 (timestamp, body, attributes_string) VALUES \
         ({}, '{{}}', map('loom.eta.estimate_id', 'e-{ts_ms}', 'loom.eta.fallback', \
         'loom-worker-2'));\n",
        ts_ms * 1_000_000
    )
}

/// An `eta.outcome`: it carries `loom.eta.estimate_id` too, but it is a
/// resolution, not an estimate arriving.
fn outcome_log(ts_ms: i64) -> String {
    format!(
        "INSERT INTO signoz_logs.logs_v2 (timestamp, body, attributes_string) VALUES \
         ({}, '{{}}', map('loom.eta.estimate_id', 'e-{ts_ms}', 'loom.eta.outcome', 'hit', \
         'loom.eta.authority', 'loom-worker-1'));\n",
        ts_ms * 1_000_000
    )
}

/// Runs the committed query over the fixture window plus `scenario`'s rows and
/// returns the values it produced.
fn values(scenario: &str) -> Vec<f64> {
    let query = embedded_query()
        .replace("{{.start_timestamp_ms}}", &START_MS.to_string())
        .replace("{{.end_timestamp_ms}}", &END_MS.to_string());
    let script = format!("{FIXTURE}\n{scenario}\n{query};");
    clickhouse(&script)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let row: serde_json::Value = serde_json::from_str(l).unwrap();
            row["value"].as_f64().unwrap()
        })
        .collect()
}

/// The committed `op: 1` / `target: 0` / `matchType: 1` decision over `values`.
fn fires(values: &[f64]) -> bool {
    let rule = rule();
    assert_eq!(rule["condition"]["op"], "1");
    assert_eq!(rule["condition"]["matchType"], "1");
    let target = rule["condition"]["target"].as_f64().unwrap();
    values.iter().any(|v| *v > target)
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_incident_fires_no_estimates_while_prs_are_open() {
    // Authority covering two repos with open review PRs and zero eta.estimate logs.
    let scenario = format!("{}{}", open_prs(1, "review_requested", 3), open_prs(2, "pr", 2));
    let out = values(&scenario);
    assert_eq!(out.len(), 1, "the query must always return exactly one row");
    assert!(fires(&out), "silence while PRs are open must fire: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn it_does_not_fire_when_estimates_exist_in_the_window() {
    let scenario =
        format!("{}{}", open_prs(1, "review_requested", 3), estimate_log(START_MS + 600_000));
    let out = values(&scenario);
    assert_eq!(out.len(), 1);
    assert!(!fires(&out), "an in-window eta.estimate must read healthy: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn it_does_not_fire_when_no_prs_are_open() {
    // Nothing to estimate: no estimates and no open PRs is a quiet fleet.
    let out = values(&open_prs(1, "review_requested", 0));
    assert_eq!(out.len(), 1);
    assert!(!fires(&out), "no open PRs must not page: {out:?}");
    let none = values("");
    assert_eq!(none.len(), 1, "even with no gauge at all the query returns a row");
    assert!(!fires(&none));
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn estimates_outside_the_half_open_window_do_not_count() {
    // One estimate one ms before the window and one exactly at its end.
    let scenario = format!(
        "{}{}{}",
        open_prs(1, "review_requested", 3),
        estimate_log(START_MS - 1),
        estimate_log(END_MS)
    );
    let out = values(&scenario);
    assert!(fires(&out), "out-of-window estimates must not mask silence: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn non_review_stages_are_not_open_prs() {
    let out = values(&open_prs(1, "building", 9));
    assert!(!fires(&out), "building issues are not review-stage PRs: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn outcomes_alone_are_not_estimates_arriving() {
    let scenario =
        format!("{}{}", open_prs(1, "review_requested", 3), outcome_log(START_MS + 600_000));
    let out = values(&scenario);
    assert!(fires(&out), "an eta.outcome must not read as an estimate emitted: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn estimates_without_the_authority_stamp_do_not_mask_a_silent_authority() {
    // A pre-#10498 host still emitting while the real authority is silent.
    let scenario = format!(
        "{}{}",
        open_prs(1, "review_requested", 3),
        unstamped_estimate_log(START_MS + 600_000)
    );
    let out = values(&scenario);
    assert!(fires(&out), "unstamped estimates must not mask silence: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_fallback_host_emitting_for_other_repos_does_not_mask_a_silent_authority() {
    // The authority has no exporter; a fallback host keeps emitting for other
    // repos while PRs are open on the authority's repos.
    let scenario = format!(
        "{}{}",
        open_prs(1, "review_requested", 3),
        fallback_estimate_log(START_MS + 600_000)
    );
    let out = values(&scenario);
    assert!(fires(&out), "fallback estimates must not mask a silent authority: {out:?}");
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_fallback_host_does_not_suppress_a_healthy_authority_either() {
    let scenario = format!(
        "{}{}{}",
        open_prs(1, "review_requested", 3),
        fallback_estimate_log(START_MS + 600_000),
        estimate_log(START_MS + 900_000)
    );
    let out = values(&scenario);
    assert!(!fires(&out), "an emitting authority must read healthy: {out:?}");
}
