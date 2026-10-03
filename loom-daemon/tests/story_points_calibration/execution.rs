//! Fixture execution half of the story-points calibration contracts: the
//! committed chain (populate -> run) executed verbatim over a synthetic
//! fleet, CAL1..CAL6 asserted on its output. Static shape contracts live in
//! `static_contracts`; shared fixtures in `shared`.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use super::shared::{split_statements, CalResult, QUERIES, QUESTION_IDS, ROLLUP};

// ---------------------------------------------------------------------------

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
/// doc): same grain, LSI NULL — not because the committed view is unfitted
/// any more (it carries the `v1-2026-10-02` fit, #9934), but because the
/// bundled SQLite this fixture runs on lacks the math functions the view
/// needs.
const LANDED_SIZE_STUB: &str = "
CREATE VIEW issue_landed_size AS
SELECT f.repo AS repo, f.issue AS issue, f.sweep_id AS landing_sweep_id,
       f.emitted_at AS landed_at,
       NULL AS LSI,
       'v1-2026-10-02' AS params_version
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
