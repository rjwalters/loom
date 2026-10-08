//! Proof for `signoz/alerts/fleet-singleton-output.json`, the cross-host
//! detector of Issue #10916 (slice 2b of #10924).
//!
//! Fleet singletons (ETA authority emission, `eta-fleet-refresh`,
//! `eta-nightly-folds`, the fit check) can stop producing output while every
//! host looks healthy: on 2026-10-07 the ETA authority moved to a host with no
//! OTLP exporter and 28 of ~30 repos received no `eta.estimate` for ~31 h. An
//! in-daemon watchdog cannot see another host's OTLP output (the
//! `captain_gauges/store.rs` module doc), so this SigNoz rule is the detector
//! that checks the OUTPUT and never the owning host.
//!
//! The rule is `max(timestamp) GROUP BY loom.kind[, loom.repo]` over the logs
//! table (`loom.kind` is on every OTLP log record since #10899/#10934), with
//! one deadline per watched `fleet_outputs::SINGLETON_OUTPUTS` row embedded in
//! the query. Two halves, as in `signoz_queue_starvation_alert.rs`:
//!
//! 1. **No drift, in ordinary CI (no Docker).** The deadlines the query
//!    embeds are parsed back out of the committed JSON and asserted equal to
//!    `SingletonOutput::deadline()` of the registry rows; every registry row
//!    is either watched or excluded here with a structural reason; every
//!    attribute the query reads is forwarded by the collector.
//! 2. **It separates an outage from a healthy fleet**, on `clickhouse local`
//!    in the pinned image (`#[ignore]`d; CI runs it with `--ignored`): the
//!    10-07 replay fires within the deadline (2 x cadence), a healthy fleet
//!    with an idle repo and an all-abstaining repo does not, and an empty
//!    logs table fires every row. Each predicate that keeps a quiet repo
//!    quiet has its breaking mutation run.
//!
//! **What this does NOT establish.** No record went through SigNoz's ingester
//! and no rule evaluator or notification ran; this is `clickhouse local` over
//! the read surface `fixtures/signoz_eta/fixture.sql` documents, the same
//! caveat every engine-level proof in this trial carries (#9006, #8525).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::{Command, Stdio};

use loom_daemon::fleet_outputs::{Scope, Severity, SINGLETON_OUTPUTS};
use loom_daemon::telemetry::{TelemetryKindOtlp, TELEMETRY_KINDS};

/// Same pin as the SigNoz trial's telemetry store and every other
/// engine-level proof in this trial.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

/// The committed alert rule, byte for byte.
const ALERT: &str =
    include_str!("../../defaults/observability/signoz/alerts/fleet-singleton-output.json");

/// The collector config whose log `keep_keys` decides which attributes reach
/// SigNoz at all.
const COLLECTOR: &str = include_str!("../../defaults/observability/collector/config.yaml");

const SCHEMA: &str = include_str!("fixtures/signoz_fleet_singleton_output/schema.sql");
const INCIDENT: &str = include_str!("fixtures/signoz_fleet_singleton_output/incident.sql");
const HEALTHY: &str = include_str!("fixtures/signoz_fleet_singleton_output/healthy.sql");

/// Registry rows this rule deliberately does not watch, each with the reason.
/// `every_registry_row_is_watched_or_excluded_for_a_structural_reason` checks
/// each reason against the code, so this list cannot go stale silently.
const EXCLUDED: &[(&str, &str)] = &[
    (
        "captain-gauges/v1:stage-dwell",
        "a per-job `as_of` in the fleet-store heartbeat, not a telemetry record kind: no OTLP \
         log carries it; the in-daemon path judges it from the store",
    ),
    (
        "captain-gauges/v1:star-facts",
        "a per-job `as_of` in the fleet-store heartbeat, not a telemetry record kind",
    ),
    (
        "captain-gauges/v1:queue-blocked",
        "a per-job `as_of` in the fleet-store heartbeat, not a telemetry record kind",
    ),
    (
        "ci.run",
        "a Warning row (a quiet CI spell is legal); this rule's single severity is critical",
    ),
];

/// Every attribute the query reads. Each must survive the collector's log
/// `keep_keys`, or the rule reads `''` and silently judges the wrong thing.
const READ_ATTRIBUTES: &[&str] = &[
    "loom.kind",
    "loom.repo",
    "loom.issue",
    "loom.eta.kind",
    "loom.eta.no_estimate_reason",
];

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

fn rows(script: &str) -> Vec<Row> {
    clickhouse(script)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("JSONEachRow line"))
        .collect()
}

fn rule() -> serde_json::Value {
    serde_json::from_str(ALERT).expect("alerts/fleet-singleton-output.json must be valid JSON")
}

fn embedded_query(rule: &serde_json::Value) -> String {
    rule["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query must be a string")
        .to_owned()
}

/// Go `time.Duration` strings (`"72h0m0s"`, `"5m0s"`) to milliseconds.
fn duration_ms(spec: &str) -> i64 {
    let mut total = 0_i64;
    let mut digits = String::new();
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: i64 = digits
            .parse()
            .unwrap_or_else(|_| panic!("no number before unit '{c}' in duration {spec:?}"));
        digits.clear();
        total += match c {
            'h' => n * 3_600_000,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                n
            }
            'm' => n * 60_000,
            's' => n * 1_000,
            other => panic!("unsupported duration unit {other:?} in {spec:?}"),
        };
    }
    assert!(digits.is_empty(), "trailing number in duration {spec:?}");
    total
}

/// The registry scope as the query spells it. Exhaustive on purpose: a new
/// `Scope` variant fails to compile here until the rule learns it.
fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::FleetWide => "fleet_wide",
        Scope::PerRepo => "per_repo",
        Scope::PerActiveRepo => "per_active_repo",
    }
}

/// One watched row as embedded in the query: `(scope, deadline_sec, closed_by)`.
type Embedded = (String, u64, String);

/// Parses the query's `arrayJoin([('kind', 'scope', secs, 'closed_by'), ...])`
/// registry literal. Panics on any shape it does not recognise, so a reworded
/// literal fails here by name rather than parsing to an empty set.
fn embedded_registry(query: &str) -> BTreeMap<String, Embedded> {
    let start = query
        .find("arrayJoin([")
        .expect("the query's registry literal `arrayJoin([` is gone")
        + "arrayJoin([".len();
    let end = start
        + query[start..]
            .find("])")
            .expect("unterminated registry literal");
    let mut out = BTreeMap::new();
    for tuple in query[start..end].split("),") {
        let inner = tuple
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim();
        let fields: Vec<&str> = inner.split(',').map(str::trim).collect();
        assert_eq!(fields.len(), 4, "registry tuple {tuple:?} is not 4 fields");
        let text = |f: &str| -> String {
            assert!(
                f.starts_with('\'') && f.ends_with('\''),
                "registry field {f:?} is not a quoted string"
            );
            f.trim_matches('\'').to_owned()
        };
        let kind = text(fields[0]);
        let secs: u64 = fields[2]
            .parse()
            .unwrap_or_else(|_| panic!("deadline {:?} is not an integer", fields[2]));
        let prev = out.insert(kind.clone(), (text(fields[1]), secs, text(fields[3])));
        assert!(prev.is_none(), "kind {kind} is embedded twice");
    }
    out
}

/// What the registry says the query must embed: every row whose output is an
/// OTLP log kind and whose severity is critical.
fn expected_registry() -> BTreeMap<String, Embedded> {
    let excluded: BTreeSet<&str> = EXCLUDED.iter().map(|(k, _)| *k).collect();
    SINGLETON_OUTPUTS
        .iter()
        .filter(|o| !excluded.contains(o.record_kind))
        .map(|o| {
            let closed_by = if o.scope == Scope::PerActiveRepo {
                // `eta.estimate` is the only PerActiveRepo row: an item stops
                // owing estimates once a `land` `eta.outcome` closes it. A new
                // PerActiveRepo row needs its own closing kind decided here.
                assert_eq!(
                    o.record_kind, "eta.estimate",
                    "a new PerActiveRepo row needs a closing record kind in the rule"
                );
                "eta.outcome"
            } else {
                ""
            };
            (
                o.record_kind.to_owned(),
                (scope_name(o.scope).to_owned(), o.deadline().as_secs(), closed_by.to_owned()),
            )
        })
        .collect()
}

// ===========================================================================
// No drift (ordinary CI, no Docker).
// ===========================================================================

/// Each deadline in the committed JSON equals `SingletonOutput::deadline()`
/// (2 x cadence, or the row's own override) of its registry row, and the rule
/// watches exactly the rows it should. No cadence is hand-copied into this
/// test: both sides are read, one from the JSON and one from the registry.
#[test]
fn embedded_deadlines_equal_the_singleton_outputs_registry() {
    let embedded = embedded_registry(&embedded_query(&rule()));
    let expected = expected_registry();
    assert!(!expected.is_empty(), "the rule must watch at least one row");
    assert_eq!(
        embedded, expected,
        "alerts/fleet-singleton-output.json has drifted from fleet_outputs::SINGLETON_OUTPUTS: \
         update the query's arrayJoin registry literal to (kind, scope, deadline().as_secs(), \
         closed_by) for every watched row"
    );
}

/// Every registry row is watched or excluded, and each exclusion still holds
/// for the reason it gives: captain-gauge rows are not OTLP log kinds, and a
/// Warning row cannot share this critical rule. Every watched row is an OTLP
/// log kind, so `loom.kind` reaches SigNoz for it.
#[test]
fn every_registry_row_is_watched_or_excluded_for_a_structural_reason() {
    let log_kind = |kind: &str| {
        TELEMETRY_KINDS
            .iter()
            .any(|m| m.kind == kind && m.otlp == TelemetryKindOtlp::Logs)
    };
    let registry: BTreeMap<&str, _> = SINGLETON_OUTPUTS
        .iter()
        .map(|o| (o.record_kind, o))
        .collect();
    for (kind, reason) in EXCLUDED {
        let row = registry
            .get(kind)
            .unwrap_or_else(|| panic!("excluded kind {kind} is no longer a registry row"));
        let holds = if kind.starts_with("captain-gauges/") {
            !log_kind(kind)
        } else {
            row.severity == Severity::Warning
        };
        assert!(holds, "the exclusion of {kind} no longer holds: {reason}");
    }
    for kind in expected_registry().keys() {
        assert!(
            log_kind(kind),
            "{kind} is watched but is not an OTLP Logs kind, so no log record carries it"
        );
        assert_eq!(
            registry[kind.as_str()].severity,
            Severity::Critical,
            "{kind} is watched by a critical rule but its registry row is not Critical"
        );
    }
    assert!(log_kind("eta.outcome"), "the closing kind must be an OTLP Logs kind");
}

/// Every attribute the query reads is in the collector's log `keep_keys`.
#[test]
fn the_collector_forwards_every_attribute_the_query_reads() {
    let query = embedded_query(&rule());
    let log_keep = COLLECTOR
        .lines()
        .find(|l| l.contains("keep_keys(attributes, [\"loom.kind\", \"loom.record_id\""))
        .expect("the collector's log keep_keys line");
    for key in READ_ATTRIBUTES {
        assert!(
            query.contains(&format!("'{key}'")),
            "{key} is listed as read but the query no longer reads it"
        );
        assert!(
            log_keep.contains(&format!("\"{key}\"")),
            "the collector drops {key}; the rule would read '' for it"
        );
    }
}

/// The window covers the longest deadline (so an on-time daily output is
/// visible), the rule re-evaluates well inside the shortest one (so an
/// outage is caught within 2 x cadence plus one evaluation), and the rule
/// cannot ship silent.
#[test]
fn the_window_and_frequency_fit_the_registry_deadlines() {
    let rule = rule();
    let window_ms = duration_ms(rule["evalWindow"].as_str().unwrap());
    let freq_ms = duration_ms(rule["frequency"].as_str().unwrap());
    let deadlines: Vec<i64> = expected_registry()
        .values()
        .map(|(_, secs, _)| i64::try_from(*secs).unwrap() * 1000)
        .collect();
    let longest = *deadlines.iter().max().unwrap();
    let shortest = *deadlines.iter().min().unwrap();
    assert!(
        window_ms > longest,
        "evalWindow {window_ms}ms must exceed the longest deadline {longest}ms, or an \
         on-time output older than the window reads as absent and fires"
    );
    assert!(
        freq_ms * 4 <= shortest,
        "frequency {freq_ms}ms is too coarse for the shortest deadline {shortest}ms"
    );
    let condition = &rule["condition"];
    assert_eq!(condition["op"], "1", "value above target");
    assert_eq!(condition["target"], 0, "value is seconds past the deadline");
    assert_eq!(condition["alertOnAbsent"], true, "a query that returns nothing must fire");
    assert_eq!(rule["labels"]["severity"], "critical");
    assert!(
        rule["disabled"] == serde_json::Value::Bool(false)
            && condition["compositeQuery"]["chQueries"]["A"]["disabled"]
                == serde_json::Value::Bool(false),
        "an imported alert rule that ships disabled is silent by construction"
    );
}

#[test]
fn go_duration_strings_parse() {
    assert_eq!(duration_ms("72h0m0s"), 259_200_000);
    assert_eq!(duration_ms("5m0s"), 300_000);
    assert_eq!(duration_ms("1m30s"), 90_000);
    assert_eq!(duration_ms("250ms"), 250);
}

// ===========================================================================
// The committed rule, executed (Docker).
// ===========================================================================

/// `(silenced_at, evaluated_at)` in seconds, read back from a scenario.
fn anchor(scenario: &str) -> (i64, i64) {
    let out = rows(&format!(
        "{SCHEMA}\n{scenario}\nSELECT silenced_at, evaluated_at FROM loom_fixture.anchor;"
    ));
    assert_eq!(out.len(), 1, "loom_fixture.anchor must hold exactly one row");
    (
        out[0]["silenced_at"].as_i64().unwrap(),
        out[0]["evaluated_at"].as_i64().unwrap(),
    )
}

/// Runs `query` the way SigNoz's evaluator does at `end_s`: the window is the
/// committed `evalWindow`, ending at `end_s`. Returns `(kind, repo) -> value`.
fn run(query: &str, scenario: &str, end_s: i64) -> BTreeMap<(String, String), f64> {
    for placeholder in ["{{.start_timestamp_ms}}", "{{.end_timestamp_ms}}"] {
        assert!(query.contains(placeholder), "the query no longer carries {placeholder}");
    }
    let window_ms = duration_ms(rule()["evalWindow"].as_str().unwrap());
    let end_ms = end_s * 1000;
    let bound = query
        .replace("{{.start_timestamp_ms}}", &(end_ms - window_ms).to_string())
        .replace("{{.end_timestamp_ms}}", &end_ms.to_string());
    let mut out = BTreeMap::new();
    for row in rows(&format!("{SCHEMA}\n{scenario}\n{bound};")) {
        let key = (
            row["kind"].as_str().unwrap().to_owned(),
            row["repo"].as_str().unwrap().to_owned(),
        );
        let value = row["value"].as_f64().expect("value column");
        assert!(out.insert(key.clone(), value).is_none(), "series {key:?} twice");
    }
    out
}

/// The series the committed `op` / `target` fire on. One point per series,
/// so `matchType` 1 (at least once) and 2 (all the time) agree.
fn firing(series: &BTreeMap<(String, String), f64>) -> BTreeSet<(String, String)> {
    let rule = rule();
    assert_eq!(rule["condition"]["op"], "1");
    assert_eq!(rule["condition"]["matchType"], "1");
    let target = rule["condition"]["target"].as_f64().unwrap();
    series
        .iter()
        .filter(|(_, v)| **v > target)
        .map(|(k, _)| k.clone())
        .collect()
}

fn repos(range: std::ops::RangeInclusive<u32>) -> impl Iterator<Item = String> {
    range.map(|n| format!("org/r{n:02}"))
}

fn deadline_secs(kind: &str) -> i64 {
    let o = SINGLETON_OUTPUTS
        .iter()
        .find(|o| o.record_kind == kind)
        .unwrap();
    i64::try_from(o.deadline().as_secs()).unwrap()
}

/// The 10-07 incident, 31 h in: exactly the 28 silenced repos fire, for both
/// per-repo outputs, and nothing else does. `org/r03`'s `start` outcome does
/// not excuse it (only a `land` outcome closes an item).
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_10_07_incident_fires_for_exactly_the_28_silenced_repos() {
    let (_, evaluated_at) = anchor(INCIDENT);
    let series = run(&embedded_query(&rule()), INCIDENT, evaluated_at);
    let expected: BTreeSet<(String, String)> = ["eta.estimate", "eta.fleet_refresh"]
        .into_iter()
        .flat_map(|k| repos(3..=30).map(move |r| (k.to_owned(), r)))
        .collect();
    assert_eq!(expected.len(), 56);
    assert_eq!(firing(&series), expected);
    assert!(
        !series.keys().any(|(k, _)| k == "sweep.outcome"),
        "a kind no registry row names must not produce a series"
    );
}

/// Detection latency: the silenced repos' `eta.estimate` series do not fire
/// one evaluation before the deadline (2 x the 30 min cadence) and do fire one
/// evaluation after it, i.e. within 2 x cadence plus one `frequency`.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_incident_fires_within_two_cadences() {
    let (silenced_at, _) = anchor(INCIDENT);
    let rule = rule();
    let query = embedded_query(&rule);
    let freq_s = duration_ms(rule["frequency"].as_str().unwrap()) / 1000;
    let deadline = deadline_secs("eta.estimate");
    assert_eq!(deadline, 3600, "2 x the 30 min eta.estimate cadence");

    let before = firing(&run(&query, INCIDENT, silenced_at + deadline - freq_s));
    assert!(before.is_empty(), "fired before the deadline: {before:?}");

    let after = firing(&run(&query, INCIDENT, silenced_at + deadline + freq_s));
    let expected: BTreeSet<(String, String)> = repos(3..=30)
        .map(|r| ("eta.estimate".to_owned(), r))
        .collect();
    assert_eq!(
        after, expected,
        "the 28 silenced repos must fire one evaluation past 2 x cadence"
    );
}

/// A healthy fleet does not fire, including an idle repo and an
/// all-abstaining one. Their series are still RETURNED (non-positive), so
/// SigNoz can see a firing repo recover.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_healthy_quiet_fleet_does_not_fire() {
    let (_, evaluated_at) = anchor(HEALTHY);
    let series = run(&embedded_query(&rule()), HEALTHY, evaluated_at);
    assert!(firing(&series).is_empty(), "a healthy fleet fired: {:?}", firing(&series));
    for repo in ["org/r29", "org/r30"] {
        assert!(
            series.contains_key(&("eta.estimate".to_owned(), repo.to_owned())),
            "{repo}'s eta.estimate series must be returned so recovery is visible"
        );
    }
    for kind in expected_registry().keys() {
        assert!(
            series.contains_key(&(kind.clone(), String::new())),
            "{kind} must always have its fleet-level series"
        );
    }
}

/// Counterfactuals: each predicate that keeps a quiet repo quiet is
/// load-bearing. Without the `land` closure the idle `org/r29` fires; without
/// the refusal check the all-abstaining `org/r30` fires; without the `land`
/// filter a `start` outcome excuses the silenced `org/r03`.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn each_quiet_repo_predicate_is_load_bearing() {
    let query = embedded_query(&rule());
    let (_, evaluated_at) = anchor(HEALTHY);
    let mutate = |from: &str, to: &str| {
        assert!(query.contains(from), "mutation marker {from:?} is gone from the query");
        query.replace(from, to)
    };
    let estimate = |repo: &str| ("eta.estimate".to_owned(), repo.to_owned());

    let no_closure = mutate(
        "AND NOT (c.closed_ms > 0 AND c.closed_ms + g.deadline_sec * 1000 >= i.last_ms)",
        "AND true",
    );
    let fired = firing(&run(&no_closure, HEALTHY, evaluated_at));
    assert_eq!(fired, BTreeSet::from([estimate("org/r29")]));

    let no_refusal = mutate("NOT i.refused_now", "true");
    let fired = firing(&run(&no_refusal, HEALTHY, evaluated_at));
    assert_eq!(fired, BTreeSet::from([estimate("org/r30")]));

    let (_, incident_at) = anchor(INCIDENT);
    let any_outcome = mutate("    WHERE c.eta_kind = 'land'\n", "");
    let fired = firing(&run(&any_outcome, INCIDENT, incident_at));
    assert!(
        !fired.contains(&estimate("org/r03")),
        "counterfactual: counting a `start` outcome as closing hides org/r03's outage"
    );
}

/// Absent data fires: with no log record at all, every watched kind's
/// fleet-level series fires.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn absent_data_fires_every_watched_row() {
    let series = run(&embedded_query(&rule()), "", 1_791_453_600);
    let expected: BTreeSet<(String, String)> = expected_registry()
        .into_keys()
        .map(|k| (k, String::new()))
        .collect();
    assert_eq!(firing(&series), expected);
}
