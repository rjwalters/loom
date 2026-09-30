//! Static + fixture-backed contract for the story-points calibration
//! artifacts (Issue #9434, epic #9429). No Docker, network, backend or
//! credential is used, so this runs in ordinary CI on any host.
//!
//! Why this exists. The calibration loop's whole claim is that "did we get
//! the story points right?" has a standing, queryable answer whose numbers
//! mean what the questions doc says they mean. That claim is one careless
//! edit away from being false in a way nothing reports: a rubric revision
//! recorded in the doc but not in the SQL (so two populations get silently
//! averaged — the exact failure the eta-queries provenance rule exists to
//! prevent), a class median edited on one side of that mirror only, churn
//! quietly dropped from a score, or a date literal pasted into one query.
//! The static tests below pin those contracts the same way
//! [`sweep_facts_artifacts`] pins the sweep-facts seam.
//!
//! The second half is stronger than the sibling artifacts' static-only
//! checks: these queries are also EXECUTED. A fixture builds a synthetic
//! `sweep_facts` through the committed rollup INSERT
//! (`sweep-facts/sweep-facts-rollup.sql` — so the fixture payloads are
//! verified against the real extraction path, not a hand-written table),
//! then runs the committed calibration file verbatim and asserts
//! HAND-COMPUTED values: medians and quartiles, Spearman ρ including the
//! average-rank tie correction, drift ratios against the rubric's claimed
//! medians, the named misassignments, and every churn-exclusion reason.
//! The synthetic population lives ONLY here — no sample data is committed
//! into the observability artifacts themselves (honest-empty discipline:
//! the joined population is expected to be tiny or empty at first, and
//! CAL1 must report zeros, not fabricate warmth).
//!
//! One documented substitution: the fixture's `issue_landed_size` is a
//! shape-compatible view whose LSI is NULL — exactly what the committed
//! `landed-size.sql` view yields under its current `v0-unfitted` parameter
//! set — because the SQLite bundled into CI lacks the math functions
//! (`ln`, `exp`) that D1 provides and that view needs. The calibration
//! queries' own arithmetic needs none (class bounds compare squares in
//! exact integer arithmetic), so everything except the LSI column is
//! executed as committed.
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::collections::BTreeMap;

use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::Connection;

const QUESTIONS: &str =
    include_str!("../../defaults/observability/story-points-calibration-questions.md");
const QUERIES: &str =
    include_str!("../../defaults/observability/story-points-calibration-queries.sql");
const RUBRIC_DOC: &str = include_str!("../../defaults/docs/story-points.md");
const ROLLUP: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-rollup.sql");
const LANDED_SIZE: &str = include_str!("../../defaults/observability/sweep-facts/landed-size.sql");

/// The canonical question IDs. Restated here deliberately (the
/// [`cycle_time_artifacts`]/[`sweep_facts_artifacts`] pattern): this is the
/// one place the *set* is pinned, and every artifact is checked against it
/// rather than against another artifact, so two files cannot drift together.
const QUESTION_IDS: &[&str] = &["CAL1", "CAL2", "CAL3", "CAL4", "CAL5", "CAL6"];

/// The measured point ratios from the story-points experiment (#9466) — the
/// one place they may be written down is `landed-size.sql`'s
/// `measured_point_values` view. The calibration file must not restate them
/// (a second copy is a second meaning of "a point", and the throughput side
/// that owns them is #9433's, not this artifact's).
const MEASURED_POINT_RATIOS: &[&str] = &[
    "1.3", "2.2", "3.4", "5.1", "8.2", "8.5", "21.0", "47.0", "82.0", "197.0",
];

// ---------------------------------------------------------------------------
// Static helpers (the sweep-facts/cycle-time contract-test vocabulary)
// ---------------------------------------------------------------------------

/// Only the EXECUTABLE lines of a SQL artifact — `--` comment lines dropped.
/// These files document their own pitfalls in prose, so a check for a
/// forbidden token has to distinguish naming it from using it.
fn non_comment_lines(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One CAL question's query body, cut at the next `-- CALn. ` marker.
fn question_body(id: &str) -> &str {
    let start = QUERIES
        .find(&format!("-- {id}. "))
        .unwrap_or_else(|| panic!("story-points-calibration-queries.sql implements no {id}"));
    let end = QUESTION_IDS
        .iter()
        .filter_map(|next| QUERIES[start + 1..].find(&format!("-- {next}. ")))
        .min()
        .map(|offset| start + 1 + offset)
        .unwrap_or(QUERIES.len());
    &QUERIES[start..end]
}

/// The landing predicate as written in `sql` after the `anchor` marker,
/// normalized: table aliases stripped and whitespace collapsed, so the same
/// predicate spelled with a different alias or wrapped at a different column
/// still compares equal. The anchor exists because these files also
/// *describe* the predicate in prose comments.
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

/// The body of a `CREATE VIEW IF NOT EXISTS <name>` block, up to its
/// terminating semicolon (statement-aware, so semicolons quoted in the
/// block's comments do not cut it short).
fn view_block<'a>(sql: &'a str, name: &str) -> &'a str {
    let header = format!("CREATE VIEW IF NOT EXISTS {name}");
    let start = sql
        .find(&header)
        .unwrap_or_else(|| panic!("no {header} block"));
    let statements = split_statements(&sql[start..]);
    let (_, first) = statements.first().expect("view block is never terminated");
    &sql[start..start + first.trim_end().len()]
}

/// The revision markers written into the `rubric_revisions` view.
fn sql_revision_markers() -> Vec<String> {
    let row = Regex::new(r"SELECT '(v\d+)'").unwrap();
    view_block(QUERIES, "rubric_revisions")
        .split(';')
        .filter_map(|part| row.captures(part).map(|capture| capture[1].to_owned()))
        .collect()
}

/// The revision markers recorded in the rubric doc's history table.
fn doc_revision_markers() -> Vec<String> {
    let history = RUBRIC_DOC
        .split("## Revision history")
        .nth(1)
        .expect("story-points.md has no '## Revision history' section (#9434)");
    Regex::new(r"(?m)^\| (v\d+) \|")
        .unwrap()
        .captures_iter(history)
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// The rubric's class table as claimed in the SQL mirror:
/// class -> (hw_lines_median, hw_files_median, tokens_median).
fn sql_rubric_classes() -> BTreeMap<String, (u64, u64, u64)> {
    let row = Regex::new(
        r"(?m)^\s*(?:UNION ALL )?SELECT '(?:v\d+)', '(\d+)',\s*([\d,]+),\s*(\d+),\s*(\d+)\s*$",
    )
    .unwrap();
    row.captures_iter(view_block(QUERIES, "rubric_classes"))
        .map(|capture| {
            (
                capture[1].to_owned(),
                (
                    capture[2].replace(',', "").parse().unwrap(),
                    capture[3].parse().unwrap(),
                    capture[4].parse().unwrap(),
                ),
            )
        })
        .collect()
}

/// The rubric's class table as published in the doc (`~12` / `1` / `~14M`).
fn doc_rubric_classes() -> BTreeMap<String, (u64, u64, u64)> {
    let row = Regex::new(r"(?m)^\| (\d+) \| ~([\d,]+) \| (\d+)\+? \| ~(\d+)M \|$").unwrap();
    row.captures_iter(RUBRIC_DOC)
        .map(|capture| {
            (
                capture[1].to_owned(),
                (
                    capture[2].replace(',', "").parse().unwrap(),
                    capture[3].parse().unwrap(),
                    capture[4].parse::<u64>().unwrap() * 1_000_000,
                ),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Static contracts
// ---------------------------------------------------------------------------

#[test]
fn the_documented_question_set_is_the_implemented_one() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "story-points-calibration-questions.md documents no {id}"
        );
        let implementations = QUERIES.matches(&format!("-- {id}. ")).count();
        assert_eq!(
            implementations, 1,
            "story-points-calibration-queries.sql implements {id} {implementations} times; \
             expected exactly once"
        );
    }
    let stray = Regex::new(r"(?m)^-- (CAL\d+)\. ").unwrap();
    for capture in stray.captures_iter(QUERIES) {
        assert!(
            QUESTION_IDS.contains(&&capture[1]),
            "story-points-calibration-queries.sql implements {}, which is not in the canonical \
             question set",
            &capture[1]
        );
    }
}

#[test]
fn the_window_is_bound_once_and_no_question_carries_a_date_literal() {
    // The D1 adaptation of the cycle-time binding rule (no client-side bind
    // parameters for a committed file): the window lives in the `spc_window`
    // view — the one place a window date literal may appear — and every
    // question reads derived views built on it instead of carrying its own
    // literal. The rubric_revisions dates are history markers, not a window,
    // and live in their own block before CAL1.
    assert_eq!(
        QUERIES
            .matches("CREATE VIEW IF NOT EXISTS spc_window")
            .count(),
        1,
        "the calibration file must define exactly one spc_window view; per-query windows are \
         the hand-edited-literal habit these artifacts replace"
    );
    let date_literal = Regex::new(r"(?m)^[^-].*'20\d\d-\d\d-\d\d").unwrap();
    for id in QUESTION_IDS {
        let body = question_body(id);
        assert!(
            body.contains("spc_scored") || body.contains("spc_landings"),
            "{id} reads neither spc_scored nor spc_landings, so it has escaped the bound \
             spc_window; editing a date literal into the SQL is the habit these artifacts \
             replace"
        );
        assert!(
            date_literal.find(body).is_none(),
            "a date literal was written into {id} outside a comment"
        );
    }
}

#[test]
fn the_landing_predicate_mirrors_the_shared_one() {
    // The calibration must score the same population the rubric's evidence
    // (story-points-queries.sql) and the landed-size index count: the
    // predicate is not merely present, it is the SAME predicate
    // landed-size.sql uses, normalized so alias/wrapping differences cannot
    // hide a divergence.
    let spc = normalized_landing_predicate(QUERIES, "CREATE VIEW IF NOT EXISTS spc_landings");
    let landed_size = normalized_landing_predicate(LANDED_SIZE, "landings AS (");
    assert_eq!(
        spc, landed_size,
        "spc_landings' landing predicate and landed-size.sql's `landings` CTE disagree; the \
         calibration would score a different population than the one the rubric was derived \
         from (#9434)"
    );
    assert!(
        landed_size.contains("disposition IS NULL"),
        "the shared landing predicate lost its documented pre-#9441 fallback"
    );
}

#[test]
fn every_calibration_view_separates_its_populations_by_rubric_revision() {
    // The eta-queries provenance rule (#9289), restated for the estimator
    // that changes: every view groups by the rubric revision a landing was
    // sized under, so a rubric change mid-window shows up as two
    // populations, never a silent average.
    for id in QUESTION_IDS {
        assert!(
            question_body(id).contains("rubric_revision"),
            "{id} does not carry the rubric revision, so it would silently average across a \
             rubric change — the exact failure the provenance rule exists to prevent (#9434)"
        );
    }
    assert!(
        view_block(QUERIES, "spc_landings").contains("FROM rubric_revisions"),
        "spc_landings no longer resolves its revision marker from the rubric_revisions view, \
         so the marker the views group by has no source"
    );
    assert!(
        QUESTIONS.contains("two populations"),
        "the questions doc no longer states the two-populations provenance rule the views \
         implement"
    );
}

#[test]
fn the_excluded_population_is_counted_beside_every_score() {
    // Churn (repair loops, re-judges) is excluded from the SCORE because it
    // is process cost, not estimate error — but excluded must never mean
    // dropped: every view that reports a figure carries the excluded count
    // beside it, and CAL6 is the reason-level breakdown.
    assert!(
        question_body("CAL2").contains("excluded_churn"),
        "CAL2 reports no per-bucket excluded_churn beside its distributions (#9434's churn \
         accounting)"
    );
    for id in ["CAL3", "CAL5"] {
        assert!(
            question_body(id).contains("excluded_from_score"),
            "{id} reports no excluded_from_score beside its figures (#9434's churn accounting)"
        );
    }
    assert!(
        question_body("CAL4").contains("excluded_churn"),
        "CAL4 reports no per-bucket excluded_churn beside its drift verdicts"
    );
    assert!(
        question_body("CAL6").contains("churn_class"),
        "CAL6 no longer breaks the excluded population down by churn_class"
    );
    let cal1 = question_body("CAL1");
    for reason in [
        "excl_no_phase_data",
        "excl_no_judge_phase",
        "excl_rejudge_loop",
        "excl_doctor_repair",
        "excl_doctor_cycles_only",
    ] {
        assert!(
            cal1.contains(reason),
            "CAL1 no longer counts the '{reason}' exclusion; a population the clean-landing \
             filter drops would become invisible (#9434)"
        );
    }
    let spc_landings = view_block(QUERIES, "spc_landings");
    for reason in [
        "no_phase_data",
        "no_judge_phase",
        "rejudge_loop",
        "doctor_repair",
        "doctor_cycles_only",
    ] {
        assert!(
            spc_landings.contains(&format!("'{reason}'")),
            "spc_landings no longer classifies the '{reason}' churn reason"
        );
    }
}

#[test]
fn the_rubric_revision_markers_match_the_documented_history() {
    let sql = sql_revision_markers();
    let doc = doc_revision_markers();
    assert!(
        !sql.is_empty() && !doc.is_empty(),
        "failed to parse revision markers from the SQL mirror and/or the doc history"
    );
    assert_eq!(
        sql, doc,
        "the rubric_revisions view and story-points.md's '## Revision history' name \
         different revision markers; the calibration would group by a revision the doc does \
         not record (or vice versa) — update both together (#9434)"
    );
    let history = RUBRIC_DOC
        .split("## Revision history")
        .nth(1)
        .expect("story-points.md has no '## Revision history' section");
    assert!(
        history.contains("CAL4"),
        "the revision history does not reference the calibration view (CAL4) that triggers a \
         revision; the doc should say updating the rubric from calibration output is normal"
    );
    assert!(
        history.contains("normal, expected change"),
        "the rubric doc no longer states that revising from calibration output is a normal, \
         expected change — the issue's revision-path deliverable"
    );
    assert!(
        history.contains("#9520"),
        "the revision history does not cite #9520 for v1, the revision that landed the rubric"
    );
}

#[test]
fn the_rubric_class_table_mirrors_the_documented_rubric() {
    let sql = sql_rubric_classes();
    let doc = doc_rubric_classes();
    let mut vocabulary: Vec<&str> = sql.keys().map(String::as_str).collect();
    vocabulary.sort_unstable();
    assert_eq!(
        vocabulary,
        ["1", "13", "2", "3", "5", "8"],
        "the SQL rubric_classes view no longer carries the closed points vocabulary"
    );
    assert_eq!(
        sql, doc,
        "the rubric_classes view and story-points.md's class table disagree; the calibration \
         would score estimates against a claim the published rubric does not make — the doc \
         is authoritative, update the mirror (the contract test exists so this cannot go \
         stale silently)"
    );
}

#[test]
fn ordinal_labels_are_never_summed_and_experiment_ratios_not_restated() {
    // Fibonacci labels are ORDINAL (#9429/#9466): a bare sum over
    // story_points is not a size, and the experiment's measured bucket
    // ratios belong to landed-size.sql's measured_point_values view alone
    // (the throughput side, #9433 — restating them here would also blur the
    // calibration/throughput scope boundary).
    let bare_sum = Regex::new(r"(?m)^.*\bsum\(\s*(?:[a-z0-9_]+\.)?story_points\s*\).*$").unwrap();
    assert!(
        bare_sum.find_iter(QUERIES).next().is_none(),
        "a calibration query sums the raw story_points column; Fibonacci labels are ordinal, \
         never a size"
    );
    for line in non_comment_lines(QUERIES).lines() {
        for ratio in MEASURED_POINT_RATIOS {
            assert!(
                !line.contains(ratio),
                "executable SQL restates the measured bucket ratio {ratio}:\n  {}\nRead it \
                 from landed-size.sql's measured_point_values view instead — a second copy is \
                 a second meaning of \"a point\"",
                line.trim()
            );
        }
    }
    assert!(
        !QUERIES.contains("measured_point_values"),
        "the calibration file joins measured_point_values; that view is the throughput side's \
         (#9433, SF8) and this artifact is the calibration side — keep the two disjoint"
    );
}

// ---------------------------------------------------------------------------
// Fixture execution: the committed chain, run verbatim
// ---------------------------------------------------------------------------

/// One CAL query's result: column names plus every row, each cell rendered
/// as text ("(null)" for SQL NULL) so assertions read like the wrangler
/// output an operator would paste into the evidence trail.
#[derive(Debug)]
struct CalResult {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl CalResult {
    fn cell(&self, row: usize, column: &str) -> String {
        let at = self
            .headers
            .iter()
            .position(|header| header == column)
            .unwrap_or_else(|| panic!("no column '{column}' in {:?}", self.headers));
        self.rows[row][at].clone()
    }

    /// The single row whose `column` equals `value` (numbers-as-text compare).
    fn one(&self, column: &str, value: &str) -> &[String] {
        self.rows
            .iter()
            .find(|row| {
                let at = self
                    .headers
                    .iter()
                    .position(|header| header == column)
                    .unwrap_or_else(|| panic!("no column '{column}'"));
                row[at] == value
            })
            .unwrap_or_else(|| panic!("no row with {column} = {value}"))
    }
}

/// Splits a SQL script into statements at top-level semicolons, aware of
/// single-quoted strings and `--`/`/* */` comments — these committed files
/// document their pitfalls in prose, and at least one (landed-size.sql)
/// quotes a `DROP VIEW …;` inside a comment, which a naive split would
/// execute. Returns each statement with its byte offset in the script.
fn split_statements(sql: &str) -> Vec<(usize, &str)> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Normal,
        LineComment,
        BlockComment,
        SingleQuoted,
        DoubleQuoted,
    }
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut state = State::Normal;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (next, step) = match (state, bytes[i]) {
            (State::Normal, b'-') if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                (State::LineComment, 2)
            }
            (State::Normal, b'/') if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                (State::BlockComment, 2)
            }
            (State::Normal, b'\'') => (State::SingleQuoted, 1),
            (State::Normal, b'"') => (State::DoubleQuoted, 1),
            (State::Normal, b';') => {
                out.push((start, &sql[start..i]));
                start = i + 1;
                (State::Normal, 1)
            }
            (State::LineComment, b'\n') => (State::Normal, 1),
            (State::BlockComment, b'*') if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                (State::Normal, 2)
            }
            (State::SingleQuoted, b'\'') if i + 1 < bytes.len() && bytes[i + 1] == b'\'' => {
                (State::SingleQuoted, 2)
            }
            (State::SingleQuoted, b'\'') => (State::Normal, 1),
            (State::DoubleQuoted, b'"') => (State::Normal, 1),
            _ => (state, 1),
        };
        state = next;
        i += step;
    }
    if !sql[start..].trim().is_empty() {
        out.push((start, &sql[start..]));
    }
    out
}

/// The first code line of a statement (leading comment lines skipped).
fn first_code_line(statement: &str) -> &str {
    statement
        .lines()
        .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with("--"))
        .unwrap_or("")
}

/// The D1 `records` envelope, restricted to the columns the committed rollup
/// INSERT reads (migrations/0001_init.sql lives on the D1 side; the rollup
/// is the authority for what the fixture must provide).
const RECORDS_DDL: &str = "
CREATE TABLE records (
    id INTEGER PRIMARY KEY,
    emitted_at TEXT,
    kind TEXT,
    repo TEXT,
    issue INTEGER,
    sweep_id TEXT,
    host_id TEXT,
    schema_version INTEGER,
    payload TEXT
);";

/// The shape-compatible stand-in for `issue_landed_size` (see the module
/// doc): same grain, LSI NULL — the committed view's own output under its
/// current `v0-unfitted` parameter set.
const LANDED_SIZE_STUB: &str = "
CREATE VIEW issue_landed_size AS
SELECT f.repo AS repo, f.issue AS issue, f.sweep_id AS landing_sweep_id,
       f.emitted_at AS landed_at,
       NULL AS LSI,
       'v0-unfitted' AS params_version
FROM sweep_facts f
WHERE f.disposition = 'landed'
   OR (f.disposition IS NULL AND f.result = 'success' AND f.pr_number IS NOT NULL);";

const PHASES_CLEAN: &str = r#"[{"phase":"curator","duration_sec":60},{"phase":"builder","duration_sec":500},{"phase":"judge","duration_sec":40}]"#;
const PHASES_DOCTOR: &str = r#"[{"phase":"curator","duration_sec":60},{"phase":"builder","duration_sec":500},{"phase":"judge","duration_sec":40},{"phase":"doctor","duration_sec":300},{"phase":"builder","duration_sec":100}]"#;
const PHASES_REJUDGE: &str = r#"[{"phase":"curator","duration_sec":60},{"phase":"builder","duration_sec":500},{"phase":"judge","duration_sec":40},{"phase":"judge","duration_sec":50}]"#;
const PHASES_NO_JUDGE: &str =
    r#"[{"phase":"curator","duration_sec":60},{"phase":"builder","duration_sec":500}]"#;

/// Insert one synthetic `sweep.outcome` record — a payload shaped exactly as
/// the rollup INSERT reads it. `points = None` leaves `story_points` absent
/// (unsized, never zero); `phases = None` leaves `phase_durations` absent
/// (the filter is unverifiable); `fallback = true` omits `disposition` and
/// exercises the pre-#9441 landing fallback.
#[allow(clippy::too_many_arguments)]
fn insert_sweep(
    conn: &Connection,
    issue: u32,
    at: &str,
    points: Option<u32>,
    hw_added: u32,
    hw_deleted: u32,
    hw_files: u32,
    tokens_in: u64,
    tokens_out: u64,
    wall_sec: u64,
    phases: Option<&str>,
    doctor_cycles: Option<u32>,
    fallback: bool,
) {
    let mut payload = String::from("{\"result\":\"success\"");
    if !fallback {
        payload.push_str(",\"disposition\":\"landed\"");
    }
    payload.push_str(&format!(
        ",\"tokens_status\":\"measured\",\"models_used\":[\"claude-sonnet-5\"],\
         \"total_duration_sec\":{wall_sec},\"tokens_in\":{tokens_in},\"tokens_out\":{tokens_out},\
         \"hw_lines_added\":{hw_added},\"hw_lines_deleted\":{hw_deleted},\"hw_files\":{hw_files}"
    ));
    if let Some(points) = points {
        payload.push_str(&format!(",\"story_points\":{points}"));
    }
    if let Some(cycles) = doctor_cycles {
        payload.push_str(&format!(",\"doctor_cycles\":{cycles}"));
    }
    payload.push_str(&format!(",\"pr_number\":{issue}"));
    if let Some(phases) = phases {
        payload.push_str(&format!(",\"phase_durations\":{phases}"));
    }
    payload.push('}');
    conn.execute(
        "INSERT INTO records (emitted_at, kind, repo, issue, sweep_id, host_id, schema_version, \
         payload) VALUES (?1, 'sweep.outcome', 'acme/widgets', ?2, ?3, 'host-a', 1, ?4)",
        rusqlite::params![at, issue, format!("sweep-{issue}"), payload,],
    )
    .expect("insert fixture record");
}

/// The synthetic fleet. Fifteen in-window landings plus one outside the
/// window, hand-shaped so every CAL answer below is computable with pencil
/// and paper:
///
/// - scored clean: #101/#102 (bucket 1), #110/#112 (2), #103/#104 (3),
///   #105 (8), plus the two deliberate misassignments #106 (a "1" that cost
///   like an "8") and #107 (an "8" that landed trivially);
/// - churn: #108 (doctor repair), #111 (no phase data), #113 (re-judge
///   loop), #114 (doctor_cycles without a doctor phase), #115 (never
///   judged);
/// - coverage gaps: #109 unsized (via the pre-#9441 fallback), #100 outside
///   the window (binding check).
fn populate(conn: &Connection) {
    //        issue  at                   points hw+/hw-  files  tokens_in/out        wall  phases           doctor_cycles
    insert_sweep(
        conn,
        100,
        "2026-09-20T00:00:00Z",
        Some(1),
        5,
        5,
        1,
        800_000,
        80_000,
        500,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        101,
        "2026-09-29T01:00:00Z",
        Some(1),
        5,
        5,
        1,
        900_000,
        100_000,
        600,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        102,
        "2026-09-29T02:00:00Z",
        Some(1),
        7,
        7,
        1,
        1_100_000,
        100_000,
        700,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        103,
        "2026-09-29T03:00:00Z",
        Some(3),
        150,
        150,
        4,
        28_000_000,
        3_000_000,
        5_000,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        104,
        "2026-09-29T04:00:00Z",
        Some(3),
        100,
        100,
        3,
        23_000_000,
        2_000_000,
        4_000,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        105,
        "2026-09-29T05:00:00Z",
        Some(8),
        500,
        500,
        8,
        65_000_000,
        5_000_000,
        20_000,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        106,
        "2026-09-29T06:00:00Z",
        Some(1),
        450,
        450,
        1,
        1_000_000,
        100_000,
        800,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        107,
        "2026-09-29T07:00:00Z",
        Some(8),
        50,
        50,
        2,
        1_400_000,
        100_000,
        900,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        108,
        "2026-09-29T08:00:00Z",
        Some(5),
        300,
        310,
        6,
        45_000_000,
        5_000_000,
        30_000,
        Some(PHASES_DOCTOR),
        None,
        false,
    );
    insert_sweep(
        conn,
        109,
        "2026-09-29T09:00:00Z",
        None,
        20,
        20,
        1,
        900_000,
        100_000,
        1_000,
        Some(PHASES_CLEAN),
        None,
        true,
    );
    insert_sweep(
        conn,
        110,
        "2026-09-29T10:00:00Z",
        Some(2),
        60,
        60,
        2,
        18_000_000,
        2_000_000,
        1_200,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        111,
        "2026-09-29T11:00:00Z",
        Some(3),
        150,
        150,
        4,
        28_000_000,
        3_000_000,
        3_000,
        None,
        None,
        false,
    );
    insert_sweep(
        conn,
        112,
        "2026-09-30T01:00:00Z",
        Some(2),
        65,
        65,
        2,
        20_000_000,
        2_000_000,
        1_300,
        Some(PHASES_CLEAN),
        None,
        false,
    );
    insert_sweep(
        conn,
        113,
        "2026-09-30T02:00:00Z",
        Some(5),
        250,
        250,
        5,
        23_000_000,
        2_000_000,
        8_000,
        Some(PHASES_REJUDGE),
        None,
        false,
    );
    insert_sweep(
        conn,
        114,
        "2026-09-30T03:00:00Z",
        Some(8),
        400,
        400,
        7,
        37_000_000,
        3_000_000,
        9_000,
        Some(PHASES_CLEAN),
        Some(2),
        false,
    );
    insert_sweep(
        conn,
        115,
        "2026-09-30T04:00:00Z",
        Some(2),
        45,
        45,
        1,
        4_500_000,
        500_000,
        700,
        Some(PHASES_NO_JUDGE),
        None,
        false,
    );
}

/// Build the fixture (through the committed rollup INSERT), run the
/// committed calibration file verbatim, and return each CAL's rows.
fn run_calibration(populated: bool) -> BTreeMap<String, CalResult> {
    let conn = Connection::open_in_memory().expect("open in-memory sqlite");
    conn.execute_batch(RECORDS_DDL).expect("create records");
    if populated {
        populate(&conn);
    }
    // The committed ingest path: records -> sweep_facts, window and all.
    conn.execute_batch(ROLLUP)
        .expect("run sweep-facts-rollup.sql");
    conn.execute_batch(LANDED_SIZE_STUB)
        .expect("stub issue_landed_size");

    let markers: Vec<(usize, String)> = Regex::new(r"(?m)^-- (CAL\d+)\. ")
        .unwrap()
        .captures_iter(QUERIES)
        .map(|capture| (capture.get(0).unwrap().start(), capture[1].to_owned()))
        .collect();
    let mut results = BTreeMap::new();
    for (offset, statement) in split_statements(QUERIES) {
        let head = first_code_line(statement).trim_start();
        if head.starts_with("SELECT") || head.starts_with("WITH") {
            let marker = markers
                .iter()
                .rev()
                .find(|(at, _)| *at >= offset && *at < offset + statement.len())
                .map(|(_, id)| id.clone())
                .expect("query statement without a CAL marker");
            let mut prepared = conn.prepare(statement).expect("prepare CAL query");
            let headers: Vec<String> = prepared
                .column_names()
                .iter()
                .map(|name| (*name).to_owned())
                .collect();
            let mut rows = Vec::new();
            let mut query = prepared.query([]).expect("run CAL query");
            while let Some(row) = query.next().expect("CAL query row") {
                let cells = (0..headers.len())
                    .map(|at| match row.get_ref(at).expect("cell") {
                        ValueRef::Null => "(null)".to_owned(),
                        ValueRef::Integer(value) => value.to_string(),
                        ValueRef::Real(value) => format!("{value}"),
                        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
                        ValueRef::Blob(_) => "<blob>".to_owned(),
                    })
                    .collect();
                rows.push(cells);
            }
            results.insert(marker, CalResult { headers, rows });
        } else {
            conn.execute_batch(statement).expect("run calibration DDL");
        }
    }
    results
}

#[test]
fn the_committed_chain_executes_end_to_end_over_a_synthetic_fleet() {
    // The umbrella fixture test: every statement in the committed
    // calibration file parses and executes against a sweep_facts built by
    // the committed rollup INSERT, and every CAL question returns rows over
    // a populated window. The per-question tests below then pin the values.
    let results = run_calibration(true);
    for id in QUESTION_IDS {
        assert!(results.contains_key(*id), "the fixture run produced no result block for {id}");
        assert!(
            !results[*id].rows.is_empty(),
            "{id} returned no rows over the populated synthetic fleet"
        );
    }
}

#[test]
fn cal1_counts_the_joined_population_and_its_gaps() {
    let results = run_calibration(true);
    let cal1 = &results["CAL1"];
    assert_eq!(cal1.rows.len(), 1, "one row per rubric revision");
    // 15 in-window landings (#100 sits outside spc_window and must not
    // appear — the binding check), 14 sized, #109 unsized via the
    // pre-#9441 fallback, 9 scored clean, and one of every churn reason.
    for (column, expected) in [
        ("rubric_revision", "v1"),
        ("landings", "15"),
        ("points_sized", "14"),
        ("points_unsized", "1"),
        ("clean_scored", "9"),
        ("excl_no_phase_data", "1"),
        ("excl_no_judge_phase", "1"),
        ("excl_rejudge_loop", "1"),
        ("excl_doctor_repair", "1"),
        ("excl_doctor_cycles_only", "1"),
        ("sized_without_rubric_class", "0"),
    ] {
        assert_eq!(
            cal1.cell(0, column),
            expected,
            "CAL1 {column}: points_sized + points_unsized must equal landings, and \
             clean_scored + the excl_* columns must equal points_sized"
        );
    }
    // Keep the identity check honest even if the roster above ever changes:
    // the columns must always add up, or a future population change would
    // silently break the accounting.
    let num = |column: &str| cal1.cell(0, column).parse::<u64>().unwrap();
    assert_eq!(num("points_sized") + num("points_unsized"), num("landings"));
    assert_eq!(
        num("clean_scored")
            + num("excl_no_phase_data")
            + num("excl_no_judge_phase")
            + num("excl_rejudge_loop")
            + num("excl_doctor_repair")
            + num("excl_doctor_cycles_only"),
        num("points_sized")
    );
}

#[test]
fn cal2_reports_cost_distributions_with_quartiles_and_churn_beside() {
    let results = run_calibration(true);
    let cal2 = &results["CAL2"];
    // Five sized buckets x five measures, whether or not the measure has
    // data — the frame guarantees an honest n=0 row, never a missing one.
    assert_eq!(cal2.rows.len(), 25, "unexpected CAL2 row count");

    let bucket = |points: &str, measure: &str| {
        cal2.rows
            .iter()
            .find(|row| {
                let p = cal2
                    .headers
                    .iter()
                    .position(|h| h == "assigned_points")
                    .unwrap();
                let m = cal2.headers.iter().position(|h| h == "measure").unwrap();
                row[p] == points && row[m] == measure
            })
            .unwrap_or_else(|| panic!("no CAL2 row for bucket {points} / {measure}"))
    };
    let get = |row: &[String], column: &str| {
        let at = cal2.headers.iter().position(|h| h == column).unwrap();
        row[at].clone()
    };

    // Bucket 1 hw_lines {10, 14, 900}: median 14 (SP4 convention), p25 the
    // 1st order statistic, p75 the 3rd. Churn beside: none excluded.
    let row = bucket("1", "hw_lines");
    assert_eq!(get(row, "sized_landings"), "3");
    assert_eq!(get(row, "clean_landings"), "3");
    assert_eq!(get(row, "excluded_churn"), "0");
    assert_eq!(get(row, "n_measured"), "3");
    assert_eq!(get(row, "median_value"), "14");
    assert_eq!(get(row, "p25_value"), "10");
    assert_eq!(get(row, "p75_value"), "900");

    // Bucket 1 tokens {1.0M, 1.1M, 1.2M}: the distribution is sorted
    // identically for the tokens measure.
    let row = bucket("1", "tokens");
    assert_eq!(get(row, "n_measured"), "3");
    assert_eq!(get(row, "median_value"), "1100000");
    assert_eq!(get(row, "p25_value"), "1000000");
    assert_eq!(get(row, "p75_value"), "1200000");

    // Bucket 2 hw_lines {120, 130}: median is the mean of both; quartiles
    // collapse onto the outer order statistics. #115 is churn beside it.
    let row = bucket("2", "hw_lines");
    assert_eq!(get(row, "sized_landings"), "3");
    assert_eq!(get(row, "clean_landings"), "2");
    assert_eq!(get(row, "excluded_churn"), "1");
    assert_eq!(get(row, "n_measured"), "2");
    assert_eq!(get(row, "median_value"), "125");
    assert_eq!(get(row, "p25_value"), "120");
    assert_eq!(get(row, "p75_value"), "130");

    // Bucket 3 tokens {25M, 31M}.
    let row = bucket("3", "tokens");
    assert_eq!(get(row, "median_value"), "28000000");

    // Bucket 5: every sized landing (#108, #113) is churn, so the frame row
    // survives with zeros and NULLs — excluded, counted, not dropped.
    let row = bucket("5", "hw_lines");
    assert_eq!(get(row, "sized_landings"), "2");
    assert_eq!(get(row, "clean_landings"), "0");
    assert_eq!(get(row, "excluded_churn"), "2");
    assert_eq!(get(row, "n_measured"), "0");
    assert_eq!(get(row, "median_value"), "(null)");

    // Bucket 8 hw_lines {100, 1000}.
    let row = bucket("8", "hw_lines");
    assert_eq!(get(row, "n_measured"), "2");
    assert_eq!(get(row, "median_value"), "550");
    assert_eq!(get(row, "p25_value"), "100");
    assert_eq!(get(row, "p75_value"), "1000");

    // The lsi arm is honestly empty under the unfitted parameter set: an
    // absent measure is n=0 with NULL figures, never a zero.
    let row = bucket("1", "lsi");
    assert_eq!(get(row, "n_measured"), "0");
    assert_eq!(get(row, "median_value"), "(null)");
}

#[test]
fn cal3_spearman_matches_hand_computed_correlations() {
    let results = run_calibration(true);
    let cal3 = &results["CAL3"];
    // The lsi arm contributes no row at all (no measured values); the other
    // four measures each return one row for revision v1.
    assert_eq!(cal3.rows.len(), 4, "unexpected CAL3 row count");
    for row in 0..cal3.rows.len() {
        for (column, expected) in [
            ("rubric_revision", "v1"),
            ("n", "9"),
            ("excluded_from_score", "5"),
            ("n_with_measured_size", "9"),
            ("within_one_bucket", "7"),
            ("within_one_bucket_pct", "77.8"),
            ("outliers_reported_by_cal5", "2"),
        ] {
            assert_eq!(
                cal3.cell(row, column),
                expected,
                "CAL3 row {row} ({}) column {column}",
                cal3.cell(row, "measure")
            );
        }
    }
    // Spearman ρ over the nine scored landings, computed by hand with
    // average ranks on both sides (points tie in threes/twos; hw_files ties
    // across buckets). hw_files carries the heaviest ties on the actual
    // side (1,1,1 / 2,2,2), so its exact match exercises the tie
    // correction, not just the monotone case.
    assert_eq!(cal3.one("measure", "hw_files").cells_rho(cal3), "0.862");
    assert_eq!(cal3.one("measure", "hw_lines").cells_rho(cal3), "0.412");
    assert_eq!(cal3.one("measure", "tokens").cells_rho(cal3), "0.764");
    assert_eq!(cal3.one("measure", "wall_sec").cells_rho(cal3), "0.764");
}

/// Small helper so the Spearman assertions read as assertions.
trait Rho {
    fn cells_rho(&self, result: &CalResult) -> String;
}
impl Rho for [String] {
    fn cells_rho(&self, result: &CalResult) -> String {
        let at = result
            .headers
            .iter()
            .position(|header| header == "spearman_rho")
            .expect("no spearman_rho column");
        self[at].clone()
    }
}

#[test]
fn cal4_compares_measured_medians_against_the_rubric_claims() {
    let results = run_calibration(true);
    let cal4 = &results["CAL4"];
    // One row per rubric class, whether scored or not: the CLAIM is always
    // enumerable, and an unscored class reads `not_scored`, not absent.
    assert_eq!(cal4.rows.len(), 6, "unexpected CAL4 row count");
    // class, n, measured median, claimed median, measured/claimed,
    // rubric ratio to 1, measured ratio to 1, excluded churn, verdict
    let expected: &[&[&str]] = &[
        &[
            "1",
            "3",
            "14",
            "12",
            "1.17",
            "1",
            "1",
            "0",
            "insufficient_n",
        ],
        &[
            "2",
            "2",
            "125",
            "110",
            "1.14",
            "9.2",
            "8.9",
            "1",
            "insufficient_n",
        ],
        &[
            "3",
            "2",
            "250",
            "285",
            "0.88",
            "23.8",
            "17.9",
            "1",
            "insufficient_n",
        ],
        &[
            "5",
            "0",
            "(null)",
            "610",
            "(null)",
            "50.8",
            "(null)",
            "2",
            "not_scored",
        ],
        &[
            "8",
            "2",
            "550",
            "1000",
            "0.55",
            "83.3",
            "39.3",
            "1",
            "insufficient_n",
        ],
        &[
            "13",
            "0",
            "(null)",
            "2000",
            "(null)",
            "166.7",
            "(null)",
            "0",
            "not_scored",
        ],
    ];
    let columns = [
        "n_scored",
        "measured_median_hw_lines",
        "rubric_claimed_median",
        "measured_over_claimed",
        "rubric_ratio_to_class1",
        "measured_ratio_to_class1",
        "excluded_churn",
        "verdict",
    ];
    for row in expected {
        let actual = cal4.one("assigned_points", row[0]);
        for (column, value) in columns.iter().zip(row.iter().skip(1)) {
            let at = cal4
                .headers
                .iter()
                .position(|header| header == column)
                .unwrap_or_else(|| panic!("no CAL4 column {column}"));
            assert_eq!(actual[at], *value, "CAL4 class {} column {column}", row[0]);
        }
    }
}

#[test]
fn cal5_names_misassignments_in_both_directions() {
    let results = run_calibration(true);
    let cal5 = &results["CAL5"];
    assert_eq!(cal5.rows.len(), 2, "exactly the two planted outliers");
    // Ordered by class_gap DESC: #106 first (a "1" whose actual size, 900
    // hand-written lines, implies class 8 — four buckets away), then #107
    // (an "8" that landed at 100 lines, implied class 2). Both directions,
    // both named, with the actual measures that make them outliers and the
    // churn count beside.
    for (row, issue) in [(0usize, "106"), (1, "107")] {
        assert_eq!(cal5.cell(row, "issue"), issue);
        assert_eq!(cal5.cell(row, "repo"), "acme/widgets");
        assert_eq!(cal5.cell(row, "rubric_revision"), "v1");
        assert_eq!(cal5.cell(row, "excluded_from_score"), "5");
        assert!(cal5.cell(row, "sweep_id").starts_with("sweep-"));
    }
    assert_eq!(cal5.cell(0, "assigned_points"), "1");
    assert_eq!(cal5.cell(0, "implied_class"), "8");
    assert_eq!(cal5.cell(0, "class_gap"), "4");
    assert_eq!(cal5.cell(0, "direction"), "underestimated");
    assert_eq!(cal5.cell(0, "hw_lines"), "900");
    assert_eq!(cal5.cell(0, "tokens"), "1100000");
    assert_eq!(cal5.cell(0, "wall_sec"), "800");
    assert_eq!(cal5.cell(1, "assigned_points"), "8");
    assert_eq!(cal5.cell(1, "implied_class"), "2");
    assert_eq!(cal5.cell(1, "class_gap"), "3");
    assert_eq!(cal5.cell(1, "direction"), "overestimated");
    assert_eq!(cal5.cell(1, "hw_lines"), "100");
    assert_eq!(cal5.cell(1, "tokens"), "1500000");
}

#[test]
fn cal6_accounts_for_every_exclusion_reason() {
    let results = run_calibration(true);
    let cal6 = &results["CAL6"];
    // One row per (bucket, reason): all five churn landings, none dropped,
    // each attributed to the reason that excluded it.
    let expected: &[(&str, &str, &str)] = &[
        ("2", "no_judge_phase", "1"),     // #115: judged never ran
        ("3", "no_phase_data", "1"),      // #111: no breakdown at all
        ("5", "doctor_repair", "1"),      // #108: repair loop
        ("5", "rejudge_loop", "1"),       // #113: judged twice
        ("8", "doctor_cycles_only", "1"), // #114: doctor_cycles with no phase
    ];
    assert_eq!(cal6.rows.len(), expected.len(), "CAL6 rows: {cal6:?}");
    for (points, reason, count) in expected {
        let row = cal6
            .rows
            .iter()
            .find(|row| {
                let p = cal6
                    .headers
                    .iter()
                    .position(|h| h == "assigned_points")
                    .unwrap();
                let r = cal6
                    .headers
                    .iter()
                    .position(|h| h == "exclusion_reason")
                    .unwrap();
                row[p] == *points && row[r] == *reason
            })
            .unwrap_or_else(|| panic!("no CAL6 row for {points}/{reason}"));
        let c = cal6.headers.iter().position(|h| h == "excluded").unwrap();
        assert_eq!(row[c], *count, "{points}/{reason}");
    }
}

#[test]
fn an_empty_window_reports_zero_populations_per_revision_not_errors() {
    // Honest-empty: over a window with no landings at all, CAL1 still
    // returns one zero row per rubric revision (the loop is not yet warm,
    // not broken) and the scored views return no rows rather than errors.
    let results = run_calibration(false);
    let cal1 = &results["CAL1"];
    assert_eq!(cal1.rows.len(), 1, "the revision frame survives empty data");
    for (column, expected) in [
        ("rubric_revision", "v1"),
        ("landings", "0"),
        ("points_sized", "0"),
        ("points_unsized", "0"),
        ("clean_scored", "0"),
        ("sized_without_rubric_class", "0"),
    ] {
        assert_eq!(cal1.cell(0, column), expected, "CAL1 {column} over empty data");
    }
    assert!(results["CAL3"].rows.is_empty());
    assert!(results["CAL5"].rows.is_empty());
}
