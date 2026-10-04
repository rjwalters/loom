//! A bundled-SQLite connection that enforces Cloudflare D1's compound-SELECT
//! ceiling (#10066).
//!
//! D1 rejects a compound SELECT of more than 5 terms with `too many terms in
//! compound SELECT`, which was verified against `loom-fleet-telemetry` on
//! 2026-10-03. Bundled SQLite defaults that limit to 500, so a test that
//! executes D1-targeted SQL on a plain `Connection::open_in_memory()` passes
//! SQL that production refuses. Every execution test of a D1 artifact opens
//! its connection here, so the test engine refuses what D1 refuses.
//!
//! Multi-row `VALUES` is exempt from the limit, in SQLite and in D1 alike.
//! That is why the committed lookup tables are written as `VALUES`.

use rusqlite::{limits::Limit, Connection};

/// D1's compound-SELECT term ceiling. A chain of `N` terms has `N - 1`
/// `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT` operators.
pub const D1_MAX_COMPOUND_SELECT_TERMS: i32 = 5;

/// An in-memory connection with D1's compound-SELECT ceiling applied.
pub fn d1_connection() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory sqlite");
    conn.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, D1_MAX_COMPOUND_SELECT_TERMS)
        .expect("set SQLITE_LIMIT_COMPOUND_SELECT");
    assert_eq!(
        conn.limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT)
            .expect("read SQLITE_LIMIT_COMPOUND_SELECT"),
        D1_MAX_COMPOUND_SELECT_TERMS,
        "the compound-SELECT limit did not take; the execution tests would accept SQL D1 \
         refuses (#10066)"
    );
    conn
}
