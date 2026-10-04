//! Live proof for the landed-size view (Issues #9466/#9934): executes
//! `defaults/observability/sweep-facts/landed-size.sql` **verbatim** against
//! the `sweep_facts` shape `sweep-facts-rollup.sql` creates, and asserts the
//! arithmetic the v1 parameter set implies — per-row `landed_size`, `LSI`,
//! `size_class`, and the components-available flag.
//!
//! No Docker, no network, no CLI: it runs on the **bundled SQLite** this
//! crate already links (rusqlite's `bundled` feature), which is the same
//! engine D1 is — with one seam the harness supplies: the bundled build does
//! not compile SQLite's math functions in, while D1 ships them, so the test
//! registers `ln` and `exp` as scalar functions with the exact semantics the
//! view's log1p convention needs (`ln(1.0 + x)`; D1's `ln`/`exp` are the
//! same real functions). Everything else is the committed SQL, character for
//! character.
//!
//! The static half is `sweep_facts_artifacts.rs` (the parameter set is named,
//! versioned, and no longer unfitted; the token verdict is checked). This
//! half checks what no static read can:
//!
//! 1. **That the SQL parses and runs against the real fact shape**, and
//! 2. **that the view implements the documented definition** — mean of the
//!    AVAILABLE standardized components, `LSI = exp(landed_size)`, fixed
//!    class cuts, absent components dropping out (never reading as 0), a
//!    token verdict that is not a measurement (#9440/#9454) reaching no
//!    standardization, and a NEGATIVE landed_size surviving as a positive
//!    LSI below 1.0 (#9934: a below-baseline landing is a small landing,
//!    not an error).
//!
//! The expected values are computed in this file from the same rounded v1
//! constants the SQL carries — the duplication is the point: when the SQL
//! and the definition diverge, this test names the row.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use rusqlite::{functions::FunctionFlags, Connection};

#[path = "support/d1_sqlite.rs"]
mod d1_sqlite;

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// The v1 constants (`landed-size.sql`'s `params` CTE), rounded exactly as
/// the SQL literals are rounded — expected values below are computed from
/// THESE, so a drift between the file and this test cannot hide behind
/// full-precision re-derivation.
const PARAMS_VERSION: &str = "v1-2026-10-02";
const MEAN_LOG_HW_LINES: f64 = 5.776_83;
const SD_LOG_HW_LINES: f64 = 1.965_216;
const MEAN_LOG_HW_FILES: f64 = 1.852_509;
const SD_LOG_HW_FILES: f64 = 0.769_972;
const MEAN_LOG_NORM_TOKENS: f64 = 0.742_732;
const SD_LOG_NORM_TOKENS: f64 = 0.352_462;
/// The fitted per-model factors, again exactly as the SQL literals carry them.
const FACTOR_OPUS_5: f64 = 20_869_614.457_459;
const FACTOR_SONNET_5: f64 = 18_208_672.096_412;
const FACTOR_OPUS_5_5: f64 = 5_417_277.931_668;

/// The `sweep_facts` DDL, copied verbatim from
/// `defaults/observability/sweep-facts/sweep-facts-rollup.sql` (the view
/// reads only the columns below, but the real shape is what runs in
/// production, so the test keeps the whole table).
const SWEEP_FACTS_DDL: &str = "
CREATE TABLE IF NOT EXISTS sweep_facts (
    repo                   TEXT    NOT NULL,
    issue                  INTEGER NOT NULL,
    sweep_id               TEXT    NOT NULL,
    host_id                TEXT,
    emitted_at             TEXT,
    result                 TEXT,
    disposition            TEXT,
    failure_class          TEXT,
    tokens_status          TEXT,
    config_arm             TEXT,
    models_used            TEXT,
    total_duration_sec     INTEGER,
    phase_durations        TEXT,
    tokens_in              INTEGER,
    tokens_out             INTEGER,
    tokens_by_model        TEXT,
    tokens_unattributed_in INTEGER,
    tokens_unattributed_out INTEGER,
    lines_added            INTEGER,
    lines_deleted          INTEGER,
    hw_lines_added         INTEGER,
    hw_lines_deleted       INTEGER,
    hw_files               INTEGER,
    generated_lines        INTEGER,
    test_lines             INTEGER,
    story_points           INTEGER,
    doctor_cycles          INTEGER,
    judge_verdicts         TEXT,
    attempt_index          INTEGER,
    previous_sweep_id      TEXT,
    trigger                TEXT,
    rework_substantive     INTEGER,
    rework_environmental   INTEGER,
    pr_number              INTEGER,
    pr_numbers             TEXT,
    suspect                INTEGER,
    schema_version         INTEGER,
    PRIMARY KEY (repo, issue, sweep_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS sweep_facts_emitted ON sweep_facts (emitted_at);
";

/// One `sweep_facts` row: the identity plus exactly the fields the view
/// reads. `None` stays NULL on the wire — the absent-vs-zero contract.
struct Row {
    issue: u32,
    sweep_id: &'static str,
    emitted_at: &'static str,
    result: &'static str,
    disposition: Option<&'static str>,
    pr_number: Option<u32>,
    models_used: &'static str,
    hw_lines_added: Option<i64>,
    hw_lines_deleted: Option<i64>,
    hw_files: Option<i64>,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    tokens_status: Option<&'static str>,
}

impl Row {
    fn insert_sql(&self) -> String {
        format!(
            "INSERT INTO sweep_facts (repo, issue, sweep_id, host_id, emitted_at, result, \
             disposition, models_used, hw_lines_added, hw_lines_deleted, hw_files, \
             tokens_in, tokens_out, tokens_status, pr_number, schema_version) \
             VALUES ('o/r', {}, '{}', 'host-a', '{}', '{}', {}, '{}', {}, {}, {}, {}, {}, {}, {}, 2)",
            self.issue,
            self.sweep_id,
            self.emitted_at,
            self.result,
            self.disposition.map_or("NULL".into(), |d| format!("'{d}'")),
            // `models_used` is a JSON array on the wire; its double quotes are
            // fine inside a single-quoted literal, its single-quoted model ids
            // are not — escape as SQL requires.
            self.models_used.replace('\'', "''"),
            self.hw_lines_added.map_or("NULL".into(), |v| v.to_string()),
            self.hw_lines_deleted.map_or("NULL".into(), |v| v.to_string()),
            self.hw_files.map_or("NULL".into(), |v| v.to_string()),
            self.tokens_in.map_or("NULL".into(), |v| v.to_string()),
            self.tokens_out.map_or("NULL".into(), |v| v.to_string()),
            self.tokens_status.map_or("NULL".into(), |s| format!("'{s}'")),
            self.pr_number.map_or("NULL".into(), |v| v.to_string()),
        )
    }
}

/// The fixture: one of every arm the view must tell apart. All rows land in
/// the same repo; `emitted_at` values are all inside the baseline window so
/// a consumer filtering on it sees the whole fixture.
const FIXTURE: &[Row] = &[
    // The full three-component landing: hw_lines 120, hw_files 4, tokens
    // 10,869,614 on claude-opus-5 (clean). The row the expected-value math
    // below mirrors component for component.
    Row {
        issue: 1,
        sweep_id: "s-full",
        emitted_at: "2026-10-01T10:00:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(1),
        models_used: "['claude-opus-5']",
        hw_lines_added: Some(100),
        hw_lines_deleted: Some(20),
        hw_files: Some(4),
        tokens_in: Some(10_000_000),
        tokens_out: Some(869_614),
        tokens_status: Some("measured"),
    },
    // Two components: no token reading at all (fields absent, never 0).
    Row {
        issue: 2,
        sweep_id: "s-no-tokens",
        emitted_at: "2026-10-01T10:10:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(2),
        models_used: "['claude-opus-5']",
        hw_lines_added: Some(100),
        hw_lines_deleted: Some(20),
        hw_files: Some(4),
        tokens_in: None,
        tokens_out: None,
        tokens_status: None,
    },
    // One component: tokens only, on a different model, clean.
    Row {
        issue: 3,
        sweep_id: "s-tokens-only",
        emitted_at: "2026-10-01T10:20:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(3),
        models_used: "['claude-sonnet-5']",
        hw_lines_added: None,
        hw_lines_deleted: None,
        hw_files: None,
        tokens_in: Some(9_000_000),
        tokens_out: Some(5_000),
        tokens_status: Some("measured"),
    },
    // A sweep-flagged token reading with nothing else: the component must be
    // absent, so the landing scores NULL — a published number that is not a
    // measurement (#9440/#9454) reaches no standardization (#9934).
    Row {
        issue: 4,
        sweep_id: "s-suspect-only",
        emitted_at: "2026-10-01T10:30:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(4),
        models_used: "['claude-sonnet-5']",
        hw_lines_added: None,
        hw_lines_deleted: None,
        hw_files: None,
        tokens_in: Some(9_000_000),
        tokens_out: Some(5_000),
        tokens_status: Some("suspect"),
    },
    // Flagged tokens BESIDE hand-written components: the hw components still
    // score (the flag is about the token counters), the tokens component
    // drops, and the raw `tokens` column keeps the published reading.
    Row {
        issue: 5,
        sweep_id: "s-suspect-with-hw",
        emitted_at: "2026-10-01T10:40:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(5),
        models_used: "['claude-opus-5']",
        hw_lines_added: Some(100),
        hw_lines_deleted: Some(20),
        hw_files: Some(4),
        tokens_in: Some(10_000_000),
        tokens_out: Some(869_614),
        tokens_status: Some("unattributable"),
    },
    // A model with no fitted factor (fewer than the script's 5 eligible
    // baseline landings): the token component is absent even though the
    // reading is clean — an under-fit factor must not impersonate a
    // normalization. With no hw components either, the landing scores NULL.
    Row {
        issue: 6,
        sweep_id: "s-no-factor",
        emitted_at: "2026-10-01T10:50:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(6),
        models_used: "['glm-5.3-flash']",
        hw_lines_added: None,
        hw_lines_deleted: None,
        hw_files: None,
        tokens_in: Some(4_000_000),
        tokens_out: Some(4_000),
        tokens_status: Some("measured"),
    },
    // The #9934 negative-validity case: a small landing. All three z-scores
    // land well below the baseline mean, so landed_size goes NEGATIVE and
    // LSI = exp(landed_size) lands in (0, 1) — a below-median landing, not
    // an error. (A nonnegative-only reader would have dropped this row.)
    Row {
        issue: 7,
        sweep_id: "s-small",
        emitted_at: "2026-10-01T11:00:00Z",
        result: "success",
        disposition: Some("landed"),
        pr_number: Some(7),
        models_used: "['claude-opus-5-5']",
        hw_lines_added: Some(3),
        hw_lines_deleted: Some(0),
        hw_files: Some(1),
        tokens_in: Some(500),
        tokens_out: Some(500),
        tokens_status: Some("measured"),
    },
    // The documented pre-#9441 fallback: no disposition at all, but a
    // success with a PR — a landing the predicate must keep.
    Row {
        issue: 8,
        sweep_id: "s-legacy",
        emitted_at: "2026-10-01T11:10:00Z",
        result: "success",
        disposition: None,
        pr_number: Some(8),
        models_used: "['claude-opus-5']",
        hw_lines_added: Some(100),
        hw_lines_deleted: Some(20),
        hw_files: Some(4),
        tokens_in: Some(10_000_000),
        tokens_out: Some(869_614),
        tokens_status: None,
    },
    // Not a landing (a cancelled sweep opened no PR): excluded from the view.
    Row {
        issue: 9,
        sweep_id: "s-cancelled",
        emitted_at: "2026-10-01T11:20:00Z",
        result: "cancelled",
        disposition: Some("cancelled"),
        pr_number: None,
        models_used: "['claude-opus-5']",
        hw_lines_added: Some(500),
        hw_lines_deleted: Some(50),
        hw_files: Some(9),
        tokens_in: Some(10_000_000),
        tokens_out: Some(10_000),
        tokens_status: Some("measured"),
    },
];

/// The committed view file split into statements — the same comment-stripping
/// split `issue_effort_sqlite.rs` runs, for the same reason (the prose
/// carries semicolons).
fn landed_size_statements() -> Vec<String> {
    let sql =
        std::fs::read_to_string(repo_file("defaults/observability/sweep-facts/landed-size.sql"))
            .expect("the committed landed-size.sql must be readable");
    let code: String = sql
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    code.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The connection: the real fact shape, the math seam D1 ships, the committed
/// views on top, on a connection that refuses what D1 refuses (#10066).
fn seeded() -> Connection {
    let conn = d1_sqlite::d1_connection();
    // The bundled build omits SQLite's math functions; D1 ships them. Register
    // the two the view needs with their D1 semantics before anything runs.
    conn.create_scalar_function(
        "ln",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // NULL propagates: the view's absent-component contract relies on
            // it (ln of an absent reading must stay absent, not error).
            let x: Option<f64> = ctx.get(0)?;
            match x {
                None => Ok(None),
                Some(x) if x <= 0.0 => Ok(None),
                Some(x) => Ok(Some(x.ln())),
            }
        },
    )
    .unwrap();
    conn.create_scalar_function(
        "exp",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let x: Option<f64> = ctx.get(0)?;
            Ok(x.map(|x| x.exp()))
        },
    )
    .unwrap();
    conn.execute_batch(SWEEP_FACTS_DDL).unwrap();
    for row in FIXTURE {
        conn.execute_batch(&row.insert_sql())
            .unwrap_or_else(|e| panic!("fixture row {} is not insertable: {e}", row.sweep_id));
    }
    for statement in landed_size_statements() {
        conn.execute_batch(&statement)
            .unwrap_or_else(|e| panic!("the committed view must execute: {e}\n{statement}"));
    }
    conn
}

/// `(landed_size, LSI, size_class, tokens_used)` for one landing, read out of
/// the committed view.
fn scored(
    conn: &Connection,
    sweep_id: &str,
) -> (Option<f64>, Option<f64>, Option<String>, Option<i64>) {
    let mut prepared = conn
        .prepare(
            "SELECT landed_size, LSI, size_class, landed_size_tokens_used \
             FROM issue_landed_size WHERE landing_sweep_id = ?1",
        )
        .unwrap();
    let mut rows = prepared.query([sweep_id]).unwrap();
    let row = rows
        .next()
        .unwrap_or_else(|e| panic!("query failed: {e}"))
        .unwrap_or_else(|| panic!("sweep {sweep_id} is missing from the view"));
    let landed_size: Option<f64> = row.get(0).unwrap();
    let lsi: Option<f64> = row.get(1).unwrap();
    let size_class: Option<String> = row.get(2).unwrap();
    let tokens_used: Option<i64> = row.get(3).unwrap();
    (landed_size, lsi, size_class, tokens_used)
}

/// The three standardized components of a fixture row, as the view computes
/// them — the same arithmetic in Rust, from the same rounded constants.
fn expected_components(
    hw_lines: Option<f64>,
    hw_files: Option<f64>,
    tokens: Option<f64>,
    factor: Option<f64>,
) -> (Vec<f64>, bool) {
    let mut parts = Vec::new();
    if let Some(v) = hw_lines {
        parts.push(((1.0 + v).ln() - MEAN_LOG_HW_LINES) / SD_LOG_HW_LINES);
    }
    if let Some(v) = hw_files {
        parts.push(((1.0 + v).ln() - MEAN_LOG_HW_FILES) / SD_LOG_HW_FILES);
    }
    let mut tokens_used = false;
    if let (Some(v), Some(f)) = (tokens, factor) {
        parts.push(((1.0 + v / f).ln() - MEAN_LOG_NORM_TOKENS) / SD_LOG_NORM_TOKENS);
        tokens_used = true;
    }
    (parts, tokens_used)
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    let tolerance = expected.abs().max(1.0) * 1e-9;
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: view says {actual}, the definition says {expected}"
    );
}

fn assert_scored(
    sweep_id: &str,
    hw_lines: Option<f64>,
    hw_files: Option<f64>,
    tokens: Option<f64>,
    factor: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    let conn = seeded();
    let (landed_size, lsi, size_class, tokens_used) = scored(&conn, sweep_id);
    let (parts, expect_tokens_used) = expected_components(hw_lines, hw_files, tokens, factor);
    assert_eq!(
        tokens_used,
        Some(i64::from(expect_tokens_used)),
        "{sweep_id}: the components-available flag disagrees with the definition"
    );
    let mean = parts.iter().sum::<f64>() / parts.len() as f64;
    assert_close(
        landed_size.expect("{sweep_id}: landed_size must not be NULL"),
        mean,
        "{sweep_id} landed_size",
    );
    let exp_lsi = mean.exp();
    assert_close(lsi.expect("{sweep_id}: LSI must not be NULL"), exp_lsi, "{sweep_id} LSI");
    // The class cuts are fixed upper bounds on LSI.
    let expected_class = |lsi: f64| -> &'static str {
        if lsi <= 1.0 {
            "1"
        } else if lsi <= 2.0 {
            "2"
        } else if lsi <= 3.0 {
            "3"
        } else if lsi <= 5.0 {
            "5"
        } else if lsi <= 8.0 {
            "8"
        } else if lsi <= 13.0 {
            "13"
        } else {
            "21"
        }
    };
    assert_eq!(
        size_class.expect("{sweep_id}: size_class must not be NULL"),
        expected_class(exp_lsi),
        "{sweep_id}: size_class disagrees with the fixed cuts"
    );
    (landed_size, lsi)
}

/// The full three-component landing scores as the definition computes it, on
/// the fitted v1 parameters, and says so.
#[test]
fn the_full_landing_scores_on_the_v1_parameters() {
    let (landed_size, _lsi) =
        assert_scored("s-full", Some(120.0), Some(4.0), Some(10_869_614.0), Some(FACTOR_OPUS_5));
    let conn = seeded();
    let mut prepared = conn
        .prepare("SELECT params_version FROM issue_landed_size WHERE landing_sweep_id = 's-full'")
        .unwrap();
    let version: String = prepared.query_row([], |row| row.get(0)).unwrap();
    assert_eq!(version, PARAMS_VERSION, "every scored row must name its fit");
    // And the row's raw facts ride along untouched.
    let mut prepared = conn
        .prepare("SELECT hw_lines, tokens FROM issue_landed_size WHERE landing_sweep_id = 's-full'")
        .unwrap();
    let (hw_lines, tokens): (i64, i64) = prepared
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    assert_eq!(
        (hw_lines, tokens),
        (120, 10_869_614),
        "the view must expose the raw components beside the score"
    );
    let _ = landed_size;
}

/// Two components (no token reading): the absent one drops out of BOTH the
/// numerator and the denominator — never read as a zero (#9446's contract).
#[test]
fn an_absent_component_drops_out_instead_of_reading_zero() {
    let (two_component, _) =
        assert_scored("s-no-tokens", Some(120.0), Some(4.0), None, Some(FACTOR_OPUS_5));
    // And it differs from the three-component score of the same hw facts.
    let (three_component, _) =
        assert_scored("s-full", Some(120.0), Some(4.0), Some(10_869_614.0), Some(FACTOR_OPUS_5));
    assert!(
        (two_component.unwrap() - three_component.unwrap()).abs() > 1e-6,
        "a two-component mean must differ from the three-component mean of the same hw facts"
    );
}

/// A clean token reading alone scores, on its model's fitted factor.
#[test]
fn a_tokens_only_landing_scores_on_its_model_factor() {
    assert_scored("s-tokens-only", None, None, Some(9_005_000.0), Some(FACTOR_SONNET_5));
}

/// A sweep-flagged token reading is published but not a measurement: alone it
/// scores NULL (an honest absence, not a zero), and beside hand-written
/// components it drops out while the hw components still score.
#[test]
fn a_flagged_token_reading_reaches_no_standardization() {
    let conn = seeded();
    let (landed_size, lsi, size_class, _flag) = scored(&conn, "s-suspect-only");
    assert!(
        landed_size.is_none() && lsi.is_none() && size_class.is_none(),
        "a suspect-token-only landing must score NULL, the honest absence"
    );
    let (_, lsi) = assert_scored("s-suspect-with-hw", Some(120.0), Some(4.0), None, None);
    let _ = lsi;
    // The two-component score of s-suspect-with-hw must equal the
    // two-component score of s-no-tokens — same hw facts, same definition.
    let (flagged_hw, _) = assert_scored("s-suspect-with-hw", Some(120.0), Some(4.0), None, None);
    let (clean_hw, _) = assert_scored("s-no-tokens", Some(120.0), Some(4.0), None, None);
    assert_close(
        flagged_hw.unwrap(),
        clean_hw.unwrap(),
        "suspect tokens must not move the hw-only score",
    );
}

/// A clean reading on a model with no fitted factor stays absent: an
/// under-fit factor must not impersonate a normalization.
#[test]
fn a_model_without_a_factor_contributes_no_token_component() {
    let conn = seeded();
    let (landed_size, lsi, size_class, flag) = scored(&conn, "s-no-factor");
    assert!(
        landed_size.is_none() && lsi.is_none() && size_class.is_none(),
        "a tokens-only landing on an unfactorized model must score NULL"
    );
    assert_eq!(flag, Some(0), "the tokens flag must say the component was not used");
}

/// The #9934 negative-validity case: a small landing goes NEGATIVE in
/// landed_size and POSITIVE below 1.0 in LSI — a below-median landing, not
/// an error, and exactly what a nonnegative-only reader would have dropped.
#[test]
fn a_below_baseline_landing_keeps_its_sign_and_its_positive_lsi() {
    let (landed_size, lsi) =
        assert_scored("s-small", Some(3.0), Some(1.0), Some(1_000.0), Some(FACTOR_OPUS_5_5));
    let landed_size = landed_size.unwrap();
    let lsi = lsi.unwrap();
    assert!(
        landed_size < 0.0,
        "a small landing must standardize negative, got {landed_size}"
    );
    assert!(lsi > 0.0 && lsi < 1.0, "LSI must stay positive and below 1.0, got {lsi}");
    assert_eq!(lsi, landed_size.exp(), "LSI is exp(landed_size)");
}

/// The landing predicate: the documented pre-#9441 fallback keeps a
/// disposition-less success-with-PR, and a cancelled sweep is excluded.
#[test]
fn the_landing_predicate_keeps_the_fallback_and_excludes_non_landings() {
    let conn = seeded();
    let mut prepared = conn
        .prepare("SELECT count(*) FROM issue_landed_size WHERE repo = 'o/r'")
        .unwrap();
    let count: i64 = prepared.query_row([], |row| row.get(0)).unwrap();
    // Nine fixture rows, one not a landing → eight in the view.
    assert_eq!(count, 8, "the view must carry exactly the landing rows");
    let mut prepared = conn
        .prepare("SELECT count(*) FROM issue_landed_size WHERE landing_sweep_id = 's-cancelled'")
        .unwrap();
    let cancelled: i64 = prepared.query_row([], |row| row.get(0)).unwrap();
    assert_eq!(cancelled, 0, "a cancelled sweep opens no PR and lands nothing");
}
