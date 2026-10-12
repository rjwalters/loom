//! Upgrade proof for the durable `sweep_facts` table and the `issue_effort`
//! view (Issue #9507): a database installed by the PREVIOUS bundle, holding
//! facts whose raw `records` were already evicted, is brought to the current
//! shape without losing a fact, and every step is safe to re-run.
//!
//! Why this exists. Every other execution test of the bundle builds a FRESH
//! database, where `CREATE TABLE IF NOT EXISTS` / `CREATE VIEW IF NOT EXISTS`
//! always take effect. On an installed database they are no-ops, so a column
//! added to the rollup's DDL never reaches the table (the rollup's INSERT then
//! fails with "has no column named …") and an edited view is silently kept in
//! its old shape (a query for a new column fails with "no such column"). Only a
//! test that starts from the previous definitions sees that transition.
//!
//! The previous definitions are vendored verbatim from the merge-base of the
//! #9507 change (`d347079cd`) under `fixtures/sweep_facts_pre_9507/`, so this
//! test keeps proving the upgrade from the shape that was actually installed.
//! Everything else is the committed SQL, executed verbatim on the bundled
//! SQLite (the D1 engine) with D1's compound-SELECT ceiling applied.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use regex::Regex;
use rusqlite::Connection;

#[path = "support/d1_sqlite.rs"]
mod d1_sqlite;

const PRE_9507_ROLLUP: &str = include_str!("fixtures/sweep_facts_pre_9507/sweep-facts-rollup.sql");
const PRE_9507_VIEW: &str = include_str!("fixtures/sweep_facts_pre_9507/issue-effort.sql");

const ROLLUP: &str = "defaults/observability/sweep-facts/sweep-facts-rollup.sql";
const VIEW: &str = "defaults/observability/sweep-facts/issue-effort.sql";
const MIGRATE: &str = "defaults/observability/sweep-facts/sweep-facts-migrate.sql";

/// The four columns #9507 added to `sweep_facts`, in table order.
const ADDED_9507: [&str; 4] = [
    "rework_substantive_sec",
    "rework_environmental_sec",
    "rework_substantive_open",
    "rework_environmental_open",
];

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn committed(relative: &str) -> String {
    std::fs::read_to_string(repo_file(relative))
        .unwrap_or_else(|e| panic!("{relative} must be readable: {e}"))
}

/// A SQL file split into statements, whole-line `--` comments dropped first
/// (their prose carries semicolons). Nothing else is rewritten — the same
/// splitter `issue_effort_sqlite.rs` uses.
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

fn execute(conn: &Connection, label: &str, sql: &str) {
    for statement in statements(sql) {
        conn.execute_batch(&statement)
            .unwrap_or_else(|e| panic!("{label} must execute on SQLite: {e}\n{statement}"));
    }
}

/// The first error executing `sql`, or `None` if every statement ran.
fn execute_err(conn: &Connection, sql: &str) -> Option<String> {
    statements(sql)
        .iter()
        .find_map(|statement| conn.execute_batch(statement).err().map(|e| e.to_string()))
}

/// Every row of `statement`, each column rendered as a string (`NULL` → `∅`,
/// so a NULL can never compare equal to an empty string).
fn rows(conn: &Connection, statement: &str) -> Vec<Vec<String>> {
    let mut prepared = conn
        .prepare(statement)
        .unwrap_or_else(|e| panic!("must prepare: {e}\n{statement}"));
    let column_count = prepared.column_count();
    let mut raw = prepared.raw_query();
    let mut out = Vec::new();
    while let Some(row) = raw.next().unwrap() {
        out.push(
            (0..column_count)
                .map(|i| match row.get::<_, rusqlite::types::Value>(i).unwrap() {
                    rusqlite::types::Value::Null => "∅".to_string(),
                    rusqlite::types::Value::Integer(n) => n.to_string(),
                    rusqlite::types::Value::Real(f) => format!("{f:?}"),
                    rusqlite::types::Value::Text(s) => s,
                    rusqlite::types::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect(),
        );
    }
    out
}

/// The DDL the committed migration says is still missing.
fn pending_ddl(conn: &Connection) -> Vec<String> {
    let migration = statements(&committed(MIGRATE));
    assert_eq!(
        migration.len(),
        1,
        "sweep-facts-migrate.sql must stay ONE read-only statement (its output is the DDL)"
    );
    rows(conn, &migration[0])
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

/// Apply what the migration prints — the runbook, step for step.
fn migrate(conn: &Connection) -> Vec<String> {
    let ddl = pending_ddl(conn);
    for statement in &ddl {
        conn.execute_batch(statement)
            .unwrap_or_else(|e| panic!("emitted DDL must execute: {e}\n{statement}"));
    }
    ddl
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    rows(conn, &format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

/// Every `sweep_facts` row over `cols`, in key order.
fn facts(conn: &Connection, cols: &[String]) -> Vec<Vec<String>> {
    rows(
        conn,
        &format!("SELECT {} FROM sweep_facts ORDER BY repo, issue, sweep_id", cols.join(", ")),
    )
}

fn effort(conn: &Connection) -> Vec<Vec<String>> {
    rows(conn, "SELECT * FROM issue_effort ORDER BY repo, issue")
}

/// `records` rows: (emitted_at, issue, sweep_id, payload).
const FIXTURE: &[(&str, u32, &str, &str)] = &[
    // Issue 60's raw records are EVICTED after the old install (D1 retention):
    // its fact row exists nowhere but `sweep_facts` from then on.
    (
        "2026-09-10T10:00:00Z",
        60,
        "u-60a",
        r#"{"trigger":"first","attempt_index":1,"disposition":"landed","pr_number":8,
            "total_duration_sec":200,"tokens_in":1000,"tokens_out":50,"story_points":2,
            "rework_events":[{"kind":"merge_conflict","classification":"environmental",
                              "duration_sec":20}]}"#,
    ),
    // Issue 61 is still in `records`, so the new rollup re-derives its seconds.
    (
        "2026-09-20T10:00:00Z",
        61,
        "u-61a",
        r#"{"trigger":"first","attempt_index":1,"disposition":"env_failure",
            "total_duration_sec":500,
            "rework_events":[{"kind":"merge_conflict","classification":"environmental",
                              "duration_sec":100}]}"#,
    ),
    (
        "2026-09-20T11:00:00Z",
        61,
        "u-61b",
        r#"{"trigger":"retry_after_env_failure","attempt_index":2,"previous_sweep_id":"u-61a",
            "disposition":"landed","pr_number":9,"total_duration_sec":300,
            "rework_events":[{"kind":"rejudge","classification":"substantive"}]}"#,
    ),
];

fn records(conn: &Connection, only_issue: Option<u32>) {
    conn.execute_batch(include_str!("fixtures/records-ddl.sql"))
        .unwrap();
    for (emitted_at, issue, sweep_id, payload) in FIXTURE {
        if only_issue.is_some_and(|keep| keep != *issue) {
            continue;
        }
        conn.execute(
            "INSERT INTO records \
             (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, \
              payload, ingested_at) \
             VALUES (2, ?1, 'host-a', 'sweep.outcome', 'o/r', 'public', ?2, ?3, json(?4), ?1)",
            rusqlite::params![emitted_at, issue, sweep_id, payload],
        )
        .unwrap();
    }
}

/// A database exactly as the PREVIOUS bundle left it: the old rollup ingested
/// every fixture record, the old view was installed, and then issue 60's raw
/// records aged out of D1.
fn installed_pre_9507() -> Connection {
    let conn = d1_sqlite::d1_connection();
    records(&conn, None);
    execute(&conn, "pre-#9507 rollup", PRE_9507_ROLLUP);
    execute(&conn, "pre-#9507 view", PRE_9507_VIEW);
    conn.execute_batch("DELETE FROM records WHERE issue = 60")
        .unwrap();
    conn
}

#[test]
fn an_installed_pre_9507_database_upgrades_without_losing_a_fact() {
    let conn = installed_pre_9507();
    let old_columns = columns(&conn, "sweep_facts");
    for added in ADDED_9507 {
        assert!(
            !old_columns.iter().any(|c| c == added),
            "fixture is not pre-#9507: {old_columns:?}"
        );
    }
    let before = facts(&conn, &old_columns);
    assert_eq!(before.len(), 3, "the old install holds three facts: {before:?}");

    // The failure modes the upgrade exists for, reproduced: the new rollup
    // refuses the old table, and the old view has no partition columns.
    let refused = execute_err(&conn, &committed(ROLLUP)).expect("the rollup ran on an old table");
    assert!(refused.contains("rework_substantive_sec"), "{refused}");
    assert_eq!(facts(&conn, &old_columns), before, "a refused rollup must write nothing");
    assert!(conn.prepare("SELECT clean_sec FROM issue_effort").is_err());

    // The migration prints exactly the four #9507 columns, in table order.
    let applied = migrate(&conn);
    let expected: Vec<String> = ADDED_9507
        .iter()
        .map(|c| format!("ALTER TABLE sweep_facts ADD COLUMN {c} INTEGER;"))
        .collect();
    assert_eq!(applied, expected);
    assert!(pending_ddl(&conn).is_empty(), "an upgraded table must need nothing more");

    execute(&conn, ROLLUP, &committed(ROLLUP));
    execute(&conn, VIEW, &committed(VIEW));

    // No historical fact lost: every pre-upgrade column of every row —
    // including issue 60, whose records are gone — is exactly as it was.
    assert_eq!(facts(&conn, &old_columns), before, "an upgrade must not change a stored fact");

    let new_columns: Vec<String> = ADDED_9507.iter().map(|c| (*c).to_owned()).collect();
    let mut keyed = old_columns[..3].to_vec();
    keyed.extend(new_columns);
    assert_eq!(
        facts(&conn, &keyed),
        [
            // Evicted: not re-derivable, so NULL — "not measured", never 0.
            ["o/r", "60", "u-60a", "∅", "∅", "∅", "∅"],
            // Still in `records`: re-derived by the new rollup.
            ["o/r", "61", "u-61a", "0", "100", "0", "0"],
            ["o/r", "61", "u-61b", "0", "0", "1", "0"],
        ]
        .map(|r| r.map(str::to_owned).to_vec())
    );

    // The installed view was REPLACED, not kept: the partition columns read.
    let split = rows(
        &conn,
        "SELECT issue, lifecycle_wall_sec, clean_sec, substantive_rework_sec, \
                environmental_rework_sec, unattributed_sec, overaccounted_sec, \
                rework_substantive_open \
           FROM issue_effort ORDER BY issue",
    );
    assert_eq!(
        split,
        [
            // Its in-sweep seconds are unknown (NULL), so the whole first
            // attempt is clean residual — the honest reading of evicted data.
            ["60", "200", "200", "0", "0", "0", "0", "∅"],
            // u-61a: 400 clean + 100 environmental; u-61b: 300 environmental
            // residual and one open substantive event.
            ["61", "800", "400", "0", "400", "0", "0", "1"],
        ]
        .map(|r| r.map(str::to_owned).to_vec())
    );

    // The upgraded database answers exactly as a fresh install over the same
    // still-retained records does.
    let fresh = d1_sqlite::d1_connection();
    records(&fresh, Some(61));
    assert!(pending_ddl(&fresh).is_empty(), "no table yet: nothing to migrate");
    execute(&fresh, ROLLUP, &committed(ROLLUP));
    execute(&fresh, VIEW, &committed(VIEW));
    assert!(pending_ddl(&fresh).is_empty(), "a fresh table needs no migration");
    let upgraded_61: Vec<Vec<String>> =
        effort(&conn).into_iter().filter(|r| r[1] == "61").collect();
    assert_eq!(upgraded_61, effort(&fresh));

    // Re-running the whole runbook on the upgraded database is a no-op.
    let all_columns = columns(&conn, "sweep_facts");
    let (facts_once, effort_once) = (facts(&conn, &all_columns), effort(&conn));
    for _ in 0..2 {
        assert!(migrate(&conn).is_empty());
        execute(&conn, ROLLUP, &committed(ROLLUP));
        execute(&conn, VIEW, &committed(VIEW));
    }
    assert_eq!(columns(&conn, "sweep_facts"), all_columns);
    assert_eq!(facts(&conn, &all_columns), facts_once);
    assert_eq!(effort(&conn), effort_once);
}

/// A half-applied upgrade (an operator stopped after two ALTERs) is resumed,
/// not re-broken: the migration prints only what is still missing.
#[test]
fn a_half_applied_upgrade_resumes_with_only_the_missing_columns() {
    let conn = installed_pre_9507();
    for column in &ADDED_9507[..2] {
        conn.execute_batch(&format!("ALTER TABLE sweep_facts ADD COLUMN {column} INTEGER"))
            .unwrap();
    }
    assert_eq!(
        migrate(&conn),
        ADDED_9507[2..]
            .iter()
            .map(|c| format!("ALTER TABLE sweep_facts ADD COLUMN {c} INTEGER;"))
            .collect::<Vec<_>>()
    );
    assert!(pending_ddl(&conn).is_empty());
    execute(&conn, ROLLUP, &committed(ROLLUP));
    execute(&conn, VIEW, &committed(VIEW));
    assert_eq!(rows(&conn, "SELECT count(*) FROM issue_effort"), [["2"]]);
}

/// The migration's canonical column list is a TWIN of the rollup's
/// `CREATE TABLE`: every non-key column, with its declared type, in order. A
/// column added to one side only would either never reach installed tables or
/// be added with the wrong type.
#[test]
fn the_migration_lists_every_non_key_rollup_column() {
    let rollup = committed(ROLLUP);
    let start = rollup
        .find("CREATE TABLE IF NOT EXISTS sweep_facts")
        .expect("rollup no longer creates sweep_facts");
    let body = &rollup[start..];
    let table: Vec<(String, String)> = Regex::new(r"(?m)^\s{4}([a-z_][a-z0-9_]*)\s+([A-Z]+)")
        .unwrap()
        .captures_iter(&body[body.find("(\n").unwrap()..body.find("\n)").unwrap()])
        .map(|c| (c[1].to_owned(), c[2].to_owned()))
        .filter(|(name, _)| !["repo", "issue", "sweep_id"].contains(&name.as_str()))
        .collect();
    let migration = committed(MIGRATE);
    let listed: Vec<(String, String)> =
        Regex::new(r"\(\s*\d+,\s*'([a-z_][a-z0-9_]*)',\s*'([A-Z]+)'\)")
            .unwrap()
            .captures_iter(&migration)
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect();
    assert!(table.len() > 30, "column parse came back short: {table:?}");
    assert_eq!(listed, table, "sweep-facts-migrate.sql and the rollup's CREATE TABLE disagree");
}
