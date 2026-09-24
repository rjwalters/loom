//! Read-only per-model token accounting from OpenCode's own session-store
//! SQLite database (Issue #8507).
//!
//! # Why this module exists
//!
//! [`crate::transcript_tokens`] sums per-model token usage **only** from
//! Claude Code's on-disk JSONL transcripts, so a sweep or role tick dispatched
//! on a non-Claude native runtime (OpenCode, today) never gets a
//! `tokens_by_model` breakdown — and downstream, never gets a model badge on
//! the public fleet feed. The numbers exist the whole time, on disk, in
//! OpenCode's own session store:
//! `~/.loom/opt/opencode-<ver>/xdg/data/opencode/opencode.db`, table
//! `session`. This module locates that database and reads them — the same role
//! [`crate::transcript_tokens`] plays for Claude, behind the runtime-dispatch
//! seam in [`crate::usage_source`].
//!
//! # Security: `session` only, ever
//!
//! The same database also holds `credential` and `account` tables carrying
//! auth material this reader must never see, let alone log. [`SESSION_QUERY`]
//! is the WHOLE SQL surface this module ever sends to the database: one query,
//! naming `session` alone.
//! [`tests::the_only_query_this_module_ever_issues_names_session_and_nothing_else`]
//! pins that by scanning the constant's own text — not merely by testing
//! behavior — so a future edit that adds a second query (or widens this one)
//! fails a test that says exactly why. The database is additionally always
//! opened `?mode=ro` **and** with `SQLITE_OPEN_READ_ONLY`, so even a mistaken
//! statement could not write to a store a live OpenCode process owns.
//!
//! # Session identification
//!
//! A sweep's or role tick's own OpenCode sessions are identified by
//! `session.directory` (the working directory OpenCode was invoked from,
//! matched against an explicit caller-supplied set — see
//! [`crate::usage_source`] for how a sweep's set gains its worktree and a role
//! tick's does not) plus the caller's own wall-clock window. That is the same
//! two-part key [`crate::role_tick_telemetry`]'s Claude-transcript attribution
//! already uses for a tick with no per-invocation id to key on.
//!
//! The issue's own design note observes that a launch's native JSON event
//! stream carries the exact `sessionID`s used, which would be a *more* precise
//! key than directory+window. That refinement is deliberately left as
//! follow-up: it requires the event stream to be captured and correlated per
//! launch, which no consumer does today, and directory+window already
//! attributes correctly for every launch shape the fleet actually runs (one
//! runtime process per directory at a time).
//!
//! # Mapping to [`ModelUsageTotals`]
//!
//! OpenCode has no analogue to Claude's prompt-caching `speed`/`service_tier`
//! axes, so every row uses the literal `"standard"` default for both — the
//! SAME default [`crate::script_helpers::transcript_usage`] already applies to
//! a Claude record carrying neither field, so the grouping key's vocabulary
//! stays one vocabulary across both readers. `tokens_reasoning` is folded into
//! `output` (reasoning tokens bill as an output-token class on every provider
//! this fleet runs), and the flat `tokens_cache_write` counter is attributed
//! entirely to the 1-hour bucket — mirroring the Claude reader's own
//! flat-cache-write fallback for a record with no 5m/1h split.
//!
//! # Schema provenance
//!
//! Verified live against `opencode 1.18.31`'s own
//! `~/.loom/opt/opencode-1.18.31/xdg/data/opencode/opencode.db` on 2026-09-22
//! (`.schema session`): `model` is a JSON object
//! (`{"id":"zai-org/GLM-5.3","providerID":"friendli","variant":"default"}`),
//! the five `tokens_*` columns are `integer NOT NULL DEFAULT 0` session
//! totals, and `time_created` is **milliseconds** since the Unix epoch. A
//! future version that renames or retypes a column degrades to `None` here
//! (every SQL/decode failure is treated as "no data", never a panic and never
//! a fabricated reading).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use crate::script_helpers::sweep_experiment::ModelUsageTotals;

/// The `speed`/`service_tier` bucket every OpenCode row is grouped under —
/// see the module doc's "Mapping" section for why.
const DEFAULT_BUCKET: &str = "standard";

/// Env override naming an exact `opencode.db` path — for tests, and for an
/// operator whose host does not match [`discover_opencode_dbs`]'s layout.
/// Mirrors the `LOOM_CLAUDE_MONITOR_DIR` override convention.
pub const OPENCODE_DB_ENV: &str = "LOOM_OPENCODE_DB";

/// The ONLY query this module ever issues (see the module doc's "Security"
/// section). Names `session` alone — never `credential`/`account`, which live
/// in the same file.
const SESSION_QUERY: &str = "SELECT model, tokens_input, tokens_output, tokens_reasoning, \
     tokens_cache_read, tokens_cache_write, directory, time_created FROM session";

/// Bounded wait for a lock held by a live OpenCode process, mirroring
/// `crate::tokens_pool::monitor_db`'s own budget for the same reason: a held
/// lock must surface as "no data" quickly, never stall a terminal transition.
const SQLITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Every `opencode.db` this host has installed, one per OpenCode version —
/// more than one version routinely coexists under `~/.loom/opt/`, and a sweep
/// or role tick may have run against any of them, so [`tokens_by_model`]
/// queries every match and merges the results.
///
/// [`OPENCODE_DB_ENV`] short-circuits the scan entirely when set (tests, and
/// an operator pinning an exact path). `home` is injectable for tests;
/// production passes `None` (resolves via `dirs::home_dir`).
#[must_use]
pub fn discover_opencode_dbs(home: Option<&Path>) -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os(OPENCODE_DB_ENV).map(PathBuf::from) {
        return if path.is_file() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    let Some(home) = home.map(Path::to_path_buf).or_else(dirs::home_dir) else {
        return Vec::new();
    };
    let opt_dir = home.join(".loom").join("opt");
    let Ok(entries) = std::fs::read_dir(&opt_dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("opencode-"))
        })
        .map(|version_dir| {
            version_dir
                .join("xdg")
                .join("data")
                .join("opencode")
                .join("opencode.db")
        })
        .filter(|db| db.is_file())
        .collect();
    found.sort();
    found
}

/// Extract `model.id` from a `session.model` JSON value
/// (`{"id":"zai-org/GLM-5.3","providerID":"friendli"}`). `None` — never a
/// fabricated model name — for anything that fails to parse or carries no
/// non-empty `id`, matching the "omit rather than guess" contract every other
/// reader in this tree follows.
fn parse_model_id(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract `model.providerID` from the same value [`parse_model_id`] reads.
///
/// Not part of [`ModelUsageTotals`] (whose grouping key is Claude-shaped) —
/// exposed for the `opencode-usage` CLI's backfill listing, where an operator
/// reconciling a past window needs to see which provider served a model id
/// that several providers can serve.
fn parse_provider_id(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value
        .get("providerID")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// One `session` row, already decoded and filtered — the shape both
/// [`tokens_by_model`] and [`sessions_in`] fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeSessionUsage {
    /// `session.model.id`, e.g. `zai-org/GLM-5.3`.
    pub model: String,
    /// `session.model.providerID`, e.g. `friendli`. `None` when the JSON
    /// carried no non-empty `providerID`.
    pub provider: Option<String>,
    /// `session.directory` verbatim.
    pub directory: String,
    /// `session.time_created`, decoded from epoch milliseconds.
    pub created_at: DateTime<Utc>,
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl OpencodeSessionUsage {
    /// Whether this session recorded any token usage at all. OpenCode writes a
    /// `session` row at creation with all five counters at their `DEFAULT 0`,
    /// so a launch that produced no turns leaves an all-zero row behind — a
    /// real artifact, but not usage, and folding it in would publish a
    /// fabricated `0`-token model badge for a model that was never called.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.reasoning == 0
            && self.cache_read == 0
            && self.cache_write == 0
    }
}

/// Decode and filter one `opencode.db`'s `session` rows: only those whose
/// `directory` is in `directories` (exact string match) and, when `window` is
/// given, whose `time_created` falls inside it inclusively.
///
/// `None` — never `Some(vec![])` — when the database cannot be opened or read
/// at all. An empty `Vec` means "opened fine, nothing matched": the two are
/// distinct, and only the caller knows which of them to collapse.
pub fn sessions_in(
    db_path: &Path,
    directories: &[PathBuf],
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<OpencodeSessionUsage>> {
    if !db_path.is_file() {
        return None;
    }
    let uri = crate::tokens_pool::monitor_db::read_only_uri(db_path);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    conn.busy_timeout(SQLITE_TIMEOUT).ok()?;

    let wanted: Vec<&str> = directories
        .iter()
        .filter_map(|d| d.to_str())
        .filter(|d| !d.is_empty())
        .collect();

    let mut stmt = conn.prepare(SESSION_QUERY).ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RawSessionRow {
                model: row.get(0)?,
                input: row.get(1)?,
                output: row.get(2)?,
                reasoning: row.get(3)?,
                cache_read: row.get(4)?,
                cache_write: row.get(5)?,
                directory: row.get(6)?,
                time_created: row.get(7)?,
            })
        })
        .ok()?
        .flatten()
        .filter_map(|raw| raw.decode(&wanted, window))
        .collect();
    Some(rows)
}

/// A `session` row exactly as [`SESSION_QUERY`] returns it, before decoding.
struct RawSessionRow {
    model: Option<String>,
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    directory: Option<String>,
    time_created: Option<i64>,
}

impl RawSessionRow {
    /// `None` when the row is not attributable to `wanted`/`window`, or when
    /// it carries no usable model id — never a guessed substitute.
    fn decode(
        self,
        wanted: &[&str],
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Option<OpencodeSessionUsage> {
        let directory = self.directory?;
        if !wanted.iter().any(|w| *w == directory) {
            return None;
        }
        // `time_created` is NOT NULL in the real schema; a row without a
        // decodable one is unattributable to a window, so it is dropped rather
        // than assumed to be inside it.
        let created_at = DateTime::<Utc>::from_timestamp_millis(self.time_created?)?;
        if let Some((start, end)) = window {
            if created_at < start || created_at > end {
                return None;
            }
        }
        let model_json = self.model?;
        let model = parse_model_id(&model_json)?;
        Some(OpencodeSessionUsage {
            model,
            provider: parse_provider_id(&model_json),
            directory,
            created_at,
            input: self.input.unwrap_or(0),
            output: self.output.unwrap_or(0),
            reasoning: self.reasoning.unwrap_or(0),
            cache_read: self.cache_read.unwrap_or(0),
            cache_write: self.cache_write.unwrap_or(0),
        })
    }
}

/// Every attributable session across EVERY installed OpenCode version's store
/// (see [`discover_opencode_dbs`]), sorted oldest-first.
///
/// The listing counterpart of [`tokens_by_model`], for the `opencode-usage`
/// CLI's backfill path: an operator reconciling a past window needs the
/// per-session rows (and their providers), not only the folded totals.
#[must_use]
pub fn sessions(
    directories: &[PathBuf],
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    home: Option<&Path>,
) -> Vec<OpencodeSessionUsage> {
    let mut all: Vec<OpencodeSessionUsage> = discover_opencode_dbs(home)
        .into_iter()
        .filter_map(|db| sessions_in(&db, directories, window))
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    all.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.model.cmp(&b.model))
    });
    all
}

/// Per-`(model, speed, service_tier)` token totals for `directories` across
/// every installed OpenCode version's session store (Issue #8507) — the
/// OpenCode counterpart of
/// [`crate::transcript_tokens::sum_sweep_tokens_by_model`], reached through
/// the runtime-dispatch seam in [`crate::usage_source`].
///
/// `directories` is the exact set of working directories to attribute (see the
/// module doc's "Session identification" section); `window`, when given, is
/// used exactly as passed with no internal slack — the caller (a sweep's
/// whole-run window vs. a role tick's few minutes) already knows what slack
/// its own cadence needs.
///
/// `home` is injectable for tests (see [`discover_opencode_dbs`]); production
/// callers pass `None`.
///
/// `None` — never `Some(vec![])` — when no `opencode.db` was found at all, or
/// when nothing attributable was found in any of them: "unknown != zero", the
/// same contract the Claude reader follows.
#[must_use]
pub fn tokens_by_model(
    directories: &[PathBuf],
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    home: Option<&Path>,
) -> Option<Vec<ModelUsageTotals>> {
    fold_sessions(sessions(directories, window, home))
}

/// Fold already-selected sessions into per-model totals — the whole of
/// [`tokens_by_model`]'s arithmetic, split out so the mapping documented above
/// has exactly one implementation (and so a test can drive it from a fixture
/// database directly, without the process-global discovery override).
///
/// Usage-free sessions are dropped here rather than by the caller: see
/// [`OpencodeSessionUsage::is_empty`].
#[must_use]
pub fn fold_sessions(
    sessions: impl IntoIterator<Item = OpencodeSessionUsage>,
) -> Option<Vec<ModelUsageTotals>> {
    let mut totals: BTreeMap<String, ModelUsageTotals> = BTreeMap::new();
    for session in sessions.into_iter().filter(|s| !s.is_empty()) {
        let entry = totals
            .entry(session.model.clone())
            .or_insert_with(|| ModelUsageTotals {
                model: session.model.clone(),
                speed: DEFAULT_BUCKET.to_string(),
                service_tier: DEFAULT_BUCKET.to_string(),
                ..ModelUsageTotals::default()
            });
        entry.input = entry.input.saturating_add(session.input);
        entry.cache_read = entry.cache_read.saturating_add(session.cache_read);
        entry.cache_write_1h = entry.cache_write_1h.saturating_add(session.cache_write);
        entry.output = entry
            .output
            .saturating_add(session.output)
            .saturating_add(session.reasoning);
    }
    (!totals.is_empty()).then(|| totals.into_values().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
