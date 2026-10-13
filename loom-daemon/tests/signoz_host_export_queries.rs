//! Proof for `host-export-queries.sql` and `alerts/host-export-silent.json` (#11198).
//!
//! Static tests read the committed files and run in ordinary CI. The replay test
//! runs queries 1-4 verbatim, and the alert's embedded query with SigNoz's
//! `{{.start_timestamp_nano}}` / `{{.end_timestamp_nano}}` substituted, on
//! `clickhouse local` in the pinned image over a synthetic
//! `signoz_logs.distributed_logs_v2`. It needs Docker, is `#[ignore]`d, and CI
//! runs it with `--ignored`. No rule evaluator or notification has run.
//!
//! The roster comes from fleet-gitops `hosts.yml`; the committed rows are
//! placeholders, and the replay substitutes a fixture roster between the markers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::{Command, Stdio};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const SQL: &str = include_str!("../../defaults/observability/signoz/host-export-queries.sql");
const ALERT: &str =
    include_str!("../../defaults/observability/signoz/alerts/host-export-silent.json");
const TEMPLATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json");
const FIXTURE: &str = include_str!("fixtures/signoz_host_export/fixture.sql");

const BEGIN: &str = "/* loom:roster:begin */";
const END: &str = "/* loom:roster:end */";

const FIXTURE_ROSTER: &str = "('a-ok','always'), ('a-silent','always'), ('a-down','always'), \
('a-edge','always'), ('w-asleep','when_awake'), ('w-gone','when_awake'), ('w-never','when_awake')";

fn alert() -> serde_json::Value {
    serde_json::from_str(ALERT).expect("alert JSON must parse")
}

fn alert_query() -> String {
    alert()["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query")
        .to_owned()
}

fn roster_of(text: &str) -> &str {
    assert_eq!(text.matches(BEGIN).count(), 1, "one begin marker");
    assert_eq!(text.matches(END).count(), 1, "one end marker");
    let (_, rest) = text.split_once(BEGIN).unwrap();
    rest.split_once(END).unwrap().0
}

fn with_roster(text: &str, roster: &str) -> String {
    let (head, rest) = text.split_once(BEGIN).unwrap();
    let (_, tail) = rest.split_once(END).unwrap();
    format!("{head}{BEGIN} {roster} {END}{tail}")
}

// ---------------------------------------------------------------- static ----

#[test]
fn alert_shares_the_template_schema() {
    let template: serde_json::Value = serde_json::from_str(TEMPLATE).unwrap();
    let rule = alert();
    let keys =
        |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&rule), keys(&template));
    assert_eq!(keys(&rule["condition"]), keys(&template["condition"]));
    assert_eq!(rule["alertType"], "LOGS_BASED_ALERT");
    assert_eq!(rule["ruleType"], "threshold_rule");
    assert_eq!(rule["version"], "v4");
    assert_eq!(rule["disabled"], false, "ships disabled = silent");
    assert_eq!(rule["labels"]["loom_signal"], "host_export_silent");
    assert_eq!(rule["condition"]["op"], "1");
    assert_eq!(rule["condition"]["target"], 0);
    let q = alert_query();
    assert!(q.contains("{{.start_timestamp_nano}}") && q.contains("{{.end_timestamp_nano}}"));
    assert!(q.contains("GROUP BY host"), "per-host attribution");
    // A when_awake host silent for exactly the whole window must still cross its
    // 86400 s threshold, and a never-seen host is silent for the whole window.
    let eval = rule["evalWindow"].as_str().unwrap();
    assert_eq!(eval, "48h0m0s", "window must exceed the 24 h when_awake threshold");
    assert_eq!(rule["frequency"], "5m0s");
}

#[test]
fn roster_markers_and_thresholds_match_between_sql_and_alert() {
    let q = alert_query();
    for (name, text) in [("sql", SQL), ("alert", q.as_str())] {
        let roster = roster_of(text);
        assert!(text.contains("900"), "{name}: always threshold");
        assert!(text.contains("86400"), "{name}: when_awake threshold");
        assert!(roster.contains("example-"), "{name}: committed roster is placeholders");
    }
    assert_eq!(
        roster_of(SQL).split_whitespace().collect::<String>(),
        roster_of(&q).split_whitespace().collect::<String>(),
        "committed rosters differ"
    );
}

#[test]
fn committed_roster_rows_are_valid() {
    let roster = roster_of(SQL);
    let rows: Vec<&str> = roster.split("),").collect();
    assert!(!rows.is_empty());
    for row in rows {
        let cells: Vec<&str> = row
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .map(|c| c.trim().trim_matches('\''))
            .collect();
        assert_eq!(cells.len(), 2, "row {row:?}");
        assert!(!cells[0].is_empty());
        assert!(["always", "when_awake"].contains(&cells[1]), "telemetry {:?}", cells[1]);
    }
}

#[test]
fn sql_status_header_no_longer_claims_unexecuted() {
    assert!(!SQL.to_lowercase().contains("not yet executed in ci"));
}

// ---------------------------------------------------------------- replay ----

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

fn rows(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The text of `host-export-queries.sql` query `n` (1-based), verbatim.
fn sql_query(n: usize) -> String {
    let start = SQL
        .find(&format!("\n-- {n}. "))
        .unwrap_or_else(|| panic!("query {n}"));
    let end = SQL[start + 1..]
        .find(&format!("\n-- {}. ", n + 1))
        .map_or(SQL.len(), |e| start + 1 + e);
    SQL[start..end].to_owned()
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_queries_find_silent_roster_hosts_and_drop_steps() {
    // Queries 1 and 2: increment, no-increase, reset, no-baseline, baseline.
    let q1: BTreeMap<String, f64> = rows(&clickhouse(&format!("{FIXTURE}\n{}", sql_query(1))))
        .iter()
        .map(|r| (r["host"].as_str().unwrap().to_owned(), r["dropped_in_window"].as_f64().unwrap()))
        .collect();
    let want: BTreeMap<String, f64> = [
        ("q1-inc", 3.0),
        ("q1-reset", 6.0),  // 10 -> 14 is +4, then a reset to 2 contributes 2
        ("q1-nobase", 2.0), // first in-window value is an unknown baseline: 0
        ("q1-base", 2.0),   // baseline 4 before the window, 6 inside it
    ]
    .into_iter()
    .map(|(h, v)| (h.to_owned(), v))
    .collect();
    assert_eq!(q1, want, "q1-flat (no increase) must be absent");

    let q2: BTreeSet<String> = rows(&clickhouse(&format!("{FIXTURE}\n{}", sql_query(2))))
        .iter()
        .map(|r| r["host"].as_str().unwrap().to_owned())
        .collect();
    let want2: BTreeSet<String> = ["w-gone", "w-never", "off-roster"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(q2, want2);

    // Query 3 over the fixture roster.
    let q3 = rows(&clickhouse(&format!(
        "{FIXTURE}\n{}",
        with_roster(&sql_query(3), FIXTURE_ROSTER)
    )));
    let got: BTreeMap<String, String> = q3
        .iter()
        .map(|r| (r["host"].as_str().unwrap().to_owned(), r["state"].as_str().unwrap().to_owned()))
        .collect();
    let expect: BTreeMap<String, String> = [
        ("a-silent", "exporter_silent"),
        ("a-down", "host_silent"),
        ("w-gone", "exporter_silent"),
        ("w-never", "exporter_silent"),
    ]
    .into_iter()
    .map(|(h, s)| (h.to_owned(), s.to_owned()))
    .collect();
    assert_eq!(got, expect, "a-ok, a-edge, w-asleep, off-roster must not be reported");
    let never = q3.iter().find(|r| r["host"] == "w-never").unwrap();
    assert!(never["last_export"].is_null(), "never-seen host has a NULL last_export");

    // The alert's embedded query, placeholders substituted as SigNoz does,
    // then the committed op/target applied.
    let rule = alert();
    assert_eq!(rule["condition"]["op"], "1");
    let target = rule["condition"]["target"].as_f64().unwrap();
    let end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let start = end - 48 * 3600;
    let q = with_roster(&alert_query(), FIXTURE_ROSTER)
        .replace("{{.start_timestamp_nano}}", &(start * 1_000_000_000).to_string())
        .replace("{{.end_timestamp_nano}}", &(end * 1_000_000_000).to_string());
    let fired: BTreeSet<String> = rows(&clickhouse(&format!("{FIXTURE}\n{q};")))
        .iter()
        .filter(|r| r["value"].as_f64().unwrap() > target)
        .map(|r| r["host"].as_str().unwrap().to_owned())
        .collect();
    let want_alert: BTreeSet<String> = expect.keys().cloned().collect();
    assert_eq!(fired, want_alert);
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_exporter_loss_keeps_reset_steps_and_unknown_coverage() {
    // Change only the clock expression, not the committed accounting SQL.
    let query = sql_query(4).replace("now64(9)", "toDateTime64('2026-10-12 17:10:00', 9, 'UTC')");
    let fixture = include_str!("fixtures/signoz_host_export/loss_steps.sql");
    let number = |v: &serde_json::Value| {
        v.as_f64()
            .unwrap_or_else(|| v.as_str().unwrap().parse::<f64>().unwrap())
    };
    let output = rows(&clickhouse(&format!("{fixture}\n{query}")));
    let losses: BTreeMap<(String, String, String), (f64, f64)> = output
        .iter()
        .map(|r| {
            (
                (
                    r["host"].as_str().unwrap().to_owned(),
                    r["exporter"].as_str().unwrap().to_owned(),
                    r["reason"].as_str().unwrap().to_owned(),
                ),
                (number(&r["delta"]), number(&r["reason_delta"])),
            )
        })
        .collect();
    let expected = [
        ("captain", "queue_overflow", 2825.0, 1584.0),
        ("captain", "unattributed", 2825.0, 1241.0),
        ("unknown", "queue_overflow", 2.0, 2.0),
        ("unknown-flat", "coverage_unknown", 0.0, 0.0),
        ("reason-reset", "a", 6.0, 4.0),
        ("reason-reset", "b", 6.0, 2.0),
        ("legacy", "a", 3.0, 1.0),
        ("legacy", "unattributed", 3.0, 2.0),
        ("inconsistent", "unattributed", 2.0, 2.0),
        ("conflict", "coverage_unknown", 0.0, 0.0),
    ]
    .into_iter()
    .map(|(h, r, total, reason)| {
        ((h.to_owned(), "https".to_owned(), r.to_owned()), (total, reason))
    })
    .collect();
    assert_eq!(losses, expected, "flat, other exporter and future sample are not loss");
    let captain = output.iter().find(|r| r["host"] == "captain").unwrap();
    assert_eq!(number(&captain["samples"]), 6.0, "repeat delivery is one sample");
    assert_eq!(number(&captain["reset_count"]), 1.0);
    assert_eq!(number(&captain["baseline_known"]), 1.0);
    assert_eq!(number(&captain["max_queue_depth"]), 2.0);
    assert_eq!(captain["versions"], serde_json::json!(["0.19.1127"]));
    for row in &output {
        let host = row["host"].as_str().unwrap();
        if host.starts_with("unknown") {
            assert_eq!(number(&row["baseline_known"]), 0.0);
        }
        if host == "reason-reset" {
            assert_eq!(number(&row["reset_count"]), 2.0);
        }
        if host == "legacy" || host == "inconsistent" {
            assert!(number(&row["unknown_reason_steps"]) > 0.0);
        }
        if host == "conflict" {
            assert_eq!(number(&row["conflicting_samples"]), 1.0);
        }
    }
}
