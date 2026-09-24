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
use super::session_analysis::build_session_analysis;
use super::session_summary::build_session_summary;
use super::transcript_parse::{parse_transcript, ParsedTranscript};
use crate::observability::session_analysis::SessionAnalysisSink;
use crate::observability::session_summary::SessionSummarySink;
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
    /// Where this pass's `session.summary` telemetry records go (Issue
    /// #8757). `None` (a CLI pass, a test, or observability disabled) skips
    /// emission entirely — the pass still writes `activity.db` exactly as
    /// before.
    pub summary_sink: Option<SessionSummarySink>,
    /// Where this pass's derived `session.analysis` telemetry records go
    /// (Issue #8760). `None` (a CLI pass, a test, or observability
    /// disabled) skips emission entirely, independent of `summary_sink` —
    /// a config that supplies one but not the other still gets exactly the
    /// record kind(s) it asked for.
    pub analysis_sink: Option<SessionAnalysisSink>,
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
            summary_sink: None,
            analysis_sink: None,
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
    /// `session.summary` records pushed onto the observability queue
    /// (Issue #8757). Zero whenever no sink is configured.
    pub session_summaries: usize,
    /// `session.analysis` records pushed onto the observability queue
    /// (Issue #8760). Zero whenever no sink is configured.
    pub session_analyses: usize,
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
            // One `session.summary` per transcript that contributed rows,
            // emitted only after its rows persisted (Issue #8757). A grown
            // transcript is re-summarized on each pass that re-reads it —
            // the last record is the complete one, mirroring the
            // replace-never-append semantics of the rows it summarizes.
            //
            // `session.analysis` (Issue #8760) rides the same emission
            // point, derived from the summary just built plus the same
            // `parsed` transcript — independent of `summary_sink`, so a
            // config that only wants one of the two record kinds gets
            // exactly that.
            if opts.summary_sink.is_some() || opts.analysis_sink.is_some() {
                let summary = build_session_summary(&path, &parsed);
                if let Some(sink) = &opts.analysis_sink {
                    sink.push(build_session_analysis(&summary, &parsed));
                    stats.session_analyses += 1;
                }
                if let Some(sink) = &opts.summary_sink {
                    sink.push(summary);
                    stats.session_summaries += 1;
                }
            }
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
        // The background pass runs inside the daemon process, where
        // `observability::spawn_task` registered the shared queue at boot
        // (well before this pass's first tick); `None` there means
        // observability is off and no `session.summary` is emitted.
        summary_sink: crate::observability::session_summary::global_session_summary_sink().cloned(),
        // Same registration pattern, one slice later (Issue #8760).
        analysis_sink: crate::observability::session_analysis::global_session_analysis_sink()
            .cloned(),
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
mod tests;
