//! Per-caller forge call accounting (Issue #9251, ADR-0021 amendment step 0).
//!
//! Before this, the only evidence of whether the ETag caches earn their free
//! `304`s was a `debug!` line on a hit; nothing counted the `200`s. This
//! module counts every instrumented forge call by **caller**, **pool** and
//! **outcome** so `loom-daemon status` can answer "is the cache not earning
//! its 304s, or do we just make too many calls?".
//!
//! # Two stores, because most callers are not the daemon
//!
//! An in-daemon counter alone would read zero for `loom-daemon serve`, the
//! short-lived `status`/`health` CLIs and agent `forge … --cached` processes.
//! So each [`record`] does two things:
//!
//! 1. bumps this process's since-start totals (bounded: one row per
//!    caller × pool), and
//! 2. appends ONE short JSON line to a **per-host append-only sink**
//!    (`${TMPDIR:-/tmp}/loom-forge-call-stats/calls-<epoch-hour>.jsonl`;
//!    `LOOM_FORGE_CALL_STATS_DIR` overrides, `off`/`0` disables). Each line is
//!    a single `O_APPEND` write far under `PIPE_BUF`, so concurrent writers
//!    never interleave. Files rotate hourly and anything older than
//!    [`RETAIN_HOURS`] is pruned when a new hour's file is created.
//!
//! `status` aggregates the sink's last [`WINDOW_SECS`] into the host-wide
//! window. Both stores are local file/memory writes: the accounting itself
//! makes **no forge call**, and a failure in it never fails the forge call.
//!
//! # Pool and the free budget reading
//!
//! GitHub sends `x-ratelimit-resource` / `-remaining` / `-reset` on every REST
//! response, `304`s included, and `gh api --include` exposes them. The pool is
//! taken from `x-ratelimit-resource` (defaulting to `core` for REST), and the
//! remaining/reset pair is kept as the latest **free** budget reading — unlike
//! [`crate::rate_limit_breaker`]'s budget, which is only probed after a trip.
//! A GraphQL caller (`gh issue/pr list` without `--cached`) prints no headers
//! and records its pool statically as [`Pool::Graphql`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::forge_listing::HttpResponse;
use crate::types::{ForgeBudgetReading, ForgeCallCounts, ForgeCallsStatus};

/// The rolling window `status` reports (the last hour).
pub const WINDOW_SECS: i64 = 3600;
/// Sink files older than this many hours are pruned.
const RETAIN_HOURS: i64 = 3;

/// How one forge call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A `2xx` answer — a full, budget-costing response.
    Ok,
    /// `304 Not Modified` — a free ETag hit.
    NotModified,
    /// Rate-limited (classified like the breaker does).
    RateLimited,
    /// Any other failure.
    Error,
}

/// The rate-limit pool a call spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pool {
    Core,
    Graphql,
    Search,
    Other,
}

impl Pool {
    /// Classify an `x-ratelimit-resource` value.
    #[must_use]
    pub fn from_resource(resource: &str) -> Self {
        match resource.trim().to_ascii_lowercase().as_str() {
            "core" => Pool::Core,
            "graphql" => Pool::Graphql,
            "search" => Pool::Search,
            _ => Pool::Other,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Pool::Core => "core",
            Pool::Graphql => "graphql",
            Pool::Search => "search",
            Pool::Other => "other",
        }
    }
}

/// The free `x-ratelimit-*` headers of one REST response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitHeaders {
    pub resource: Option<String>,
    pub remaining: Option<u64>,
    pub reset_epoch: Option<i64>,
}

impl RateLimitHeaders {
    /// Absorb one `name: value` header line if it is a rate-limit header.
    pub fn absorb(&mut self, name: &str, value: &str) {
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "x-ratelimit-resource" => self.resource = Some(value.to_string()),
            "x-ratelimit-remaining" => self.remaining = value.parse().ok(),
            "x-ratelimit-reset" => self.reset_epoch = value.parse().ok(),
            _ => {}
        }
    }
}

/// Classify one `gh api --include` result into `(pool, outcome)`. `gh` exits
/// non-zero on a `304`, so the status line — not the exit code — decides.
#[must_use]
pub fn classify(response: Option<&HttpResponse>, exit_ok: bool, stderr: &str) -> (Pool, Outcome) {
    let pool = response
        .and_then(|r| r.ratelimit.resource.as_deref())
        .map_or(Pool::Core, Pool::from_resource);
    let status = response.map(|r| r.status);
    let exhausted = response.is_some_and(|r| r.ratelimit.remaining == Some(0));
    let outcome = match status {
        Some(304) => Outcome::NotModified,
        Some(200..=299) if exit_ok => Outcome::Ok,
        _ if crate::rate_limit_breaker::indicates_rate_limit(stderr) => Outcome::RateLimited,
        Some(429) => Outcome::RateLimited,
        Some(403) if exhausted => Outcome::RateLimited,
        _ => Outcome::Error,
    };
    (pool, outcome)
}

/// Record one `gh api --include` call by `caller` (see [`classify`]).
pub fn record_gh_api(
    caller: &'static str,
    response: Option<&HttpResponse>,
    exit_ok: bool,
    stderr: &str,
) {
    let (pool, outcome) = classify(response, exit_ok, stderr);
    record(caller, pool, outcome, response.map(|r| &r.ratelimit));
}

/// Record one forge call. Never blocks or fails the caller: a poisoned lock
/// or an unwritable sink is silently skipped.
pub fn record(
    caller: &'static str,
    pool: Pool,
    outcome: Outcome,
    headers: Option<&RateLimitHeaders>,
) {
    let line = SinkLine {
        t: Utc::now().timestamp(),
        c: caller.to_string(),
        p: pool,
        o: outcome,
        rem: headers.and_then(|h| h.remaining),
        rst: headers.and_then(|h| h.reset_epoch),
    };
    if let Ok(mut state) = process_state().lock() {
        state.add(&line);
    }
    if let Some(dir) = sink_dir() {
        if let Err(e) = append(&dir, &line) {
            log::debug!("forge_call_stats: sink append to {} failed: {e}", dir.display());
        }
    }
}

// ============================================================================
// Aggregation
// ============================================================================

/// One sink line (short keys: ~7k lines/hour on a busy host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SinkLine {
    t: i64,
    c: String,
    p: Pool,
    o: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rem: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rst: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    ok: u64,
    not_modified: u64,
    rate_limited: u64,
    error: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    remaining: u64,
    reset_epoch: Option<i64>,
    observed_at: i64,
}

/// Counts per `(caller, pool)` plus the newest header reading per pool.
/// Bounded by the number of distinct callers × pools, never by call volume.
#[derive(Debug, Default)]
struct Aggregate {
    started_at: i64,
    counts: BTreeMap<(String, Pool), Counts>,
    latest: BTreeMap<Pool, Reading>,
}

impl Aggregate {
    fn add(&mut self, line: &SinkLine) {
        let c = self.counts.entry((line.c.clone(), line.p)).or_default();
        match line.o {
            Outcome::Ok => c.ok += 1,
            Outcome::NotModified => c.not_modified += 1,
            Outcome::RateLimited => c.rate_limited += 1,
            Outcome::Error => c.error += 1,
        }
        if let Some(remaining) = line.rem {
            let newer = self
                .latest
                .get(&line.p)
                .is_none_or(|r| r.observed_at <= line.t);
            if newer {
                let reading = Reading {
                    remaining,
                    reset_epoch: line.rst,
                    observed_at: line.t,
                };
                self.latest.insert(line.p, reading);
            }
        }
    }

    fn rows(&self) -> Vec<ForgeCallCounts> {
        self.counts
            .iter()
            .map(|((caller, pool), c)| ForgeCallCounts {
                caller: caller.clone(),
                pool: pool.as_str().to_string(),
                ok: c.ok,
                not_modified: c.not_modified,
                rate_limited: c.rate_limited,
                error: c.error,
            })
            .collect()
    }
}

/// Aggregate sink `lines` with `t >= since`; unparseable lines are skipped.
fn aggregate_lines<'a>(lines: impl Iterator<Item = &'a str>, since: i64) -> Aggregate {
    let mut agg = Aggregate {
        started_at: since,
        ..Aggregate::default()
    };
    for line in lines {
        if let Ok(parsed) = serde_json::from_str::<SinkLine>(line) {
            if parsed.t >= since {
                agg.add(&parsed);
            }
        }
    }
    agg
}

fn process_state() -> &'static Mutex<Aggregate> {
    static STATE: OnceLock<Mutex<Aggregate>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(Aggregate {
            started_at: Utc::now().timestamp(),
            ..Aggregate::default()
        })
    })
}

// ============================================================================
// Host sink
// ============================================================================

#[cfg(not(test))]
fn sink_dir() -> Option<PathBuf> {
    match std::env::var("LOOM_FORGE_CALL_STATS_DIR") {
        Ok(d) if d == "off" || d == "0" => None,
        Ok(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(crate::forge_etag_store::host_tmp_base().join("loom-forge-call-stats")),
    }
}

/// Test builds: no sink unless the current test thread opts in, so the many
/// fake-`gh` tests never write into a real host directory.
#[cfg(test)]
fn sink_dir() -> Option<PathBuf> {
    TEST_SINK_DIR.with(|d| d.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_SINK_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point THIS test thread's sink at `dir` (`None` = off).
#[cfg(test)]
pub(crate) fn set_test_sink_dir(dir: Option<PathBuf>) {
    TEST_SINK_DIR.with(|d| *d.borrow_mut() = dir);
}

fn sink_file(dir: &Path, hour: i64) -> PathBuf {
    dir.join(format!("calls-{hour}.jsonl"))
}

fn append(dir: &Path, line: &SinkLine) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(line)?;
    buf.push(b'\n');
    let hour = line.t.div_euclid(3600);
    let path = sink_file(dir, hour);
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true);
    let (mut file, fresh) = match opts.clone().create_new(true).open(&path) {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (opts.open(&path)?, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            (opts.create(true).open(&path)?, true)
        }
        Err(e) => return Err(e),
    };
    // One write of one short line: atomic under O_APPEND.
    file.write_all(&buf)?;
    if fresh {
        prune(dir, hour);
    }
    Ok(())
}

/// Remove sink files more than [`RETAIN_HOURS`] older than `current_hour`.
fn prune(dir: &Path, current_hour: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let hour = name
            .to_str()
            .and_then(|n| n.strip_prefix("calls-"))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .and_then(|n| n.parse::<i64>().ok());
        if hour.is_some_and(|h| h < current_hour - RETAIN_HOURS) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Aggregate the sink's last [`WINDOW_SECS`] as of `now` (missing hour files
/// simply contribute nothing).
fn read_window(dir: &Path, now: i64) -> Aggregate {
    let since = now - WINDOW_SECS;
    let mut raw = String::new();
    for hour in since.div_euclid(3600)..=now.div_euclid(3600) {
        if let Ok(text) = std::fs::read_to_string(sink_file(dir, hour)) {
            raw.push_str(&text);
        }
    }
    aggregate_lines(raw.lines(), since)
}

// ============================================================================
// Status
// ============================================================================

fn epoch(t: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(t, 0).single()
}

/// Build the `status` view as of `now`: host-wide window from the sink, this
/// process's since-start totals, and the newest budget reading per pool —
/// header-derived, or `breaker`'s probe when that is newer.
#[must_use]
pub fn status_report(
    now: DateTime<Utc>,
    breaker: Option<&crate::rate_limit_breaker::RateLimitSnapshot>,
) -> ForgeCallsStatus {
    let now_ts = now.timestamp();
    let window = sink_dir().map(|d| read_window(&d, now_ts));
    let (since_start, since, process_latest) = match process_state().lock() {
        Ok(s) => (s.rows(), epoch(s.started_at), s.latest.clone()),
        Err(_) => (Vec::new(), None, BTreeMap::new()),
    };
    let mut latest = process_latest;
    for (pool, r) in window.iter().flat_map(|w| w.latest.iter()) {
        if latest
            .get(pool)
            .is_none_or(|l| l.observed_at < r.observed_at)
        {
            latest.insert(*pool, *r);
        }
    }
    let mut budget: BTreeMap<Pool, ForgeBudgetReading> = latest
        .iter()
        .filter_map(|(pool, r)| {
            Some((
                *pool,
                ForgeBudgetReading {
                    pool: pool.as_str().to_string(),
                    remaining: r.remaining,
                    reset_at: r.reset_epoch.and_then(epoch),
                    observed_at: epoch(r.observed_at)?,
                    source: "headers".to_string(),
                },
            ))
        })
        .collect();
    if let Some(b) = breaker {
        if let Some(probed_at) = b.budget_probed_at {
            for (pool, remaining) in [
                (Pool::Core, b.core_remaining),
                (Pool::Graphql, b.graphql_remaining),
            ] {
                let Some(remaining) = remaining else { continue };
                if budget.get(&pool).is_none_or(|r| r.observed_at < probed_at) {
                    let reading = ForgeBudgetReading {
                        pool: pool.as_str().to_string(),
                        remaining,
                        reset_at: None,
                        observed_at: probed_at,
                        source: "breaker_probe".to_string(),
                    };
                    budget.insert(pool, reading);
                }
            }
        }
    }
    ForgeCallsStatus {
        window_secs: WINDOW_SECS.unsigned_abs(),
        host_window: window.map(|w| w.rows()),
        since_start,
        since,
        budget: budget.into_values().collect(),
    }
}

#[cfg(test)]
#[path = "forge_call_stats_tests.rs"]
mod tests;
