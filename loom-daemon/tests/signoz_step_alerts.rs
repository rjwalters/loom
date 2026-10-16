//! Static checks for the step-dispatch alert rules shipped with the cutover
//! (#11909): `alerts/step-dispatch-stalled.json`,
//! `alerts/step-draw-pass-rate.json` and `alerts/step-slots-idle.json`.
//!
//! The step tick is the only item dispatch path, rolled out fleet-wide and
//! fixed forward, so these watch it from the first minute: no step spawned
//! while work waits (15 min), draws mostly refused by the pool gate or the
//! claim (30 min), and slots mostly idle while work waits (30 min). The
//! checks read the committed JSON only; no rule evaluator has run.
#![allow(clippy::unwrap_used, clippy::expect_used)]

const TEMPLATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json");
const STALLED: &str =
    include_str!("../../defaults/observability/signoz/alerts/step-dispatch-stalled.json");
const PASS_RATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/step-draw-pass-rate.json");
const IDLE: &str = include_str!("../../defaults/observability/signoz/alerts/step-slots-idle.json");
const ROLE_TICK: &str =
    include_str!("../../defaults/observability/signoz/alerts/role-runner-item-tick.json");

fn parse(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("alert JSON must parse")
}

fn query(rule: &serde_json::Value) -> String {
    rule["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query")
        .to_owned()
}

#[test]
fn step_alerts_share_the_template_schema_and_are_per_host() {
    let template = parse(TEMPLATE);
    let keys =
        |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    for (name, raw) in [
        ("step-dispatch-stalled", STALLED),
        ("step-draw-pass-rate", PASS_RATE),
        ("step-slots-idle", IDLE),
        ("role-runner-item-tick", ROLE_TICK),
    ] {
        let rule = parse(raw);
        assert_eq!(keys(&rule), keys(&template), "{name}: top-level keys");
        assert_eq!(keys(&rule["condition"]), keys(&template["condition"]), "{name}");
        assert_eq!(rule["ruleType"], "threshold_rule", "{name}");
        let logs = rule["alertType"] == "LOGS_BASED_ALERT";
        assert_eq!(rule["disabled"], false, "{name}: ships enabled");
        assert_eq!(rule["labels"]["loom_signal"], name.replace('-', "_"), "{name}");
        let q = query(&rule);
        let window = if logs {
            ["{{.start_timestamp_nano}}", "{{.end_timestamp_nano}}"]
        } else {
            ["{{.start_timestamp_ms}}", "{{.end_timestamp_ms}}"]
        };
        for p in window {
            assert!(q.contains(p), "{name}: query lacks {p}");
        }
        // Attributable: per repo (the stage gauge is the captain's, keyed
        // by repo) or per host.
        assert!(q.contains("GROUP BY repo") || q.contains("GROUP BY host"), "{name}");
        assert!(!q.contains("\"loom:"), "{name}: no label literals");
    }
}

#[test]
fn the_thresholds_are_the_ruled_ones() {
    let stalled = parse(STALLED);
    assert_eq!(stalled["evalWindow"], "15m0s");
    assert_eq!(
        (stalled["condition"]["op"].as_str(), stalled["condition"]["target"].as_f64()),
        (Some("1"), Some(0.0))
    );
    let q = query(&stalled);
    assert!(q.contains("name = 'loom.dispatch.step'"));
    assert!(q.contains("'loom.dispatch.admission_result'] = 'spawned'"));
    // Actionable depth, never the forge's stage gauge (which counts parked
    // and held work the tick never draws).
    assert!(q.contains("metric = 'loom.step.actionable'"));
    assert!(!q.contains("loom.forge.stage_items"));
    assert!(q.contains("max_concurrent > 0"), "dispatch hosts only");
    // Keyed by repo: the gauge is the captain's, per repo, and a host
    // dispatching for the repo must have a free slot.
    assert!(q.contains("repo_depth.repo AS repo"));
    assert!(q.contains("hosts_with_free_slots > 0"));
    assert!(q.contains("lower(attributes_string['loom.repo']) AS repo, count() AS n"));

    let pass = parse(PASS_RATE);
    assert_eq!(pass["evalWindow"], "30m0s");
    assert_eq!(pass["condition"]["op"], "1");
    assert_eq!(pass["condition"]["target"].as_f64(), Some(0.8));
    let q = query(&pass);
    for reason in [
        "pool_exhausted",
        "token_selection_failed",
        "'claimed'",
        "claim_lost",
    ] {
        assert!(q.contains(reason), "pass reason {reason}");
    }

    let idle = parse(IDLE);
    assert_eq!(idle["evalWindow"], "30m0s");
    assert_eq!(idle["condition"]["op"], "2", "below");
    assert_eq!(idle["condition"]["target"].as_f64(), Some(0.5));
    let q = query(&idle);
    assert!(
        q.contains("role = 'sweep' AND pool = ''"),
        "the slot total, not the per-role split"
    );
    assert!(q.contains("min_depth > 0"), "work waited the whole window");
    assert!(!q.contains("loom.forge.stage_items"));
    assert!(q.contains("repo_slots.repo AS repo"));

    let ticks = parse(ROLE_TICK);
    let q = query(&ticks);
    assert_eq!(ticks["alertType"], "LOGS_BASED_ALERT");
    assert!(q.contains("attributes_string['loom.role_tick.source'] != 'step'"));
    assert!(q.contains("('curator', 'judge', 'doctor')"));
}

// ---------------------------------------------------------------- replay ----

#[path = "common/hub_image.rs"]
mod hub_image;

use std::io::Write;
use std::process::{Command, Stdio};

/// The image the other alert replays pin.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

/// 2026-10-14T14:00:00Z.
const T0: i64 = 1_791_986_400;

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

const SCHEMA: &str = "CREATE DATABASE signoz_metrics;\n\
CREATE TABLE signoz_metrics.samples_v4 (metric_name LowCardinality(String), fingerprint UInt64, \
unix_milli Int64, value Float64) ENGINE = Memory;\n\
CREATE TABLE signoz_metrics.time_series_v4 (metric_name LowCardinality(String), \
fingerprint UInt64, unix_milli Int64, labels String) ENGINE = Memory;\n\
CREATE DATABASE signoz_traces;\n\
CREATE TABLE signoz_traces.signoz_index_v3 (name String, timestamp DateTime64(9), \
attributes_string Map(String, String), resources_string Map(String, String)) ENGINE = Memory;\n";

/// A synthetic store: series keyed by (metric, host, repo, role, pool).
#[derive(Default)]
struct Store {
    sql: String,
    samples: Vec<String>,
    spans: Vec<String>,
    series: Vec<String>,
}

impl Store {
    fn point(&mut self, metric: &str, host: &str, labels: &[(&str, &str)], ts: i64, v: f64) {
        let mut json = format!(
            "{{\"__name__\":\"{metric}\",\"host.name\":\"{host}\",\"host.id\":\"id-{host}\""
        );
        for (k, val) in labels {
            json += &format!(",\"{k}\":\"{val}\"");
        }
        json += "}";
        let key = format!("{metric}|{json}");
        let fp = match self.series.iter().position(|s| *s == key) {
            Some(i) => i as u64 + 1,
            None => {
                self.series.push(key);
                let fp = self.series.len() as u64;
                self.sql += &format!(
                    "INSERT INTO signoz_metrics.time_series_v4 VALUES ('{metric}', {fp}, {}, '{json}');\n",
                    T0 * 1000
                );
                fp
            }
        };
        self.samples
            .push(format!("('{metric}', {fp}, {}, {v})", ts * 1000));
    }

    fn spawned(&mut self, host: &str, repo: &str, ts: i64) {
        self.spans.push(format!(
            "('loom.dispatch.step', fromUnixTimestamp64Milli(toInt64({})), \
             map('loom.repo', '{repo}', 'loom.dispatch.admission_result', 'spawned'), \
             map('host.name', '{host}'))",
            ts * 1000
        ));
    }

    /// One dispatching host's tick: its slots, and the actionable depth of
    /// each repo it lists.
    fn tick(&mut self, host: &str, ts: i64, busy: f64, max: f64, repos: &[(&str, f64)]) {
        self.point("loom.dispatch.busy_slots", host, &[("role", "sweep")], ts, busy);
        self.point("loom.dispatch.max_concurrent", host, &[], ts, max);
        for (repo, n) in repos {
            self.point("loom.step.actionable", host, &[("repo", repo)], ts, *n);
        }
    }

    fn script(&self) -> String {
        let mut s = format!(
            "{SCHEMA}{}INSERT INTO signoz_metrics.samples_v4 VALUES {};\n",
            self.sql,
            self.samples.join(", ")
        );
        if !self.spans.is_empty() {
            s += &format!(
                "INSERT INTO signoz_traces.signoz_index_v3 VALUES {};\n",
                self.spans.join(", ")
            );
        }
        s
    }
}

/// The repos a rule fires for over the window ending at `end`, applying the
/// committed op and target.
fn firing_repos(raw: &str, store: &Store, end: i64) -> Vec<String> {
    let rule = parse(raw);
    let spec = rule["evalWindow"].as_str().unwrap();
    let window = spec.trim_end_matches("m0s").parse::<i64>().unwrap() * 60;
    let q = query(&rule)
        .replace("{{.start_timestamp_ms}}", &((end - window) * 1000).to_string())
        .replace("{{.end_timestamp_ms}}", &(end * 1000).to_string());
    let out = clickhouse(&format!("{}\n{q};", store.script()));
    let target = rule["condition"]["target"].as_f64().unwrap();
    let above = rule["condition"]["op"] == "1";
    let mut repos: Vec<String> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|row| {
            let v = row["value"].as_f64().unwrap();
            if above {
                v > target
            } else {
                v < target
            }
        })
        .map(|row| row["repo"].as_str().unwrap().to_owned())
        .collect();
    repos.sort();
    repos.dedup();
    repos
}

/// 14:00Z to 15:00Z, one tick a minute.
/// - `acme/stuck`: 3 drawable items on worker-1, a free slot, never spawned:
///   stalled.
/// - `acme/moving`: drawable items on worker-2, a spawn every 5 min: fine.
/// - `acme/parked`: listed, but nothing drawable (all parked or held): never
///   pages, however much the forge's stage gauge would count.
/// - `acme/full`: drawable items on worker-3, whose slots are all busy: not
///   stalled (no free slot), but not idle either.
/// - `acme/idle`: drawable items on worker-4, which spawns but never fills
///   more than one of its eight slots: idle.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_stalled_and_idle_page_on_drawable_work_only() {
    if !hub_image::available(CLICKHOUSE_IMAGE) {
        return;
    }
    let mut s = Store::default();
    for min in 0..60 {
        let ts = T0 + min * 60;
        s.tick("worker-1", ts, 1.0, 4.0, &[("acme/stuck", 3.0), ("acme/parked", 0.0)]);
        s.tick("worker-2", ts, 3.0, 4.0, &[("acme/moving", 5.0)]);
        s.tick("worker-3", ts, 4.0, 4.0, &[("acme/full", 2.0)]);
        s.tick("worker-4", ts, 1.0, 8.0, &[("acme/idle", 6.0)]);
        if min % 5 == 0 {
            s.spawned("worker-2", "acme/moving", ts);
            s.spawned("worker-4", "acme/idle", ts);
        }
    }
    let end = T0 + 50 * 60;
    assert_eq!(firing_repos(STALLED, &s, end), ["acme/stuck"]);
    assert_eq!(firing_repos(IDLE, &s, end), ["acme/idle", "acme/stuck"]);
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
