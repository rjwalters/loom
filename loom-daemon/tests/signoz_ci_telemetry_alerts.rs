//! Proof for the CI-telemetry silence alerts added by #12908 (the 2026-10-15
//! `loom-fleet-captain` outage, #12792, which nothing surfaced for five days):
//! `alerts/ci-telemetry-export-lagging.json`, `alerts/ci-telemetry-poll-failing.json`
//! and `alerts/ci-telemetry-arrival-silent.json`, labelled
//! `state = export_lagging | poll_failing | arrival_silent`.
//!
//! Same two groups as `signoz_ops_alerts.rs`. The static tests read the committed
//! JSON only and run in ordinary CI. The replay tests run each rule's embedded query
//! verbatim on `clickhouse local` in the pinned image over a synthetic
//! `signoz_metrics` / `signoz_logs` surface, substituting SigNoz's
//! `{{.start_timestamp_*}}` / `{{.end_timestamp_*}}`, then apply the committed
//! `op` / `matchType` / `target`. They need Docker, are `#[ignore]`d, and CI runs
//! them with `--ignored`. No rule evaluator or notification has run; this
//! establishes the queries and thresholds, not delivery.
//!
//! Replay shape (`captain`): healthy until `T0`, then the export cursor sticks at
//! ~2.3 GB of a 4.9 GB journal (2.38M envelopes pending) so `export_lag` grows
//! monotonically, every poll cycle ends `discovery_failed`, and no `ci.run` arrives,
//! for five days while `host.export` stays healthy.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const LAGGING: &str =
    include_str!("../../defaults/observability/signoz/alerts/ci-telemetry-export-lagging.json");
const POLL_FAILING: &str =
    include_str!("../../defaults/observability/signoz/alerts/ci-telemetry-poll-failing.json");
const ARRIVAL: &str =
    include_str!("../../defaults/observability/signoz/alerts/ci-telemetry-arrival-silent.json");
const TEMPLATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json");

/// 2026-10-15T00:00:00Z in unix seconds: the last good captain cycle.
const T0: i64 = 1_791_936_000;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

const LAG: &str = "loom.ci_telemetry.export_lag";
const CYCLES: &str = "loom.ci_telemetry.cycles";

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
    for (name, raw, state) in [
        ("ci-telemetry-export-lagging", LAGGING, "export_lagging"),
        ("ci-telemetry-poll-failing", POLL_FAILING, "poll_failing"),
        ("ci-telemetry-arrival-silent", ARRIVAL, "arrival_silent"),
    ] {
        let rule = parse(raw);
        assert_eq!(keys(&rule), keys(&template), "{name}: top-level keys differ from template");
        assert_eq!(keys(&rule["condition"]), keys(&template["condition"]), "{name}: condition");
        assert_eq!(rule["ruleType"], "threshold_rule", "{name}");
        assert_eq!(rule["version"], "v4", "{name}");
        assert_eq!(rule["disabled"], false, "{name}: ships disabled = silent");
        assert_eq!(rule["condition"]["compositeQuery"]["chQueries"]["A"]["disabled"], false);
        assert_eq!(rule["labels"]["state"], state, "{name}: state label");
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

/// Every metric the rules read is one the daemon emits under that exact name; a
/// rename would otherwise leave the rules silent.
#[test]
fn every_metric_the_rules_read_is_emitted_by_the_daemon() {
    let emitted = [
        include_str!("../src/telemetry/ops.rs"),
        include_str!("../src/observability/otlp/mapping.rs"),
    ]
    .concat();
    for metric in [LAG, CYCLES] {
        assert!(emitted.contains(&format!("\"{metric}\"")), "daemon no longer emits {metric}");
        let quoted = format!("'{metric}'");
        assert!(
            [LAGGING, POLL_FAILING, ARRIVAL]
                .iter()
                .any(|r| query(&parse(r)).contains(&quoted)),
            "{metric} is listed here but no rule reads it"
        );
    }
}

#[test]
fn thresholds_are_the_filed_values() {
    let lag = parse(LAGGING);
    assert_eq!(lag["evalWindow"], "30m0s", "sustained for 30 min");
    assert_eq!(lag["condition"]["target"], 7200, "above 2 h");
    assert_eq!(lag["condition"]["matchType"], "2", "every sample in the window");
    let poll = parse(POLL_FAILING);
    assert_eq!(poll["evalWindow"], "120m0s", "no ok cycle for 2 h");
    assert!(query(&poll).contains("ok_cycles = 0"));
    let arrival = parse(ARRIVAL);
    assert!(query(&arrival).contains("- 21600"), "6 h arrival limit");
    assert!(query(&arrival).contains("body = 'ci.run'"));
    assert!(query(&arrival).contains("attributes_string['loom.kind'] = 'host.export'"));
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

const SCHEMA: &str = "CREATE DATABASE signoz_metrics;\n\
CREATE DATABASE signoz_logs;\n\
CREATE TABLE signoz_metrics.samples_v4 (metric_name LowCardinality(String), fingerprint UInt64, \
unix_milli Int64, value Float64) ENGINE = Memory;\n\
CREATE TABLE signoz_metrics.time_series_v4 (metric_name LowCardinality(String), \
fingerprint UInt64, unix_milli Int64, labels String) ENGINE = Memory;\n\
CREATE TABLE signoz_logs.distributed_logs_v2 (timestamp UInt64, observed_timestamp UInt64, \
body String, attributes_string Map(LowCardinality(String), String), \
attributes_number Map(LowCardinality(String), Float64), \
attributes_bool Map(LowCardinality(String), Bool), \
resources_string Map(LowCardinality(String), String)) ENGINE = Memory;\n";

/// A synthetic SigNoz store: metric series (one `time_series_v4` row per series per
/// hour, so a join that did not de-duplicate would double every point) and logs.
#[derive(Default)]
struct Store {
    series_sql: String,
    samples: Vec<String>,
    logs: Vec<String>,
    series: BTreeMap<String, u64>,
}

impl Store {
    fn point(&mut self, metric: &str, host: &str, outcome: &str, ts: i64, value: f64) {
        let key = format!("{metric}|{host}|{outcome}");
        let fp = if let Some(fp) = self.series.get(&key) {
            *fp
        } else {
            let fp = self.series.len() as u64 + 1;
            self.series.insert(key, fp);
            // host.name set, host.id set: the rule labels by host.name.
            let labels = format!(
                "{{\"__name__\":\"{metric}\",\"host.name\":\"{host}\",\"host.id\":\"id-{host}\",\"outcome\":\"{outcome}\"}}"
            );
            for hour in [0, 1] {
                self.series_sql += &format!(
                    "INSERT INTO signoz_metrics.time_series_v4 VALUES ('{metric}', {fp}, {}, '{labels}');\n",
                    (T0 + hour * HOUR) * 1000
                );
            }
            fp
        };
        self.samples
            .push(format!("('{metric}', {fp}, {}, {value})", ts * 1000));
    }

    /// One log record; `observed` is when SigNoz received it.
    fn log(&mut self, host: &str, body: &str, kind: &str, ts: i64, observed: i64) {
        self.logs.push(format!(
            "({}, {}, '{body}', map('loom.kind', '{kind}'), map(), map(), map('host.name', '{host}', 'host.id', 'id-{host}'))",
            ts * 1_000_000_000,
            observed * 1_000_000_000
        ));
    }

    fn script(&self) -> String {
        format!(
            "{SCHEMA}{}INSERT INTO signoz_metrics.samples_v4 VALUES {};\n\
INSERT INTO signoz_logs.distributed_logs_v2 VALUES {};\n",
            self.series_sql,
            self.samples.join(", "),
            self.logs.join(", ")
        )
    }
}

/// Every host exports `host.export` every 5 min from `from` to `to`, except those
/// whose export is meant to be silent.
fn exports(s: &mut Store, host: &str, from: i64, to: i64) {
    let mut ts = from;
    while ts <= to {
        s.log(host, "", "host.export", ts, ts);
        ts += 300;
    }
}

const END: i64 = T0 + 5 * DAY + 2 * HOUR;

fn fixture() -> String {
    let mut s = Store::default();
    let start = T0 - DAY;
    for host in [
        "captain",
        "healthy",
        "recovering",
        "newfail",
        "exporter-silent",
        "nopoller",
    ] {
        if host != "exporter-silent" {
            exports(&mut s, host, start, END);
        }
    }
    exports(&mut s, "exporter-silent", start, T0);

    let mut ts = start;
    while ts <= END {
        // healthy: caught up, ok cycles, a ci.run arriving every hour.
        s.point(LAG, "healthy", "", ts, 0.0);
        s.point(CYCLES, "healthy", "ok", ts, 1.0);
        if (ts - start) % HOUR == 0 {
            s.log("healthy", "ci.run", "ci.run", ts, ts);
        }

        // captain: good until T0, then the cursor sticks (2.3 GB of 4.9 GB, 2.38M
        // envelopes pending): lag grows monotonically, every cycle fails discovery,
        // no ci.run arrives, host.export stays healthy.
        if ts <= T0 {
            s.point(LAG, "captain", "", ts, 0.0);
            s.point(CYCLES, "captain", "ok", ts, 1.0);
            if (ts - start) % HOUR == 0 {
                s.log("captain", "ci.run", "ci.run", ts, ts);
            }
        } else {
            s.point(LAG, "captain", "", ts, (ts - T0) as f64);
            s.point("loom.ci_telemetry.journal_bytes", "captain", "", ts, 4.9e9);
            s.point("loom.ci_telemetry.export_cursor_offset", "captain", "", ts, 2.3e9);
            s.point(CYCLES, "captain", "discovery_failed", ts, 1.0);
        }

        // recovering: same outage for 4 h, then the backlog drains and ci.run resumes.
        if ts <= T0 || ts >= T0 + 4 * HOUR {
            s.point(LAG, "recovering", "", ts, 0.0);
            s.point(CYCLES, "recovering", "ok", ts, 1.0);
            if (ts - start) % HOUR == 0 {
                s.log("recovering", "ci.run", "ci.run", ts, ts);
            }
        } else {
            s.point(LAG, "recovering", "", ts, (ts - T0) as f64);
            s.point(CYCLES, "recovering", "io_failed", ts, 1.0);
        }

        // newfail: a poller that only starts at T0+2h40m and fails for 40 min: not
        // yet a 2 h outage, so never judged. ci.run arrives from elsewhere.
        if (ts - start) % HOUR == 0 {
            s.log("newfail", "ci.run", "ci.run", ts, ts);
        }
        if (T0 + 3 * HOUR - 1200..T0 + 3 * HOUR + 1200).contains(&ts) {
            s.point(CYCLES, "newfail", "rate_limited", ts, 1.0);
        } else if ts >= T0 + 3 * HOUR + 1200 {
            s.point(CYCLES, "newfail", "ok", ts, 1.0);
        }

        // exporter-silent: polls fine, no lag gauge (unreadable journal), no ci.run,
        // and its host.export stopped at T0: host-export-silent's job, not ours.
        s.point(CYCLES, "exporter-silent", "ok", ts, 1.0);

        // nopoller: exports and other logs, never a poll cycle or lag gauge, no ci.run.
        s.log("nopoller", "", "sweep.event", ts, ts);

        ts += 300;
    }
    s.script()
}

/// Hosts whose rows, over the window ending at `end`, satisfy the committed
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

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_export_lag_fires_only_once_sustained_and_clears_on_recovery() {
    if !hub_image::available(CLICKHOUSE_IMAGE) {
        return;
    }
    let f = fixture();
    // Healthy before the outage, and under 2 h of lag.
    assert!(firing(LAGGING, &f, T0 - HOUR).is_empty());
    assert!(firing(LAGGING, &f, T0 + HOUR).is_empty());
    // 2 h 25 m in: the 30 min window still holds samples under 2 h.
    assert!(firing(LAGGING, &f, T0 + 2 * HOUR + 1500).is_empty());
    // Every sample in the last 30 min is past 2 h: captain and the recovering host.
    assert_eq!(firing(LAGGING, &f, T0 + 3 * HOUR), ["captain", "recovering"]);
    // Recovering clears the moment its lag drops (one sample under target in the window).
    assert_eq!(firing(LAGGING, &f, T0 + 4 * HOUR + 600), ["captain"]);
    // The real captain shape, five days in: cursor still stuck, lag monotonic.
    assert_eq!(firing(LAGGING, &f, T0 + 5 * DAY), ["captain"]);
    // Never: healthy, a host with no lag gauge, a host with no poller.
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_failing_poll_fires_after_two_hours_without_an_ok_cycle() {
    if !hub_image::available(CLICKHOUSE_IMAGE) {
        return;
    }
    let f = fixture();
    // Healthy before, and inside the first 2 h of failure the window still holds ok cycles.
    assert!(firing(POLL_FAILING, &f, T0 - HOUR).is_empty());
    assert!(firing(POLL_FAILING, &f, T0 + HOUR).is_empty());
    // 3 h: the whole window failed for captain and recovering. newfail has failed
    // only 20 min, so it is not judged.
    assert_eq!(firing(POLL_FAILING, &f, T0 + 3 * HOUR), ["captain", "recovering"]);
    // Recovering gets an ok cycle at 4 h and clears.
    assert_eq!(firing(POLL_FAILING, &f, T0 + 4 * HOUR + 600), ["captain"]);
    // Five days in: still failing (the 478-consecutive-failures shape).
    assert_eq!(firing(POLL_FAILING, &f, T0 + 5 * DAY), ["captain"]);
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn replayed_arrival_silence_fires_for_a_poller_host_with_healthy_export() {
    if !hub_image::available(CLICKHOUSE_IMAGE) {
        return;
    }
    let f = fixture();
    // Last ci.run at T0: under the 6 h limit at T0+5h.
    assert!(firing(ARRIVAL, &f, T0 + 5 * HOUR).is_empty());
    // 7 h without arrivals: captain, and recovering until it resumes at 4 h (it has).
    assert_eq!(firing(ARRIVAL, &f, T0 + 7 * HOUR), ["captain"]);
    // Five days in, no ci.run anywhere in the 12 h window.
    assert_eq!(firing(ARRIVAL, &f, T0 + 5 * DAY), ["captain"]);
    // Never: healthy, a recovered host, a host with no poller, and exporter-silent
    // (cycles but host.export also silent: host-export-silent's job).
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
