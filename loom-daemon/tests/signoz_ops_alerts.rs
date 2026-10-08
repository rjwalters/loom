//! Proof for the SigNoz alert rules added by #10973 (the 2026-10-08 loom-worker-1
//! disk-full incident): `alerts/host-disk-low.json`, `alerts/host-disk-critical.json`
//! and `alerts/eta-ready-coverage.json`.
//!
//! Two groups, as in `signoz_queue_starvation_alert.rs`. The static tests read the
//! committed JSON only and run in ordinary CI. The replay tests run each rule's
//! embedded query verbatim on `clickhouse local` in the pinned image over a
//! synthetic `signoz_logs.distributed_logs_v2`, substituting SigNoz's
//! `{{.start_timestamp_nano}}` / `{{.end_timestamp_nano}}` the way its evaluator
//! does, then apply the committed `op` / `matchType` / `target`. They need Docker,
//! are `#[ignore]`d, and CI runs them with `--ignored`. No rule evaluator or
//! notification has run; this establishes the queries and thresholds, not delivery.
//!
//! Replay: free GB on one host falls linearly from 120 GB at 14:00Z to 0 at
//! 15:20Z (total 386 GB) and stays 0. The warning fires once every point in its
//! 10 minute window is under 30 GB or 10%; the critical rule once every point is
//! under 5 GB or 3%. A second replay puts a healthy and a 0 GB reading in the same
//! minute: each minute takes the min of the breach predicate, so that host never pages.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const DISK_LOW: &str =
    include_str!("../../defaults/observability/signoz/alerts/host-disk-low.json");
const DISK_CRITICAL: &str =
    include_str!("../../defaults/observability/signoz/alerts/host-disk-critical.json");
const ETA_COVERAGE: &str =
    include_str!("../../defaults/observability/signoz/alerts/eta-ready-coverage.json");
const TEMPLATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json");

/// 2026-10-08T14:00:00Z in unix seconds.
const T0: i64 = 1_791_468_000;
const TOTAL_GB: f64 = 386.0;

fn parse(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("alert JSON must parse")
}

fn query(rule: &serde_json::Value) -> String {
    rule["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query")
        .to_owned()
}

fn minutes(spec: &str) -> i64 {
    let m = spec.trim_end_matches("0s").trim_end_matches('m');
    m.parse::<i64>().expect("evalWindow like 10m0s")
}

// ---------------------------------------------------------------- static ----

#[test]
fn new_alerts_share_the_template_schema() {
    let template = parse(TEMPLATE);
    let keys =
        |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    for (name, raw) in [
        ("host-disk-low", DISK_LOW),
        ("host-disk-critical", DISK_CRITICAL),
        ("eta-ready-coverage", ETA_COVERAGE),
    ] {
        let rule = parse(raw);
        assert_eq!(keys(&rule), keys(&template), "{name}: top-level keys differ from template");
        assert_eq!(keys(&rule["condition"]), keys(&template["condition"]), "{name}: condition");
        assert_eq!(rule["ruleType"], "threshold_rule", "{name}");
        assert_eq!(rule["version"], "v4", "{name}");
        assert_eq!(rule["disabled"], false, "{name}: ships disabled = silent");
        assert_eq!(rule["condition"]["compositeQuery"]["chQueries"]["A"]["disabled"], false);
        assert!(rule["labels"]["loom_signal"].is_string(), "{name}: loom_signal label");
        assert!(
            rule["annotations"]["summary"].is_string()
                && rule["annotations"]["description"].is_string()
        );
        let q = query(&rule);
        for p in ["{{.start_timestamp_nano}}", "{{.end_timestamp_nano}}"] {
            assert!(q.contains(p), "{name}: query lacks {p}; it would scan all history");
        }
        assert!(q.contains("GROUP BY ts, host"), "{name}: must be attributable per host");
        let eval = minutes(rule["evalWindow"].as_str().unwrap());
        let freq = minutes(rule["frequency"].as_str().unwrap());
        assert!(freq <= eval, "{name}: frequency {freq}m > evalWindow {eval}m leaves gaps");
    }
}

#[test]
fn disk_alerts_key_on_d1_export_host_health_and_keep_unknown_out() {
    for (raw, free, pct, sev) in [
        (DISK_LOW, "30", "0.1", "warning"),
        (DISK_CRITICAL, "5", "0.03", "critical"),
    ] {
        let rule = parse(raw);
        let q = query(&rule);
        assert!(q.contains("'loom-ui-d1-export'"));
        assert!(q.contains("= 'host.health'"));
        // unknown != zero: a record without the field is excluded, not treated as 0 GB.
        assert!(q.contains("JSONHas(body, 'worktree_root_free_gb')"));
        assert!(q.contains(&format!("< {free}\n")), "absolute GB threshold {free}");
        assert!(q.contains(&format!("< {pct})")), "fraction threshold {pct}");
        // "every record breaches": a minute bucket is 1 only if all of its records
        // breach. max() would let one breaching record hide a healthy one.
        assert!(q.contains("min(toUInt8("), "per-minute breach must be min(), not max()");
        assert!(!q.contains("max(toUInt8("), "max() discards healthy readings");
        assert_eq!(rule["labels"]["severity"], sev);
        assert_eq!(rule["condition"]["op"], "1");
        assert_eq!(rule["condition"]["target"], 0);
    }
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

const SCHEMA: &str = "CREATE DATABASE signoz_logs;\n\
CREATE TABLE signoz_logs.distributed_logs_v2 (timestamp UInt64, \
resources_string Map(String, String), attributes_string Map(String, String), body String) \
ENGINE = Memory;\n";

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn insert(ts_sec: i64, res: &[(&str, &str)], attrs: &[(&str, &str)], body: &str) -> String {
    let map = |kv: &[(&str, &str)]| {
        let items: Vec<String> = kv
            .iter()
            .map(|(k, v)| format!("'{}', '{}'", esc(k), esc(v)))
            .collect();
        format!("map({})", items.join(", "))
    };
    format!(
        "INSERT INTO signoz_logs.distributed_logs_v2 VALUES ({}, {}, {}, '{}');\n",
        ts_sec * 1_000_000_000,
        map(res),
        map(attrs),
        esc(body)
    )
}

fn host_health(ts_sec: i64, host: &str, free: Option<f64>, total: Option<f64>) -> String {
    let mut body = format!("{{\"kind\":\"host.health\",\"host_id\":\"{host}\"");
    if let Some(f) = free {
        body.push_str(&format!(",\"worktree_root_free_gb\":{f}"));
    }
    if let Some(t) = total {
        body.push_str(&format!(",\"worktree_root_total_gb\":{t}"));
    }
    body.push('}');
    insert(ts_sec, &[("service.name", "loom-ui-d1-export")], &[], &body)
}

fn disk_fixture() -> String {
    let mut s = String::from(SCHEMA);
    for min in 0..120 {
        let ts = T0 + min * 60;
        // The incident host: 120 GB falling 1.5 GB/min to a flat 0 at 15:20Z.
        let free = (120.0 - 1.5 * min as f64).max(0.0);
        s += &host_health(ts, "loom-worker-1", Some(free), Some(TOTAL_GB));
        // Healthy host.
        s += &host_health(ts, "loom-worker-2", Some(250.0), Some(TOTAL_GB));
        // Total unknown, absolute breach only.
        s += &host_health(ts, "loom-worker-3", Some(20.0), None);
        // free unmeasurable: must never page as zero.
        s += &host_health(ts, "loom-worker-4", None, Some(TOTAL_GB));
    }
    // A non-host.health row from the same service must not count.
    s += &insert(
        T0,
        &[("service.name", "loom-ui-d1-export")],
        &[],
        "{\"kind\":\"label.transition\",\"host_id\":\"loom-worker-5\",\"worktree_root_free_gb\":0}",
    );
    s
}

/// Hosts whose series, over the window ending at `end`, satisfy the committed
/// op/matchType (all-the-times / at-least-once) against the committed target.
fn firing(raw: &str, fixture: &str, end_sec: i64) -> Vec<String> {
    let rule = parse(raw);
    let window = minutes(rule["evalWindow"].as_str().unwrap()) * 60;
    let start = (end_sec - window) * 1_000_000_000;
    let end = end_sec * 1_000_000_000;
    let q = query(&rule)
        .replace("{{.start_timestamp_nano}}", &start.to_string())
        .replace("{{.end_timestamp_nano}}", &end.to_string());
    let out = clickhouse(&format!("{fixture}\n{q};"));
    let mut by_host: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        by_host
            .entry(row["host"].as_str().unwrap().to_owned())
            .or_default()
            .push(row["value"].as_f64().unwrap());
    }
    let target = rule["condition"]["target"].as_f64().unwrap();
    assert_eq!(rule["condition"]["op"], "1", "test models 'above' only");
    let all = match rule["condition"]["matchType"].as_str().unwrap() {
        "2" => true,
        "1" => false,
        other => panic!("matchType {other} not modelled"),
    };
    by_host
        .into_iter()
        .filter(|(_, v)| {
            if all {
                v.iter().all(|x| *x > target)
            } else {
                v.iter().any(|x| *x > target)
            }
        })
        .map(|(h, _)| h)
        .collect()
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_free_gb_falling_to_zero_trips_warning_then_critical() {
    let f = disk_fixture();
    let at = |hh: i64, mm: i64| T0 + (hh - 14) * 3600 + mm * 60;
    // 14:10 -- 120..105 GB: nothing but the absolute-breach host without a total.
    assert_eq!(firing(DISK_LOW, &f, at(14, 10)), ["loom-worker-3"]);
    assert!(firing(DISK_CRITICAL, &f, at(14, 10)).is_empty());
    // 15:00 -- 45..30 GB: above 10% of 386 GB (38.6) for part of the window.
    assert_eq!(firing(DISK_LOW, &f, at(15, 0)), ["loom-worker-3"]);
    // 15:10 -- 30..15 GB: under 10% for the whole window: warning fires, critical does not.
    assert_eq!(firing(DISK_LOW, &f, at(15, 10)), ["loom-worker-1", "loom-worker-3"]);
    assert!(firing(DISK_CRITICAL, &f, at(15, 10)).is_empty());
    // 15:30 -- flat 0 GB for the whole window: critical fires for the incident host
    // only (worker-3 at 20 GB is warning-only; worker-4 never measured; worker-5 is
    // a different record kind; worker-2 is healthy).
    assert_eq!(firing(DISK_CRITICAL, &f, at(15, 30)), ["loom-worker-1"]);
    assert_eq!(firing(DISK_LOW, &f, at(15, 30)), ["loom-worker-1", "loom-worker-3"]);
    // The incident's own window (the 14:00-16:00 replay) fires within one interval:
    // free hits 0 at 15:20Z, the 10 minute window is all-zero by 15:30Z.
}

/// Two readings per host per minute. `loom-worker-6` alternates 0 GB and a
/// recovered 100 GB inside every minute, so only half its records breach: neither
/// rule may page (a per-minute max() of the breach predicate would). `loom-worker-7`
/// reports 0 GB twice a minute, so every record breaches and both rules page.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_mixed_readings_within_a_minute_do_not_page() {
    let mut f = String::from(SCHEMA);
    for min in 0..120 {
        let ts = T0 + min * 60;
        f += &host_health(ts, "loom-worker-6", Some(0.0), Some(TOTAL_GB));
        f += &host_health(ts + 30, "loom-worker-6", Some(100.0), Some(TOTAL_GB));
        f += &host_health(ts, "loom-worker-7", Some(0.0), Some(TOTAL_GB));
        f += &host_health(ts + 30, "loom-worker-7", Some(0.0), Some(TOTAL_GB));
    }
    let at = |hh: i64, mm: i64| T0 + (hh - 14) * 3600 + mm * 60;
    for end in [at(14, 10), at(15, 0), at(15, 30)] {
        assert_eq!(firing(DISK_LOW, &f, end), ["loom-worker-7"]);
        assert_eq!(firing(DISK_CRITICAL, &f, end), ["loom-worker-7"]);
    }
}

fn eta_row(ts_sec: i64, host: &str, stage: &str, reason: Option<&str>) -> String {
    let mut attrs = vec![
        ("loom.eta.trigger", "start"),
        ("loom.eta.stage", stage),
        ("loom.eta.estimate_id", "x"),
    ];
    if let Some(r) = reason {
        attrs.push(("loom.eta.no_estimate_reason", r));
    }
    insert(ts_sec, &[("host.id", host)], &attrs, "{}")
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_all_stale_inputs_ready_refusals_trip_the_coverage_alert() {
    let mut f = String::from(SCHEMA);
    let at = |hh: i64, mm: i64| T0 + (hh - 14) * 3600 + mm * 60;
    // Authority, healthy until 14:20: Ready estimates answered.
    for m in (0..20).step_by(5) {
        f += &eta_row(at(14, m), "loom-worker-1", "ready_wait", None);
    }
    // From 14:20 every Ready item is a stale_inputs refusal; PR-stage rows continue,
    // mostly no_model -- exactly the incident's shape.
    for m in 20..50 {
        f += &eta_row(at(14, m), "loom-worker-1", "ready_wait", Some("stale_inputs"));
        f += &eta_row(at(14, m), "loom-worker-1", "review_wait", Some("no_model"));
    }
    // A healthy host: mostly answered, some stale_inputs (answered > 0 => no alert).
    for m in 0..50 {
        let r = if m % 3 == 0 {
            Some("stale_inputs")
        } else {
            None
        };
        f += &eta_row(at(14, m), "loom-worker-2", "ready_wait", r);
    }
    assert!(firing(ETA_COVERAGE, &f, at(14, 20)).is_empty(), "before the outage");
    assert_eq!(firing(ETA_COVERAGE, &f, at(14, 40)), ["loom-worker-1"]);
    assert_eq!(firing(ETA_COVERAGE, &f, at(15, 0)), ["loom-worker-1"]);
}
