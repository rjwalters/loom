//! OpenCode burn source — the Z.ai GLM subscription and any other provider an
//! OpenCode launch used (Issue #8930).
//!
//! OpenCode keeps one row per API step in its SQLite store's `message` table.
//! An assistant row is inserted with zero tokens when the step starts and
//! **updated in place** when it completes: `data` (JSON) gains
//! `tokens.{input,output,reasoning,cache.read,cache.write}` and
//! `time.completed` (epoch ms), and the `time_updated` column moves. So the
//! cursor is completion time, not a byte offset: each poll selects the rows
//! completed in `(through, now - lag]` and advances `through`, which counts
//! every step once. The `time_updated > through` predicate is implied (a row
//! is updated when it completes) and keeps the read to recent rows. Rows
//! with no tokens (a failed request, e.g. the 429 "limit exhausted" reply)
//! are not usage.
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
//! only the provider/model ids, the five counters and the completion time out
//! of `data` — never message content or error bodies. The store is opened
//! read-only, as [`opencode_usage`] opens it.
//!
//! # Mapping
//!
//! `input` and `cache.read`/`cache.write` are disjoint in OpenCode's schema.
//! `reasoning` is a separate counter billed as output, so it is added to
//! `output` ([`opencode_usage`]'s rule). The `provider` label is the API-key
//! pool's namespace where one is known (`zai-coding-plan` → `zai`, see
//! [`pool_provider`]), else OpenCode's own provider id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OpenFlags};

use super::burn::{BurnEvent, BurnSource, Emit, ModelBurn, MESSAGE_SETTLE_LAG_SECS};
use crate::opencode_usage;

/// The ONLY query this module issues. `?1`/`?2` bound the completion time.
pub const BURN_QUERY: &str = "SELECT json_extract(data, '$.providerID'), \
     json_extract(data, '$.modelID'), json_extract(data, '$.tokens.input'), \
     json_extract(data, '$.tokens.output'), json_extract(data, '$.tokens.reasoning'), \
     json_extract(data, '$.tokens.cache.read'), json_extract(data, '$.tokens.cache.write'), \
     json_extract(data, '$.time.completed') FROM message \
     WHERE time_updated > ?1 AND json_extract(data, '$.role') = 'assistant' \
     AND json_extract(data, '$.time.completed') > ?1 \
     AND json_extract(data, '$.time.completed') <= ?2";

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

/// Steps completed in `(since, until]` in one store, or `None` when it cannot
/// be read (the caller then keeps its cursor and retries).
pub fn completed_steps(
    db: &Path,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Option<Vec<BurnEvent>> {
    let uri = crate::tokens_pool::monitor_db::read_only_uri(db);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    conn.busy_timeout(SQLITE_TIMEOUT).ok()?;
    let mut stmt = conn.prepare(BURN_QUERY).ok()?;
    let rows = stmt
        .query_map([since.timestamp_millis(), until.timestamp_millis()], |row| {
            let count = |i: usize| -> rusqlite::Result<i64> {
                Ok(row.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0))
            };
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                ModelBurn {
                    input: count(2)?,
                    output: count(3)?.saturating_add(count(4)?),
                    cache_read: count(5)?,
                    cache_write: count(6)?,
                    requests: 1,
                },
                row.get::<_, Option<i64>>(7)?,
            ))
        })
        .ok()?;
    let mut events = Vec::new();
    for row in rows {
        let (provider, model, usage, completed) = row.ok()?;
        let (Some(provider), Some(model), Some(at)) = (
            provider.filter(|p| !p.trim().is_empty()),
            model.filter(|m| !m.trim().is_empty()),
            completed.and_then(DateTime::<Utc>::from_timestamp_millis),
        ) else {
            continue;
        };
        if (usage.input, usage.output, usage.cache_read, usage.cache_write) == (0, 0, 0, 0) {
            continue;
        }
        events.push(BurnEvent {
            provider: pool_provider(&provider),
            model: model.trim().to_string(),
            at,
            usage,
        });
    }
    Some(events)
}

/// Every OpenCode store, polled by completion time.
#[derive(Debug, Default)]
pub struct OpencodeSource {
    /// Overrides [`opencode_dbs`] (tests).
    pub dbs: Option<Vec<PathBuf>>,
    /// Per store: every step completed at or before this has been read.
    through: HashMap<PathBuf, DateTime<Utc>>,
}

impl BurnSource for OpencodeSource {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>) {
        let until = emit.now - Duration::seconds(MESSAGE_SETTLE_LAG_SECS);
        let dbs = self.dbs.clone().unwrap_or_else(|| opencode_dbs(None));
        let mut through = HashMap::new();
        for db in dbs {
            // A store first seen now starts at the emit bound: older steps
            // are history.
            let since = self.through.get(&db).copied().unwrap_or(emit.not_before);
            if until <= since {
                through.insert(db, since);
                continue;
            }
            match completed_steps(&db, since, until) {
                Some(events) => {
                    out.extend(events.into_iter().filter(|e| emit.accepts(e.at)));
                    through.insert(db, until);
                }
                None => {
                    through.insert(db, since);
                }
            }
        }
        self.through = through;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "opencode_tests.rs"]
mod tests;
