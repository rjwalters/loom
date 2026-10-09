//! Live proof for the SigNoz trial's one saved *alert rule*,
//! `signoz/alerts/queue-starvation.json` (#8856) — Issue #8528 scope item 4
//! ("host/token gauges") and scope item 5 ("missing stays distinguishable from
//! zero"). The eight tests that execute SQL require Docker and are
//! `#[ignore]`d for CI's explicit `--ignored` invocation; the two that only
//! read the committed JSON run in ordinary CI. Neither group ever converts
//! missing Docker into a pass.
//!
//! Each of this trial's other *query* artifacts already has an engine-level
//! execution proof — `usage-queries.sql` via `signoz_usage_queries.rs` (#9705),
//! `cycle-time-extract.sql` via `signoz_cycle_time.rs` (#9775), `queue-dwell.sql`
//! and `quota-utilization.sql` via `signoz_queue_quota_queries.rs` (#9833).
//! `alerts/queue-starvation.json` did not, and `evidence.md` said so in as many
//! words: *"Nor is it a proof of the alert: `alerts/queue-starvation.json`
//! embeds the same query 1 shape with `{{.start_timestamp_ms}}` placeholders
//! SigNoz substitutes, and only its vocabulary is guarded."* This closes that
//! gap the same way, on `clickhouse local` in the pinned
//! `clickhouse/clickhouse-server:25.12.5` image the trial's telemetry store
//! runs. No multi-container SigNoz deployment, no persistent volume, no
//! network, no credential.
//!
//! An alert rule has one failure mode a saved dashboard query does not. A
//! dashboard that returns zero rows is *visibly* empty — someone looks at it.
//! An alert whose query returns zero rows, or returns rows the threshold can
//! never cross, is **silently** healthy forever: the absence of a page is
//! exactly what a working queue looks like. That is the same
//! absent-versus-zero hazard this epic's scope item 5 names, applied to the one
//! artifact in this directory nobody is watching.
//!
//! Two halves have to hold, and the static vocabulary guard
//! (`queue_starvation_alert_matches_the_ops_metric_vocabulary` in
//! `signoz_trial_artifacts.rs`) can only see the first:
//!
//! 1. **The embedded query names signals the emitters still produce.** Guarded
//!    statically already, in ordinary CI, with no Docker.
//! 2. **The query plus the rule's threshold actually separate a starved host
//!    from a healthy one.** Only executing it can show that, and it is where
//!    the interesting properties live: the half-open window, `max()` absorbing
//!    the duplicated `time_series_v4` hour-row, the `state = 'ready'` filter,
//!    `GROUP BY host` keeping the annotation's host label meaningful, and the
//!    fact that a measured zero is dropped by the *threshold* rather than by
//!    the query (there is no `HAVING starved > 0` here, unlike `queue-dwell.sql`
//!    query 1).
//!
//! **Derived, not restated.** The window bounds come from the committed
//! `evalWindow`; the firing decision comes from the committed `op`, `target`
//! and `matchType`. Editing any of those four in the JSON changes what this
//! test computes, so a semantic edit fails here by name instead of silently
//! re-tuning a production alert. The one thing that cannot be derived from the
//! repo is SigNoz's own integer → semantic mapping for `op` and `matchType`;
//! `MatchType` below records it as upstream's published mapping and the test
//! evaluates *both* candidate match semantics over the same engine output, so
//! the result is attributable either way.
//!
//! **What this does NOT establish.** No point went through SigNoz's own
//! ingester, metric migrator, or `time_series_v4` as SigNoz actually creates and
//! fingerprints it; no rule evaluator ran; no notification was delivered. This
//! is `clickhouse local` over a hand-written read-surface schema, the same
//! caveat `signoz_usage_queries.rs`, `signoz_cycle_time.rs` and
//! `signoz_queue_quota_queries.rs` carry. The live fire-and-resolve check on
//! the trial deployment is #9006, and the real-canary gap is #8525.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`) and
/// as every other engine-level proof in this trial, so no proof in this repo
/// can drift from the deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

/// The committed alert rule, byte for byte.
const ALERT: &str =
    include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json");

/// The synthetic metric read surface the alert's query runs over.
const FIXTURE: &str = include_str!("fixtures/signoz_queue_starvation/fixture.sql");

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

fn rows(script: &str) -> Vec<Row> {
    clickhouse(script, "JSONEachRow")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("JSONEachRow line"))
        .collect()
}

/// The committed rule, parsed.
fn rule() -> serde_json::Value {
    serde_json::from_str(ALERT).expect("alerts/queue-starvation.json must be valid JSON")
}

/// The rule's embedded ClickHouse query, verbatim — placeholders and all.
fn embedded_query(rule: &serde_json::Value) -> String {
    rule["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query must be a string")
        .to_owned()
}

/// SigNoz's `evalWindow` / `frequency` are Go `time.Duration` strings
/// (`"15m0s"`, `"5m0s"`). Parsed rather than hardcoded so the fixture's window
/// length is checked against the committed value instead of a copy of it.
fn duration_ms(spec: &str) -> i64 {
    let mut total = 0_i64;
    let mut digits = String::new();
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: i64 = digits.parse().unwrap_or_else(|_| {
            panic!("no number before unit '{c}' in duration {spec:?}");
        });
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

/// SigNoz's published threshold-rule comparators. The repo cannot derive these
/// integers, so they are recorded here as upstream's mapping and every test
/// below states which one the committed rule selected.
#[derive(Debug, PartialEq, Eq)]
enum CompareOp {
    Above,
    Below,
    Equal,
    NotEqual,
}

impl CompareOp {
    fn from_rule(op: &str) -> Self {
        match op {
            "1" => Self::Above,
            "2" => Self::Below,
            "3" => Self::Equal,
            "4" => Self::NotEqual,
            other => panic!("unknown SigNoz threshold op {other:?}"),
        }
    }

    fn holds(&self, value: f64, target: f64) -> bool {
        match self {
            Self::Above => value > target,
            Self::Below => value < target,
            Self::Equal => (value - target).abs() < f64::EPSILON,
            Self::NotEqual => (value - target).abs() >= f64::EPSILON,
        }
    }
}

/// SigNoz's published `matchType` values. Only the two this test needs to
/// distinguish are modelled; the rest panic rather than being guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchType {
    AtLeastOnce,
    AllTheTimes,
}

impl MatchType {
    fn from_rule(match_type: &str) -> Self {
        match match_type {
            "1" => Self::AtLeastOnce,
            "2" => Self::AllTheTimes,
            other => panic!(
                "SigNoz matchType {other:?} is not modelled by this test (3=OnAverage, \
                 4=InTotal, 5=Last); if the committed rule moved to it, extend \
                 MatchType and re-derive the expectations below"
            ),
        }
    }

    fn fires(&self, op: &CompareOp, target: f64, values: &[f64]) -> bool {
        if values.is_empty() {
            // No data point is not a breach: an alert that fires on silence
            // would page on every daemon restart. SigNoz's "no data" handling
            // is a separate rule setting, not this comparator.
            return false;
        }
        match self {
            Self::AtLeastOnce => values.iter().any(|v| op.holds(*v, target)),
            Self::AllTheTimes => values.iter().all(|v| op.holds(*v, target)),
        }
    }
}

/// The fixture's window bounds, read back from the engine so the test and the
/// fixture cannot disagree about them.
fn window() -> (i64, i64) {
    let script = format!("{FIXTURE}\nSELECT start_ms, end_ms FROM loom_fixture.window;");
    let out = rows(&script);
    assert_eq!(out.len(), 1, "loom_fixture.window must hold exactly one row");
    (out[0]["start_ms"].as_i64().unwrap(), out[0]["end_ms"].as_i64().unwrap())
}

/// Substitutes SigNoz's evaluation-window placeholders the way its rule
/// evaluator does, then prefixes the fixture. Panics if a placeholder is
/// missing — a query that stopped being window-bounded would scan all of
/// history on every evaluation, and must not quietly pass here.
fn evaluated(query: &str, start_ms: i64, end_ms: i64) -> String {
    for placeholder in ["{{.start_timestamp_ms}}", "{{.end_timestamp_ms}}"] {
        assert!(
            query.contains(placeholder),
            "the alert's embedded query no longer carries {placeholder}; it would scan \
             outside its evaluation window"
        );
    }
    let bound = query
        .replace("{{.start_timestamp_ms}}", &start_ms.to_string())
        .replace("{{.end_timestamp_ms}}", &end_ms.to_string());
    format!("{FIXTURE}\n{bound};")
}

/// `host` -> the bucket values the alert's query produced for it, in `ts` order.
fn series_by_host(result: &[Row]) -> BTreeMap<String, Vec<f64>> {
    let mut by_host: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for row in result {
        let host = row["host"].as_str().expect("host column").to_owned();
        let ts = row["ts"].as_str().expect("ts column").to_owned();
        let value = row["value"].as_f64().expect("value column");
        by_host.entry(host).or_default().push((ts, value));
    }
    by_host
        .into_iter()
        .map(|(host, mut points)| {
            points.sort_by(|a, b| a.0.cmp(&b.0));
            (host, points.into_iter().map(|(_, v)| v).collect())
        })
        .collect()
}

/// Runs the committed alert query over the committed window and returns its
/// per-host series.
fn committed_run() -> (serde_json::Value, BTreeMap<String, Vec<f64>>, usize) {
    let rule = rule();
    let (start_ms, end_ms) = window();
    let eval_ms = duration_ms(rule["evalWindow"].as_str().expect("evalWindow"));
    assert_eq!(
        end_ms - start_ms,
        eval_ms,
        "fixtures/signoz_queue_starvation/fixture.sql's window is {}ms long but the \
         committed rule's evalWindow is {}ms; the fixture exists to model exactly one \
         evaluation window, so rework it rather than letting the two drift",
        end_ms - start_ms,
        eval_ms
    );
    let buckets = usize::try_from(eval_ms / 60_000).unwrap();
    let query = embedded_query(&rule);
    let result = rows(&evaluated(&query, start_ms, end_ms));
    (rule, series_by_host(&result), buckets)
}

// ===========================================================================
// The committed rule, executed.
// ===========================================================================

/// The alert's embedded query parses and runs on the pinned engine, and returns
/// exactly the ready-state hosts — one series per host, one bucket per minute
/// of the evaluation window.
///
/// Three predicates are load-bearing in this one assertion, each with its own
/// counterfactual further down:
///
/// - `state = 'ready'` excludes `host-blocked-only`, whose blocked-queue count
///   sits far above the threshold for the whole window.
/// - `metric_name = 'loom.queue.starved'` excludes the sibling
///   `loom.queue.starved.by_reason` subtotals the fixture parks on
///   host-healthy's own fingerprint.
/// - the half-open window gives every host exactly `buckets` points: the
///   out-of-window spikes at `start_ms - 60s` and at exactly `end_ms` are both
///   absent, and the point at exactly `start_ms` is present.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_committed_alert_query_returns_one_minute_series_per_ready_host() {
    let (_, by_host, buckets) = committed_run();

    assert_eq!(
        by_host.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            String::new(),
            "host-flapping".to_owned(),
            "host-healthy".to_owned(),
            "host-starved".to_owned(),
        ]),
        "the alert must report every ready-state series and only those: \
         host-blocked-only is blocked-state, and loom.queue.starved.by_reason is a \
         different metric"
    );

    for (host, values) in &by_host {
        assert_eq!(
            values.len(),
            buckets,
            "host {host:?} produced {} buckets, not the {buckets} minutes of the \
             evaluation window — either the half-open `>= start` / `< end` window \
             changed, or a point leaked in from outside it",
            values.len()
        );
    }
}

/// `max()` is the aggregate, and it absorbs the duplicated `time_series_v4`
/// hour-row that `USING (fingerprint)` multiplies every point by.
///
/// The fixture gives bucket 0 of host-starved three points — 2, 3, then 1 — on
/// a series with TWO hour-rows. The answer is 3 only under `max()`: the last
/// value is 1, the first is 2, and the sum is 12 (6, doubled by the join). The
/// `sum()` counterfactual below runs that doubling rather than asserting it.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn max_absorbs_the_duplicated_hour_row_that_sum_would_double() {
    let (rule, by_host, _) = committed_run();
    let starved = &by_host["host-starved"];
    assert_eq!(
        starved[0], 3.0,
        "bucket 0 of host-starved must answer 3 (the maximum of 2, 3, 1), not the \
         last value, the first value, or any sum"
    );

    let query = embedded_query(&rule);
    assert!(
        query.contains("max(s.value)"),
        "the alert's aggregate is no longer max(); the hour-row multiplication below \
         is only harmless under max()"
    );
    let (start_ms, end_ms) = window();
    let naive = query.replace("max(s.value)", "sum(s.value)");
    let naive_rows = rows(&evaluated(&naive, start_ms, end_ms));
    let naive_starved = &series_by_host(&naive_rows)["host-starved"];
    assert_eq!(
        naive_starved[0], 12.0,
        "counterfactual: sum() over the same rows must answer 12 — the bucket's real \
         total of 6, doubled by the series' two time_series_v4 hour-rows. If this is \
         ever 6, the fixture stopped exercising the duplicate join and the max() \
         assertion above proves nothing"
    );
}

// ===========================================================================
// The rule's threshold, applied to that output.
// ===========================================================================

/// The committed `op` / `target` / `matchType` separate a genuinely starved
/// host from a healthy one and from a flapping one.
///
/// This is the whole point of the artifact, and it is the half no static guard
/// can see. Note which host does NOT fire: `host-flapping`, whose ready queue
/// was starved in 8 of the 15 minutes. Under `matchType` "all the time" that is
/// deliberately not a page — a queue that clears and re-fills is working, and
/// the rule's 15-minute window exists to demand sustained starvation.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_committed_threshold_fires_only_on_sustained_starvation() {
    let (rule, by_host, _) = committed_run();
    let condition = &rule["condition"];
    let op = CompareOp::from_rule(condition["op"].as_str().expect("op must be a string"));
    let match_type = MatchType::from_rule(
        condition["matchType"]
            .as_str()
            .expect("matchType must be a string"),
    );
    let target = condition["target"].as_f64().expect("target");
    assert_eq!(
        condition["selectedQueryName"], "A",
        "the rule must evaluate the query this test executes"
    );
    assert_eq!(op, CompareOp::Above, "committed rule: value above target");
    assert_eq!(
        match_type,
        MatchType::AllTheTimes,
        "committed rule: the breach must hold for the whole evaluation window"
    );
    assert_eq!(target, 0.0, "committed rule: any non-zero starvation");

    let firing: BTreeSet<&str> = by_host
        .iter()
        .filter(|(_, values)| match_type.fires(&op, target, values))
        .map(|(host, _)| host.as_str())
        .collect();

    assert!(
        firing.contains("host-starved"),
        "host-starved was above zero in every minute of the window and MUST fire; \
         otherwise this alert is decorative"
    );
    assert!(
        !firing.contains("host-healthy"),
        "host-healthy reported a measured zero in every minute and must not fire"
    );
    assert!(
        !firing.contains("host-flapping"),
        "host-flapping cleared its queue in 7 of 15 minutes; under matchType \
         'all the time' that is not sustained starvation"
    );
}

/// A measured zero is dropped by the **threshold**, not by the query.
///
/// `queue-dwell.sql` query 1 ends in `HAVING starved > 0`; the alert's embedded
/// query deliberately does not, because SigNoz needs the zero-valued points to
/// decide the series has *recovered*. So host-healthy's rows are present in the
/// result, carrying real zeros, and only `op`/`target` keep them from paging.
///
/// The consequence is worth recording: loosening `target` below zero — or
/// flipping `op` — turns every reporting host into a firing host, with nothing
/// in the SQL to stop it. The counterfactual runs that.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_measured_zero_is_present_in_the_result_and_dropped_only_by_the_threshold() {
    let (rule, by_host, buckets) = committed_run();
    let healthy = &by_host["host-healthy"];
    assert_eq!(
        healthy.len(),
        buckets,
        "host-healthy's zero-valued points must be RETURNED — a query that filtered \
         them out would leave SigNoz unable to see the series recover"
    );
    assert!(
        healthy.iter().all(|v| *v == 0.0),
        "host-healthy must read exactly 0.0, not NULL and not absent: {healthy:?}"
    );
    assert!(
        !embedded_query(&rule).contains("HAVING"),
        "the alert's query must not borrow queue-dwell.sql query 1's HAVING starved > 0; \
         the zero rows are what tell SigNoz the series recovered"
    );

    let op = CompareOp::from_rule(rule["condition"]["op"].as_str().unwrap());
    let match_type = MatchType::from_rule(rule["condition"]["matchType"].as_str().unwrap());
    assert!(
        match_type.fires(&op, -1.0, healthy),
        "counterfactual: with target = -1 the same healthy rows DO fire. Nothing in the \
         SQL prevents it — the committed target of 0 is the only thing distinguishing \
         'reporting zero starvation' from 'starved'"
    );
}

/// `matchType` is load-bearing, not decoration: the same engine output yields a
/// different firing set under each of SigNoz's two point-wise match semantics.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn match_type_changes_which_hosts_fire_on_identical_data() {
    let (rule, by_host, _) = committed_run();
    let op = CompareOp::from_rule(rule["condition"]["op"].as_str().unwrap());
    let target = rule["condition"]["target"].as_f64().unwrap();

    let fire_set = |m: MatchType| -> BTreeSet<String> {
        by_host
            .iter()
            .filter(|(_, values)| m.fires(&op, target, values))
            .map(|(host, _)| host.clone())
            .collect()
    };

    let all_the_times = fire_set(MatchType::AllTheTimes);
    let at_least_once = fire_set(MatchType::AtLeastOnce);

    assert_eq!(
        all_the_times,
        BTreeSet::from([String::new(), "host-starved".to_owned()]),
        "matchType 'all the time' (the committed value) fires on sustained starvation only"
    );
    assert_eq!(
        at_least_once,
        BTreeSet::from([
            String::new(),
            "host-flapping".to_owned(),
            "host-starved".to_owned(),
        ]),
        "counterfactual: 'at least once' additionally pages on host-flapping, whose \
         queue cleared in 7 of 15 minutes. Switching matchType in the committed JSON \
         changes this alert from 'sustained starvation' to 'any starvation', which is \
         a different operational contract"
    );
}

// ===========================================================================
// The query predicates, each with its counterfactual.
// ===========================================================================

/// `state = 'ready'` is what makes this "Loom **ready** queue starved" rather
/// than "some queue is non-empty". Dropping it admits host-blocked-only, whose
/// blocked work is waiting on a dependency rather than on capacity — a page
/// nobody can act on.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn dropping_the_ready_state_filter_pages_on_blocked_work() {
    let (rule, by_host, _) = committed_run();
    assert!(
        !by_host.contains_key("host-blocked-only"),
        "the committed query must not see blocked-state series at all"
    );

    let query = embedded_query(&rule);
    let marker = "  AND JSONExtractString(t.labels, 'state') = 'ready'\n";
    assert!(
        query.contains(marker),
        "the alert's state filter is no longer the line this counterfactual removes; \
         re-derive the mutation from the committed query text"
    );
    let (start_ms, end_ms) = window();
    let unfiltered = rows(&evaluated(&query.replace(marker, ""), start_ms, end_ms));
    let unfiltered_hosts = series_by_host(&unfiltered);
    let op = CompareOp::from_rule(rule["condition"]["op"].as_str().unwrap());
    let match_type = MatchType::from_rule(rule["condition"]["matchType"].as_str().unwrap());
    let target = rule["condition"]["target"].as_f64().unwrap();
    assert!(
        match_type.fires(&op, target, &unfiltered_hosts["host-blocked-only"]),
        "counterfactual: without the state filter, host-blocked-only's blocked-queue \
         count fires this alert for the whole window"
    );
}

/// `GROUP BY ts, host` is what makes the annotation's "see the alert's host
/// label" true, and it is also what keeps one starved host from being averaged
/// away. Drop `host` and the result collapses to a single label-less series
/// whose per-bucket `max()` is the worst host's — so the alert still fires, but
/// it can no longer say where, and a healthy host becomes invisible rather than
/// visibly healthy.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn dropping_the_host_grouping_collapses_every_host_into_one_unattributable_series() {
    let (rule, by_host, buckets) = committed_run();
    assert!(
        by_host.contains_key("host-starved") && by_host.contains_key("host-healthy"),
        "the committed query reports the starved and the healthy host separately"
    );

    let query = embedded_query(&rule);
    let collapsed = query
        .replace("       JSONExtractString(t.labels, 'host.id') AS host,\n", "")
        .replace("GROUP BY ts, host", "GROUP BY ts");
    let (start_ms, end_ms) = window();
    let out = rows(&evaluated(&collapsed, start_ms, end_ms));
    assert_eq!(
        out.len(),
        buckets,
        "counterfactual: without the host grouping the alert has ONE series, not one \
         per host — the annotation's host label would be empty and host-healthy would \
         be hidden behind host-starved's maximum rather than reported as healthy"
    );
    let worst = out
        .iter()
        .map(|r| r["value"].as_f64().unwrap())
        .fold(f64::MIN, f64::max);
    let ready_worst = by_host.values().flatten().copied().fold(f64::MIN, f64::max);
    assert_eq!(
        worst, ready_worst,
        "counterfactual: every bucket of the collapsed series is the worst ready-state \
         host's value, so host-healthy's measured zero and host-starved's identity are \
         both gone — one page, no attribution. Only the state filter still holds here; \
         GROUP BY is what carries the host label"
    );
    assert_eq!(
        worst, 5.0,
        "the worst ready-state value in this fixture is the unlabelled series' 5 \
         (host-blocked-only's 7 is still excluded by the state filter, which this \
         counterfactual leaves in place)"
    );
}

/// A ready-state series with no `host.id` label fires with an **empty** host
/// label rather than being dropped.
///
/// This is observed behaviour, not a desired outcome: `JSONExtractString`
/// answers `''` for a missing key, so the alert would page with an annotation
/// pointing at nothing. The static vocabulary guard
/// (`queue_starvation_alert_matches_the_ops_metric_vocabulary`) is what keeps
/// the gateway's DATAPOINT allowlist forwarding `host.id`; this test records
/// what the alert does if that guard is ever defeated, so the failure mode is
/// written down rather than discovered during an incident.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_series_without_host_id_fires_with_an_empty_host_label() {
    let (rule, by_host, buckets) = committed_run();
    let unlabelled = by_host
        .get("")
        .expect("the unlabelled ready-state series must appear, with host = ''");
    assert_eq!(unlabelled.len(), buckets);
    let op = CompareOp::from_rule(rule["condition"]["op"].as_str().unwrap());
    let match_type = MatchType::from_rule(rule["condition"]["matchType"].as_str().unwrap());
    let target = rule["condition"]["target"].as_f64().unwrap();
    assert!(
        match_type.fires(&op, target, unlabelled),
        "a host.id-less series above the threshold still fires — with nothing in the \
         alert's \"see the alert's host label\" annotation to act on"
    );
}

// ===========================================================================
// The rule's own cadence.
// ===========================================================================

/// The rule re-evaluates more often than its window is long, so consecutive
/// evaluations overlap and no minute of starvation goes unevaluated.
///
/// A `frequency` longer than `evalWindow` would leave blind gaps between
/// evaluations — sustained starvation entirely inside a gap would never be
/// seen. Both values are read from the committed JSON, and the overlap factor is
/// stated so a future edit has to justify shrinking it.
#[test]
fn the_rule_re_evaluates_more_often_than_its_window_is_long() {
    let rule = rule();
    let eval_ms = duration_ms(rule["evalWindow"].as_str().unwrap());
    let freq_ms = duration_ms(rule["frequency"].as_str().unwrap());
    assert_eq!(eval_ms, 900_000, "committed evalWindow: 15m");
    assert_eq!(freq_ms, 300_000, "committed frequency: 5m");
    assert!(
        freq_ms <= eval_ms,
        "frequency {freq_ms}ms exceeds evalWindow {eval_ms}ms: consecutive evaluations \
         would leave unobserved gaps, and starvation inside a gap would never page"
    );
    assert!(
        rule["disabled"] == serde_json::Value::Bool(false)
            && rule["condition"]["compositeQuery"]["chQueries"]["A"]["disabled"]
                == serde_json::Value::Bool(false),
        "an imported alert rule that ships disabled is silent by construction"
    );
}

/// Go duration parsing, exercised on the shapes SigNoz emits plus the ones a
/// hand-edit is likely to produce.
#[test]
fn go_duration_strings_parse() {
    assert_eq!(duration_ms("15m0s"), 900_000);
    assert_eq!(duration_ms("5m0s"), 300_000);
    assert_eq!(duration_ms("1h0m0s"), 3_600_000);
    assert_eq!(duration_ms("30s"), 30_000);
    assert_eq!(duration_ms("250ms"), 250);
    assert_eq!(duration_ms("1m30s"), 90_000);
}
