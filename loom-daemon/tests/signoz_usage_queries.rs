//! Live proof for the SigNoz measured-usage artifact (Issue #8528, scope item
//! 4): requires Docker, never converts missing Docker to a pass. The static
//! half is `signoz_trial_artifacts.rs`, which runs everywhere.
//!
//! `usage-queries.sql` is executed **verbatim** — the whole committed file, as
//! committed, in one pass, and then statement by statement — against the same pinned
//! ClickHouse the SigNoz trial's telemetry store runs, over a fixture built to
//! make each of the file's five stated traps fail loudly if the SQL stops
//! handling it:
//!
//! 1. **Every span attribute is a string.** The trace mapper renders the whole
//!    attribute map with `kv_string`, so the token counts and the USD estimate
//!    land in `attributes_string`. The negative control below reads
//!    `attributes_number['loom.tokens.total']` on the same rows and observes
//!    the failure mode the header warns about: **0 on every row, no error** —
//!    the SigNoz-side equivalent of the `ci-queries.sql` container rule, which
//!    the static test can assert but cannot demonstrate.
//! 2. **Scope is not additive.** Trace T1 carries an `execution` span whose
//!    counters deliberately overlap its `attempt` spans. A query that summed
//!    both would report 330 tokens for a sweep that used 165.
//! 3. **An unpriced model carries no cost attributes.** Trace T2's model has
//!    real tokens and no `loom.cost.usd_estimate`, so `sum()` skips it and the
//!    spend total is a lower bound — which the query has to say out loud.
//! 4. **Absence is not zero.** T1 has a Judge attempt with no usage child and a
//!    Doctor attempt whose measured total is `"0"`. They must never merge.
//! 5. **`loom.repo` is not on the usage span.** It appears only on the trace's
//!    root, so section 2's repo column is only non-empty if the join works.
//!
//! Plus the property every section depends on: delivery is at least once, so
//! the fixture delivers one execution span **twice** with the same derived
//! `(trace_id, span_id)`, and no section may count it twice.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`) and
/// as `cycle_time_clickhouse.rs`, so no proof in this repo can drift from the
/// deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const QUERIES: &str = include_str!("../../defaults/observability/signoz/usage-queries.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_usage/fixture.sql");

/// A window that covers the whole fixture.
const SINCE: &str = "2026-09-01 00:00:00";

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
            "--param_repo=",
            "--param_top=50",
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
/// split on `;`, because the file's own prose uses semicolons and a naive split
/// would cut a statement in half mid-sentence. No `;` or `--` occurs inside a
/// string literal in this artifact, so what remains is each statement's exact
/// committed text. The strict verbatim proof is the whole-file run above, which
/// executes the bytes as committed, comments included.
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

/// Every section of the committed file, executed in one session after the
/// fixture, each preceded by a marker row so the concatenated `JSONEachRow`
/// output can be attributed. The statements themselves are verbatim; only the
/// marker `SELECT`s are added.
fn sections() -> Vec<Vec<Row>> {
    let committed = statements(QUERIES);
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
        "every committed statement must have produced a section"
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

fn by<'a>(rows: &'a [Row], key: &str, value: &str) -> &'a Row {
    rows.iter()
        .find(|row| text(row, key) == value)
        .unwrap_or_else(|| panic!("no row with {key} = {value} in {rows:?}"))
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_usage_queries_answer_the_measured_usage_questions_on_real_clickhouse() {
    // ---- the file runs verbatim, as one pass, exactly as documented --------
    let verbatim = format!("{FIXTURE}\n{QUERIES}");
    clickhouse(&verbatim, "TSV");

    let sections = sections();
    assert_eq!(sections.len(), 7, "usage-queries.sql is documented as sections 0 through 6");

    // ---- 0. arrival and shape ---------------------------------------------
    let preflight = &sections[0];
    let execution = by(preflight, "scope", "execution");
    assert_eq!(
        num(execution, "spans"),
        1,
        "the execution span was delivered twice; the preflight must de-duplicate \
         on (trace_id, span_id) and report one"
    );
    assert_eq!(num(execution, "priced"), 1);
    let attempt = by(preflight, "scope", "attempt");
    assert_eq!(num(attempt, "spans"), 3);
    assert_eq!(num(attempt, "priced"), 2);
    assert_eq!(
        num(attempt, "unpriced"),
        1,
        "the fixture's unknown model must be counted as unpriced, never as $0"
    );
    assert_eq!(
        num(attempt, "with_runtime"),
        2,
        "the in-session usage span carries no loom.runtime; that must show as a \
         missing attribute, not as an empty runtime named ''"
    );

    // ---- 1. spend and tokens by model --------------------------------------
    let by_model = &sections[1];
    assert_eq!(by_model.len(), 2, "two models in the fixture window");
    let opus = by(by_model, "model", "claude-opus-5");
    assert_eq!(
        num(opus, "total_tokens"),
        165,
        "the sweep carries BOTH scopes with overlapping counters; totalling both \
         would report 330 for a sweep that used 165"
    );
    assert_eq!(num(opus, "spans"), 1);
    assert_eq!(num(opus, "unpriced_spans"), 0);
    let unpriced = by(by_model, "model", "unpriced-model-1");
    assert_eq!(num(unpriced, "total_tokens"), 999);
    assert_eq!(
        num(unpriced, "unpriced_spans"),
        1,
        "the spend total excludes this model, so the query must say how many \
         spans it excluded — otherwise the total reads as a complete figure"
    );
    assert!(
        unpriced.get("usd_lower_bound").is_some_and(|v| v.is_null()),
        "an unpriced model must yield NULL dollars, never 0.0: {unpriced:?}"
    );

    // ---- 2. the ClickStack parity view: by repo/role/runtime/model ---------
    let by_repo = &sections[2];
    let alpha = by(by_repo, "repo", "org/alpha");
    assert_eq!(
        text(alpha, "model"),
        "claude-opus-5",
        "repo is not an attribute of the usage span; this row only exists if the \
         join to the trace's other spans found it"
    );
    assert_eq!(num(alpha, "total_tokens"), 165);
    assert_eq!(num(alpha, "repos_in_trace"), 1);
    assert_eq!(
        text(alpha, "role"),
        "",
        "an execution-scoped span carries no loom.role; the empty cell is a \
         MISSING attribute and must not be back-filled from the attempt spans"
    );
    let beta = by(by_repo, "repo", "org/beta");
    assert_eq!(num(beta, "total_tokens"), 999);
    assert_eq!(text(beta, "role"), "builder");

    // ---- 3. missing usage vs measured zero ---------------------------------
    let coverage = &sections[3];
    let judge = by(coverage, "role", "judge");
    assert_eq!(num(judge, "usage_unknown"), 1);
    assert_eq!(num(judge, "measured_zero_only"), 0);
    assert_eq!(num(judge, "measured_usage"), 0);
    let doctor = by(coverage, "role", "doctor");
    assert_eq!(
        num(doctor, "measured_zero_only"),
        1,
        "an attempt measured at zero tokens must stay distinguishable from one \
         whose usage was never determined"
    );
    assert_eq!(num(doctor, "usage_unknown"), 0);
    let builder = by(coverage, "role", "builder");
    assert_eq!(num(builder, "attempts"), 2);
    assert_eq!(num(builder, "measured_usage"), 2);

    // ---- 4. which models the spend total excludes --------------------------
    let unpriced_models = &sections[4];
    assert_eq!(unpriced_models.len(), 1);
    assert_eq!(text(&unpriced_models[0], "model"), "unpriced-model-1");
    assert_eq!(num(&unpriced_models[0], "total_tokens"), 999);

    // ---- 5. pricing provenance ---------------------------------------------
    let provenance = &sections[5];
    assert_eq!(provenance.len(), 1, "one rate card priced the whole fixture");
    assert_eq!(text(&provenance[0], "source"), "compiled");
    assert_eq!(text(&provenance[0], "verified_on"), "2026-09-18");
    assert_eq!(
        num(&provenance[0], "priced_spans"),
        3,
        "the census covers both scopes after de-duplication: two attempt spans \
         and the one execution span delivered twice"
    );

    // ---- 6. cache composition ----------------------------------------------
    let cache = &sections[6];
    let opus = by(cache, "model", "claude-opus-5");
    assert_eq!(
        num(opus, "input_side_total"),
        115,
        "the four input-side counters are DISJOINT: 100 uncached + 10 cache read \
         + 3 five-minute writes + 2 one-hour writes"
    );
    assert_eq!(num(opus, "cache_read_tokens"), 10);
    assert_eq!(num(opus, "cache_write_5m_tokens"), 3);
    assert_eq!(
        num(opus, "cache_write_1h_tokens"),
        2,
        "the two cache-write horizons are priced differently and must not be \
         collapsed into one column"
    );
}

/// The negative control for the artifact's first documented trap. Reading a
/// token counter from `attributes_number` — the container a reader coming from
/// `ci-queries.sql`'s log sections would reach for — does not error on the
/// pinned engine. It returns 0 for every row, which is indistinguishable from
/// a backend that lost the data. Demonstrated here so the header's claim is an
/// observation rather than an assertion.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn reading_a_token_counter_from_attributes_number_silently_returns_zero() {
    let probe = format!(
        "{FIXTURE}\n\
         SELECT count() AS usage_spans,\n\
                sum(attributes_number['loom.tokens.total']) AS wrong_container,\n\
                sum(toInt64OrNull(attributes_string['loom.tokens.total'])) AS right_container\n\
         FROM signoz_traces.signoz_index_v3\n\
         WHERE name = 'loom.runtime.usage';\n"
    );
    let output = clickhouse(&probe, "JSONEachRow");
    let row: Row = serde_json::from_str(output.trim()).expect("one JSONEachRow line");
    assert_eq!(
        num(&row, "usage_spans"),
        5,
        "the probe must see every usage row, duplicate included"
    );
    assert_eq!(
        num(&row, "wrong_container"),
        0,
        "if this ever stops being 0, the pinned SigNoz schema changed where span \
         attributes land and usage-queries.sql's container rule must be revisited"
    );
    assert_eq!(
        num(&row, "right_container"),
        1494,
        "165 (attempt) + 0 (measured zero) + 165 + 165 (the execution span, \
         delivered twice) + 999 — the un-de-duplicated, both-scopes sum every \
         committed section avoids in its own way"
    );
}
