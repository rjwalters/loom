//! Live proof for the SigNoz trial's two *metric*-signal artifacts (Issue
//! #8528 scope item 4, "host/token gauges"): `signoz/queue-dwell.sql` (#8856)
//! and `signoz/quota-utilization.sql` (#9005). Requires Docker; never converts
//! missing Docker to a pass.
//!
//! Both files had only a static *vocabulary* guard in `signoz_trial_artifacts.rs`
//! — it proves they name metrics and labels the emitters still produce, which is
//! what stops a saved view going quietly empty after a rename. It cannot see
//! whether the SQL computes the right number. `queue-dwell.sql`'s README entry
//! said outright "Neither has been executed against a live SigNoz", and
//! `quota-utilization.sql`'s said "Verified against a local `clickhouse-local`
//! with a mock `samples_v4` / `time_series_v4` shape" — an ad-hoc session whose
//! fixture and output were never committed, so nothing re-ran it.
//!
//! This closes that gap the way `signoz_usage_queries.rs` (#9705) and
//! `signoz_cycle_time.rs` (#9775) closed it for this trial's two *trace*/*log*
//! artifacts: `clickhouse local` in the same pinned
//! `clickhouse/clickhouse-server:25.12.5` image the trial's telemetry store
//! runs, over rows shaped like the real metric schema's read surface. No
//! multi-container SigNoz deployment, no persistent volume, no network.
//!
//! These are the FIRST committed artifacts in this trial proven against
//! `signoz_metrics.samples_v4` / `time_series_v4` at all; every earlier proof
//! read `signoz_traces.signoz_index_v3` or `signoz_logs.distributed_logs_v2`.
//! The metric tables have their own trap, which both files' headers name and
//! this test demonstrates rather than asserts: SigNoz writes **one
//! `time_series_v4` row per series per hour**, so the `USING (fingerprint)`
//! join multiplies every data point by that series' hour-row count. `max()`
//! absorbs it; `sum()` does not. The fixture gives six fingerprints two
//! hour-rows each so the multiplication is real here, and
//! `the_naive_fingerprint_join_doubles_query_4s_wait_seconds` runs the
//! counterfactual to show what the de-duplicating sub-select buys.
//!
//! The other properties proven below, each a documented claim of one of the two
//! files rather than an invented one:
//!
//! - **A measured zero is not starvation.** A host reporting
//!   `loom.queue.starved` = 0 must be dropped by `HAVING starved > 0`, not
//!   reported as a starved host with a zero.
//! - **Absent is not zero** (`quota-utilization.sql`'s own header): a provider
//!   with no utilization source emits only `loom.tokens.exhausted`, so its
//!   utilization reads NULL via `maxOrNullIf()` — plain `maxIf()` would answer
//!   0, i.e. "fully idle".
//! - **Over-100% utilization clamps to zero headroom**, not to negative
//!   headroom (`least(prev_value, 1)`).
//! - **A missing companion metric yields NULL, not a division error**:
//!   `loom.queue.dispatch_wait` without `.samples` goes through
//!   `nullIf(dispatches, 0)`.
//! - **Query 5 reads SPANS, not metrics**, and every dispatch-span attribute is
//!   a string (`observability/ops/disposition.rs` stores `rank.to_string()`),
//!   so a cause-less pre-#9673 row reads `halt_cause` as `''` and a reader who
//!   reached into `attributes_number` would silently get 0.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`) and
/// as `signoz_usage_queries.rs` / `signoz_cycle_time.rs`, so no proof in this
/// repo can drift from the deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const QUEUE_DWELL: &str = include_str!("../../defaults/observability/signoz/queue-dwell.sql");
const QUOTA: &str = include_str!("../../defaults/observability/signoz/quota-utilization.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_queue_quota/fixture.sql");

/// One result row: column name -> JSON value, as `JSONEachRow` renders it.
type Row = BTreeMap<String, serde_json::Value>;

/// Runs `script` through `clickhouse local` in the pinned image and returns
/// stdout. Panics with the engine's own stderr on failure — a query that does
/// not parse must fail this test, not be silently skipped.
///
/// Neither committed file takes a bound parameter: both are windowed on
/// `now()`, which is why the fixture anchors its rows to computed points
/// instead of fixed timestamps.
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

/// The committed file's statements. Line comments are stripped **before** the
/// split on `;`, mirroring `signoz_usage_queries.rs` / `signoz_cycle_time.rs`:
/// no `;` or `--` occurs inside a string literal in either artifact, so what
/// remains is each statement's exact committed text. The strict verbatim proof
/// is the whole-file run in each test, which executes the bytes as committed,
/// comments included; this is only used to attribute each query's output to its
/// question.
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

/// Every numbered query in `sql`, executed in one session after the fixture,
/// each preceded by a marker row so the concatenated `JSONEachRow` output can
/// be attributed. The queries themselves are verbatim; only the marker
/// `SELECT`s are added.
fn sections(sql: &str) -> Vec<Vec<Row>> {
    let committed = statements(sql);
    let mut script = String::from(FIXTURE);
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
        "every committed query must have produced a section"
    );
    sections
}

fn cell<'a>(row: &'a Row, key: &str) -> &'a serde_json::Value {
    row.get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"))
}

fn num(row: &Row, key: &str) -> f64 {
    let value = cell(row, key);
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("column {key} is not a number in {row:?}"))
}

fn text<'a>(row: &'a Row, key: &str) -> &'a str {
    cell(row, key)
        .as_str()
        .unwrap_or_else(|| panic!("column {key} is not a string in {row:?}"))
}

fn is_null(row: &Row, key: &str) -> bool {
    cell(row, key).is_null()
}

/// Float equality for values the queries themselves have already rounded.
#[track_caller]
fn close(row: &Row, key: &str, expected: f64) {
    let actual = num(row, key);
    assert!(
        (actual - expected).abs() < 1e-9,
        "column {key}: expected {expected}, got {actual} in {row:?}"
    );
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_queue_dwell_queries_answer_the_starvation_questions_on_real_clickhouse() {
    // ---- the file runs verbatim, as one pass, exactly as the README says ---
    clickhouse(&format!("{FIXTURE}\n{QUEUE_DWELL}"), "TSV");

    let sections = sections(QUEUE_DWELL);
    assert_eq!(sections.len(), 5, "queue-dwell.sql is documented as queries 1 through 5");

    // ---- 1. starved issues per host and state, last 24 h -------------------
    let starved = &sections[0];
    assert_eq!(
        starved.len(),
        2,
        "host-aaa in two states; host-bbb's measured ZERO must be dropped by \
         HAVING starved > 0, and fp 1001's three-days-old point must fall \
         outside the 24 h window: {starved:?}"
    );
    assert!(
        starved.iter().all(|row| text(row, "host") == "host-aaa"),
        "host-bbb reported starved = 0, i.e. a healthy queue. Reporting it as a \
         starved host with a zero would page on a non-event: {starved:?}"
    );
    // ORDER BY bucket, host, state — one bucket, one host, so state orders.
    assert_eq!(text(&starved[0], "state"), "curated");
    close(&starved[0], "starved", 1.0);
    assert_eq!(text(&starved[1], "state"), "ready");
    close(
        &starved[1],
        "starved",
        // Three points land in ONE 5-minute bucket at 2, 3, 1. `max()` is the
        // only aggregate that answers 3; `sum()` would say 6 and the last
        // value would say 1. The series also has two time_series_v4 hour-rows,
        // so the join emits each point twice — which `max()` absorbs.
        3.0,
    );
    assert_eq!(
        text(&starved[0], "bucket"),
        text(&starved[1], "bucket"),
        "both states were sampled in the same 5-minute bucket"
    );

    // ---- 2. why the starved issues are held --------------------------------
    let reasons = &sections[1];
    assert_eq!(reasons.len(), 2);
    assert_eq!(
        text(&reasons[0], "reason"),
        "concurrency_cap",
        "ORDER BY peak_starved DESC: the dominant reason leads"
    );
    close(&reasons[0], "peak_starved", 3.0);
    assert_eq!(text(&reasons[1], "reason"), "repo_slice");
    close(&reasons[1], "peak_starved", 1.0);

    // ---- 3. oldest waiting issue per host and state, hourly ---------------
    let oldest = &sections[2];
    assert_eq!(oldest.len(), 3, "two hours on host-aaa, one on host-bbb");
    // ORDER BY hour, host, state. day_a precedes day_b.
    close(&oldest[0], "oldest_wait_hours", 7.0); // 25200 s, peak of the hour
    assert_eq!(text(&oldest[0], "host"), "host-aaa");
    close(&oldest[1], "oldest_wait_hours", 1.0); // 3600 s the next day
    assert_eq!(text(&oldest[1], "host"), "host-aaa");
    close(&oldest[2], "oldest_wait_hours", 0.5); // 1800 s, sub-hour wait
    assert_eq!(text(&oldest[2], "host"), "host-bbb");
    assert!(
        text(&oldest[0], "hour") < text(&oldest[1], "hour"),
        "the two hour buckets must be distinct and ascending: {oldest:?}"
    );
    assert_eq!(
        text(&oldest[1], "hour"),
        text(&oldest[2], "hour"),
        "both hosts' second-day samples share one hour bucket"
    );

    // ---- 4. mean dispatch wait per host per day ---------------------------
    // The one SUMMING query, and the reason the de-duplicating sub-select
    // exists. See `the_naive_fingerprint_join_doubles_query_4s_wait_seconds`.
    let wait = &sections[3];
    assert_eq!(wait.len(), 2, "one day per host: {wait:?}");
    let aaa = &wait[0];
    assert_eq!(text(aaa, "host"), "host-aaa");
    close(aaa, "wait_secs", 1800.0);
    close(aaa, "dispatches", 6.0);
    close(aaa, "mean_wait_minutes", 5.0);
    let bbb = &wait[1];
    assert_eq!(text(bbb, "host"), "host-bbb");
    close(bbb, "wait_secs", 600.0);
    close(bbb, "dispatches", 0.0);
    assert!(
        is_null(bbb, "mean_wait_minutes"),
        "host-bbb has wait-seconds but no `.samples` companion series. \
         nullIf(dispatches, 0) must make the mean NULL — a 0 would read as \
         'dispatches are instant' and an unguarded divide would error: {bbb:?}"
    );
    assert_ne!(
        text(aaa, "day"),
        text(bbb, "day"),
        "the two hosts' samples are on different days, so toDate must split them"
    );

    // ---- 5. why hasn't owner/repo#98 started? (SPANS, not metrics) ---------
    let trail = &sections[4];
    assert_eq!(
        trail.len(),
        4,
        "owner/repo#98's last 24 h: three dispositions and one admission. The \
         other three dispositions — issue 97, owner/other#98, and #98 three \
         days ago — and all four tick spans must be filtered out: {trail:?}"
    );
    // ORDER BY timestamp DESC: the admission attempt is newest.
    let admission = &trail[0];
    assert_eq!(text(admission, "name"), "loom.dispatch.admission");
    assert_eq!(text(admission, "admission_result"), "rejected");
    assert_eq!(text(admission, "admission_reason"), "concurrency_cap");
    assert_eq!(
        text(admission, "disposition"),
        "",
        "an admission span carries no loom.queue.disposition; the empty cell is \
         a MISSING attribute, not a disposition named ''"
    );
    let capacity = &trail[1];
    assert_eq!(text(capacity, "name"), "loom.dispatch.disposition");
    assert_eq!(text(capacity, "disposition"), "capacity");
    assert_eq!(text(capacity, "rank"), "2");
    assert_eq!(text(capacity, "candidate_rank"), "2");
    assert_eq!(text(capacity, "total_candidates"), "4");
    assert_eq!(text(capacity, "priority_score"), "0.61");
    assert_eq!(text(capacity, "transition"), "workspace_halted->capacity");
    assert_eq!(
        text(capacity, "halt_cause"),
        "",
        "a capacity row has no halt cause to carry; the empty cell must not be \
         read as a cause named ''"
    );
    let legacy = &trail[2];
    assert_eq!(text(legacy, "disposition"), "workspace_halted");
    assert_eq!(
        text(legacy, "halt_cause"),
        "",
        "this row omits loom.queue.halt_cause entirely — the shape every \
         halted row had before #9673. Reading an absent Map key yields '' \
         rather than erroring, which is exactly why the file's trailing note \
         sends the reader to the parent tick; observed here, not assumed"
    );
    assert_eq!(
        text(legacy, "total_candidates"),
        "",
        "and it predates #9669's queue-position metadata too"
    );
    let halted = &trail[3];
    assert_eq!(text(halted, "disposition"), "workspace_halted");
    assert_eq!(
        text(halted, "halt_cause"),
        "main_red",
        "a #9673-era row names its own cause inline, so the parent-tick join \
         below is a fallback and not the only path"
    );
    assert_eq!(text(halted, "candidate_rank"), "1");
    assert_eq!(text(halted, "priority_score"), "0.87");
    assert!(
        trail.iter().all(|row| text(row, "host") == "host-aaa"),
        "host comes from resources_string, not attributes_string: {trail:?}"
    );

    // The documented fallback for the cause-less halted row: join to its parent
    // `loom.dispatch.tick` and read `loom.dispatch.result` there.
    let parent = clickhouse(
        &format!(
            "{FIXTURE}\n\
             SELECT d.span_id AS span_id,\n\
                    t.attributes_string['loom.dispatch.result'] AS tick_result\n\
             FROM signoz_traces.signoz_index_v3 AS d\n\
             INNER JOIN signoz_traces.signoz_index_v3 AS t\n\
                     ON t.span_id = d.parent_span_id\n\
             WHERE d.name = 'loom.dispatch.disposition'\n\
               AND d.attributes_string['loom.queue.disposition'] = 'workspace_halted'\n\
               AND d.attributes_string['loom.queue.halt_cause'] = ''\n\
               AND t.name = 'loom.dispatch.tick';\n"
        ),
        "JSONEachRow",
    );
    let parent: Vec<Row> = parent
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONEachRow line"))
        .collect();
    assert_eq!(parent.len(), 1, "exactly one cause-less halted row in the fixture: {parent:?}");
    assert_eq!(
        text(&parent[0], "tick_result"),
        "halted_main_red",
        "the cause-less row's parent tick carries the cause, exactly as the \
         file's trailing note instructs — so the documented escape hatch works"
    );
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_quota_utilization_queries_answer_the_saturation_questions_on_real_clickhouse() {
    // ---- the file runs verbatim, as one pass, exactly as the README says ---
    clickhouse(&format!("{FIXTURE}\n{QUOTA}"), "TSV");

    let sections = sections(QUOTA);
    assert_eq!(sections.len(), 3, "quota-utilization.sql is documented as queries 1 through 3");

    // ---- 1. per-account 5h and weekly utilization, hourly peak ------------
    let util = &sections[0];
    assert_eq!(
        util.len(),
        4,
        "two anthropic accounts over two hours. acct-z emits only \
         loom.tokens.exhausted, so it has no utilization series to group and \
         must not appear at all: {util:?}"
    );
    assert!(
        util.iter().all(|row| text(row, "provider") == "anthropic"),
        "the zai account has no usage source; inventing a row for it would \
         imply a measurement that does not exist: {util:?}"
    );
    // ORDER BY hour, provider, account.
    let a_day_a = &util[0];
    assert_eq!(text(a_day_a, "account"), "acct-a");
    close(a_day_a, "util_5h", 0.30);
    close(
        a_day_a,
        "util_weekly",
        // Two weekly readings in the hour (0.40 then 0.82); the hourly PEAK is
        // 0.82. The series also has two time_series_v4 hour-rows, so each
        // reading arrives twice — absorbed by max().
        0.82,
    );
    let b_day_a = &util[1];
    assert_eq!(text(b_day_a, "account"), "acct-b");
    assert!(
        is_null(b_day_a, "util_5h"),
        "acct-b has a weekly reading in this hour but no 5-hour reading. \
         maxOrNullIf() must answer NULL; the plain maxIf() the header warns \
         about would answer 0.0, i.e. 'this account used none of its 5h \
         window' — a measurement that was never taken: {b_day_a:?}"
    );
    close(b_day_a, "util_weekly", 1.05);
    let a_day_b = &util[2];
    assert_eq!(text(a_day_b, "account"), "acct-a");
    close(a_day_b, "util_5h", 0.55);
    close(a_day_b, "util_weekly", 0.05);
    let b_day_b = &util[3];
    assert_eq!(text(b_day_b, "account"), "acct-b");
    close(b_day_b, "util_5h", 0.20);
    close(b_day_b, "util_weekly", 0.10);
    assert!(
        text(a_day_a, "hour") < text(a_day_b, "hour"),
        "the two hour buckets must be distinct and ascending: {util:?}"
    );

    // ---- 2. idle headroom at each detected weekly reset --------------------
    let resets = &sections[1];
    assert_eq!(
        resets.len(),
        2,
        "exactly one reset per account. Each account's weekly series has TWO \
         time_series_v4 hour-rows, so an un-de-duplicated join would emit the \
         same reset twice: {resets:?}"
    );
    let a = &resets[0];
    assert_eq!(text(a, "account"), "acct-a");
    close(a, "util_at_reset", 0.82);
    close(a, "idle_headroom", 0.18);
    let b = &resets[1];
    assert_eq!(text(b, "account"), "acct-b");
    close(b, "util_at_reset", 1.05);
    close(
        b,
        "idle_headroom",
        // least(prev_value, 1) clamps a reading above 100% of the window.
        // Without it this reads -0.05: negative idle capacity.
        0.0,
    );
    assert_eq!(
        text(a, "window_end"),
        text(b, "window_end"),
        "both accounts' windows rolled over at the same sampled instant"
    );

    // ---- 3. capacity used per provider last week --------------------------
    let providers = &sections[2];
    assert_eq!(providers.len(), 2, "ORDER BY provider: anthropic, then zai");
    let anthropic = &providers[0];
    assert_eq!(text(anthropic, "provider"), "anthropic");
    close(anthropic, "accounts", 2.0);
    close(anthropic, "accounts_measured", 2.0);
    assert_eq!(text(anthropic, "coverage"), "measured");
    // avg(0.82, 1.05).
    close(anthropic, "used_fraction_last_week", 0.935);
    // 1 - avg(0.82, min(1.05, 1)) = 1 - 0.91. The idle column clamps where the
    // used column does not, so a provider that overshot one account's window is
    // never credited with negative idle capacity.
    close(anthropic, "idle_fraction_last_week", 0.09);
    let zai = &providers[1];
    assert_eq!(text(zai, "provider"), "zai");
    close(zai, "accounts", 1.0);
    close(
        zai,
        "accounts_measured",
        // The account exists in the pool (it reports `exhausted`) but has no
        // weekly utilization source at all.
        0.0,
    );
    assert_eq!(
        text(zai, "coverage"),
        "unknown",
        "a provider with no utilization source must SAY it is unmeasured"
    );
    assert!(
        is_null(zai, "used_fraction_last_week") && is_null(zai, "idle_fraction_last_week"),
        "no source, no number. A 0 here would read as a completely unused \
         subscription and a 1.0 idle fraction as free capacity to dispatch \
         into — the exact inversion of the truth for an exhausted account: \
         {zai:?}"
    );
}

/// The counterfactual that makes query 4's de-duplicating sub-select a measured
/// decision rather than a stylistic one. Both files' headers state that
/// `time_series_v4` holds one row per series per hour and that a plain join
/// would count each sample once per hour-row; this runs the plain join against
/// the same fixture and observes the damage.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_naive_fingerprint_join_doubles_query_4s_wait_seconds() {
    let committed = statements(QUEUE_DWELL);
    let query_4 = &committed[3];
    assert!(
        query_4.contains("GROUP BY fingerprint"),
        "query 4 is the one that de-duplicates the series join; if the file is \
         reordered, re-point this counterfactual: {query_4}"
    );
    // The same query with the de-duplicating sub-select replaced by the direct
    // table reference — i.e. what a reader who copied query 1's join shape into
    // a summing query would write.
    let naive = "SELECT JSONExtractString(t.labels, 'host.id') AS host,\n\
        sumIf(s.value, s.metric_name = 'loom.queue.dispatch_wait') AS wait_secs,\n\
        sumIf(s.value, s.metric_name = 'loom.queue.dispatch_wait.samples') AS dispatches\n\
        FROM signoz_metrics.samples_v4 AS s\n\
        INNER JOIN signoz_metrics.time_series_v4 AS t USING (fingerprint)\n\
        WHERE s.metric_name IN ('loom.queue.dispatch_wait', 'loom.queue.dispatch_wait.samples')\n\
          AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 30 DAY) * 1000\n\
        GROUP BY host ORDER BY host";

    let output = clickhouse(
        &format!("{FIXTURE}\nSELECT 0 AS loom_section_marker;\n{query_4};\n{naive};\n"),
        "JSONEachRow",
    );
    let mut committed_rows: Vec<Row> = Vec::new();
    let mut naive_rows: Vec<Row> = Vec::new();
    let mut seen_marker = false;
    for line in output.lines() {
        let row: Row = serde_json::from_str(line).expect("JSONEachRow line");
        if row.contains_key("loom_section_marker") {
            seen_marker = true;
            continue;
        }
        assert!(seen_marker, "the marker precedes every result row");
        // Only the committed query emits a `day` column.
        if row.contains_key("day") {
            committed_rows.push(row);
        } else {
            naive_rows.push(row);
        }
    }

    let committed_aaa = committed_rows
        .iter()
        .find(|row| text(row, "host") == "host-aaa")
        .expect("host-aaa in the committed answer");
    close(committed_aaa, "wait_secs", 1800.0);
    close(committed_aaa, "dispatches", 6.0);

    let naive_aaa = naive_rows
        .iter()
        .find(|row| text(row, "host") == "host-aaa")
        .expect("host-aaa in the naive answer");
    close(
        naive_aaa,
        "wait_secs",
        // Both of host-aaa's series carry two hour-rows, so every sample is
        // joined twice.
        3600.0,
    );
    close(naive_aaa, "dispatches", 12.0);
    assert!(
        num(naive_aaa, "wait_secs") == 2.0 * num(committed_aaa, "wait_secs"),
        "the naive join must visibly double the total, otherwise this fixture \
         is not exercising the duplicate hour-rows the sub-select exists for"
    );
}

/// The metric-signal analogue of `signoz_usage_queries.rs`'s negative control.
/// Query 5 reads every dispatch attribute out of `attributes_string`, because
/// `observability/ops/disposition.rs` builds the span's attribute map as
/// `BTreeMap<String, String>` and the OTLP mapper renders it with `kv_string`.
/// A reader who assumed `loom.queue.rank` and `loom.queue.priority_score` were
/// numbers would get 0 for every row and no error at all.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn reading_a_dispatch_attribute_from_attributes_number_silently_returns_zero() {
    let probe = format!(
        "{FIXTURE}\n\
         SELECT count() AS disposition_spans,\n\
                sum(attributes_number['loom.queue.total_candidates']) AS wrong_container,\n\
                sum(toInt64OrZero(attributes_string['loom.queue.total_candidates'])) \
                    AS right_container\n\
         FROM signoz_traces.signoz_index_v3\n\
         WHERE name = 'loom.dispatch.disposition';\n"
    );
    let output = clickhouse(&probe, "JSONEachRow");
    let row: Row = serde_json::from_str(output.trim()).expect("one JSONEachRow line");
    close(&row, "disposition_spans", 6.0);
    close(&row, "wrong_container", 0.0);
    close(
        &row,
        "right_container",
        // Only two of the six disposition spans carry the key (4 + 4); the
        // other four predate #9669's queue-position metadata.
        8.0,
    );
    assert!(
        num(&row, "wrong_container") == 0.0 && num(&row, "right_container") > 0.0,
        "if this ever stops holding, the pinned SigNoz schema changed where span \
         attributes land and queue-dwell.sql query 5's container choice must be \
         revisited"
    );
}
