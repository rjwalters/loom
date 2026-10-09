//! Proof for the SigNoz alert rules added by #10973 (the 2026-10-08 loom-worker-1
//! disk-full incident): `alerts/host-disk-low.json`, `alerts/host-disk-critical.json`,
//! `alerts/work-finder-stale.json` and `alerts/eta-ready-coverage.json`.
//!
//! Two groups, as in `signoz_queue_starvation_alert.rs`. The static tests read the
//! committed JSON only and run in ordinary CI. The replay tests run each rule's
//! embedded query verbatim on `clickhouse local` in the pinned image over a
//! synthetic `signoz_metrics` / `signoz_logs` read surface, substituting SigNoz's
//! `{{.start_timestamp_*}}` / `{{.end_timestamp_*}}` the way its evaluator does,
//! then apply the committed `op` / `matchType` / `target`. They need Docker, are
//! `#[ignore]`d, and CI runs them with `--ignored`. No rule evaluator or
//! notification has run; this establishes the queries and thresholds, not delivery.
//!
//! The disk rules key on the direct OTLP gauges (`loom.host.worktree_root_free_gb`,
//! `loom.host.worktree_volume.{free,total}_bytes`), labelled `host.name` with a
//! `host.id` fallback, and carry a time-to-full arm (least-squares slope of free GB
//! over the 1 h window): warning under 6 h, critical under 1 h (operator direction
//! on #10973). Replay: free GB on the incident host falls from 120 GB at 14:00Z to 0
//! at 15:20Z (total 386 GB) and stays 0; a second host drains 20 GB/h from 99 GB, the
//! rate the operator measured on loom-worker-1 after its 19:00Z recovery.
//!
//! The work-finder rule fires when a host still heartbeating has not exported a
//! `loom.queue.issues` tick for more than 2x its own observed tick interval
//! (floored at 300 s). Replay: loom-worker-1's finder stops at 14:04Z while
//! `host.health` continues, as on 2026-10-08.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const DISK_LOW: &str =
    include_str!("../../defaults/observability/signoz/alerts/host-disk-low.json");
const DISK_CRITICAL: &str =
    include_str!("../../defaults/observability/signoz/alerts/host-disk-critical.json");
const WORK_FINDER: &str =
    include_str!("../../defaults/observability/signoz/alerts/work-finder-stale.json");
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

/// `evalWindow` / `frequency` like `10m0s` or `24h0m0s`, in minutes.
fn minutes(spec: &str) -> i64 {
    let (h, rest) = spec
        .split_once('h')
        .map_or((0, spec), |(h, r)| (h.parse::<i64>().expect("hours"), r));
    let m = rest.trim_end_matches("0s").trim_end_matches('m');
    h * 60 + m.parse::<i64>().expect("evalWindow like 10m0s")
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
        ("work-finder-stale", WORK_FINDER),
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
        let placeholders = match rule["alertType"].as_str().unwrap() {
            "METRIC_BASED_ALERT" => ["{{.start_timestamp_ms}}", "{{.end_timestamp_ms}}"],
            "LOGS_BASED_ALERT" => ["{{.start_timestamp_nano}}", "{{.end_timestamp_nano}}"],
            other => panic!("{name}: alertType {other} not modelled"),
        };
        for p in placeholders {
            assert!(q.contains(p), "{name}: query lacks {p}; it would scan all history");
        }
        assert!(
            q.contains("GROUP BY ts, host") || q.contains("GROUP BY host"),
            "{name}: must be attributable per host"
        );
        let eval = minutes(rule["evalWindow"].as_str().unwrap());
        let freq = minutes(rule["frequency"].as_str().unwrap());
        assert!(freq <= eval, "{name}: frequency {freq}m > evalWindow {eval}m leaves gaps");
    }
}

#[test]
fn disk_alerts_key_on_the_direct_metric_with_time_to_full() {
    for (raw, free, pct, ttf, sev) in [
        (DISK_LOW, "30", "0.1", "6", "warning"),
        (DISK_CRITICAL, "5", "0.03", "1", "critical"),
    ] {
        let rule = parse(raw);
        let q = query(&rule);
        assert_eq!(rule["alertType"], "METRIC_BASED_ALERT");
        assert!(!q.contains("loom-ui-d1-export"), "no d1-export hop (operator, #10973)");
        assert!(q.contains("'loom.host.worktree_root_free_gb'"));
        // host.name, falling back to host.id for the series family with an empty host.name.
        assert!(q.contains("JSONExtractString(labels, 'host.name') != ''"));
        assert!(q.contains("JSONExtractString(labels, 'host.id')"));
        // unknown != zero: a host with no reading yields NULL, never 0 GB.
        assert!(q.contains("maxOrNullIf(value, metric = 'loom.host.worktree_root_free_gb'"));
        // "every reading breaches" over the last 10 min is max(free) < threshold.
        assert!(q.contains(&format!("recent_max_gb < {free},")), "absolute GB {free}");
        assert!(q.contains(&format!("/ total_bytes < {pct},")), "fraction {pct}");
        assert!(
            q.contains(&format!("latest_gb / -gb_per_hour < {ttf})")),
            "time to full {ttf} h"
        );
        assert!(q.contains("simpleLinearRegressionIf"), "slope over the window");
        assert_eq!(rule["evalWindow"], "60m0s", "time to full uses the slope over 1 h");
        assert_eq!(rule["labels"]["severity"], sev);
        assert_eq!(rule["condition"]["op"], "1");
        assert_eq!(rule["condition"]["target"], 0);
    }
}

/// Every metric the rules read is one the daemon emits under that exact name.
/// A rename would otherwise leave the disk rules silent and make the work-finder
/// rule treat every host as "never ticked", which it does not judge.
#[test]
fn every_metric_the_rules_read_is_emitted_by_the_daemon() {
    let emitted = [
        include_str!("../src/telemetry/ops.rs"),
        include_str!("../src/observability/otlp/mapping.rs"),
    ]
    .concat();
    for metric in [
        "loom.host.worktree_root_free_gb",
        "loom.host.worktree_volume.free_bytes",
        "loom.host.worktree_volume.total_bytes",
        "loom.host.uptime_seconds",
        "loom.queue.issues",
    ] {
        assert!(emitted.contains(&format!("\"{metric}\"")), "daemon no longer emits {metric}");
        let quoted = format!("'{metric}'");
        assert!(
            [DISK_LOW, DISK_CRITICAL, WORK_FINDER]
                .iter()
                .any(|r| query(&parse(r)).contains(&quoted)),
            "{metric} is listed here but no rule reads it"
        );
    }
}

#[test]
fn work_finder_rule_judges_live_hosts_on_their_own_interval() {
    let rule = parse(WORK_FINDER);
    let q = query(&rule);
    assert_eq!(rule["alertType"], "METRIC_BASED_ALERT");
    assert!(q.contains("metric = 'loom.queue.issues'"), "ticks come from the per-tick gauge");
    assert!(q.contains("metric = 'loom.host.uptime_seconds'"), "liveness is host.health");
    assert!(q.contains("greatest(300, 2 * "), "2x the observed interval, floored at 300 s");
    assert!(q.contains("WHERE length(ticks) > 0"), "a disabled finder is not judged");
    assert!(q.contains("last_health_ms >= end_ms - 600000"), "a dead host is not judged");
    assert_eq!(rule["labels"]["severity"], "critical");
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

const SCHEMA: &str = "CREATE DATABASE signoz_logs;\n\
CREATE TABLE signoz_logs.distributed_logs_v2 (timestamp UInt64, \
resources_string Map(String, String), attributes_string Map(String, String), body String) \
ENGINE = Memory;\n";

const METRICS_SCHEMA: &str = "CREATE DATABASE signoz_metrics;\n\
CREATE TABLE signoz_metrics.samples_v4 (metric_name LowCardinality(String), fingerprint UInt64, \
unix_milli Int64, value Float64) ENGINE = Memory;\n\
CREATE TABLE signoz_metrics.time_series_v4 (metric_name LowCardinality(String), \
fingerprint UInt64, unix_milli Int64, labels String) ENGINE = Memory;\n";

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

/// A synthetic SigNoz metrics store: one `time_series_v4` row per series
/// (fingerprint) and its `samples_v4` points.
#[derive(Default)]
struct Metrics {
    sql: String,
    samples: Vec<String>,
    series: BTreeMap<String, u64>,
}

impl Metrics {
    /// Fingerprint of (metric, host.name, host.id, extra label), registering the
    /// series on first use. An empty `name` models the series family whose
    /// `host.name` is empty (operator note on #10973).
    fn fp(&mut self, metric: &str, name: &str, id: &str, extra: &str) -> u64 {
        let key = format!("{metric}|{name}|{id}|{extra}");
        if let Some(fp) = self.series.get(&key) {
            return *fp;
        }
        let fp = self.series.len() as u64 + 1;
        self.series.insert(key, fp);
        let labels = format!(
            "{{\"__name__\":\"{metric}\",\"host.name\":\"{name}\",\"host.id\":\"{id}\",\"state\":\"{extra}\"}}"
        );
        // SigNoz writes one time_series_v4 row per series per hour: two rows here,
        // so a join that did not de-duplicate fingerprints would double every point.
        for hour in [0, 1] {
            self.sql += &format!(
                "INSERT INTO signoz_metrics.time_series_v4 VALUES ('{metric}', {fp}, {}, '{}');\n",
                (T0 + hour * 3600) * 1000,
                esc(&labels)
            );
        }
        fp
    }

    fn point(&mut self, metric: &str, host: &str, ts_sec: i64, value: f64) {
        self.point_with(metric, host, &format!("id-{host}"), "", ts_sec, value);
    }

    fn point_with(&mut self, metric: &str, name: &str, id: &str, extra: &str, ts: i64, v: f64) {
        let fp = self.fp(metric, name, id, extra);
        self.samples
            .push(format!("('{metric}', {fp}, {}, {v})", ts * 1000));
    }

    /// One `host.health` disk sample: `free` GB (`None` = unmeasurable, no point),
    /// with the volume byte gauges when `total` is known.
    fn disk(&mut self, host: &str, ts: i64, free: Option<f64>, total: Option<f64>) {
        if let Some(f) = free {
            self.point("loom.host.worktree_root_free_gb", host, ts, f);
            if total.is_some() {
                self.point("loom.host.worktree_volume.free_bytes", host, ts, f * 1e9);
            }
        }
        if let Some(t) = total {
            self.point("loom.host.worktree_volume.total_bytes", host, ts, t * 1e9);
        }
    }

    /// The schema, the series rows, then every sample in one INSERT (one
    /// statement per sample made the replay several times slower).
    fn script(&self) -> String {
        format!(
            "{METRICS_SCHEMA}{}INSERT INTO signoz_metrics.samples_v4 VALUES {};\n",
            self.sql,
            self.samples.join(", ")
        )
    }
}

const FREE_GB: &str = "loom.host.worktree_root_free_gb";

/// 14:00Z to 17:00Z, one disk sample per host per minute.
fn disk_fixture() -> String {
    let mut m = Metrics::default();
    for min in 0..180 {
        let ts = T0 + min * 60;
        let mf = min as f64;
        // The incident host: 120 GB falling 1.5 GB/min (90 GB/h) to a flat 0 at 15:20Z.
        m.disk("loom-worker-1", ts, Some((120.0 - 1.5 * mf).max(0.0)), Some(TOTAL_GB));
        // Healthy host.
        m.disk("loom-worker-2", ts, Some(250.0), Some(TOTAL_GB));
        // Volume size unknown: judged on the absolute arm only.
        m.disk("loom-worker-3", ts, Some(20.0), None);
        // Free space unmeasurable: must never page as zero.
        m.disk("loom-worker-4", ts, None, Some(TOTAL_GB));
        // The operator's post-recovery drain: 99 GB falling 20 GB/h.
        m.disk("loom-worker-8", ts, Some(99.0 - mf / 3.0), Some(TOTAL_GB));
        // Recovering after a reclaim: 40 GB rising 1 GB/min.
        m.disk("loom-worker-9", ts, Some(40.0 + mf), Some(TOTAL_GB));
        // Flat 4 GB: critical on the absolute arm with a zero slope.
        m.disk("loom-worker-11", ts, Some(4.0), Some(TOTAL_GB));
        // Flat 35 GB of 386 GB (9.1%): warning on the percent arm only.
        m.disk("loom-worker-12", ts, Some(35.0), Some(TOTAL_GB));
        // Empty host.name: labelled by host.id.
        m.point_with(FREE_GB, "", "host-e1d4c843", "", ts, 2.0);
    }
    m.script()
}

/// Hosts whose series, over the window ending at `end`, satisfy the committed
/// op/matchType (all-the-times / at-least-once) against the committed target.
fn firing(raw: &str, fixture: &str, end_sec: i64) -> Vec<String> {
    let rule = parse(raw);
    let window = minutes(rule["evalWindow"].as_str().unwrap()) * 60;
    let (start, end) = (end_sec - window, end_sec);
    let q = query(&rule)
        .replace("{{.start_timestamp_nano}}", &(start * 1_000_000_000).to_string())
        .replace("{{.end_timestamp_nano}}", &(end * 1_000_000_000).to_string())
        .replace("{{.start_timestamp_ms}}", &(start * 1000).to_string())
        .replace("{{.end_timestamp_ms}}", &(end * 1000).to_string());
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

fn at(hh: i64, mm: i64) -> i64 {
    T0 + (hh - 14) * 3600 + mm * 60
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_free_gb_falling_to_zero_trips_warning_then_critical() {
    let f = disk_fixture();
    // Always firing on level alone, whatever the slope: worker-3 (20 GB, warning),
    // worker-11 (4 GB, both), worker-12 (9.1%, warning), host-e1d4c843 (2 GB, both).
    let level_low = [
        "host-e1d4c843",
        "loom-worker-11",
        "loom-worker-12",
        "loom-worker-3",
    ];
    let level_crit = ["host-e1d4c843", "loom-worker-11"];
    let with = |base: &[&str], extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
        v.sort();
        v
    };

    // 14:10 -- the incident host has 9 minutes of history: too short a span for a
    // slope, and 105+ GB is above every level. Not yet.
    assert_eq!(firing(DISK_LOW, &f, at(14, 10)), with(&level_low, &[]));
    assert_eq!(firing(DISK_CRITICAL, &f, at(14, 10)), with(&level_crit, &[]));
    // 14:30 -- 29 minutes of history, still under the 30 minute minimum span.
    assert_eq!(firing(DISK_CRITICAL, &f, at(14, 30)), with(&level_crit, &[]));
    // 14:31 -- 30 minutes at -90 GB/h with 75 GB left: full in ~50 min, so the
    // critical time-to-full arm pages 50 minutes BEFORE the disk is full, and
    // the warning with it. worker-8 (20 GB/h, 89 GB) projects ~4.4 h: warning only.
    assert_eq!(
        firing(DISK_LOW, &f, at(14, 31)),
        with(&level_low, &["loom-worker-1", "loom-worker-8"])
    );
    assert_eq!(firing(DISK_CRITICAL, &f, at(14, 31)), with(&level_crit, &["loom-worker-1"]));
    // 15:30 -- flat 0 GB for the last 10 minutes: both arms page.
    assert_eq!(firing(DISK_CRITICAL, &f, at(15, 30)), with(&level_crit, &["loom-worker-1"]));
    // 17:00 -- 0 GB with a flat slope for the whole hour: the level arm alone keeps
    // the critical page up. worker-8 is at 40 GB (10.3%, above every level), but
    // ~2 h from full: warning on time-to-full alone, never critical.
    assert_eq!(
        firing(DISK_LOW, &f, at(17, 0)),
        with(&level_low, &["loom-worker-1", "loom-worker-8"])
    );
    assert_eq!(firing(DISK_CRITICAL, &f, at(17, 0)), with(&level_crit, &["loom-worker-1"]));
    // Never: worker-2 (healthy), worker-4 (unmeasured), worker-9 (recovering).
}

/// Two readings per host per minute. `loom-worker-6` alternates 0 GB and a
/// recovered 100 GB inside every minute, so only half its readings breach and its
/// slope is flat: neither rule may page. `loom-worker-7` reports 0 GB twice a
/// minute, so every reading breaches and both rules page.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_mixed_readings_within_a_minute_do_not_page() {
    let mut m = Metrics::default();
    for min in 0..120 {
        let ts = T0 + min * 60;
        m.disk("loom-worker-6", ts, Some(0.0), Some(TOTAL_GB));
        m.disk("loom-worker-6", ts + 30, Some(100.0), Some(TOTAL_GB));
        m.disk("loom-worker-7", ts, Some(0.0), Some(TOTAL_GB));
        m.disk("loom-worker-7", ts + 30, Some(0.0), Some(TOTAL_GB));
    }
    let f = m.script();
    for end in [at(14, 10), at(15, 0), at(15, 30)] {
        assert_eq!(firing(DISK_LOW, &f, end), ["loom-worker-7"]);
        assert_eq!(firing(DISK_CRITICAL, &f, end), ["loom-worker-7"]);
    }
}

/// One work-finder tick: the `loom.queue.issues` gauges a tick exports, all
/// stamped with the same instant (two dispositions here).
fn tick(m: &mut Metrics, name: &str, id: &str, ts: i64) {
    for state in ["ready", "running"] {
        m.point_with("loom.queue.issues", name, id, state, ts, 1.0);
    }
}

fn heartbeat(m: &mut Metrics, name: &str, id: &str, ts: i64) {
    m.point_with("loom.host.uptime_seconds", name, id, "", ts, (ts - T0) as f64);
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_work_finder_stall_on_a_live_host_pages_within_one_interval() {
    let mut m = Metrics::default();
    // 12:00Z to 16:00Z, one heartbeat per minute per live host.
    for min in -120..120 {
        let ts = T0 + min * 60;
        let id = |h: &str| format!("id-{h}");
        // The incident: loom-worker-1's finder ticks every 60 s until 14:04Z and
        // then stops, while host.health keeps flowing.
        if ts <= at(14, 4) {
            tick(&mut m, "loom-worker-1", &id("loom-worker-1"), ts);
        }
        heartbeat(&mut m, "loom-worker-1", &id("loom-worker-1"), ts);
        // Healthy.
        tick(&mut m, "loom-worker-2", &id("loom-worker-2"), ts);
        heartbeat(&mut m, "loom-worker-2", &id("loom-worker-2"), ts);
        // Work finder disabled: heartbeats, never ticks. Not judged.
        heartbeat(&mut m, "loom-worker-3", &id("loom-worker-3"), ts);
        // A 300 s interval, stopping after 15:00Z: deadline 2 x 300 s.
        if min % 5 == 0 && ts <= at(15, 0) {
            tick(&mut m, "loom-worker-4", &id("loom-worker-4"), ts);
        }
        heartbeat(&mut m, "loom-worker-4", &id("loom-worker-4"), ts);
        // A dead host: ticks and heartbeats both stop at 14:30Z. Not this rule's.
        if ts <= at(14, 30) {
            tick(&mut m, "loom-worker-5", &id("loom-worker-5"), ts);
            heartbeat(&mut m, "loom-worker-5", &id("loom-worker-5"), ts);
        }
        // Empty host.name, finder stalled since 13:00Z: labelled by host.id.
        if ts <= at(13, 0) {
            tick(&mut m, "", "host-e1d4c843", ts);
        }
        heartbeat(&mut m, "", "host-e1d4c843", ts);
    }
    let f = m.script();
    // 14:05 / 14:09 -- the last tick is 1 and 5 min old: within the 300 s floor.
    assert_eq!(firing(WORK_FINDER, &f, at(14, 5)), ["host-e1d4c843"]);
    assert_eq!(firing(WORK_FINDER, &f, at(14, 9)), ["host-e1d4c843"]);
    // 14:10 -- 6 min without a tick on a 60 s finder: pages, within one 5 min
    // evaluation of the deadline.
    assert_eq!(firing(WORK_FINDER, &f, at(14, 10)), ["host-e1d4c843", "loom-worker-1"]);
    // 15:08 -- worker-4's last tick is 8 min old, under 2 x its 300 s interval.
    assert_eq!(firing(WORK_FINDER, &f, at(15, 8)), ["host-e1d4c843", "loom-worker-1"]);
    // 15:11 -- 11 min: past 600 s, pages. worker-5 stopped heartbeating, and
    // worker-3 never ticked: neither is judged.
    assert_eq!(
        firing(WORK_FINDER, &f, at(15, 11)),
        ["host-e1d4c843", "loom-worker-1", "loom-worker-4"]
    );
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
