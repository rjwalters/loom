//! Live proof for the SigNoz trial's ETA-accuracy artifact (Issue #8528 scope
//! items 4 and 5 / Issue #9289): `signoz/eta-queries.sql`, executed verbatim
//! against the same pinned ClickHouse the trial's telemetry store runs.
//! Requires Docker; never converts missing Docker to a pass.
//!
//! This was the last query artifact in the trial whose only guard was static.
//! `eta_artifacts.rs` ties every attribute the file reads to one the OTLP
//! mapping emits and the gateway forwards — which is what stops a saved view
//! going quietly empty after a rename — and `signoz_trial_artifacts.rs` checks
//! the same against the allowlist. Neither can see whether the SQL computes
//! the right number, and the file's header makes several claims that only an
//! engine can settle. Three of them turned out to be wrong or incomplete, and
//! the artifact was corrected in the same change that added this test:
//!
//! 1. **A constant feature is not scored as uncorrelated.** `rankCorr`
//!    average-ranks ties, so a feature that never varied over n observations
//!    comes out at exactly **0.5** — which `ORDER BY abs(rank_corr) DESC`
//!    ranks above any genuine correlation weaker than that — while `corr`
//!    answers NaN for the same column (in a spelling that depends on the
//!    architecture — see [`is_nan`]). Q3 now carries `distinct_values` so the
//!    two are distinguishable.
//! 2. **The typed feature extraction's damage is not the one the header
//!    described.** `JSONExtractKeysAndValues(body, 'features', 'Float64')`
//!    does not read an unmeasured (`null`) feature as 0 — it drops the key,
//!    as it drops `"refactor"`. What it actually does is *coerce* `"42"` to
//!    42 and `true` to 1, admitting two non-numeric features into the ranking
//!    as constants. `the_typed_feature_extraction_coerces_non_numbers` runs
//!    that counterfactual.
//! 3. **ROLLUP's subtotal rows were indistinguishable from a real group of
//!    unlabelled records.** ROLLUP fills an aggregated column with the type's
//!    default, which for these `String` columns is `''` — exactly what
//!    `attributes_string['loom.eta.heuristic']` answers when the key is
//!    missing. On this fixture Q2 emitted **two rows with the identical key**
//!    `('', '', '')`: the grand total and the unlabelled heuristic's own
//!    total. Q2 now carries `rolled_up`.
//!
//! The properties proven below are each a documented claim of the artifact
//! rather than an invented one, and every one has the mutation of the
//! **committed SQL** that breaks it run as a counterfactual rather than
//! described:
//!
//! - **Absent is never zero.** An `abandoned` outcome carries no
//!   `loom.eta.error_sec` key at all (`opt_int` only emits a field it has).
//!   Drop the presence filter and it is scored as a *perfect* prediction,
//!   improving the heuristic's apparent MAE — the worst possible direction for
//!   a silent defect to move a promotion metric.
//! - **Provenance groups the accuracy figures.** A regressed build pooled with
//!   its predecessor disappears into an average.
//! - **Delivery is at least once.** One duplicated outcome moves the MAE,
//!   the median and the bias of a 21-observation window.
//! - **The `since` bound is closed below** — a row at exactly the bound counts.
//! - **Unpinned builds are excluded, not merely flagged.**
//! - **The `HAVING n >= 20` floor** keeps an undersampled feature out of the
//!   ranking, which is also what makes Q3 return *nothing* on a short window.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`) and
/// as `signoz_usage_queries.rs` / `signoz_cycle_time.rs` /
/// `signoz_queue_quota_queries.rs`, so no proof in this repo can drift from
/// the deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const QUERIES: &str = include_str!("../../defaults/observability/signoz/eta-queries.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_eta/fixture.sql");

/// The window the fixture is built around. Group K's outcome sits at exactly
/// this instant and group J's one second before it.
const SINCE: &str = "2026-09-01 00:00:00";

/// The estimating build of groups A, F, G, I, K and of `c-1`/`c-2`/`d-0`/`d-1`.
const REV_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
/// The regressed build (group B).
const REV_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
/// `stage-v2`'s build (group H).
const REV_C: &str = "c1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

/// One result row: column name -> JSON value, as `JSONEachRow` renders it.
type Row = BTreeMap<String, serde_json::Value>;

/// Runs `script` through `clickhouse local` in the pinned image and returns
/// stdout. Panics with the engine's own stderr on failure — a query that does
/// not parse must fail this test, not be silently skipped.
///
/// `output_format_json_quote_denormals=1` is set so a NaN correlation comes
/// back as a quoted denormal rather than JSON `null`: Q3's whole point is that
/// an unmeasured feature and a constant one must stay distinguishable, and
/// collapsing both to `null` in the transport would hide the difference this
/// test exists to observe. The denormal's *spelling* is architecture-dependent
/// — see [`is_nan`].
fn clickhouse(script: &str, format: &str, repo: &str) -> String {
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
            &format!("--param_since={SINCE}"),
            &format!("--param_repo={repo}"),
            "--output_format_json_quote_denormals=1",
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
/// the file's own prose uses semicolons and a naive split would cut a
/// statement in half mid-sentence. No `;` or `--` occurs inside a string
/// literal in this artifact, so what remains is each statement's exact
/// committed text. The strict verbatim proof is the whole-file run in
/// [`committed_eta_queries_answer_the_accuracy_questions_on_real_clickhouse`],
/// which executes the bytes as committed, comments included; this is only used
/// to attribute each query's output to its question and to build the
/// counterfactuals.
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

/// Parses a marker-delimited `JSONEachRow` stream into one `Vec<Row>` per
/// marker.
fn split_sections(output: &str, expected: usize) -> Vec<Vec<Row>> {
    let mut sections: Vec<Vec<Row>> = Vec::new();
    for line in output.lines() {
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
    assert_eq!(sections.len(), expected, "every statement must have produced a section");
    sections
}

/// Runs `queries` (one statement per entry) after the fixture, each preceded
/// by a marker row so the concatenated output can be attributed.
fn run(queries: &[String], repo: &str) -> Vec<Vec<Row>> {
    let mut script = String::from(FIXTURE);
    for (index, statement) in queries.iter().enumerate() {
        script.push_str(&format!("\nSELECT {index} AS loom_section_marker;\n"));
        script.push_str(statement);
        script.push_str(";\n");
    }
    split_sections(&clickhouse(&script, "JSONEachRow", repo), queries.len())
}

/// Every section of the committed file, in order.
fn sections(repo: &str) -> Vec<Vec<Row>> {
    let committed = statements(QUERIES);
    assert_eq!(committed.len(), 4, "eta-queries.sql is documented as section 0 plus Q1-Q3");
    run(&committed, repo)
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

fn real(row: &Row, key: &str) -> f64 {
    let value = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("column {key} is not a number in {row:?}"))
}

fn text<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"))
        .as_str()
        .unwrap_or_else(|| panic!("column {key} is not a string in {row:?}"))
}

/// Whether `key` is a NaN, **whichever way the engine spelled it**.
///
/// `output_format_json_quote_denormals=1` emits a quoted denormal, and its text
/// is architecture-dependent: ClickHouse 25.12.5 writes `nan` on arm64 macOS
/// and `-nan` on amd64 Linux (observed on this trial host and on a CI runner
/// respectively — the sign bit a libc `printf` happens to carry out of the
/// hardware's quiet NaN, not a different result). Asserting the literal string
/// would make this test pass on one of the trial's two verified architectures
/// and fail on the other, so NaN-ness is what is checked. Worth knowing beyond
/// this test: any consumer that string-matches this column — a dashboard, a CSV
/// export, a downstream parser — has to accept both spellings.
fn is_nan(row: &Row, key: &str) -> bool {
    let value = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    match value {
        serde_json::Value::String(text) => {
            matches!(
                text.trim_start_matches(['-', '+'])
                    .to_ascii_lowercase()
                    .as_str(),
                "nan"
            )
        }
        other => other.as_f64().is_some_and(f64::is_nan),
    }
}

/// The row whose `(heuristic, revision, kind)` match. Panics rather than
/// returning `None`: a missing group is always a failure here, never a
/// vacuously-passing assertion.
fn group<'a>(rows: &'a [Row], heuristic: &str, revision: &str, kind: &str) -> &'a Row {
    rows.iter()
        .find(|row| {
            text(row, "heuristic") == heuristic
                && text(row, "revision") == revision
                && text(row, "kind") == kind
        })
        .unwrap_or_else(|| panic!("no row for ({heuristic}, {revision}, {kind}) in {rows:?}"))
}

/// Q1 additionally splits by repository.
fn q1_row<'a>(rows: &'a [Row], heuristic: &str, revision: &str, kind: &str, repo: &str) -> &'a Row {
    rows.iter()
        .find(|row| {
            text(row, "heuristic") == heuristic
                && text(row, "revision") == revision
                && text(row, "kind") == kind
                && text(row, "repo") == repo
        })
        .unwrap_or_else(|| {
            panic!("no Q1 row for ({heuristic}, {revision}, {kind}, {repo}) in {rows:?}")
        })
}

/// Q2's rows are keyed by the three dimensions **plus** `rolled_up`, which is
/// the only thing separating the grand total from a real group of unlabelled
/// records.
fn q2_row<'a>(rows: &'a [Row], heuristic: &str, revision: &str, kind: &str, level: i64) -> &'a Row {
    rows.iter()
        .find(|row| {
            text(row, "heuristic") == heuristic
                && text(row, "revision") == revision
                && text(row, "kind") == kind
                && num(row, "rolled_up") == level
        })
        .unwrap_or_else(|| {
            panic!("no Q2 row for ({heuristic}, {revision}, {kind}) at level {level} in {rows:?}")
        })
}

fn feature<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter()
        .find(|row| text(row, "feature") == name)
        .unwrap_or_else(|| panic!("no feature row for {name} in {rows:?}"))
}

/// Asserts that `needle` occurs in the committed text before replacing it, so
/// a counterfactual cannot silently become a no-op after the artifact is
/// edited.
fn mutate(statement: &str, needle: &str, replacement: &str) -> String {
    assert!(
        statement.contains(needle),
        "the committed query no longer contains `{needle}`; re-point this \
         counterfactual rather than letting it pass vacuously:\n{statement}"
    );
    statement.replace(needle, replacement)
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_eta_queries_answer_the_accuracy_questions_on_real_clickhouse() {
    // ---- the file runs verbatim, as one pass, exactly as documented --------
    clickhouse(&format!("{FIXTURE}\n{QUERIES}"), "TSV", "");

    let sections = sections("");

    // ---- 0. preflight ------------------------------------------------------
    let preflight = &sections[0];
    assert_eq!(preflight.len(), 8, "eight (heuristic, revision, kind) groups: {preflight:?}");

    let land_a = group(preflight, "land-v1", REV_A, "land");
    assert_eq!(
        num(land_a, "estimates"),
        25,
        "21 group-A estimates, the duplicate of a-1, g-0, d-0's estimate and \
         the d-1 refusal — the refusal carries loom.eta.trigger too"
    );
    assert_eq!(
        num(land_a, "refusals"),
        1,
        "`refusals` is a SUBSET of `estimates`, not a disjoint population; \
         adding the two columns double-counts d-1"
    );
    assert_eq!(num(land_a, "outcomes"), 24);
    assert_eq!(
        num(land_a, "scored"),
        23,
        "one of the 24 outcomes is the abandonment, which carries no \
         loom.eta.error_sec key at all"
    );
    assert_eq!(num(land_a, "abandoned"), 1);
    assert_eq!(
        num(land_a, "outcomes") - num(land_a, "scored"),
        num(land_a, "abandoned"),
        "counted-but-not-scored must reconcile exactly to the abandonment"
    );
    assert_eq!(num(land_a, "incomplete_provenance"), 0);

    // The three incomplete-provenance shapes, all on kind `finish`.
    let finish_a = group(preflight, "land-v1", REV_A, "finish");
    assert_eq!(num(finish_a, "scored"), 2, "c-1 and c-2 are both scored rows");
    assert_eq!(
        num(finish_a, "incomplete_provenance"),
        2,
        "c-1's outcome flags its OBSERVING build unpinned; c-2's outcome omits \
         loom.eta.provenance_complete from attributes_bool entirely, and the \
         Map's default for a missing Bool key is false — so `!= true` counts it"
    );
    let finish_unknown = group(preflight, "land-v1", "unknown", "finish");
    assert_eq!(num(finish_unknown, "scored"), 1);
    assert_eq!(
        num(finish_unknown, "incomplete_provenance"),
        2,
        "a tarball build reports on both its estimate and its outcome row"
    );

    // Group K: the outcome is inside the window and its estimate is not.
    let boundary = group(preflight, "boundary-v0", REV_A, "land");
    assert_eq!(
        num(boundary, "estimates"),
        0,
        "k-0's estimate is an hour before the window; section 0 earns its \
         read-it-first billing by showing a scored outcome with no estimate"
    );
    assert_eq!(num(boundary, "outcomes"), 1);
    assert_eq!(num(boundary, "scored"), 1);

    let unlabelled = group(preflight, "", REV_A, "land");
    assert_eq!(
        num(unlabelled, "scored"),
        1,
        "i-0 carries no loom.eta.heuristic; ClickHouse answers '' for the \
         missing Map key rather than erroring, so the group is real and empty-named"
    );

    let rev_b = group(preflight, "land-v1", REV_B, "land");
    assert_eq!(
        num(rev_b, "scored"),
        5,
        "j-0 sits one second before the bound and must be absent — if it \
         leaked, this would read 6 and every revB figure would move"
    );

    // ---- Q1. MAE, coverage and bias ----------------------------------------
    let q1 = &sections[1];
    assert_eq!(q1.len(), 7, "seven scored populations: {q1:?}");

    let alpha = q1_row(q1, "land-v1", REV_A, "land", "org/alpha");
    assert_eq!(
        num(alpha, "scored"),
        21,
        "a-0's outcome was delivered twice; LIMIT 1 BY estimate_id must keep one"
    );
    assert_eq!(num(alpha, "mae_sec"), 1048, "22000 / 21 = 1047.62");
    assert_eq!(
        real(alpha, "coverage_25_75"),
        0.524,
        "11 of 21 actuals land inside [p25, p75] — 11 / 21 = 0.5238"
    );
    assert_eq!(
        num(alpha, "median_error_sec"),
        0,
        "the errors are symmetric about zero: -2000 … +2000 step 200"
    );
    assert_eq!(num(alpha, "mean_error_sec"), 0, "no bias in this population");
    assert_eq!(text(alpha, "horizon_bucket"), "15m_1h");

    let beta = q1_row(q1, "land-v1", REV_A, "land", "org/beta");
    assert_eq!(
        num(beta, "scored"),
        1,
        "the second repository is a separate row; `loom.repo` is on the record"
    );
    assert_eq!(num(beta, "mae_sec"), 300);

    let regressed = q1_row(q1, "land-v1", REV_B, "land", "org/alpha");
    assert_eq!(num(regressed, "scored"), 5);
    assert_eq!(num(regressed, "mae_sec"), 5000);
    assert_eq!(
        real(regressed, "coverage_25_75"),
        0.0,
        "a measured zero coverage: the build reported, and missed every time. \
         Not the same as a heuristic that produced no coverage figure at all"
    );
    assert_eq!(num(regressed, "median_error_sec"), 5000);

    let start = q1_row(q1, "land-v1", REV_A, "start", "org/alpha");
    assert_eq!(
        text(start, "horizon_bucket"),
        "lt_15m",
        "a p50 of 100s buckets differently from one of 2000s; pooling the two \
         would average a sub-minute prediction with an hour-long one"
    );
    assert_eq!(num(start, "mae_sec"), 20);

    let perfect = q1_row(q1, "stage-v2", REV_C, "land", "org/alpha");
    assert_eq!(
        num(perfect, "mae_sec"),
        0,
        "a MEASURED zero error must produce a row reading 0, not vanish — the \
         pair to the abandonment, which produces no row at all"
    );
    assert_eq!(num(perfect, "scored"), 1);

    let at_bound = q1_row(q1, "boundary-v0", REV_A, "land", "org/alpha");
    assert_eq!(
        num(at_bound, "scored"),
        1,
        "an outcome at exactly `since` is included: the bound is closed below"
    );
    assert_eq!(num(at_bound, "mae_sec"), 700);

    assert!(
        !q1.iter().any(|row| text(row, "revision") == "unknown"),
        "no unpinned build may appear in an accuracy figure: {q1:?}"
    );
    assert!(
        !q1.iter().any(|row| text(row, "kind") == "finish"),
        "every `finish` row in this fixture has incomplete provenance and must \
         be excluded wholesale: {q1:?}"
    );

    // ---- Q2. pinball loss, with the ROLLUP levels --------------------------
    let q2 = &sections[2];
    assert_eq!(q2.len(), 16, "five base groups plus eleven ROLLUP rows: {q2:?}");

    let base = q2_row(q2, "land-v1", REV_A, "land", 0);
    assert_eq!(num(base, "scored"), 22, "group A plus org/beta's g-0");
    assert_eq!(num(base, "mean_pinball_loss_sec"), 1280, "28150 / 22 = 1279.5");
    assert_eq!(num(base, "mae_sec"), 1014);
    assert_eq!(real(base, "coverage_25_75"), 0.545);

    let regressed = q2_row(q2, "land-v1", REV_B, "land", 0);
    assert_eq!(
        num(regressed, "mean_pinball_loss_sec"),
        7000,
        "the deciding metric separates the builds by 5.5x; this is the number \
         a promotion is read off"
    );

    let per_kind = q2_row(q2, "land-v1", "", "land", 1);
    assert_eq!(
        num(per_kind, "scored"),
        27,
        "the revision-rolled-up subtotal pools revA and revB on purpose"
    );
    assert_eq!(
        num(per_kind, "mean_pinball_loss_sec"),
        2339,
        "and the pooled loss sits between the two builds, naming neither — \
         which is exactly why `rolled_up` has to be visible"
    );

    // The collision this column exists for: two rows, identical ('', '', '').
    let unlabelled_total = q2_row(q2, "", "", "", 2);
    assert_eq!(
        num(unlabelled_total, "scored"),
        1,
        "the unlabelled heuristic's OWN total: two of its three dimensions \
         were rolled up, so rolled_up = 2"
    );
    let grand_total = q2_row(q2, "", "", "", 3);
    assert_eq!(
        num(grand_total, "scored"),
        31,
        "the grand total over every scored row in the window"
    );
    assert_eq!(num(grand_total, "mae_sec"), 1552);
    let collisions = q2
        .iter()
        .filter(|row| {
            text(row, "heuristic").is_empty()
                && text(row, "revision").is_empty()
                && text(row, "kind").is_empty()
        })
        .count();
    assert_eq!(
        collisions, 2,
        "ROLLUP really does emit two rows with the identical key ('', '', ''). \
         Without `rolled_up` a reader cannot tell the 1-observation unlabelled \
         group from the 31-observation grand total: {q2:?}"
    );

    // ---- Q3. feature ranking -----------------------------------------------
    let q3 = &sections[3];
    assert_eq!(
        q3.len(),
        3,
        "only group A clears HAVING n >= 20, and only its three numeric \
         features: {q3:?}"
    );
    for row in q3 {
        assert_eq!(text(row, "heuristic"), "land-v1");
        assert_eq!(text(row, "revision"), REV_A);
        assert_eq!(text(row, "kind"), "land");
    }

    let queue_depth = feature(q3, "queue_depth");
    assert_eq!(
        num(queue_depth, "n"),
        21,
        "a-1's estimate was delivered twice; `any(body) GROUP BY estimate_id` \
         must collapse it"
    );
    assert_eq!(num(queue_depth, "distinct_values"), 21);
    assert_eq!(real(queue_depth, "rank_corr"), 1.0);
    assert_eq!(real(queue_depth, "pearson_corr"), 1.0);

    let slack = feature(q3, "slack_sec");
    assert_eq!(
        real(slack, "rank_corr"),
        -1.0,
        "a perfectly ANTI-correlated feature is as informative as a correlated \
         one, which is why the file orders by abs(rank_corr)"
    );
    assert_eq!(real(slack, "pearson_corr"), -1.0);

    // The finding that motivated `distinct_values`.
    let constant = feature(q3, "open_prs");
    assert_eq!(num(constant, "n"), 21);
    assert_eq!(
        num(constant, "distinct_values"),
        1,
        "the feature took one value across the whole window"
    );
    assert_eq!(
        real(constant, "rank_corr"),
        0.5,
        "rankCorr average-ranks ties, so a CONSTANT feature scores exactly 0.5 \
         — not 0, and not nan. `ORDER BY abs(rank_corr) DESC` therefore ranks \
         a feature that never varied above any real correlation weaker than \
         0.5, which is why distinct_values must be read beside it"
    );
    assert!(
        is_nan(constant, "pearson_corr"),
        "corr() answers NaN for a zero-variance column. A dashboard that \
         renders NaN as a blank — or worse as 0 — says 'no correlation' about \
         the same column rankCorr put at 0.5: {constant:?}"
    );

    // Group K's features are absent, necessarily rather than accidentally.
    assert!(
        !q3.iter().any(|row| text(row, "heuristic") == "boundary-v0"),
        "k-0's estimate predates the window and the estimate sub-select \
         carries the same `since` bound, so Q1/Q2 score it and Q3 cannot see \
         its features. Documented in the artifact's header: {q3:?}"
    );
    for absent in ["unmeasured", "label", "numeric_string", "flaky", "partial"] {
        assert!(
            !q3.iter().any(|row| text(row, "feature") == absent),
            "`{absent}` must not appear: a null, a string, a number recorded \
             as a string, a boolean and an undersampled feature are each \
             dropped rather than ranked"
        );
    }
}

/// The `{repo}` parameter is documented as scoping every section. Proven by
/// re-running the whole committed file with it set and observing that the
/// other repository's population disappears from the figures rather than
/// being silently folded into them.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_repo_parameter_scopes_every_section() {
    let scoped = sections("org/alpha");
    let q1 = &scoped[1];
    assert_eq!(q1.len(), 6, "org/beta's row is gone: {q1:?}");
    assert!(!q1.iter().any(|row| text(row, "repo") == "org/beta"), "{q1:?}");
    let base = q2_row(&scoped[2], "land-v1", REV_A, "land", 0);
    assert_eq!(
        num(base, "scored"),
        21,
        "22 unscoped, 21 scoped to org/alpha — the parameter reaches Q2's \
         inner sub-select, not only Q1's"
    );
    assert_eq!(num(base, "mean_pinball_loss_sec"), 1310, "27500 / 21 = 1309.5");
    let grand = q2_row(&scoped[2], "", "", "", 3);
    assert_eq!(num(grand, "scored"), 30);
}

/// Absent is never zero, as a measured consequence rather than a claim. An
/// `abandoned` outcome has no `loom.eta.error_sec` key; `attributes_number`
/// answers the Float64 default for a missing key, so dropping the presence
/// filter scores the abandonment as a flawless prediction — and *improves*
/// the heuristic's MAE. A promotion metric that moves in the favourable
/// direction when data is missing is the worst shape a silent defect can take.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn an_abandoned_outcome_scores_as_perfect_without_the_presence_filter() {
    let committed = statements(QUERIES);
    let q1 = &committed[1];
    let naive = mutate(q1, "      AND mapContains(attributes_number, 'loom.eta.error_sec')\n", "");
    let out = run(&[q1.clone(), naive], "");

    let honest = q1_row(&out[0], "land-v1", REV_A, "land", "org/alpha");
    assert_eq!(num(honest, "scored"), 21);
    assert_eq!(num(honest, "mae_sec"), 1048);

    let fabricated = q1_row(&out[1], "land-v1", REV_A, "land", "org/alpha");
    assert_eq!(num(fabricated, "scored"), 22, "the abandonment joins the scored population");
    assert_eq!(
        num(fabricated, "mae_sec"),
        1000,
        "and lowers the MAE from 1048 to 1000: a sweep that never finished \
         reads as the most accurate prediction in the window"
    );
    assert_eq!(
        real(fabricated, "coverage_25_75"),
        0.5,
        "its absent `covered` flag reads false, so the same row also drags \
         coverage down — one missing record moves two metrics in opposite \
         directions"
    );
}

/// The provenance rule the artifact's header states: every accuracy view
/// groups by `(heuristic, revision)` so a daemon roll shows as two rows
/// instead of silently averaging two builds.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn pooling_two_revisions_hides_a_regressed_build() {
    let committed = statements(QUERIES);
    let q1 = &committed[1];
    let pooled = mutate(
        &mutate(
            &mutate(
                q1,
                "SELECT heuristic, revision, kind, repo, horizon_bucket,",
                "SELECT heuristic, kind, repo, horizon_bucket,",
            ),
            "GROUP BY heuristic, revision, kind, repo, horizon_bucket",
            "GROUP BY heuristic, kind, repo, horizon_bucket",
        ),
        "ORDER BY heuristic, revision, kind, repo, horizon_bucket",
        "ORDER BY heuristic, kind, repo, horizon_bucket",
    );
    let out = run(&[pooled], "");
    let rows = &out[0];
    let merged = rows
        .iter()
        .find(|row| {
            text(row, "heuristic") == "land-v1"
                && text(row, "kind") == "land"
                && text(row, "repo") == "org/alpha"
        })
        .expect("the pooled land-v1 population");
    assert_eq!(num(merged, "scored"), 26, "21 revA plus 5 revB");
    assert_eq!(
        num(merged, "mae_sec"),
        1808,
        "neither build's MAE (1048 and 5000) is recoverable from 1808, and \
         nothing in the row says two builds are in it"
    );
    assert_eq!(real(merged, "coverage_25_75"), 0.423);
    assert_eq!(
        num(merged, "mean_error_sec"),
        962,
        "the unbiased build acquires a 16-minute fast bias it does not have"
    );
}

/// Delivery is at least once, which is why every section de-duplicates on
/// `loom.eta.estimate_id`. One duplicated outcome out of 21 is enough to move
/// the MAE, the median and the bias.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn one_duplicated_outcome_skews_the_whole_window_without_the_dedupe() {
    let committed = statements(QUERIES);
    let q1 = &committed[1];
    let out = run(&[mutate(q1, "    LIMIT 1 BY estimate_id\n", "")], "");
    let skewed = q1_row(&out[0], "land-v1", REV_A, "land", "org/alpha");
    assert_eq!(num(skewed, "scored"), 22, "a-0 counted twice");
    assert_eq!(num(skewed, "mae_sec"), 1091, "1048 -> 1091");
    assert_eq!(
        num(skewed, "median_error_sec"),
        -100,
        "the median stops being zero, so the heuristic looks like it runs slow"
    );
    assert_eq!(num(skewed, "mean_error_sec"), -91);
}

/// The `since` bound is closed below (`>=`). A row at exactly the bound is
/// included, which matters because a reader paging through a long history in
/// contiguous windows would otherwise lose one record per boundary — or
/// double-count it, depending on which way the bound is wrong.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_since_bound_includes_a_row_at_exactly_the_bound() {
    let committed = statements(QUERIES);
    let q1 = &committed[1];
    let out = run(
        &[
            q1.clone(),
            mutate(q1, "timestamp >= toUInt64", "timestamp > toUInt64"),
        ],
        "",
    );
    assert!(
        out[0]
            .iter()
            .any(|row| text(row, "heuristic") == "boundary-v0"),
        "the committed query reports the outcome at exactly `since`: {:?}",
        out[0]
    );
    assert!(
        !out[1]
            .iter()
            .any(|row| text(row, "heuristic") == "boundary-v0"),
        "a strict bound drops it entirely — not a changed number, a vanished \
         population: {:?}",
        out[1]
    );
    assert_eq!(
        out[0].len() - 1,
        out[1].len(),
        "and nothing else moves: {:?} vs {:?}",
        out[0],
        out[1]
    );
}

/// Unpinned builds are excluded from the figures, not merely flagged. A
/// tarball build's result cannot be attributed to a heuristic's code, so
/// including it would credit or blame the wrong revision.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn unpinned_builds_enter_the_figures_without_the_provenance_filters() {
    let committed = statements(QUERIES);
    let q1 = &committed[1];
    let lax = mutate(
        &mutate(q1, "      AND attributes_bool['loom.eta.provenance_complete'] = true\n", ""),
        "      AND attributes_bool['loom.eta.outcome_provenance_complete'] = true\n",
        "",
    );
    let out = run(&[lax], "");
    let rows = &out[0];
    assert_eq!(
        rows.len(),
        9,
        "the committed query's seven populations plus two `finish` ones: {rows:?}"
    );
    let unknown = q1_row(rows, "land-v1", "unknown", "finish", "org/alpha");
    assert_eq!(
        num(unknown, "mae_sec"),
        4242,
        "a revision literally named `unknown` acquires an accuracy figure"
    );
    let partially_pinned = q1_row(rows, "land-v1", REV_A, "finish", "org/alpha");
    assert_eq!(
        num(partially_pinned, "scored"),
        2,
        "c-1 (unpinned OBSERVING build) and c-2 (the flag missing from the map \
         entirely) are both admitted, and both are attributed to revA — which \
         did not necessarily produce them"
    );
    assert_eq!(num(partially_pinned, "mae_sec"), 4394);
}

/// The typed feature extraction's real damage, which is not the one the
/// artifact's header originally described. `JSONExtractKeysAndValues(…,
/// 'Float64')` drops a `null` and a non-numeric string rather than zeroing
/// them — but it *coerces* a number recorded as a string and a boolean, which
/// puts two features that are not numbers into the ranking as constants, each
/// at the same spurious |0.5| as a genuinely constant one.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_typed_feature_extraction_coerces_non_numbers() {
    let committed = statements(QUERIES);
    let q3 = &committed[3];
    let typed = mutate(
        &mutate(
            &mutate(
                q3,
                "kv.1 AS feature, toFloat64OrNull(kv.2) AS value",
                "kv.1 AS feature, kv.2 AS value",
            ),
            "JSONExtractKeysAndValuesRaw(e.body, 'features')",
            "JSONExtractKeysAndValues(e.body, 'features', 'Float64')",
        ),
        "WHERE value IS NOT NULL",
        "",
    );
    let out = run(&[q3.clone(), typed], "");

    let committed_features: Vec<&str> = out[0].iter().map(|row| text(row, "feature")).collect();
    assert_eq!(committed_features.len(), 3, "{committed_features:?}");

    let typed_rows = &out[1];
    let typed_features: Vec<&str> = typed_rows.iter().map(|row| text(row, "feature")).collect();
    assert!(
        typed_features.contains(&"numeric_string"),
        "`\"42\"` is coerced to 42 by the typed extraction: {typed_features:?}"
    );
    assert!(typed_features.contains(&"flaky"), "`true` is coerced to 1: {typed_features:?}");
    assert!(
        !typed_features.contains(&"unmeasured") && !typed_features.contains(&"label"),
        "and a null / a non-numeric string are DROPPED, not read as 0 — the \
         header's original reasoning was wrong about which way this fails: \
         {typed_features:?}"
    );
    let coerced = feature(typed_rows, "numeric_string");
    assert_eq!(num(coerced, "n"), 21);
    assert_eq!(
        real(coerced, "rank_corr"),
        0.5,
        "and it enters at the same spurious 0.5 a constant feature scores, \
         directly above any real correlation weaker than that"
    );
}

/// `HAVING n >= 20` is a floor, not decoration: without it a 12-observation
/// feature with a perfect correlation tops the ranking, and single-observation
/// groups appear with `nan`. With it, a short window returns *nothing* — which
/// is the hazard worth knowing, since an empty Q3 reads like "no feature
/// tracks the error" rather than "not enough data".
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_sample_floor_keeps_undersampled_features_out_of_the_ranking() {
    let committed = statements(QUERIES);
    let q3 = &committed[3];
    let out = run(&[mutate(q3, "HAVING n >= 20", "")], "");
    let rows = &out[0];
    let partial = feature(rows, "partial");
    assert_eq!(
        num(partial, "n"),
        12,
        "`partial` is a number on 12 of group A's 21 estimates and null on the \
         rest; the nulls are dropped rather than zeroed, which is what makes \
         its n honest"
    );
    assert_eq!(
        real(partial, "rank_corr"),
        1.0,
        "a perfect correlation on 12 observations — exactly the row the floor \
         exists to keep out of a promotion decision"
    );
    assert!(
        rows.iter().any(|row| text(row, "revision") == REV_B),
        "the regressed build's own five-observation features also surface: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| num(row, "n") == 1 && is_nan(row, "rank_corr")),
        "and a one-observation group correlates as NaN — which the floor also \
         keeps out: {rows:?}"
    );
}
