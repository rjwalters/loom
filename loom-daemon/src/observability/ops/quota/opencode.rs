//! OpenCode burn source — the Z.ai GLM subscription and any other provider an
//! OpenCode launch used (Issue #8930).
//!
//! OpenCode keeps one row per API step in its SQLite store's `message` table.
//! An assistant row is inserted with zero tokens when the step starts and
//! **updated in place** when it completes: `data` (JSON) gains
//! `tokens.{input,output,reasoning,cache.read,cache.write}` and
//! `time.completed` (epoch ms). Rows with no tokens (a failed request, e.g.
//! the 429 "limit exhausted" reply) are not usage.
//!
//! # Cursor: row identity, not completion time (Issue #8966)
//!
//! A step is counted **once, by `rowid`**, the first poll that sees it
//! completed. Per store the source keeps `high` (every row at or below it has
//! been read) and `open` (rows at or below `high` read before they completed).
//! A poll reads only `rowid > floor`, where `floor` is `high`, or just below
//! the oldest open row: an indexed range on the table's own b-tree, so a poll
//! costs the rows written since the oldest unfinished step rather than a scan
//! of the whole table (#8956 measured ~0.7 s per poll at 37k rows for the
//! old `time_updated`/`json_extract` predicate, which no index serves).
//!
//! Counting by identity also means a completion committed late is still
//! counted once: its `time.completed` timestamp places it, and the ledger
//! counts a late event in the current window up to
//! [`super::burn::LATE_GRACE_SECS`] late. The only bounds:
//!
//! - a step still open [`OPEN_ROW_MAX_AGE_SECS`] after it was created is
//!   forgotten (it is not counted if it ever completes), and at most
//!   [`MAX_OPEN_ROWS`] open rows are kept (oldest forgotten first);
//! - `rowid` is SQLite's implicit row id (`message.id` is a text key, not a
//!   `rowid` alias). It grows with each insert; SQLite reuses one only after
//!   the current maximum row is deleted, which OpenCode does only when a
//!   session is deleted.
//!
//! The first poll of a store reads it once from the start (to learn `high`
//! and the open rows); what completed before the emit bound is history and is
//! not emitted.
//!
//! Stores: `${XDG_DATA_HOME:-~/.local/share}/opencode/opencode.db` (an
//! operator's own OpenCode) plus every Loom-managed
//! `~/.loom/opt/opencode-<ver>/…/opencode.db`
//! ([`opencode_usage::discover_opencode_dbs`], whose `LOOM_OPENCODE_DB`
//! override replaces the whole set).
//!
//! # Security: one query, naming `message` only
//!
//! The same file holds `credential` and `account` tables. [`BURN_QUERY`] is
//! the whole SQL surface of this module, pinned by a test, and it extracts
//! only the row id and creation time, the provider/model ids, the five
//! counters and the completion time out of `data` — never message content or
//! error bodies. The store is opened
//! read-only, as [`opencode_usage`] opens it.
//!
//! # Mapping
//!
//! `input` and `cache.read`/`cache.write` are disjoint in OpenCode's schema.
//! `reasoning` is a separate counter billed as output, so it is added to
//! `output` ([`opencode_usage`]'s rule). The `provider` label is the API-key
//! pool's namespace where one is known (`zai-coding-plan` → `zai`, see
//! [`pool_provider`]), else OpenCode's own provider id.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OpenFlags};

use super::burn::{BurnEvent, BurnSource, Emit, ModelBurn};
use crate::opencode_usage;

/// The ONLY query this module issues. `?1` is the row-id floor: an indexed
/// range, never a whole-table scan once a store has been read once.
pub const BURN_QUERY: &str = "SELECT rowid, time_created, json_extract(data, '$.providerID'), \
     json_extract(data, '$.modelID'), json_extract(data, '$.tokens.input'), \
     json_extract(data, '$.tokens.output'), json_extract(data, '$.tokens.reasoning'), \
     json_extract(data, '$.tokens.cache.read'), json_extract(data, '$.tokens.cache.write'), \
     json_extract(data, '$.time.completed') FROM message \
     WHERE rowid > ?1 AND json_extract(data, '$.role') = 'assistant' ORDER BY rowid";

/// A step still open this long after it was created is forgotten.
pub const OPEN_ROW_MAX_AGE_SECS: i64 = 6 * 3600;

/// Most open rows remembered per store.
pub const MAX_OPEN_ROWS: usize = 4096;

/// Bounded wait for a lock a live OpenCode process holds.
const SQLITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The `provider` label for an OpenCode provider id: the API-key pool's
/// namespace when the subscription has one (Z.ai's coding plan is `zai` to the
/// pool and `zai-coding-plan` to OpenCode), else OpenCode's id.
#[must_use]
pub fn pool_provider(opencode_provider: &str) -> String {
    let id = opencode_provider.trim().to_ascii_lowercase();
    match id.as_str() {
        "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" => "zai".to_string(),
        "kimi-for-coding" | "moonshotai" | "moonshotai-cn" => "kimi".to_string(),
        _ => id,
    }
}

/// Every OpenCode store on this host, canonical and de-duplicated — exactly
/// [`opencode_usage::discover_opencode_dbs`], which owns the XDG-default
/// addition so the per-sweep readers see the same stores (Issue #8965).
#[must_use]
pub fn opencode_dbs(home: Option<&Path>) -> Vec<PathBuf> {
    opencode_usage::discover_opencode_dbs(home)
}

/// One assistant row as [`BURN_QUERY`] reads it.
#[derive(Debug, Clone)]
pub struct StepRow {
    pub rowid: i64,
    pub created_ms: i64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub usage: ModelBurn,
    /// `time.completed`, epoch ms; `None` while the step is running.
    pub completed_ms: Option<i64>,
}

/// Assistant rows with `rowid > floor`, ascending, or `None` when the store
/// cannot be read (the caller then keeps its state and retries).
pub fn rows_after(db: &Path, floor: i64) -> Option<Vec<StepRow>> {
    let uri = crate::tokens_pool::monitor_db::read_only_uri(db);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    conn.busy_timeout(SQLITE_TIMEOUT).ok()?;
    let mut stmt = conn.prepare(BURN_QUERY).ok()?;
    let rows = stmt
        .query_map([floor], |row| {
            let count = |i: usize| -> rusqlite::Result<i64> {
                Ok(row.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0))
            };
            Ok(StepRow {
                rowid: row.get(0)?,
                created_ms: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                provider: row.get(2)?,
                model: row.get(3)?,
                usage: ModelBurn {
                    input: count(4)?,
                    output: count(5)?.saturating_add(count(6)?),
                    cache_read: count(7)?,
                    cache_write: count(8)?,
                    requests: 1,
                },
                completed_ms: row.get(9)?,
            })
        })
        .ok()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().ok()
}

/// Per-store cursor: see the module doc.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoreCursor {
    /// Every row at or below this has been read at least once.
    pub high: i64,
    /// Rows at or below `high` read before they completed, with their
    /// creation time (epoch ms).
    pub open: BTreeMap<i64, i64>,
}

impl StoreCursor {
    /// The row-id floor the next read starts above.
    #[must_use]
    pub fn floor(&self) -> i64 {
        self.open
            .keys()
            .next()
            .map_or(self.high, |oldest| (oldest - 1).min(self.high))
    }

    /// Fold one read of `rows_after(self.floor())` into the cursor, pushing
    /// each newly completed, non-empty step `emit` accepts.
    pub fn advance(&mut self, rows: Vec<StepRow>, emit: Emit, out: &mut Vec<BurnEvent>) {
        for row in rows {
            let seen_before = row.rowid <= self.high;
            if seen_before && !self.open.contains_key(&row.rowid) {
                continue; // already counted (or not usage)
            }
            self.high = self.high.max(row.rowid);
            let Some(at) = row
                .completed_ms
                .and_then(DateTime::<Utc>::from_timestamp_millis)
            else {
                self.open.insert(row.rowid, row.created_ms);
                continue;
            };
            self.open.remove(&row.rowid);
            let (Some(provider), Some(model)) = (
                row.provider.filter(|p| !p.trim().is_empty()),
                row.model.filter(|m| !m.trim().is_empty()),
            ) else {
                continue;
            };
            let usage = row.usage;
            if (usage.input, usage.output, usage.cache_read, usage.cache_write) == (0, 0, 0, 0)
                || !emit.accepts(at)
            {
                continue;
            }
            out.push(BurnEvent {
                provider: pool_provider(&provider),
                model: model.trim().to_string(),
                at,
                usage,
            });
        }
        let horizon = (emit.now - Duration::seconds(OPEN_ROW_MAX_AGE_SECS)).timestamp_millis();
        self.open.retain(|_, created| *created >= horizon);
        while self.open.len() > MAX_OPEN_ROWS {
            self.open.pop_first();
        }
    }
}

/// Every OpenCode store, polled by row identity.
#[derive(Debug, Default)]
pub struct OpencodeSource {
    /// Overrides [`opencode_dbs`] (tests).
    pub dbs: Option<Vec<PathBuf>>,
    cursors: HashMap<PathBuf, StoreCursor>,
}

impl OpencodeSource {
    /// The cursor kept for `db` (tests).
    #[must_use]
    pub fn cursor(&self, db: &Path) -> Option<&StoreCursor> {
        self.cursors.get(db)
    }
}

impl BurnSource for OpencodeSource {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>) {
        let dbs = self.dbs.clone().unwrap_or_else(|| opencode_dbs(None));
        let mut cursors = HashMap::new();
        for db in dbs {
            let mut cursor = self.cursors.remove(&db).unwrap_or_default();
            // An unreadable store keeps its cursor and is retried next poll.
            if let Some(rows) = rows_after(&db, cursor.floor()) {
                cursor.advance(rows, emit, out);
            }
            cursors.insert(db, cursor);
        }
        self.cursors = cursors;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "opencode_tests.rs"]
mod tests;
