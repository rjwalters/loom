//! Static contract for the sweep-facts / issue-effort artifacts (Issues
//! #9446, #9466, #9433). No Docker, network, backend or credential is used, so
//! this runs in ordinary CI on any host; the live D1 backfill run that feeds
//! the rollup is operator-side (2AMLogic/2am#1608).
//!
//! Why this exists. The bundle's whole claim is that "what did it cost to land
//! issue X" means the same thing in every store that answers it, because the
//! facts are extracted once per backend (`sweep-facts-extract-*.sql`, the D1
//! `INSERT … SELECT` in `sweep-facts-rollup.sql`) into one shared column
//! contract, and the SF question set is implemented once beside the
//! definitions doc. That claim is one careless edit away from being false in a
//! way nothing reports: a column renamed on one side only (so values silently
//! shift into neighbouring columns), a question documented in prose that no
//! query implements, a window edited into one query but not the others, or a
//! landed-size parameter set that nothing versions.
//!
//! Follows the shape of [`cycle_time_artifacts`] (#8665): the authorities are
//! derived from the committed artifacts themselves and compared against one
//! pinned set stated here, so two files cannot drift *together* into agreeing
//! on the wrong thing. Static only — the D1-side SQL is SQLite, not
//! executable from a Rust test without a database, so this checks the
//! contracts a database would otherwise silently absorb.
#![allow(clippy::unwrap_used)]

use regex::Regex;

const QUESTIONS: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-questions.md");
const ROLLUP: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-rollup.sql");
const QUERIES: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-queries.sql");
const ISSUE_EFFORT: &str =
    include_str!("../../defaults/observability/sweep-facts/issue-effort.sql");
const LANDED_SIZE: &str = include_str!("../../defaults/observability/sweep-facts/landed-size.sql");
const CLICKSTACK_EXTRACT: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-extract-clickstack.sql");
const SIGNOZ_EXTRACT: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-extract-signoz.sql");
const IE_QUERIES: &str = include_str!("../../defaults/observability/issue-effort-queries.sql");

/// The canonical question IDs. Restated here deliberately: this is the one
/// place the *set* is pinned, and every artifact below is checked against it
/// rather than against another artifact, so two files cannot drift together.
const QUESTION_IDS: &[&str] = &["SF1", "SF2", "SF3", "SF4", "SF5", "SF6", "SF7", "SF8"];

/// The measured point value per size class (#9466) — the experiment's bucket
/// ratios. Pinned here so the tests below can assert they are written down in
/// exactly ONE artifact: `landed-size.sql`'s `measured_point_values` view. Two
/// copies would be two meanings of "a point", and the copy a reader did not
/// open would be the wrong one (#9433). `1.0` is deliberately absent from this
/// list: class 1's value collides with ordinary `1.0` coefficients elsewhere.
const MEASURED_POINT_RATIOS: &[&str] = &[
    "1.3", "2.2", "3.4", "5.1", "8.2", "8.5", "21.0", "47.0", "82.0", "197.0",
];

/// The only `sweep_facts` column with no place in the ClickHouse extraction
/// views: `schema_version` is a field of the D1 envelope (migrations/
/// 0001_init.sql), and an OTel log record carries no envelope. Every other
/// fact column must be producible by BOTH extraction paths.
const ENVELOPE_ONLY_COLUMNS: &[&str] = &["schema_version"];

/// Output aliases of an extraction view: every `… AS name` at the end of a line
/// between the top-level `SELECT` and its `FROM`. Order is preserved because a
/// silent transposition is exactly what an unordered comparison would miss.
fn extract_output_columns(sql: &str) -> Vec<String> {
    let select = sql
        .find("\nSELECT\n")
        .expect("extraction view has no top-level SELECT");
    let from = sql[select..]
        .find("\nFROM ")
        .expect("extraction view has no FROM")
        + select;
    Regex::new(r"(?m)\bAS\s+([a-z_][a-z0-9_]*)\s*,?\s*$")
        .unwrap()
        .captures_iter(&sql[select..from])
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// The rollup table's own column names, from its `CREATE TABLE` body. The
/// lowercase-only match skips the uppercase `PRIMARY KEY (…)` line.
fn fact_table_columns() -> Vec<String> {
    let start = ROLLUP
        .find("CREATE TABLE IF NOT EXISTS sweep_facts")
        .expect("rollup DDL no longer creates sweep_facts");
    let body = &ROLLUP[start..];
    let open = body.find("(\n").unwrap();
    let close = body.find("\n)").unwrap();
    Regex::new(r"(?m)^\s{4}([a-z_][a-z0-9_]*)\s")
        .unwrap()
        .captures_iter(&body[open..close])
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// The column list of the rollup's explicit `INSERT OR REPLACE INTO … (…)`.
fn rollup_insert_columns() -> Vec<String> {
    let start = ROLLUP
        .find("INSERT OR REPLACE INTO sweep_facts")
        .expect("rollup no longer has an explicit INSERT");
    let body = &ROLLUP[start..];
    let list = &body[body.find('(').unwrap() + 1..body.find(')').unwrap()];
    list.split(',').map(|name| name.trim().to_owned()).collect()
}

/// Output aliases of the `issue_effort` view's top-level SELECT. Every CTE
/// `SELECT` in `issue-effort.sql` is indented, so the at-column-0 marker is
/// the view's own SELECT, and the select list is pure `x AS name` lines, so
/// the first `FROM` after it starts the joins.
fn issue_effort_columns() -> Vec<String> {
    let select = ISSUE_EFFORT
        .find("\nSELECT\n")
        .expect("issue-effort.sql has no top-level SELECT");
    let from = ISSUE_EFFORT[select..]
        .find("\nFROM ")
        .expect("issue_effort view has no FROM")
        + select;
    Regex::new(r"(?m)\bAS\s+([a-z_][a-z0-9_]*)\s*,?\s*$")
        .unwrap()
        .captures_iter(&ISSUE_EFFORT[select..from])
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// Only the EXECUTABLE lines of a SQL artifact — every `--` comment line
/// dropped. These files document their own pitfalls in prose ("do not use
/// `%V`", "the ratios are 1 : 1.3 : …"), so a check for a forbidden token has to
/// distinguish naming it from using it, or the documentation trips the test that
/// documents it.
fn non_comment_lines(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The landing predicate as written in `sql` after the `anchor` marker,
/// normalized: table aliases (`f.`, `l.`, …) stripped and whitespace collapsed,
/// so the same predicate spelled with a different alias or wrapped at a
/// different column still compares equal. The anchor exists because these files
/// also *describe* the predicate in prose comments, and a comment must not be
/// mistaken for the executable copy.
fn normalized_landing_predicate(sql: &str, anchor: &str) -> String {
    const OPEN: &str = "disposition = 'landed'";
    const CLOSE: &str = "pr_number IS NOT NULL";
    let at = sql
        .find(anchor)
        .unwrap_or_else(|| panic!("no '{anchor}' anchor to read a landing predicate after"));
    let start = sql[at..]
        .find(OPEN)
        .unwrap_or_else(|| panic!("no landing predicate after '{anchor}'"))
        + at;
    let end = sql[start..]
        .find(CLOSE)
        .unwrap_or_else(|| panic!("landing predicate after '{anchor}' has no pre-#9441 fallback"))
        + start
        + CLOSE.len();
    let de_aliased = Regex::new(r"\b[a-z]\.")
        .unwrap()
        .replace_all(&sql[start..end], "");
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(&de_aliased, " ")
        .into_owned()
}

/// One SF question's query body, cut at the next `-- SFn. ` marker.
fn question_body(id: &str) -> &str {
    let start = QUERIES
        .find(&format!("-- {id}. "))
        .unwrap_or_else(|| panic!("sweep-facts-queries.sql implements no {id}"));
    let end = QUESTION_IDS
        .iter()
        .filter_map(|next| QUERIES[start + 1..].find(&format!("-- {next}. ")))
        .min()
        .map(|offset| start + 1 + offset)
        .unwrap_or(QUERIES.len());
    &QUERIES[start..end]
}

#[test]
fn both_backends_extract_the_same_normalized_sweep_fact_columns() {
    let clickstack = extract_output_columns(CLICKSTACK_EXTRACT);
    let signoz = extract_output_columns(SIGNOZ_EXTRACT);
    assert!(
        !clickstack.is_empty(),
        "parsed no output columns from the ClickStack extraction view"
    );
    assert_eq!(
        clickstack, signoz,
        "the ClickStack and SigNoz extraction views no longer expose the same sweep-fact \
         columns in the same order, so the two backends' facts are no longer the same fact \
         and cross-backend parity is broken (#8529, #9446)"
    );
}

#[test]
fn the_rollup_ingests_exactly_the_columns_the_table_declares() {
    assert_eq!(
        rollup_insert_columns(),
        fact_table_columns(),
        "the rollup's INSERT column list and the sweep_facts DDL disagree; values would be \
         written into neighbouring columns"
    );
}

#[test]
fn both_extractions_expose_every_fact_column_the_d1_store_declares() {
    let mut declared = fact_table_columns();
    for envelope_only in ENVELOPE_ONLY_COLUMNS {
        assert!(
            declared.contains(&envelope_only.to_string()),
            "sweep_facts no longer declares '{envelope_only}', which this test treats as the \
             one envelope-only column"
        );
        declared.retain(|column| column != envelope_only);
    }
    declared.sort();
    let mut extracted = extract_output_columns(CLICKSTACK_EXTRACT);
    extracted.sort();
    assert_eq!(
        declared, extracted,
        "sweep_facts's columns and the extraction views' output disagree (beyond the \
         envelope-only columns); the ClickHouse facts and the D1 facts are no longer the \
         same fact shape (#9446)"
    );
}

#[test]
fn the_documented_question_set_is_the_implemented_one() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "sweep-facts-questions.md documents no {id}"
        );
        let implementations = QUERIES.matches(&format!("-- {id}. ")).count();
        assert_eq!(
            implementations, 1,
            "sweep-facts-queries.sql implements {id} {implementations} times; expected exactly once"
        );
    }
    let stray = Regex::new(r"(?m)^-- (SF\d+)\. ").unwrap();
    for capture in stray.captures_iter(QUERIES) {
        assert!(
            QUESTION_IDS.contains(&&capture[1]),
            "sweep-facts-queries.sql implements {}, which is not in the canonical question set",
            &capture[1]
        );
    }
}

#[test]
fn every_question_reads_the_one_bound_window() {
    // The D1 adaptation of the cycle-time binding rule: a committed D1 file
    // has no client-side bind parameters, so the window lives in the
    // `sf_window` view — the one place a date literal may appear — and every
    // question reads it instead of carrying its own literal.
    assert_eq!(
        QUERIES
            .matches("CREATE VIEW IF NOT EXISTS sf_window")
            .count(),
        1,
        "sweep-facts-queries.sql must define exactly one sf_window view; per-query windows \
         are the hand-edited-literal habit these artifacts replace"
    );
    for id in QUESTION_IDS {
        assert!(
            question_body(id).contains("sf_window"),
            "{id} does not read the bound sf_window view; editing a date literal into the \
             SQL is the hand-written-SQL habit these artifacts replace"
        );
    }
    // Below the sf_window definition no non-comment line carries a date
    // literal: a window pasted into one query while sf_window says another is
    // the drift this file exists to make loud.
    let after_window = &QUERIES[QUERIES.find("-- SF1. ").unwrap()..];
    let date_literal = Regex::new(r"(?m)^[^-].*'20\d\d-\d\d-\d\d").unwrap();
    assert!(
        date_literal.find(after_window).is_none(),
        "a date literal was written into a sweep-facts question outside the sf_window view \
         or a comment"
    );
}

#[test]
fn the_effort_view_exposes_the_documented_effort_columns() {
    let expected: &[&str] = &[
        "repo",
        "issue",
        "landing_sweep_id",
        "landed_at",
        "landing_pr_number",
        "attempts",
        "lifecycle_wall_sec",
        "lifecycle_tokens_in",
        "lifecycle_tokens_out",
        "token_completeness",
        "clean_wall_sec",
        "clean_tokens_in",
        "clean_tokens_out",
        "rework_substantive",
        "rework_environmental",
        "rework_substantive_open",
        "rework_environmental_open",
        "clean_sec",
        "substantive_rework_sec",
        "environmental_rework_sec",
        "unattributed_sec",
        "overaccounted_sec",
        "attributed_attempts",
        "attributed_wall_pct",
    ];
    assert_eq!(
        issue_effort_columns(),
        expected,
        "the issue_effort view's output columns no longer match the documented effort \
         contract (lifecycle beside clean, with the completeness flag and the rework \
         split) (#9446)"
    );
}

/// The `(trigger, bucket)` pairs of the first `CASE … END` block that starts
/// at `anchor` in `sql`, in written order.
fn trigger_buckets(sql: &str, anchor: &str) -> Vec<(String, String)> {
    let start = sql
        .find(anchor)
        .unwrap_or_else(|| panic!("no `{anchor}` trigger CASE to read"));
    let end = start
        + sql[start..]
            .find("END")
            .unwrap_or_else(|| panic!("the `{anchor}` CASE never terminates"));
    let pairs: Vec<(String, String)> = Regex::new(r"WHEN '([a-z_]+)'\s+THEN '([a-z]+)'")
        .unwrap()
        .captures_iter(&sql[start..end])
        .map(|capture| (capture[1].to_owned(), capture[2].to_owned()))
        .collect();
    let else_arm = Regex::new(r"ELSE '([a-z]+)'")
        .unwrap()
        .captures(&sql[start..end])
        .map(|capture| capture[1].to_owned());
    assert_eq!(
        else_arm.as_deref(),
        Some("unattributed"),
        "the `{anchor}` trigger CASE must send everything it does not name — \
         operator_redispatch, unknown, a triggerless pre-#9444 record — to 'unattributed'"
    );
    pairs
}

#[test]
fn the_bundle_buckets_triggers_exactly_as_ie1_does() {
    // Drift guard (#9507): the bundle's seconds partition and IE1 (raw
    // `records`) must classify every trigger identically — two tables that
    // disagreed would make the dashboards' split and the raw-store split two
    // different answers to one question. IE1's own table is checked against
    // the daemon's trigger vocabulary by `issue_effort_artifacts.rs`, so
    // equality here carries that coverage over to the bundle.
    let mut bundle = trigger_buckets(ISSUE_EFFORT, "CASE f.trigger");
    let mut ie1 = trigger_buckets(IE_QUERIES, "CASE json_extract(r.payload, '$.trigger')");
    assert!(ie1.len() >= 8, "parsed too little of IE1's trigger table: {ie1:?}");
    bundle.sort();
    ie1.sort();
    assert_eq!(
        bundle, ie1,
        "issue-effort.sql's trigger → bucket table and IE1's (issue-effort-queries.sql) \
         disagree; they are twins and must be edited together (#9507)"
    );
    for unbucketed in ["operator_redispatch", "unknown"] {
        assert!(
            !bundle.iter().any(|(trigger, _)| trigger == unbucketed),
            "`{unbucketed}` must fall to 'unattributed', never into a named bucket (#9507)"
        );
    }
}

#[test]
fn the_rework_seconds_are_fact_columns_on_every_side_of_the_seam() {
    // The generic parity tests above compare the DDL, the INSERT list and both
    // extraction views against each other; this names the #9507 columns so
    // their removal from all of them at once is still loud.
    let declared = fact_table_columns();
    let clickstack = extract_output_columns(CLICKSTACK_EXTRACT);
    let signoz = extract_output_columns(SIGNOZ_EXTRACT);
    for column in [
        "rework_substantive_sec",
        "rework_environmental_sec",
        "rework_substantive_open",
        "rework_environmental_open",
    ] {
        for (side, columns) in [
            ("sweep_facts DDL", &declared),
            ("ClickStack extraction", &clickstack),
            ("SigNoz extraction", &signoz),
        ] {
            assert!(
                columns.contains(&column.to_owned()),
                "{side} no longer exposes `{column}`; the bundle's seconds partition reads it \
                 (#9507)"
            );
        }
    }
    // An open event (no `duration_sec`) must stay NULL in ClickHouse, not be
    // defaulted to 0 by JSONExtract — that is what lets `_open` count it.
    for (backend, sql) in [
        ("ClickStack", CLICKSTACK_EXTRACT),
        ("SigNoz", SIGNOZ_EXTRACT),
    ] {
        assert!(
            sql.contains("duration_sec Nullable(Int64)"),
            "the {backend} extraction reads rework `duration_sec` as a non-Nullable integer, \
             so an open event would read as a measured 0 (#9507)"
        );
    }
}

#[test]
fn landed_size_parameters_are_named_and_versioned() {
    assert!(
        LANDED_SIZE.contains("params_version"),
        "landed-size.sql names no versioned parameter set; the fit that produced it \
         (#9934) would have nothing to bump and no way to announce itself"
    );
    // The fit shipped in v1-2026-10-02 (#9934). A regression to the unfitted
    // state must fail here rather than pass as a fitted parameter set: if the
    // version goes back to NULL constants, this test names it. (Scoped to the
    // params CTE: the file's prose may honestly mention the v0 history.)
    let params_for_version = LANDED_SIZE
        .split("params AS (")
        .nth(1)
        .unwrap_or_else(|| panic!("landed-size.sql has no params CTE"));
    let params_for_version = &params_for_version[..params_for_version
        .find("),")
        .unwrap_or(params_for_version.len())];
    assert!(
        params_for_version.contains("v1-2026-10-02"),
        "landed-size.sql's params CTE no longer names its fitted parameter version; a \
         refit (#9934's procedure) must bump params_version so every consumer can see \
         which standardization produced its numbers"
    );
    assert!(
        !params_for_version.contains("v0-unfitted"),
        "landed-size.sql's params CTE still marks the parameter set unfitted; an \
         unfitted standardization must not pass as fitted"
    );
    // Structurally: every fitted constant must be a literal, not a NULL —
    // the v0 state carried `NULL AS mean_log_*` lines, and any regression to
    // that shape (or a partial paste of a new fit) fails here.
    let params_block = LANDED_SIZE
        .split("params AS (")
        .nth(1)
        .unwrap_or_else(|| panic!("landed-size.sql has no params CTE"));
    let params_block = &params_block[..params_block.find("),").unwrap_or(params_block.len())];
    for name in [
        "mean_log_hw_lines",
        "sd_log_hw_lines",
        "mean_log_hw_files",
        "sd_log_hw_files",
        "mean_log_norm_tokens",
        "sd_log_norm_tokens",
    ] {
        let line = params_block
            .lines()
            .find(|line| line.contains(&format!("AS {name}")))
            .unwrap_or_else(|| panic!("landed-size.sql's params CTE lost the {name} constant"));
        assert!(
            !line.contains("NULL"),
            "landed-size.sql's {name} is NULL: the parameter set is unfitted, and an \
             unfitted standardization must not pass as fitted (#9934)"
        );
    }
    assert!(
        LANDED_SIZE.contains("LSI"),
        "landed-size.sql never names LSI; the KPI whose sums must never be raw Fibonacci \
         labels (#9466) has no home in its own definition file"
    );
    assert!(
        LANDED_SIZE.contains("exp("),
        "LSI is defined as exp(landed_size); the definition file must compute it, not \
         assume a consumer will"
    );
    // The token verdict gate (#9934): the tokens component must be scored only
    // on a clean measurement — `suspect`/`unattributable`/`not_spawned`
    // (#9440/#9454) are published numbers, not measurements.
    assert!(
        LANDED_SIZE.contains("tokens_status = 'measured'"),
        "landed-size.sql's tokens component no longer checks the sweep's token \
         verdict; token readings the sweep flagged as suspect or unattributable \
         would be standardized as if they were measurements (#9440/#9454, #9934)"
    );
}

#[test]
fn the_reconciliation_query_reads_both_sides_of_the_seam() {
    let body = question_body("SF7");
    assert!(
        body.contains("sweep_facts"),
        "SF7 no longer reads the rollup; without it the reconciliation cannot count what \
         the facts claim (#9446)"
    );
    assert!(
        body.contains("records"),
        "SF7 no longer reads the raw records table; without it the reconciliation cannot \
         detect drift between the rollup and its source (#9446's unexplained-drops \
         criterion, the SF analogue of CT8)"
    );
}

#[test]
fn the_rollup_stays_idempotent_on_its_declared_key() {
    // SF7 reconciles the rollup against the raw records; this asserts the
    // mechanism that makes the reconciliation pass a re-run at all. Adding a
    // column to `sweep_facts` (#9433 added `story_points`) must not quietly
    // become adding a second row per sweep — that is why this is checked here
    // rather than left implied by the DDL.
    assert_eq!(
        ROLLUP.matches("INSERT OR REPLACE INTO sweep_facts").count(),
        1,
        "the rollup must have exactly one `INSERT OR REPLACE INTO sweep_facts` write path; \
         a plain INSERT, or a second write path, duplicates rows on the re-run the ingest \
         pipeline's at-least-once delivery guarantees will eventually cause (#9446)"
    );
    assert!(
        ROLLUP.contains("PRIMARY KEY (repo, issue, sweep_id)"),
        "sweep_facts no longer declares PRIMARY KEY (repo, issue, sweep_id); without that \
         key `INSERT OR REPLACE` has nothing to replace ON, so re-running the rollup over \
         an already-ingested window appends instead of overwriting (#9446's idempotence \
         criterion)"
    );
}

#[test]
fn the_forecast_column_is_a_fact_column_on_every_side_of_the_seam() {
    // `story_points` (#9432) is the one FORECAST column on the fact table; #9433
    // consumes it. The generic parity tests above already compare the DDL, the
    // INSERT list and both extraction views against each other — this names the
    // column so its removal from all of them at once is still loud.
    assert!(
        fact_table_columns().contains(&"story_points".to_owned()),
        "sweep_facts no longer declares story_points; SF8's whole question (points landed \
         per day, #9433) reads that column"
    );
    assert!(
        ROLLUP.contains("json_extract(r.payload, '$.story_points')"),
        "the rollup no longer reads story_points out of the sweep.outcome payload, so the \
         column it declares would stay NULL forever (#9432/#9433)"
    );
    for (backend, sql) in [
        ("ClickStack", CLICKSTACK_EXTRACT),
        ("SigNoz", SIGNOZ_EXTRACT),
    ] {
        assert!(
            sql.contains("loom.story_points"),
            "the {backend} extraction view no longer reads the `loom.story_points` OTLP \
             attribute (telemetry-schema.md §story_points), so that backend's facts and \
             D1's are no longer the same fact"
        );
    }
}

#[test]
fn the_throughput_question_counts_only_landings() {
    // A failed, cancelled or no-op sweep lands nothing, so it may contribute to
    // neither the points sum nor the landings count (#9433's acceptance
    // criterion). The predicate is not merely present: it is the SAME predicate
    // `landed-size.sql` uses, so SF2 and SF8 cannot come to disagree about what
    // "landed" means while both claim to measure throughput.
    let sf8 = normalized_landing_predicate(question_body("SF8"), "sf8_landed AS (");
    let landed_size = normalized_landing_predicate(LANDED_SIZE, "landings AS (");
    assert_eq!(
        sf8, landed_size,
        "SF8's landing predicate and landed-size.sql's `landings` CTE disagree; the \
         forecast side (SF8) and the measured side (SF2) would then be counting different \
         populations while being read as one pair (#9433)"
    );
    assert!(
        landed_size.contains("disposition IS NULL"),
        "the shared landing predicate lost its documented pre-#9441 fallback; landings \
         recorded before `disposition` existed would silently drop out of both SF2 and SF8"
    );
}

#[test]
fn the_throughput_question_reports_the_unsized_population_as_a_gap() {
    // CT7/SF4 discipline: missing is never zero. An unsized landing must be
    // counted and reported, never folded into the points sum as a 0 (#9433).
    let body = question_body("SF8");
    assert!(
        body.contains("points_missing"),
        "SF8 reports no `points_missing` column; landings the Curator never sized would be \
         invisible, and a day that simply was not sized would read as a day that landed \
         nothing (#9433's data-gap criterion)"
    );
    assert!(
        body.contains("story_points IS NULL"),
        "SF8's data-gap count is not derived from an IS NULL test on story_points, so it is \
         not counting the absent-vs-zero population the telemetry schema defines"
    );
    assert!(
        body.contains("points_sized"),
        "SF8 reports no `points_sized` column; without it a reader cannot check \
         points_sized + points_missing = landings, which is the coverage check the data-gap \
         count exists to enable"
    );
    assert!(
        body.contains("sized_without_measured_value"),
        "SF8 no longer counts landings whose assigned class has no measured point value; an \
         out-of-vocabulary story_points would be silently dropped by the LEFT JOIN instead \
         of reported as the emitter defect it is"
    );
}

#[test]
fn raw_fibonacci_labels_are_never_summed_as_a_size() {
    // The experiment's pre-registered ±30% additivity test fails on every
    // measured axis: a "13" is not thirteen "1"s (#9429/#9466). A `sum()` over
    // the bare `story_points` column is therefore not a size, and the ONLY
    // place one may appear is under a column name that says so. This is the
    // mistake that would pass review unnoticed, so it is checked mechanically.
    let bare_sum = Regex::new(r"(?m)^.*\bsum\(\s*(?:[a-z0-9_]+\.)?story_points\s*\).*$").unwrap();
    let mut found = 0;
    for occurrence in bare_sum.find_iter(QUERIES) {
        found += 1;
        assert!(
            occurrence
                .as_str()
                .contains("labels_summed_ordinal_do_not_use_as_size"),
            "a sweep-facts question sums the raw story_points column under the name of a \
             size:\n  {}\nFibonacci labels are ORDINAL — sum the measured point value per \
             bucket (`measured_point_values`) instead, and if the raw label sum is reported \
             at all, name it `labels_summed_ordinal_do_not_use_as_size` (#9433)",
            occurrence.as_str().trim()
        );
    }
    // At least one, not exactly one: SF8 emits two grains as a compound SELECT,
    // so its select list — including the deliberately-named ordinal column —
    // legitimately appears once per arm. The per-occurrence assertion above is
    // the actual guard; this only proves the guard had something to inspect.
    assert!(
        found > 0,
        "no raw story_points sum found anywhere in the question set, so the assertion above \
         inspected nothing — has SF8's `labels_summed_ordinal_do_not_use_as_size` column, or \
         the column name this test keys on, been renamed? (#9433)"
    );
    let body = question_body("SF8");
    assert!(
        body.contains("measured_point_values"),
        "SF8 does not join `measured_point_values`, so \"points landed\" is not the measured \
         point value per bucket it is required to be (#9433)"
    );
}

#[test]
fn the_measured_point_ratios_are_written_down_in_exactly_one_place() {
    for ratio in MEASURED_POINT_RATIOS {
        assert!(
            LANDED_SIZE.contains(ratio),
            "landed-size.sql no longer carries the measured bucket ratio {ratio}; the \
             `measured_point_values` view is the single definition of a point value (#9466)"
        );
    }
    // Comments may quote the ratios (SF2's and SF8's prose both do); executable
    // SQL may not restate them — that copy is the one that silently goes stale.
    for line in non_comment_lines(QUERIES).lines() {
        for ratio in MEASURED_POINT_RATIOS {
            assert!(
                !line.contains(ratio),
                "a sweep-facts question restates the measured bucket ratio {ratio} in \
                 executable SQL:\n  {}\nRead it from landed-size.sql's \
                 `measured_point_values` view instead — a second copy is a second meaning \
                 of \"a point\" (#9433)",
                line.trim()
            );
        }
    }
    assert!(
        LANDED_SIZE.contains("CREATE VIEW IF NOT EXISTS measured_point_values"),
        "landed-size.sql no longer exposes the measured point values as a view, so SF8 has \
         nothing to join and would have to restate the ratios (#9433)"
    );
}

#[test]
fn the_throughput_question_reports_both_grains_portably() {
    let body = question_body("SF8");
    for grain in ["'day'", "'iso_week'"] {
        assert!(
            body.contains(grain),
            "SF8 does not emit the {grain} grain; #9433 asks for points landed per day AND \
             per ISO week, from one definition so the two readings cannot drift"
        );
    }
    // %V / %G need SQLite >= 3.46. The ISO week is computed off the Thursday of
    // the landing's week instead, which is exact on every SQLite that has
    // `weekday`. Using the modifiers would make the file silently return wrong
    // or no rows on an older engine. Checked against EXECUTABLE lines only —
    // SF8's own comment names the modifiers in order to rule them out.
    let executable = non_comment_lines(body);
    for modifier in ["%V", "%G"] {
        assert!(
            !executable.contains(modifier),
            "SF8 uses the {modifier} strftime modifier, which requires SQLite >= 3.46; \
             derive the ISO week from the Thursday of the landing's week instead so the \
             committed file runs on any engine D1 or a local check may present"
        );
    }
    assert!(
        executable.contains("weekday 4"),
        "SF8 no longer derives its ISO week from the Thursday of the landing's week; that \
         derivation is what makes the week number exact without the %V/%G modifiers (#9433)"
    );
}
