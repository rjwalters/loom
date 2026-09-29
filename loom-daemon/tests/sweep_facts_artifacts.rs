//! Static contract for the sweep-facts / issue-effort artifacts (Issues
//! #9446, #9466). No Docker, network, backend or credential is used, so this
//! runs in ordinary CI on any host; the live D1 backfill run that feeds the
//! rollup is operator-side (2AMLogic/2am#1608).
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

/// The canonical question IDs. Restated here deliberately: this is the one
/// place the *set* is pinned, and every artifact below is checked against it
/// rather than against another artifact, so two files cannot drift together.
const QUESTION_IDS: &[&str] = &["SF1", "SF2", "SF3", "SF4", "SF5", "SF6", "SF7"];

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
    ];
    assert_eq!(
        issue_effort_columns(),
        expected,
        "the issue_effort view's output columns no longer match the documented effort \
         contract (lifecycle beside clean, with the completeness flag and the rework \
         split) (#9446)"
    );
}

#[test]
fn landed_size_parameters_are_named_and_versioned() {
    assert!(
        LANDED_SIZE.contains("params_version"),
        "landed-size.sql names no versioned parameter set; the fit done during the D1 \
         backfill run (2AMLogic/2am#1608) would have nothing to bump and no way to \
         announce itself"
    );
    assert!(
        LANDED_SIZE.contains("v0-unfitted"),
        "landed-size.sql no longer marks its parameter set unfitted; an unfitted \
         standardization must not pass as fitted"
    );
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
