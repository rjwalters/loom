//! Live proof for the per-issue effort query (Issue #9444): executes
//! `defaults/observability/issue-effort-queries.sql` **verbatim**, against the
//! real `records` table `dashboard/migrations/0001_init.sql` defines — the D1
//! shape the fleet telemetry store actually has.
//!
//! No Docker, no network, no CLI: it runs on the **bundled SQLite** this crate
//! already links (`rusqlite`'s `bundled` feature), which is the same engine D1
//! is. So unlike the ClickHouse proof beside it (#8665) this is not
//! `#[ignore]`d — it runs in ordinary CI, on every host, every time.
//!
//! The static half is `issue_effort_artifacts.rs`, which checks the query's
//! vocabulary and field names against the Rust source. This half checks the
//! two things no static read can:
//!
//! 1. **That the SQL parses and runs at all.** The first draft of this file set
//!    shipped a runner recipe that bound nothing — `sqlite3` executes `-cmd`
//!    options *before* positional arguments, so a `.read` passed as `-cmd` ran
//!    before the `.param set` lines following it, and every query returned zero
//!    rows against a populated table (the `.param set` values were
//!    double-quoted too, which SQL reads as an identifier, not a string). A
//!    query that answers "this fleet has no rework" from a store full of it is
//!    the exact failure this whole issue exists to end, and nothing about the
//!    SQL text reveals it.
//! 2. **That the arithmetic partitions the lifecycle.** `clean + substantive +
//!    environmental + unattributed` must equal the summed attempt durations —
//!    no double-counted second, no dropped one — with each attempt's measured
//!    in-sweep rework carved out of its OWN residual rather than added on top.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use rusqlite::Connection;

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// One `sweep.outcome` row as the D1 `records` table stores it.
struct Row {
    emitted_at: &'static str,
    issue: u32,
    sweep_id: &'static str,
    payload: &'static str,
}

/// Three attempts at one issue that exercise every arm of IE1 — a clean first
/// landing, an environmental retry carrying a measured in-sweep conflict, and
/// a substantive retry carrying both a measured re-judge and an *unresolved*
/// conflict (no `duration_sec`) — plus a fourth, pre-#9444 record on another
/// issue with no `trigger` at all, which must land in `unattributed`.
const FIXTURE: &[Row] = &[
    Row {
        emitted_at: "2026-09-20T10:00:00Z",
        issue: 42,
        sweep_id: "s-1",
        payload: r#"{"kind":"sweep.outcome","trigger":"first","attempt_index":1,
            "disposition":"landed","pr_number":1,"total_duration_sec":1000,
            "rework_events":[]}"#,
    },
    Row {
        emitted_at: "2026-09-20T11:00:00Z",
        issue: 42,
        sweep_id: "s-2",
        payload: r#"{"kind":"sweep.outcome","trigger":"retry_after_env_failure","attempt_index":2,
            "previous_sweep_id":"s-1","disposition":"env_failure","total_duration_sec":600,
            "rework_events":[{"kind":"merge_conflict","classification":"environmental",
                              "reason":"loom:merge-conflict","duration_sec":200}]}"#,
    },
    Row {
        emitted_at: "2026-09-20T12:00:00Z",
        issue: 42,
        sweep_id: "s-3",
        payload: r#"{"kind":"sweep.outcome","trigger":"retry_after_substantive_failure",
            "attempt_index":3,"previous_sweep_id":"s-2","disposition":"landed","pr_number":2,
            "total_duration_sec":900,
            "rework_events":[{"kind":"rejudge","classification":"substantive","duration_sec":300},
                             {"kind":"merge_conflict","classification":"environmental"}]}"#,
    },
    Row {
        emitted_at: "2026-09-20T13:00:00Z",
        issue: 43,
        sweep_id: "s-4",
        payload: r#"{"kind":"sweep.outcome","disposition":"landed","pr_number":3,
            "total_duration_sec":500}"#,
    },
];

/// The `records` DDL, lifted out of `dashboard/migrations/0001_init.sql` so
/// the query runs against the real column set rather than an invented one. A
/// renamed or dropped column fails here instead of in production.
fn records_ddl() -> String {
    let migration = std::fs::read_to_string(repo_file("dashboard/migrations/0001_init.sql"))
        .expect("the D1 initial migration must be readable");
    let start = migration
        .find("CREATE TABLE records (")
        .expect("0001_init.sql must define the `records` table");
    let end = migration[start..]
        .find(");")
        .expect("the `records` DDL must terminate")
        + start
        + 2;
    migration[start..end].to_string()
}

/// The committed query file split into its individual statements.
///
/// Whole-line `--` comments are dropped first: the file's prose carries
/// semicolons, and splitting the raw text on `;` would cut a statement in the
/// middle of an explanation. Nothing else is rewritten — what is executed is
/// the committed SQL, character for character.
fn statements(sql: &str) -> Vec<String> {
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

/// The window/limit binds this query set declares. Not every statement uses
/// all three (only the two `LIMIT`ed ones take `:top_n`), so they are bound by
/// NAME LOOKUP rather than as a fixed set — binding a name a statement does
/// not declare is an error in SQLite, and papering over that by editing the
/// SQL would defeat the point of executing it verbatim.
const BINDS: &[(&str, &str)] = &[
    (":since", "2026-09-01T00:00:00Z"),
    (":until", "2026-10-01T00:00:00Z"),
    (":top_n", "20"),
];

/// Every row of `statement`, each column rendered as a string (`NULL` → `""`),
/// with `binds` applied to whichever of their names the statement declares.
fn query_with(conn: &Connection, statement: &str, binds: &[(&str, &str)]) -> Vec<Vec<String>> {
    let mut prepared = conn
        .prepare(statement)
        .unwrap_or_else(|e| panic!("the committed query must be valid SQLite: {e}\n{statement}"));
    for (name, value) in binds {
        if let Some(index) = prepared.parameter_index(name).unwrap() {
            prepared.raw_bind_parameter(index, *value).unwrap();
        }
    }
    let column_count = prepared.column_count();
    let mut rows = prepared.raw_query();
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .unwrap_or_else(|e| panic!("query execution failed: {e}\n{statement}"))
    {
        out.push(
            (0..column_count)
                .map(|i| match row.get::<_, rusqlite::types::Value>(i).unwrap() {
                    rusqlite::types::Value::Null => String::new(),
                    rusqlite::types::Value::Integer(n) => n.to_string(),
                    // `{:?}` keeps the `.0` on a whole float, matching what
                    // the `sqlite3` CLI prints for a REAL column — so an
                    // assertion here reads the same as a hand-run does.
                    rusqlite::types::Value::Real(f) => format!("{f:?}"),
                    rusqlite::types::Value::Text(s) => s,
                    rusqlite::types::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<String>>(),
        );
    }
    out
}

/// [`query_with`] against the canonical window.
fn query(conn: &Connection, statement: &str) -> Vec<Vec<String>> {
    query_with(conn, statement, BINDS)
}

fn seeded() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&records_ddl()).unwrap();
    for row in FIXTURE {
        // Written through `json()` so a malformed fixture fails at insert
        // rather than yielding silent NULLs from `json_extract` later.
        conn.execute(
            "INSERT INTO records \
             (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, \
              payload, ingested_at) \
             VALUES (2, ?1, 'host-a', 'sweep.outcome', 'o/r', 'public', ?2, ?3, json(?4), ?1)",
            rusqlite::params![row.emitted_at, row.issue, row.sweep_id, row.payload],
        )
        .unwrap_or_else(|e| panic!("fixture row {} is not insertable: {e}", row.sweep_id));
    }
    conn
}

fn committed_statements() -> Vec<String> {
    let sql = std::fs::read_to_string(repo_file("defaults/observability/issue-effort-queries.sql"))
        .expect("the committed query file must be readable");
    let statements = statements(&sql);
    assert_eq!(statements.len(), 5, "IE1..IE5 are five statements; found {}", statements.len());
    statements
}

/// IE1 — the acceptance criterion itself: per-issue effort split into clean /
/// substantive-rework / environmental-rework for a bound window.
#[test]
fn ie1_splits_an_issues_cost_into_clean_substantive_and_environmental() {
    let conn = seeded();
    let rows = query(&conn, &committed_statements()[0]);
    assert_eq!(rows.len(), 2, "one row per issue in the window: {rows:?}");

    // repo, issue, attempts, max_attempt_index, lifecycle_sec, clean_sec,
    // substantive_rework_sec, environmental_rework_sec, unattributed_sec,
    // rework_events, open_rework_events, landed_attempts
    let busy = &rows[0];
    assert_eq!(&busy[0..5], ["o/r", "42", "3", "3", "2500"], "{busy:?}");
    // s-1 is `first` and carries no measured rework, so its whole 1000 s is
    // clean — the only source of clean time.
    assert_eq!(busy[5], "1000", "clean: {busy:?}");
    // s-3: its 300 s measured re-judge plus its 600 s residual (its trigger is
    // the substantive retry). The *unresolved* conflict on the same record has
    // no duration and therefore charges nothing — it is reported in
    // `open_rework_events` instead of smoothed to a fabricated figure.
    assert_eq!(busy[6], "900", "substantive rework: {busy:?}");
    // s-2: its 200 s measured conflict plus its 400 s residual.
    assert_eq!(busy[7], "600", "environmental rework: {busy:?}");
    assert_eq!(busy[8], "0", "nothing unattributed on this issue: {busy:?}");
    assert_eq!(busy[9], "3", "three rework events: {busy:?}");
    assert_eq!(busy[10], "1", "one of them never closed: {busy:?}");
    assert_eq!(busy[11], "2", "two landed attempts: {busy:?}");

    // The partition property: every measured second is charged exactly once.
    let charged: i64 = busy[5..9].iter().map(|v| v.parse::<i64>().unwrap()).sum();
    assert_eq!(
        charged,
        busy[4].parse::<i64>().unwrap(),
        "clean + substantive + environmental + unattributed must equal the lifecycle: {busy:?}"
    );

    // A pre-#9444 record carries no `trigger`, so its whole cost is
    // unattributed — never silently folded into a rework bucket, which would
    // manufacture exactly the number this query exists to measure.
    let legacy = &rows[1];
    assert_eq!(&legacy[0..2], ["o/r", "43"], "{legacy:?}");
    assert_eq!(legacy[5], "0", "a triggerless record contributes no clean time");
    assert_eq!(legacy[8], "500", "it is unattributed in full: {legacy:?}");
}

/// IE2 — the issues whose cost was mostly the environment, ranked.
#[test]
fn ie2_ranks_the_issues_that_were_fought_rather_than_hard() {
    let conn = seeded();
    let rows = query(&conn, &committed_statements()[1]);
    assert_eq!(rows.len(), 1, "only issue 42 has environmental cost: {rows:?}");
    // repo, issue, attempts, environmental_attempts, lifecycle_sec,
    // environmental_sec, environmental_pct
    assert_eq!(&rows[0][0..4], ["o/r", "42", "3", "1"], "{:?}", rows[0]);
    assert_eq!(rows[0][4], "2500");
    // 200 s measured in-sweep conflict + s-2's whole 600 s attempt.
    assert_eq!(rows[0][5], "800", "{:?}", rows[0]);
    assert_eq!(rows[0][6], "32.0", "{:?}", rows[0]);
}

/// IE3 — the attempt/trigger distribution, with the triggerless population
/// reported as its own row rather than merged into a real trigger.
#[test]
fn ie3_reports_the_trigger_distribution_and_names_the_triggerless() {
    let conn = seeded();
    let rows = query(&conn, &committed_statements()[2]);
    let triggers: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert!(triggers.contains(&"first"), "{triggers:?}");
    assert!(triggers.contains(&"retry_after_env_failure"), "{triggers:?}");
    assert!(triggers.contains(&"retry_after_substantive_failure"), "{triggers:?}");
    assert!(
        triggers.contains(&"(absent: pre-#9444)"),
        "the pre-#9444 population must be visible, not silently absent: {triggers:?}"
    );
    let retry = rows
        .iter()
        .find(|r| r[0] == "retry_after_env_failure")
        .unwrap();
    assert_eq!(retry[3], "1", "it names a predecessor: {retry:?}");
}

/// IE4 — rework by kind, with the unbounded one counted as open rather than
/// contributing a fabricated zero to the measured total.
#[test]
fn ie4_breaks_rework_down_by_kind_and_reports_the_unbounded_separately() {
    let conn = seeded();
    let rows = query(&conn, &committed_statements()[3]);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let rejudge = rows.iter().find(|r| r[0] == "rejudge").unwrap();
    assert_eq!(rejudge[1], "substantive", "{rejudge:?}");
    assert_eq!(rejudge[5], "300", "{rejudge:?}");
    let conflict = rows.iter().find(|r| r[0] == "merge_conflict").unwrap();
    assert_eq!(conflict[1], "environmental", "{conflict:?}");
    assert_eq!(conflict[2], "2", "two conflict events: {conflict:?}");
    assert_eq!(conflict[4], "1", "one of them never closed: {conflict:?}");
    assert_eq!(conflict[5], "200", "the unclosed one contributes no seconds: {conflict:?}");
}

/// IE5 — the coverage row an operator must read before trusting IE1..IE4.
#[test]
fn ie5_reports_what_fraction_of_attempts_it_can_attribute() {
    let conn = seeded();
    let rows = query(&conn, &committed_statements()[4]);
    assert_eq!(rows.len(), 1);
    // attempts, no_trigger, operator_redispatch, unknown_trigger,
    // no_timeline_read, no_attempt_index, attributed_pct
    assert_eq!(rows[0][0], "4", "{:?}", rows[0]);
    assert_eq!(rows[0][1], "1", "one pre-#9444 record: {:?}", rows[0]);
    assert_eq!(rows[0][2], "0");
    assert_eq!(rows[0][3], "0");
    assert_eq!(rows[0][4], "1", "the same record had no timeline read");
    assert_eq!(rows[0][5], "1", "…and no attempt_index");
    assert_eq!(rows[0][6], "75.0", "three of four attributable: {:?}", rows[0]);
}

/// The window is a bound parameter, not a literal edited into the SQL: a
/// window that excludes every row must return nothing rather than everything.
#[test]
fn the_window_is_bound_not_baked_in() {
    let conn = seeded();
    for statement in committed_statements() {
        let rows = query_with(
            &conn,
            &statement,
            &[
                (":since", "2020-01-01T00:00:00Z"),
                (":until", "2020-01-02T00:00:00Z"),
                (":top_n", "20"),
            ],
        );
        // IE5 is an unfiltered aggregate, so it always returns one row — but
        // that row must report zero attempts, not the whole table's.
        if rows.len() == 1 && rows[0].len() == 7 {
            assert_eq!(rows[0][0], "0", "IE5 over an empty window: {rows:?}");
        } else {
            assert!(rows.is_empty(), "a window with no records must return no rows: {rows:?}");
        }
    }
}
