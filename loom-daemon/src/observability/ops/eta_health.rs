//! Per-host ETA pipeline health gauges (Issue #10391, slice 2).
//!
//! The fit loop and the fleet refresh loop each run on their own task and can
//! go silent without a log line (a stood-down worker, a stalled cycle, a dead
//! export path). The collector's 5-minute pass therefore reads a small
//! process-global [`EtaHealth`] that those tasks update, plus the local fit
//! and snapshot files, and exports `loom.eta.health.*` gauges through
//! `metric.points`. The gauges stay alive when no `eta.fleet_refresh` record
//! is emitted, which is exactly the stand-down case.
//!
//! Rules: an unmeasurable reading emits **no point** (unknown is not zero);
//! labels are closed vocabularies (`kind`, `heuristic`, `reason`, `state`)
//! plus `repo` on the per-snapshot age. Never an issue number, sha or path.
//!
//! [`points`] is pure over [`Facts`]; [`record`] gathers the facts and emits.
//! The gather/export path takes the health state as a parameter: production
//! hands it a snapshot of the one global, tests build their own and never
//! touch the global (other modules' tests write it through the `note_*` seams).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::eta::score::EstimateSummary;
use crate::telemetry::ops::{MetricName, MetricPoint};

/// What the long-running tasks last reported. Updated by the refresh task
/// ([`note_tick`]), the fit check ([`note_fit_check`]) and the snapshot
/// builder ([`note_snapshot`]); read by [`record`].
#[derive(Debug, Default, Clone)]
pub struct EtaHealth {
    /// The last refresh tick: when it started and its gate state.
    tick: Option<(DateTime<Utc>, String)>,
    /// Repos per stop reason in the last tick that refreshed.
    refresh_repos: BTreeMap<String, u64>,
    /// The last fit check: when it started and its outcome or skip reason.
    fit_check: Option<(DateTime<Utc>, String)>,
    /// Rows, and rows with alternates, of the last built `eta.snapshot`.
    snapshot_rows: Option<(u64, u64)>,
    /// Pending estimates the cap evicted since start; `None` before a pass.
    pending_over_cap: Option<u64>,
    /// Cached fleet snapshot `as_of`, keyed by file, re-read on an mtime change.
    ages: BTreeMap<PathBuf, (SystemTime, String, DateTime<Utc>)>,
    /// Series keys already exported, so a bucket that empties is zeroed
    /// rather than left at its last nonzero value on the backend.
    emitted_items: BTreeSet<ItemKey>,
    emitted_reasons: BTreeSet<String>,
}

/// `(kind, heuristic, reason)` of one items bucket.
type ItemKey = (String, String, String);

/// The closed refresh-gate vocabulary: every state is emitted each pass
/// (one at 1, the rest at 0) so a transition never leaves two states active.
const GATE_STATES: [&str; 4] = ["captain", "stand_down", "no_captain", "disabled"];

static HEALTH: Mutex<Option<EtaHealth>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut EtaHealth) -> T) -> T {
    let mut guard = HEALTH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(EtaHealth::default))
}

impl EtaHealth {
    /// Record a refresh tick (stand-down ticks included). The per-reason repo
    /// counts are kept from the last tick that actually refreshed.
    pub fn tick(&mut self, state: &crate::eta::health::RefreshCycleState) {
        self.tick = Some((state.started_at, state.gate.clone()));
        if state.gate != "stand_down" {
            self.refresh_repos = state.stop_reasons.clone();
        }
    }

    /// Record a fit check: its start and its outcome (or skip reason).
    pub fn fit_check(&mut self, started_at: DateTime<Utc>, reason: &str) {
        self.fit_check = Some((started_at, reason.to_string()));
    }

    /// Record the last built `eta.snapshot`: its rows and those with alternates.
    pub fn snapshot(&mut self, rows: u64, alternates_rows: u64) {
        self.snapshot_rows = Some((rows, alternates_rows));
    }
}

/// Add `n` cap-evicted pending estimates to the cumulative count (#10496).
pub fn note_over_cap(n: usize) {
    with(|h| {
        let total = h.pending_over_cap.unwrap_or(0);
        h.pending_over_cap = Some(total.saturating_add(n as u64));
    });
}

/// Record a refresh tick in the global state ([`EtaHealth::tick`]).
pub fn note_tick(state: &crate::eta::health::RefreshCycleState) {
    with(|h| h.tick(state));
}

/// Record a fit check in the global state ([`EtaHealth::fit_check`]).
pub fn note_fit_check(started_at: DateTime<Utc>, reason: &str) {
    with(|h| h.fit_check(started_at, reason));
}

/// Record the last built `eta.snapshot` in the global state
/// ([`EtaHealth::snapshot`]).
pub fn note_snapshot(rows: u64, alternates_rows: u64) {
    with(|h| h.snapshot(rows, alternates_rows));
}

/// Live items per `(kind, heuristic, reason)`: the newest estimate per
/// `(repo, issue, kind, heuristic)`, `reason` being `answered` or its
/// `no_estimate_reason`. Pure.
#[must_use]
pub fn buckets(pending: &[EstimateSummary]) -> BTreeMap<(String, String, String), u64> {
    let mut newest: BTreeMap<(&str, u32, &str, &str), &EstimateSummary> = BTreeMap::new();
    for p in pending {
        let key = (p.repo.as_str(), p.issue, p.kind.as_str(), p.heuristic.as_str());
        if newest.get(&key).is_none_or(|n| n.as_of < p.as_of) {
            newest.insert(key, p);
        }
    }
    let mut out = BTreeMap::new();
    for p in newest.into_values() {
        let reason = p.no_estimate_reason.map_or("answered", |r| r.as_str());
        *out.entry((p.kind.as_str().to_string(), p.heuristic.clone(), reason.to_string()))
            .or_default() += 1;
    }
    out
}

/// Everything one pass reads. `None` / empty = unmeasurable.
#[derive(Debug, Default)]
pub struct Facts {
    pub now: DateTime<Utc>,
    /// `None` when ETA is disabled (no tracker).
    pub items: Option<BTreeMap<(String, String, String), u64>>,
    /// The loaded coefficient file's cutoff, when one is loaded.
    pub fit_cutoff: Option<DateTime<Utc>>,
    pub fit_check: Option<(DateTime<Utc>, String)>,
    pub snapshots: Vec<(String, DateTime<Utc>)>,
    /// The refresh gate state, when known.
    pub gate: Option<String>,
    pub last_tick: Option<DateTime<Utc>>,
    pub refresh_repos: BTreeMap<String, u64>,
    pub snapshot_rows: Option<(u64, u64)>,
    /// Cumulative cap evictions; `None` before the first ETA pass.
    pub pending_over_cap: Option<u64>,
    /// Items bucket keys exported on earlier passes; zeroed when absent now.
    pub prev_items: BTreeSet<ItemKey>,
    /// Refresh stop reasons exported on earlier passes; zeroed when absent now.
    pub prev_reasons: BTreeSet<String>,
}

fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> i64 {
    (now - then).num_seconds().max(0)
}

fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The gauges for `facts`. Pure.
#[must_use]
pub fn points(facts: &Facts) -> Vec<MetricPoint> {
    let mut out = Vec::new();
    // A measured pass (`items` is `Some`) zeroes every previously exported
    // bucket it no longer holds; an unknown pass (`None`) emits nothing.
    if let Some(items) = &facts.items {
        let zeroed = facts.prev_items.iter().filter(|k| !items.contains_key(*k));
        let zeroed = zeroed.map(|k| (k, 0));
        for ((kind, heuristic, reason), n) in items.iter().map(|(k, n)| (k, *n)).chain(zeroed) {
            out.push(
                MetricPoint::int(MetricName::EtaHealthItems, count(n))
                    .label("kind", kind)
                    .label("heuristic", heuristic)
                    .label("reason", reason),
            );
        }
    }
    out.push(MetricPoint::int(
        MetricName::EtaHealthFitLoaded,
        i64::from(facts.fit_cutoff.is_some()),
    ));
    if let Some(cutoff) = facts.fit_cutoff {
        out.push(MetricPoint::int(MetricName::EtaHealthFitAgeSeconds, age(facts.now, cutoff)));
    }
    if let Some((at, reason)) = &facts.fit_check {
        out.push(
            MetricPoint::int(MetricName::EtaHealthFitCheckAgeSeconds, age(facts.now, *at))
                .label("reason", reason),
        );
    }
    for (repo, as_of) in &facts.snapshots {
        out.push(
            MetricPoint::int(MetricName::EtaHealthSnapshotAgeSeconds, age(facts.now, *as_of))
                .label("repo", repo),
        );
    }
    if let Some(gate) = &facts.gate {
        let others = GATE_STATES.iter().copied().filter(|s| s != gate);
        for state in std::iter::once(gate.as_str()).chain(others) {
            let on = i64::from(state == gate);
            out.push(MetricPoint::int(MetricName::EtaHealthRefreshGate, on).label("state", state));
        }
    }
    if let Some(at) = facts.last_tick {
        out.push(MetricPoint::int(
            MetricName::EtaHealthRefreshLastCycleAgeSeconds,
            age(facts.now, at),
        ));
    }
    let gone = facts
        .prev_reasons
        .iter()
        .filter(|r| !facts.refresh_repos.contains_key(*r));
    let reasons = facts
        .refresh_repos
        .iter()
        .map(|(r, n)| (r, *n))
        .chain(gone.map(|r| (r, 0)));
    for (reason, n) in reasons {
        out.push(
            MetricPoint::int(MetricName::EtaHealthRefreshRepos, count(n)).label("reason", reason),
        );
    }
    if let Some((rows, alternates)) = facts.snapshot_rows {
        out.push(MetricPoint::int(MetricName::EtaHealthSnapshotRows, count(rows)));
        out.push(MetricPoint::int(MetricName::EtaHealthSnapshotAlternatesRows, count(alternates)));
    }
    if let Some(n) = facts.pending_over_cap {
        out.push(MetricPoint::int(MetricName::EtaHealthPendingOverCap, count(n)));
    }
    out
}

/// `as_of` of every fleet snapshot file, re-parsing only a file whose mtime
/// changed since the last pass (the files carry whole sample sets).
fn snapshot_ages(
    root: &Path,
    cache: &mut BTreeMap<PathBuf, (SystemTime, String, DateTime<Utc>)>,
) -> Vec<(String, DateTime<Utc>)> {
    let dir = crate::eta::fleet::snapshot_dir(root);
    let mut seen = Vec::new();
    let paths = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"));
    for path in paths {
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        if cache.get(&path).is_none_or(|(t, _, _)| *t != mtime) {
            match crate::eta::fleet::read(&path) {
                Some(s) => {
                    cache.insert(path.clone(), (mtime, s.repo, s.as_of));
                }
                None => {
                    cache.remove(&path);
                }
            }
        }
        seen.push(path);
    }
    cache.retain(|p, _| seen.contains(p));
    cache
        .values()
        .map(|(_, repo, as_of)| (repo.clone(), *as_of))
        .collect()
}

/// Gather the facts for `root` as of `now` from `health` (whose snapshot
/// `as_of` cache this pass refreshes). Blocking (file reads).
fn gather(root: &Path, host_id: &str, now: DateTime<Utc>, health: &mut EtaHealth) -> Facts {
    let eta = crate::eta::config::read(root);
    let snapshots = snapshot_ages(root, &mut health.ages);
    let (tick, repos, fit_check, snapshot_rows) = (
        health.tick.clone(),
        health.refresh_repos.clone(),
        health.fit_check.clone(),
        health.snapshot_rows,
    );
    // Before the first tick the gate is what the read-only resolver says it
    // would be (it never arms the singleton job), or `disabled` when the
    // task does not run at all.
    let gate = match &tick {
        Some((_, gate)) => gate.clone(),
        None if !super::super::eta_fleet_refresh::should_spawn(&eta) => "disabled".into(),
        None => match crate::fleet_captain::resolve_gate_for_root(root, host_id) {
            crate::fleet_captain::CaptainGate::Armed { .. } => "captain",
            crate::fleet_captain::CaptainGate::Refused { .. } => "stand_down",
            crate::fleet_captain::CaptainGate::NoCaptainDeclared => "no_captain",
        }
        .into(),
    };
    Facts {
        now,
        items: super::super::eta::health_items(),
        fit_cutoff: crate::eta::fit::coeffs::load_latest(root, now).map(|f| f.as_of),
        fit_check,
        snapshots,
        gate: Some(gate),
        last_tick: tick.map(|(at, _)| at),
        refresh_repos: repos,
        snapshot_rows,
        pending_over_cap: health.pending_over_cap,
        prev_items: health.emitted_items.clone(),
        prev_reasons: health.emitted_reasons.clone(),
    }
}

/// Gather the facts from `health` and emit the gauges through the global
/// path (so a test can [`super::capture::capture`] them). Blocking (file reads).
fn export(root: &Path, host_id: &str, now: DateTime<Utc>, health: &mut EtaHealth) {
    let facts = gather(root, host_id, now, health);
    let out = points(&facts);
    // Remember what this pass exported; an unknown reading keeps the memory
    // so the series is still zeroed once it is measurable again.
    if let Some(items) = facts.items {
        health.emitted_items = items.into_keys().collect();
    }
    health.emitted_reasons = facts.refresh_repos.into_keys().collect();
    super::emit_metrics(out);
}

/// One collector pass over the global state: snapshot it (taking the `as_of`
/// cache rather than cloning it), export outside the lock, then hand the
/// refreshed cache back. A writer landing mid-pass is kept; only the cache
/// slot is replaced.
fn export_global(root: &Path, host_id: &str, now: DateTime<Utc>) {
    let mut health = with(|h| {
        let ages = std::mem::take(&mut h.ages);
        EtaHealth { ages, ..h.clone() }
    });
    export(root, host_id, now, &mut health);
    with(|h| {
        h.ages = health.ages;
        h.emitted_items = health.emitted_items;
        h.emitted_reasons = health.emitted_reasons;
    });
}

/// Gather and export the gauges, when an ops sink is registered.
pub async fn record(root: &Path) {
    if super::global_ops_sink().is_none() {
        return;
    }
    // The same identity the refresh tick's captain gate resolves against, so
    // the pre-first-tick gate matches what the first tick will report.
    let (root, host_id) = (root.to_path_buf(), crate::sweep_registry::host_identity());
    let _ = tokio::task::spawn_blocking(move || export_global(&root, &host_id, Utc::now())).await;
}

#[cfg(test)]
#[path = "eta_health_tests.rs"]
mod tests;
