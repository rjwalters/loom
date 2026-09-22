//! Persist Claude Code transcript token usage into `activity.db` (issue #8059).
//!
//! # Why this exists
//!
//! `resource_usage` had exactly one live writer — the IPC `GetTerminalOutput`
//! handler, which scrapes a *managed terminal's* scrollback — and `token_usage`
//! had none at all. `dispatch_sweep`'s detached `claude -p` process model, the
//! dominant fleet workload, issues no `SendInput`/`GetTerminalOutput` round
//! trips, so on every host whose work arrives by dispatch both tables (and the
//! `cost_by_role` / `cost_by_month` / `cost_per_issue` views over them) were
//! structurally empty forever. The token data was on disk the whole time, in
//! the sweep's own transcripts — [`crate::transcript_tokens`] already reads it
//! for the completion feed. This module writes it down.
//!
//! # Which table (the #8059 either/or, decided)
//!
//! Rows go to **`resource_usage` only**; `token_usage` is left unwritten.
//! `token_usage` carries no column `resource_usage` lacks that matters here
//! (its `prompt_tokens`/`completion_tokens`/`total_tokens` are the same two
//! axes plus their sum, and its extra `metric_id` links `agent_metrics` rows
//! that transcript ingestion does not produce), while `resource_usage` is the
//! table all six cost analytics views and the `agent_effectiveness` /
//! `cost_per_issue` / `daily_velocity` stats views already read. Writing both
//! would double-book the same tokens in one database and hand every future
//! reporter (#8062) the job of knowing which table not to sum.
//!
//! # How a row is attributed
//!
//! `resource_usage` has no role/repo/session columns of its own — it reaches
//! them through `input_id -> agent_inputs`, which is exactly how `cost_by_role`
//! groups. Ingestion therefore writes **one `agent_inputs` row per ingested
//! transcript** (`terminal_id = "transcript:<session>"`, `input_type = system`,
//! `agent_role` = the attributed role, `context` = workspace/repo/branch/issue)
//! and hangs that transcript's `resource_usage` rows off it. Consequence worth
//! knowing: `agent_inputs` is what `stats` counts as "prompts", so an ingesting
//! host counts one extra "prompt" per ingested transcript. That is the price of
//! populating `cost_by_role` without changing any view, and a transcript really
//! is one agent session.
//!
//! # Re-running is safe
//!
//! Every transcript ingested is recorded in `transcript_ingest` with the file's
//! size and mtime. An unchanged file is skipped; a grown file (a sweep still
//! running) is re-read in full and its previous rows are **replaced**, never
//! appended to, so a repeated pass can neither double-count nor miss the tail
//! of a live session.
//!
//! # On by default (issue #8477)
//!
//! Claude Code deletes a session transcript `cleanupPeriodDays` (default 30)
//! after it was last touched. Until #8477 the background pass above was
//! opt-in (`LOOM_TRANSCRIPT_INGEST=1`), so a host that never hand-set it lost
//! its fleet token/cost history to that fuse permanently, silently, on every
//! default install. [`resolve_enabled`] now defaults to **on**; a host opts
//! *out* instead, via `LOOM_TRANSCRIPT_INGEST=0` or
//! `autonomous.transcriptIngest.enabled: false` in `.loom/config.json` (see
//! [`TranscriptIngestConfig`] / [`read_transcript_ingest_config`] for the
//! full **env > config > default** knob set). [`collect_health_status`] feeds
//! `loom-daemon health`'s `transcript_ingest` section
//! (`health::transcript_ingest_section`), which warns when ingestion is off
//! or has stopped keeping up with transcripts actually on disk.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::db::ActivityDb;
use super::resource_usage::{detect_provider, ModelPricing};
use super::transcript_parse::{parse_transcript, ParsedTranscript};
use crate::transcript_tokens::{claude_projects_dir, project_slug, session_transcripts};

/// Per-file size ceiling, mirroring [`crate::transcript_tokens::MAX_TRANSCRIPT_BYTES`]:
/// a pathological transcript must not be read into memory wholesale by a
/// background maintenance pass.
pub const MAX_TRANSCRIPT_BYTES: u64 = crate::transcript_tokens::MAX_TRANSCRIPT_BYTES;

/// What to ingest.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// `${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects`.
    pub projects_dir: PathBuf,
    /// Restrict to one workspace's project directory (all of them when `None`).
    pub workspace: Option<PathBuf>,
    /// Skip transcripts whose mtime is older than this.
    pub since: Option<DateTime<Utc>>,
    /// Re-ingest transcripts the ledger says are unchanged.
    pub force: bool,
    /// Parse and report, write nothing.
    pub dry_run: bool,
    pub max_transcript_bytes: u64,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            projects_dir: claude_projects_dir().unwrap_or_else(|| PathBuf::from("projects")),
            workspace: None,
            since: None,
            force: false,
            dry_run: false,
            max_transcript_bytes: MAX_TRANSCRIPT_BYTES,
        }
    }
}

/// What one ingestion pass did.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct IngestStats {
    pub transcripts_seen: usize,
    pub skipped_by_window: usize,
    pub skipped_unchanged: usize,
    pub skipped_oversize: usize,
    pub transcripts_without_usage: usize,
    pub transcripts_ingested: usize,
    pub rows_written: usize,
    pub usage_records: usize,
    pub duplicate_records: usize,
    pub synthetic_skipped: usize,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub cost_usd: f64,
}

/// Every transcript under `projects_dir` (parent sessions plus their
/// subagents), optionally narrowed to one workspace's project directory.
#[must_use]
pub fn collect_transcripts(projects_dir: &Path, workspace: Option<&Path>) -> Vec<PathBuf> {
    let project_dirs: Vec<PathBuf> = match workspace {
        Some(root) => vec![projects_dir.join(project_slug(root))],
        None => {
            let Ok(entries) = std::fs::read_dir(projects_dir) else {
                return Vec::new();
            };
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            dirs
        }
    };

    let mut out = Vec::new();
    for dir in project_dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut sessions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        sessions.sort();
        for session in sessions {
            out.extend(session_transcripts(&session));
        }
    }
    out
}

/// Key a transcript is remembered by in the `transcript_ingest` ledger:
/// its path relative to `projects_dir` when possible, else the absolute path.
///
/// `pub(crate)` since #8494: `activity::transcript_archive` keys its own
/// `transcript_archive` ledger by the exact same relative path, so a second
/// private copy of this derivation would be free to drift from this one.
pub(crate) fn ledger_key(projects_dir: &Path, path: &Path) -> String {
    path.strip_prefix(projects_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Stable identifier for the agent session a transcript records: the session
/// uuid, suffixed with the subagent's own file stem for a `subagents/` file so
/// a sweep's several phases stay distinguishable.
fn session_identifier(path: &Path, parsed: &ParsedTranscript) -> String {
    let is_subagent = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "subagents");
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let base = parsed
        .session_id
        .clone()
        .or_else(|| {
            is_subagent
                .then(|| {
                    path.parent()
                        .and_then(Path::parent)
                        .and_then(Path::file_name)
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .flatten()
        })
        .unwrap_or_else(|| stem.clone());

    if is_subagent {
        format!("{base}/{stem}")
    } else {
        base
    }
}

/// Ingest every eligible transcript, returning what the pass did.
///
/// # Errors
///
/// Propagates SQLite failures. Filesystem problems on an individual transcript
/// are logged and skipped, never fatal — one unreadable file must not stop a
/// maintenance pass.
pub fn ingest(db: &ActivityDb, opts: &IngestOptions) -> Result<IngestStats> {
    // Another daemon thread (or the IPC handler) may hold the write lock;
    // wait rather than failing a whole pass on a transient lock.
    let _ = db.conn.busy_timeout(Duration::from_secs(10));

    let mut stats = IngestStats::default();
    for path in collect_transcripts(&opts.projects_dir, opts.workspace.as_deref()) {
        stats.transcripts_seen += 1;

        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let file_size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        let modified: DateTime<Utc> = meta.modified().map_or_else(|_| Utc::now(), Into::into);
        let file_mtime = modified.timestamp();

        if opts.since.is_some_and(|since| modified < since) {
            stats.skipped_by_window += 1;
            continue;
        }

        let key = ledger_key(&opts.projects_dir, &path);
        let previous = lookup_ledger(db, &key)?;
        if !opts.force
            && previous
                .as_ref()
                .is_some_and(|p| p.file_size == file_size && p.file_mtime == file_mtime)
        {
            stats.skipped_unchanged += 1;
            continue;
        }

        if meta.len() > opts.max_transcript_bytes {
            log::warn!(
                "transcript-ingest: skipping oversized transcript {} ({} bytes)",
                path.display(),
                meta.len()
            );
            stats.skipped_oversize += 1;
            continue;
        }

        let parsed = parse_transcript(&path, modified);
        stats.usage_records += parsed.usage_records;
        stats.duplicate_records += parsed.duplicate_records;
        stats.synthetic_skipped += parsed.synthetic_skipped;

        if !parsed.has_usage() {
            stats.transcripts_without_usage += 1;
            if !opts.dry_run {
                // Remembered anyway so an assistant-message-free transcript is
                // not re-parsed on every pass.
                write_ledger(
                    db,
                    &key,
                    &parsed,
                    previous.and_then(|p| p.input_id),
                    file_size,
                    file_mtime,
                    0,
                )?;
            }
            continue;
        }

        for bucket in &parsed.buckets {
            stats.tokens_input = stats.tokens_input.saturating_add(bucket.tokens_input);
            stats.tokens_output = stats.tokens_output.saturating_add(bucket.tokens_output);
            stats.tokens_cache_read = stats
                .tokens_cache_read
                .saturating_add(bucket.tokens_cache_read);
            stats.tokens_cache_write = stats
                .tokens_cache_write
                .saturating_add(bucket.tokens_cache_write);
            stats.cost_usd += bucket_cost(bucket);
        }

        stats.transcripts_ingested += 1;
        stats.rows_written += parsed.buckets.len();
        if !opts.dry_run {
            write_transcript(
                db,
                &key,
                &path,
                &parsed,
                previous.and_then(|p| p.input_id),
                file_size,
                file_mtime,
            )?;
        }
    }

    Ok(stats)
}

fn bucket_cost(bucket: &super::transcript_parse::UsageBucket) -> f64 {
    ModelPricing::for_model(&bucket.model).calculate_cost(
        bucket.tokens_input,
        bucket.tokens_output,
        Some(bucket.tokens_cache_read),
        Some(bucket.tokens_cache_write),
    )
}

#[derive(Debug, Clone, Copy)]
struct LedgerEntry {
    input_id: Option<i64>,
    file_size: i64,
    file_mtime: i64,
}

fn lookup_ledger(db: &ActivityDb, key: &str) -> Result<Option<LedgerEntry>> {
    let row = db
        .conn
        .query_row(
            "SELECT input_id, file_size, file_mtime FROM transcript_ingest WHERE transcript_path = ?1",
            params![key],
            |row| {
                Ok(LedgerEntry {
                    input_id: row.get(0)?,
                    file_size: row.get(1)?,
                    file_mtime: row.get(2)?,
                })
            },
        )
        .optional()
        .context("reading transcript_ingest ledger")?;
    Ok(row)
}

/// Write (or rewrite) one transcript's `agent_inputs` anchor row plus its
/// `resource_usage` rows, atomically.
fn write_transcript(
    db: &ActivityDb,
    key: &str,
    path: &Path,
    parsed: &ParsedTranscript,
    previous_input_id: Option<i64>,
    file_size: i64,
    file_mtime: i64,
) -> Result<()> {
    let session = session_identifier(path, parsed);
    let terminal_id = format!("transcript:{session}");
    let context = serde_json::json!({
        "workspace": parsed.cwd,
        "repo": parsed.repo,
        "branch": parsed.branch,
        "issue_number": parsed.issue,
        "source": "transcript-ingest",
        "transcript": key,
    })
    .to_string();
    let content = format!("transcript-ingest: {key}");
    let anchor_timestamp = parsed
        .buckets
        .first()
        .map_or_else(Utc::now, |b| b.timestamp)
        .to_rfc3339();

    let tx = db.conn.unchecked_transaction()?;

    let input_id = match previous_input_id {
        Some(id)
            if tx.execute(
                "UPDATE agent_inputs SET terminal_id = ?1, timestamp = ?2, agent_role = ?3, \
                 content = ?4, context = ?5 WHERE id = ?6",
                params![
                    terminal_id,
                    anchor_timestamp,
                    parsed.role,
                    content,
                    context,
                    id
                ],
            )? == 1 =>
        {
            id
        }
        // No previous anchor, or it was deleted out from under the ledger.
        _ => {
            tx.execute(
                "INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context) \
                 VALUES (?1, ?2, 'system', ?3, ?4, ?5)",
                params![terminal_id, anchor_timestamp, content, parsed.role, context],
            )?;
            tx.last_insert_rowid()
        }
    };

    // Replace, never append: a live sweep's transcript grows, and its earlier
    // rows are a prefix of what the full file now says.
    tx.execute("DELETE FROM resource_usage WHERE input_id = ?1", params![input_id])?;

    for bucket in &parsed.buckets {
        tx.execute(
            "INSERT INTO resource_usage (\
                input_id, timestamp, model, tokens_input, tokens_output, \
                tokens_cache_read, tokens_cache_write, cost_usd, duration_ms, provider\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
            params![
                input_id,
                bucket.timestamp.to_rfc3339(),
                bucket.model,
                bucket.tokens_input,
                bucket.tokens_output,
                bucket.tokens_cache_read,
                bucket.tokens_cache_write,
                bucket_cost(bucket),
                detect_provider(&bucket.model),
            ],
        )?;
    }

    upsert_ledger(
        &tx,
        key,
        &session,
        Some(input_id),
        file_size,
        file_mtime,
        i64::try_from(parsed.buckets.len()).unwrap_or(i64::MAX),
    )?;

    tx.commit()?;
    Ok(())
}

/// Ledger-only write, for a transcript that produced no rows.
fn write_ledger(
    db: &ActivityDb,
    key: &str,
    parsed: &ParsedTranscript,
    input_id: Option<i64>,
    file_size: i64,
    file_mtime: i64,
    rows: i64,
) -> Result<()> {
    let session = parsed.session_id.clone().unwrap_or_else(|| key.to_string());
    upsert_ledger(&db.conn, key, &session, input_id, file_size, file_mtime, rows)
}

// ---------------------------------------------------------------------------
// Background maintenance pass (on by default, issue #8477)
// ---------------------------------------------------------------------------

/// The subset of `.loom/config.json` -> `autonomous.transcriptIngest` this
/// module consumes. Each field is `Option` so an absent key falls through to
/// the env-var / built-in-default resolution — precedence is **env > config >
/// default** for every knob, matching [`crate::work_finder::WorkFinderConfig`].
///
/// Unlike most `autonomous.*` blocks, [`resolve_enabled`]'s *default* (when
/// neither env nor config sets anything) is **on**, not off — see that
/// function's doc for why (#8477).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptIngestConfig {
    /// `autonomous.transcriptIngest.enabled`.
    pub enabled: Option<bool>,
    /// `autonomous.transcriptIngest.intervalSecs` (a zero/invalid value is
    /// dropped to `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.transcriptIngest.windowHours`. `0` is meaningful (no
    /// window — a full backfill each pass) and is kept, not dropped.
    pub window_hours: Option<i64>,
}

/// Read `.loom/config.json` -> `autonomous.transcriptIngest`, soft-failing
/// every field to `None` (env/default resolution) on a missing file,
/// malformed JSON, or a missing `autonomous`/`transcriptIngest` block —
/// mirrors [`crate::work_finder::read_work_finder_config`]'s soft-fail
/// contract exactly.
#[must_use]
pub fn read_transcript_ingest_config(repo_root: &Path) -> TranscriptIngestConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let node = crate::config_resolver::get_path(&effective, "autonomous.transcriptIngest");

    TranscriptIngestConfig {
        enabled: node
            .and_then(|n| n.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        interval_secs: node
            .and_then(|n| n.get("intervalSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        window_hours: node
            .and_then(|n| n.get("windowHours"))
            .and_then(serde_json::Value::as_i64),
    }
}

/// Parse an env var's value as a loud on/off toggle, distinguishing "unset"
/// from "explicitly set" so an explicit `0`/`false` can override a
/// default-on feature — unrecognized values fall through to `None` ("defer to
/// the next layer"), same as an unset var.
fn parse_bool_env(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `LOOM_TRANSCRIPT_INGEST`'s value, when it decides the outcome outright —
/// `None` when unset (or set to something unrecognized), deferring to
/// [`TranscriptIngestConfig::enabled`] / the built-in default.
#[must_use]
fn env_enabled_override() -> Option<bool> {
    std::env::var("LOOM_TRANSCRIPT_INGEST")
        .ok()
        .and_then(|v| parse_bool_env(&v))
}

fn env_interval_secs() -> Option<u64> {
    std::env::var("LOOM_TRANSCRIPT_INGEST_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
}

fn env_window_hours() -> Option<i64> {
    std::env::var("LOOM_TRANSCRIPT_INGEST_WINDOW_HOURS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
}

/// Whether the background ingestion pass runs, with precedence **env >
/// config > default(true)** (issue #8477).
///
/// This flips the polarity every other `autonomous.*` toggle in this repo
/// uses (FLAGS-OFF, opt-in): Claude Code deletes session transcripts after
/// `cleanupPeriodDays` (default 30), and `resource_usage` had no other
/// dispatch-driven writer (see the module doc) — an install that never
/// hand-sets `LOOM_TRANSCRIPT_INGEST=1` silently lost its fleet token/cost
/// history to that fuse forever. Opting a host *out* (rather than in) is now
/// the deliberate action: `LOOM_TRANSCRIPT_INGEST=0` (or
/// `autonomous.transcriptIngest.enabled: false`) — both still work exactly as
/// before for a host that already sets them.
#[must_use]
pub fn resolve_enabled(config: &TranscriptIngestConfig) -> bool {
    if let Some(v) = env_enabled_override() {
        return v;
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the pass interval (seconds) with precedence **env > config >
/// default(900)**.
#[must_use]
pub fn resolve_interval_secs(config: &TranscriptIngestConfig) -> u64 {
    env_interval_secs().or(config.interval_secs).unwrap_or(900)
}

/// Resolve the lookback window (hours; `0` = full backfill) with precedence
/// **env > config > default(24)**.
#[must_use]
pub fn resolve_window_hours(config: &TranscriptIngestConfig) -> i64 {
    env_window_hours().or(config.window_hours).unwrap_or(24)
}

/// Resolve `(interval_secs, window_hours)`, or `None` when
/// [`resolve_enabled`] says the pass is off.
#[must_use]
pub fn resolve_settings(config: &TranscriptIngestConfig) -> Option<(u64, i64)> {
    resolve_enabled(config).then(|| (resolve_interval_secs(config), resolve_window_hours(config)))
}

/// Run one pass against `db_path`, returning what it did.
///
/// # Errors
///
/// Fails when the database cannot be opened or a write fails.
pub fn run_once(db_path: &Path, window_hours: i64) -> Result<IngestStats> {
    let db = ActivityDb::new(db_path.to_path_buf())?;
    let opts = IngestOptions {
        since: (window_hours > 0).then(|| Utc::now() - chrono::Duration::hours(window_hours)),
        ..IngestOptions::default()
    };
    ingest(&db, &opts)
}

/// Start the periodic ingestion thread unless this host opted out.
///
/// Mirrors `metrics_collector::try_init_metrics_collector`: returns the
/// `JoinHandle` (whose thread keeps running when the handle is dropped) or
/// `None` when the feature is off. `repo_root` is read only for
/// [`read_transcript_ingest_config`] — ingestion itself is workspace-
/// independent (see the module doc).
pub fn try_init_transcript_ingest(
    db_path: &Path,
    repo_root: &Path,
) -> Option<std::thread::JoinHandle<()>> {
    let config = read_transcript_ingest_config(repo_root);
    let (interval, window_hours) = resolve_settings(&config)?;
    let db_path = db_path.to_path_buf();
    log::info!(
        "📥 Transcript token ingestion enabled (every {}min, {} window)",
        interval / 60,
        if window_hours > 0 {
            format!("{window_hours}h")
        } else {
            "full-history".to_string()
        }
    );
    Some(std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(interval));
        match run_once(&db_path, window_hours) {
            Ok(stats) => log::info!(
                "📥 Transcript ingestion: {} transcript(s), {} row(s), {} duplicate chunk(s) collapsed",
                stats.transcripts_ingested,
                stats.rows_written,
                stats.duplicate_records
            ),
            Err(e) => log::error!("❌ Transcript ingestion failed: {e}"),
        }
    }))
}

// ---------------------------------------------------------------------------
// Health/status reporting (#8477)
// ---------------------------------------------------------------------------

/// A ledger entry older than this, while a newer transcript exists on disk,
/// means the background pass has stopped keeping up (a crashed thread, a
/// wedged database lock, …) rather than merely not having ticked yet — see
/// [`collect_health_status`]. Comfortably above the 15-minute default
/// interval so a busy host's normal jitter never trips it.
pub const STALE_THRESHOLD_HOURS: f64 = 6.0;

/// Health-check snapshot of the background ingestion pass, computed without a
/// daemon IPC round-trip — the same "local, best-effort" shape
/// `limit_calibration::compute_with_fallback` already feeds `loom-daemon
/// health` (see `cli/health.rs`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct IngestHealthStatus {
    /// [`resolve_enabled`]'s verdict for this host.
    pub enabled: bool,
    /// Age in hours of `MAX(transcript_ingest.ingested_at)`, when the ledger
    /// has at least one row.
    pub newest_ingested_age_hours: Option<f64>,
    /// Age in hours of the newest transcript file on disk (by mtime), across
    /// every project under `projects_dir`.
    pub newest_transcript_age_hours: Option<f64>,
}

/// Collect [`IngestHealthStatus`] for `db_path`/`projects_dir` under `config`.
/// Every I/O step is best-effort: an unopenable database or unreadable
/// `projects_dir` degrades the corresponding field to `None` rather than
/// failing the whole probe — a health check must never itself error out.
#[must_use]
pub fn collect_health_status(
    db_path: &Path,
    projects_dir: &Path,
    config: &TranscriptIngestConfig,
) -> IngestHealthStatus {
    let now = Utc::now();

    let newest_ingested_age_hours = ActivityDb::new(db_path.to_path_buf())
        .ok()
        .and_then(|db| {
            db.conn
                .query_row("SELECT MAX(ingested_at) FROM transcript_ingest", [], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .ok()
                .flatten()
        })
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| age_hours(dt.with_timezone(&Utc), now));

    let newest_transcript_age_hours = collect_transcripts(projects_dir, None)
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).ok().and_then(|m| m.modified().ok()))
        .map(DateTime::<Utc>::from)
        .max()
        .map(|mtime| age_hours(mtime, now));

    IngestHealthStatus {
        enabled: resolve_enabled(config),
        newest_ingested_age_hours,
        newest_transcript_age_hours,
    }
}

/// Age of `then` as of `now`, in hours, **clamped at zero**.
///
/// A negative age is real and routinely observed: a transcript being written
/// *right now* can carry an mtime a few seconds ahead of this process's clock
/// (filesystem timestamp granularity, or a clock that has since stepped
/// back), which surfaced live on a fleet host as `-0.0039` hours. Reporting
/// "-0.0h old" in a health summary is noise, so a not-yet-aged timestamp is
/// reported as `0.0` — an age can never meaningfully be negative, and the
/// staleness rule in `health::transcript_ingest_section` only ever compares
/// ages against a positive threshold and against each other, both of which
/// the clamp preserves.
fn age_hours(then: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    ((now - then).num_seconds() as f64 / 3600.0).max(0.0)
}

fn upsert_ledger(
    conn: &rusqlite::Connection,
    key: &str,
    session: &str,
    input_id: Option<i64>,
    file_size: i64,
    file_mtime: i64,
    rows: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO transcript_ingest (\
            transcript_path, session_id, input_id, file_size, file_mtime, rows_written, ingested_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(transcript_path) DO UPDATE SET \
            session_id = excluded.session_id, \
            input_id = excluded.input_id, \
            file_size = excluded.file_size, \
            file_mtime = excluded.file_mtime, \
            rows_written = excluded.rows_written, \
            ingested_at = excluded.ingested_at",
        params![
            key,
            session,
            input_id,
            file_size,
            file_mtime,
            rows,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::transcript_tokens::project_slug;

    const WORKSPACE: &str = "/home/ubuntu/GitHub/loom";

    fn assistant_line(id: &str, model: &str, ts: &str, input: i64, output: i64) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "gitBranch": "main",
            "message": {
                "model": model,
                "id": id,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": 1000,
                    "cache_creation_input_tokens": 100,
                },
            },
        })
        .to_string()
    }

    fn user_line(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    /// Seed `<projects>/<slug>/<uuid>.jsonl` plus one subagent transcript, in
    /// the layout Claude Code actually writes.
    fn seed(projects: &Path, uuid: &str, parent: &[String], subagent: &[String]) {
        let dir = projects.join(project_slug(Path::new(WORKSPACE)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{uuid}.jsonl")), parent.join("\n") + "\n").unwrap();
        if !subagent.is_empty() {
            let sub = dir.join(uuid).join("subagents");
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("agent-1.jsonl"), subagent.join("\n") + "\n").unwrap();
        }
    }

    fn opts(projects: &Path) -> IngestOptions {
        IngestOptions {
            projects_dir: projects.to_path_buf(),
            ..IngestOptions::default()
        }
    }

    fn open_db(dir: &Path) -> ActivityDb {
        ActivityDb::new(dir.join("activity.db")).unwrap()
    }

    fn count(db: &ActivityDb, sql: &str) -> i64 {
        db.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_dispatch_sweeps_transcripts_populate_resource_usage_and_the_cost_views() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[
                user_line("<command-name>/loom:sweep</command-name>\n<command-args>8059 --claim-owned 8059</command-args>"),
                assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
                // A streamed repeat, which must not be counted twice.
                assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:01Z", 10, 20),
                assistant_line("msg_syn", "<synthetic>", "2026-09-18T04:00:02Z", 9999, 9999),
            ],
            &[
                user_line("You are the Loom Builder (Development Worker) for this repository."),
                assistant_line("msg_2", "claude-opus-5", "2026-09-18T04:05:00Z", 30, 40),
            ],
        );

        let db = open_db(home.path());
        let stats = ingest(&db, &opts(&projects)).unwrap();

        assert_eq!(stats.transcripts_seen, 2, "parent session + one subagent");
        assert_eq!(stats.transcripts_ingested, 2);
        assert_eq!(stats.rows_written, 2, "one (model, day) row per transcript");
        assert_eq!(stats.duplicate_records, 1, "the streamed repeat collapsed");
        assert_eq!(stats.synthetic_skipped, 1);
        assert_eq!(stats.tokens_input, 40, "10 (deduped) + 30");
        assert_eq!(stats.tokens_output, 60);

        // The table that was structurally empty on every dispatch-driven host.
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 2);
        // The superseded table stays unwritten (see the module doc).
        assert_eq!(count(&db, "SELECT COUNT(*) FROM token_usage"), 0);

        // Role attribution reaches resource_usage through agent_inputs, which
        // is exactly how cost_by_role groups.
        let roles: Vec<(String, i64)> = db
            .conn
            .prepare("SELECT agent_role, request_count FROM cost_by_role ORDER BY agent_role")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(roles, vec![("builder".to_string(), 1), ("sweep".to_string(), 1)]);

        let months: Vec<String> = db
            .conn
            .prepare("SELECT month FROM cost_by_month")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(months, vec!["2026-09".to_string()]);

        // Repo and issue are recoverable from the anchor row's context.
        let context: String = db
            .conn
            .query_row("SELECT context FROM agent_inputs WHERE agent_role = 'sweep'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let context: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(context["repo"], "loom");
        assert_eq!(context["issue_number"], 8059);

        // Session id is recoverable from the anchor row's terminal_id.
        let terminals: Vec<String> = db
            .conn
            .prepare("SELECT terminal_id FROM agent_inputs ORDER BY terminal_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            terminals,
            vec![
                "transcript:uuid-a".to_string(),
                "transcript:uuid-a/agent-1".to_string()
            ]
        );
    }

    #[test]
    fn re_running_over_unchanged_transcripts_writes_nothing_new() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[
                user_line("<command-name>/loom:judge</command-name>"),
                assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
            ],
            &[],
        );

        let db = open_db(home.path());
        let first = ingest(&db, &opts(&projects)).unwrap();
        assert_eq!(first.rows_written, 1);

        let second = ingest(&db, &opts(&projects)).unwrap();
        assert_eq!(second.skipped_unchanged, 1);
        assert_eq!(second.rows_written, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM agent_inputs"), 1);

        // --force re-reads it, and still does not double-count.
        let forced = ingest(
            &db,
            &IngestOptions {
                force: true,
                ..opts(&projects)
            },
        )
        .unwrap();
        assert_eq!(forced.rows_written, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM agent_inputs"), 1);
        assert_eq!(
            count(&db, "SELECT SUM(tokens_input) FROM resource_usage"),
            10,
            "re-ingestion replaces rows rather than adding to them"
        );
    }

    #[test]
    fn a_still_growing_transcript_has_its_rows_replaced_not_appended() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        let lines = vec![
            user_line("<command-name>/loom:sweep</command-name>\n<command-args>42</command-args>"),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
        ];
        seed(&projects, "uuid-a", &lines, &[]);

        let db = open_db(home.path());
        ingest(&db, &opts(&projects)).unwrap();
        assert_eq!(count(&db, "SELECT SUM(tokens_input) FROM resource_usage"), 10);

        // The sweep continues and the transcript grows.
        let mut grown = lines;
        grown.push(assistant_line("msg_2", "claude-sonnet-5", "2026-09-18T04:10:00Z", 5, 5));
        seed(&projects, "uuid-a", &grown, &[]);

        let second = ingest(
            &db,
            &IngestOptions {
                force: true,
                ..opts(&projects)
            },
        )
        .unwrap();
        assert_eq!(second.rows_written, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 1);
        assert_eq!(
            count(&db, "SELECT SUM(tokens_input) FROM resource_usage"),
            15,
            "the whole file is re-read; the first pass's row is replaced"
        );
    }

    #[test]
    fn a_dry_run_reports_without_writing() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[assistant_line(
                "msg_1",
                "claude-sonnet-5",
                "2026-09-18T04:00:00Z",
                10,
                20,
            )],
            &[],
        );

        let db = open_db(home.path());
        let stats = ingest(
            &db,
            &IngestOptions {
                dry_run: true,
                ..opts(&projects)
            },
        )
        .unwrap();

        assert_eq!(stats.rows_written, 1, "it reports what it would write");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_ingest"), 0);
    }

    #[test]
    fn transcripts_outside_the_window_are_not_read() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[assistant_line(
                "msg_1",
                "claude-sonnet-5",
                "2026-09-18T04:00:00Z",
                10,
                20,
            )],
            &[],
        );

        let db = open_db(home.path());
        let stats = ingest(
            &db,
            &IngestOptions {
                since: Some(Utc::now() + chrono::Duration::hours(1)),
                ..opts(&projects)
            },
        )
        .unwrap();

        assert_eq!(stats.skipped_by_window, 1);
        assert_eq!(stats.rows_written, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 0);
    }

    #[test]
    fn a_transcript_with_no_usage_is_remembered_but_writes_no_anchor_row() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(&projects, "uuid-a", &[user_line("hello with no reply")], &[]);

        let db = open_db(home.path());
        let stats = ingest(&db, &opts(&projects)).unwrap();

        assert_eq!(stats.transcripts_without_usage, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM agent_inputs"), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 0);
        // Remembered, so the next pass does not re-parse it.
        assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_ingest"), 1);
        assert_eq!(ingest(&db, &opts(&projects)).unwrap().skipped_unchanged, 1);
    }

    #[test]
    fn workspace_scoping_ignores_other_projects() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[assistant_line(
                "msg_1",
                "claude-sonnet-5",
                "2026-09-18T04:00:00Z",
                10,
                20,
            )],
            &[],
        );
        // A second, unrelated project directory.
        let other = projects.join(project_slug(Path::new("/home/ubuntu/GitHub/other")));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("uuid-b.jsonl"),
            assistant_line("msg_9", "claude-sonnet-5", "2026-09-18T04:00:00Z", 77, 77) + "\n",
        )
        .unwrap();

        let db = open_db(home.path());
        let scoped = ingest(
            &db,
            &IngestOptions {
                workspace: Some(PathBuf::from(WORKSPACE)),
                ..opts(&projects)
            },
        )
        .unwrap();
        assert_eq!(scoped.transcripts_seen, 1);
        assert_eq!(count(&db, "SELECT SUM(tokens_input) FROM resource_usage"), 10);

        // Unscoped, both projects are ingested.
        let all = ingest(&db, &opts(&projects)).unwrap();
        assert_eq!(all.transcripts_seen, 2);
        assert_eq!(count(&db, "SELECT SUM(tokens_input) FROM resource_usage"), 87);
    }

    /// Every env var this file's `#[serial]` tests mutate — cleared before and
    /// after each one so they never leak into an unrelated test.
    fn clear_ingest_env() {
        for var in [
            "LOOM_TRANSCRIPT_INGEST",
            "LOOM_TRANSCRIPT_INGEST_INTERVAL",
            "LOOM_TRANSCRIPT_INGEST_WINDOW_HOURS",
        ] {
            std::env::remove_var(var);
        }
    }

    #[test]
    #[serial_test::serial]
    fn the_background_pass_is_on_by_default_issue_8477() {
        clear_ingest_env();
        assert!(
            resolve_enabled(&TranscriptIngestConfig::default()),
            "#8477: fleet cost history must not require a hand-set env var to survive Claude \
             Code's 30-day transcript retention fuse"
        );
        assert_eq!(
            resolve_settings(&TranscriptIngestConfig::default()),
            Some((900, 24)),
            "documented defaults"
        );
        clear_ingest_env();
    }

    #[test]
    #[serial_test::serial]
    fn env_explicit_off_overrides_the_default_on() {
        clear_ingest_env();
        std::env::set_var("LOOM_TRANSCRIPT_INGEST", "0");
        assert!(
            !resolve_enabled(&TranscriptIngestConfig::default()),
            "existing opt-out still works"
        );
        assert_eq!(resolve_settings(&TranscriptIngestConfig::default()), None);
        clear_ingest_env();
    }

    #[test]
    #[serial_test::serial]
    fn env_explicit_on_still_tunes_interval_and_window() {
        clear_ingest_env();
        std::env::set_var("LOOM_TRANSCRIPT_INGEST", "1");
        std::env::set_var("LOOM_TRANSCRIPT_INGEST_INTERVAL", "300");
        std::env::set_var("LOOM_TRANSCRIPT_INGEST_WINDOW_HOURS", "6");
        assert_eq!(resolve_settings(&TranscriptIngestConfig::default()), Some((300, 6)));
        clear_ingest_env();
    }

    #[test]
    #[serial_test::serial]
    fn config_can_opt_a_host_out_with_no_env_var_set() {
        clear_ingest_env();
        let config = TranscriptIngestConfig {
            enabled: Some(false),
            ..TranscriptIngestConfig::default()
        };
        assert!(!resolve_enabled(&config), "the config-tier opt-out this issue's AC requires");
        assert_eq!(resolve_settings(&config), None);
    }

    #[test]
    #[serial_test::serial]
    fn env_takes_precedence_over_a_conflicting_config_value() {
        clear_ingest_env();
        std::env::set_var("LOOM_TRANSCRIPT_INGEST", "1");
        let config = TranscriptIngestConfig {
            enabled: Some(false),
            ..TranscriptIngestConfig::default()
        };
        assert!(resolve_enabled(&config), "env > config");
        clear_ingest_env();
    }

    // The next two tests mutate `config_resolver::PRIVATE_DEFAULTS_ENV`, which
    // `config_resolver.rs`'s own tests already serialize under the *named*
    // `loom_config_env` key (see that file's comment on issue #6177: a bare
    // `#[serial]` does NOT exclude a `#[serial(loom_config_env)]` test, so
    // both sides must use the same named key or they race).

    #[test]
    #[serial_test::serial(loom_config_env)]
    fn read_transcript_ingest_config_soft_fails_on_a_repo_with_no_config_at_all() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempfile::tempdir().unwrap();
        let config = read_transcript_ingest_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(config, TranscriptIngestConfig::default());
    }

    #[test]
    #[serial_test::serial(loom_config_env)]
    fn read_transcript_ingest_config_reads_the_committed_block() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"autonomous": {"transcriptIngest": {"enabled": false, "intervalSecs": 120, "windowHours": 0}}}"#,
        )
        .unwrap();
        let config = read_transcript_ingest_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            config,
            TranscriptIngestConfig {
                enabled: Some(false),
                interval_secs: Some(120),
                window_hours: Some(0),
            }
        );
    }

    #[test]
    fn collect_health_status_reports_disabled_without_touching_disk() {
        let home = tempfile::tempdir().unwrap();
        let config = TranscriptIngestConfig {
            enabled: Some(false),
            ..TranscriptIngestConfig::default()
        };
        let status = collect_health_status(
            &home.path().join("activity.db"),
            &home.path().join("projects"),
            &config,
        );
        assert!(!status.enabled);
        assert_eq!(status.newest_ingested_age_hours, None);
        assert_eq!(status.newest_transcript_age_hours, None);
    }

    #[test]
    fn collect_health_status_reports_fresh_after_a_pass() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[assistant_line(
                "msg_1",
                "claude-sonnet-5",
                "2026-09-18T04:00:00Z",
                10,
                20,
            )],
            &[],
        );
        let db_path = home.path().join("activity.db");
        let db = open_db(home.path());
        ingest(&db, &opts(&projects)).unwrap();
        drop(db);

        let status = collect_health_status(&db_path, &projects, &TranscriptIngestConfig::default());
        assert!(status.enabled);
        let ingested_age = status
            .newest_ingested_age_hours
            .expect("a ledger row exists after ingest()");
        assert!(ingested_age < 1.0, "just ingested: {ingested_age}");
        assert!(status.newest_transcript_age_hours.is_some());
    }

    #[test]
    fn collect_health_status_is_none_when_nothing_was_ever_ingested() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        seed(
            &projects,
            "uuid-a",
            &[assistant_line(
                "msg_1",
                "claude-sonnet-5",
                "2026-09-18T04:00:00Z",
                10,
                20,
            )],
            &[],
        );
        let status = collect_health_status(
            &home.path().join("activity.db"),
            &projects,
            &TranscriptIngestConfig::default(),
        );
        assert!(status.enabled);
        assert_eq!(status.newest_ingested_age_hours, None, "no pass has run yet");
        assert!(status.newest_transcript_age_hours.is_some(), "the transcript is still on disk");
    }

    /// A transcript being written right now can carry an mtime a few seconds
    /// *ahead* of this process's clock — observed live on a fleet host as
    /// `newestTranscriptAgeHours: -0.0039`. An age is never negative.
    #[test]
    fn a_future_timestamp_reports_a_zero_age_not_a_negative_one() {
        let now = Utc::now();
        assert_eq!(age_hours(now + chrono::Duration::seconds(14), now), 0.0);
        assert_eq!(age_hours(now, now), 0.0);
        let past = age_hours(now - chrono::Duration::hours(3), now);
        assert!((past - 3.0).abs() < 0.01, "an ordinary past timestamp is unaffected: {past}");
    }
}
