//! The fleet snapshot refresh task (#10263): keeps every registered repo's
//! fleet snapshot fresh, on its own cadence, ahead of the daily fit.
//!
//! # Why a task, and why here
//!
//! `land-2026-10-04-twin-otter` (#10223) trains daily on the fleet snapshots
//! alone, and `historyScope = augment` reads them on every estimate. Without a
//! daemon-side writer they were only ever as fresh as the last manual
//! `eta fleet backfill|refresh`, so a fleet host's fit had nothing to train on.
//!
//! The task is spawned beside `eta::spawn_task` and inherits its gate (an
//! observability exporter configured, `autonomous.eta.enabled`), plus its own
//! `autonomous.eta.fleetRefresh.enabled` (default on). It never runs on the
//! ETA pass or the work-finder tick and never takes the ETA state lock: one
//! `tokio::time::interval` (`intervalSecs`, first cycle [`FIRST_CYCLE_DELAY`]
//! after spawn, missed ticks skipped), each cycle one `spawn_blocking` call
//! under `catch_unwind`, so a panic is a `warn` and the next tick retries.
//!
//! # One cycle
//!
//! 1. **Repo set** ([`repo_targets`]): every provisioned root and the daemon's
//!    own, by their `origin` remote, plus every published snapshot's repo.
//!    Deleting a snapshot file stops refreshing only a snapshot-only repo: a
//!    provisioned root or the daemon's own repo gets a full backfill next
//!    cycle instead. To opt out, turn `fleetRefresh.enabled` off or remove the
//!    root from the workspace pool.
//!    Each gets its reader App or a zero-call skip (`no_reader`,
//!    `unsupported_forge`; warned once per daemon lifetime).
//! 2. **Gates**: inside a rate-limit backoff, or with the breaker suppressed,
//!    every repo is recorded (`backoff` / `breaker_open`) and no call is made;
//!    a due backfill is recorded as in progress, so it holds the fit (#10292).
//! 3. **Snapshots**: [`crate::eta::fleet_refresh::run_cycle`] under the two
//!    budgets and the reserve floor.
//! 4. **Raw events** (#10250): each repo's issue-events cache is synced
//!    in-process through a reader-only source, from what the matching budget
//!    has left. After the snapshots, and never ahead of the fit.
//! 5. **Fit** ([`after_cycle`]): #10245's daily fit check, in the same blocking
//!    call, so the fit always sees this cycle's snapshots. It is held while a
//!    backfill younger than six hours is in progress, and skipped when
//!    `autonomous.eta.fit.enabled` is off. With this task on, #10245's
//!    standalone refit task is not spawned ([`owns_fit`]).
//! 6. **Telemetry**: one `eta.fleet_refresh` record per repo, and one `info`
//!    line with the stop-reason counts and the calls spent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::config::{EtaConfig, FleetRefreshConfig};
use crate::eta::fit::{run, Fitter};
use crate::eta::fleet;
use crate::eta::fleet_events::{self, EventLog, EventsCursor, SyncMode};
use crate::eta::fleet_events_forge::{ForgeEndpoint, ForgeEventSource};
use crate::eta::fleet_fetch::{ForgeRead, NoReader, Reader, ReaderForge, RepoTarget};
use crate::eta::fleet_refresh::{self, Budgets, CycleReport, RepoReport, StopReason};
use crate::eta::Provenance;
use crate::telemetry::kinds::eta_fleet_refresh::EtaFleetRefreshRecord;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use crate::workspace_pool::WorkspacePool;

/// The first cycle runs this long after spawn, so a daemon start is not also a
/// burst of forge reads.
pub const FIRST_CYCLE_DELAY: Duration = Duration::from_secs(120);

/// Whether the task runs at all for this configuration.
#[must_use]
pub fn should_spawn(config: &EtaConfig) -> bool {
    config.enabled && config.fleet_refresh.enabled
}

/// Whether this task owns the daily fit check (#10245): with fleet refresh on,
/// the fit runs at the end of every refresh cycle ([`after_cycle`]), so the
/// standalone fit task must not also be spawned — two loops calling the same
/// check would only race.
#[must_use]
pub fn owns_fit(config: &EtaConfig) -> bool {
    should_spawn(config)
}

/// State carried between cycles. In memory: a restart forgets a backoff, and
/// the first cycle after it meets a still-withdrawn reader as `no_reader`.
#[derive(Debug, Default)]
pub struct TaskState {
    /// No forge call before this instant (a reader App was rate limited).
    pub backoff_until: Option<DateTime<Utc>>,
    /// Repos already warned about having no usable reader.
    warned: BTreeSet<String>,
}

/// Start the task. `None` when it is configured off.
pub fn spawn_task(
    workspace_root: PathBuf,
    workspace_pool: Arc<WorkspacePool>,
    otlp_queues: Vec<Arc<DurableQueue>>,
    host_id: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let eta = crate::eta::config::read(&workspace_root);
    if !should_spawn(&eta) {
        log::info!(
            "eta fleet refresh: disabled (autonomous.eta.enabled={}, fleetRefresh.enabled={})",
            eta.enabled,
            eta.fleet_refresh.enabled
        );
        return None;
    }
    let config = eta.fleet_refresh.clone();
    let fit_enabled = eta.fit_enabled;
    let fitter = run::current_fitter();
    let sink: Option<Arc<dyn QueueSink>> = (!otlp_queues.is_empty())
        .then(|| Arc::new(FanoutQueue::new(otlp_queues)) as Arc<dyn QueueSink>);
    log::info!(
        "eta fleet refresh: enabled (every {}s, budgets refresh={} backfill={}, reserve={}, \
         backfill {}d)",
        config.interval_secs,
        config.max_calls_per_cycle,
        config.backfill_max_calls_per_cycle,
        config.reserve_calls,
        config.backfill_days
    );
    let task = Arc::new(Mutex::new(TaskState::default()));
    Some(tokio::spawn(async move {
        tokio::time::sleep(FIRST_CYCLE_DELAY).await;
        let mut interval = tokio::time::interval(Duration::from_secs(config.interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut roots = super::collector::provisioned_roots(&workspace_pool);
            roots.push(workspace_root.clone());
            let (root, config, host_id, sink, task, fitter) = (
                workspace_root.clone(),
                config.clone(),
                host_id.clone(),
                sink.clone(),
                task.clone(),
                fitter.clone(),
            );
            let joined = tokio::task::spawn_blocking(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut task = task
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    run_production_cycle(
                        &root,
                        &roots,
                        &config,
                        &host_id,
                        sink.as_deref(),
                        &mut task,
                        (fit_enabled, &fitter),
                    );
                }))
            })
            .await;
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(_)) => log::warn!("eta fleet refresh: cycle panicked; retrying next tick"),
                Err(e) => {
                    log::warn!("eta fleet refresh: cycle failed to run: {e}; retrying next tick")
                }
            }
        }
    }))
}

/// One production cycle: the real repo set, reader resolution and forge.
fn run_production_cycle(
    root: &Path,
    roots: &[PathBuf],
    config: &FleetRefreshConfig,
    host_id: &str,
    sink: Option<&dyn QueueSink>,
    task: &mut TaskState,
    (fit_enabled, fitter): (bool, &Fitter),
) {
    let targets = repo_targets(
        root,
        roots,
        |r| crate::forge_etag_store::remote_identity(r),
        |repo, host| {
            crate::forge_identity::read_credential(repo, host)
                .map(|(dir, app_id)| Reader { app_id, dir })
        },
    );
    let mut forge = ReaderForge::new();
    let mut events = |target: &RepoTarget, reader: &Reader, left: u64, mode: SyncMode| {
        sync_events(root, target, reader, left, mode, config.reserve_calls)
    };
    let outcome = cycle(root, &targets, &mut forge, &mut events, config, task, Utc::now());
    let loom = Provenance::current();
    for record in records(&outcome.report, host_id, outcome.started_at, &loom) {
        if !record.has_provenance() {
            log::warn!("eta fleet refresh: dropped record for {}: invalid provenance", record.repo);
            continue;
        }
        if let Some(sink) = sink {
            sink.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::EtaFleetRefresh(record)));
        }
    }
    // After the records: a fit that panics must not cost the cycle's telemetry.
    after_cycle(root, Utc::now(), outcome.fit_held, fit_enabled, fitter);
}

/// The repo set, deduplicated by lowercase slug: provisioned roots first (so a
/// repo's own checkout is its `cwd`), then `workspace_root`, then every
/// published snapshot's repo.
pub fn repo_targets(
    workspace_root: &Path,
    roots: &[PathBuf],
    identity: impl Fn(&Path) -> Option<(String, String)>,
    reader: impl Fn(&str, Option<&str>) -> Option<Reader>,
) -> Vec<RepoTarget> {
    let mut seen: BTreeMap<String, (Option<String>, PathBuf, String)> = BTreeMap::new();
    for root in roots.iter().map(PathBuf::as_path).chain([workspace_root]) {
        if let Some((host, slug)) = identity(root) {
            seen.entry(slug.to_ascii_lowercase())
                .or_insert((Some(host), root.to_path_buf(), slug));
        }
    }
    for snapshot in fleet::load_all(workspace_root) {
        seen.entry(snapshot.repo.to_ascii_lowercase()).or_insert((
            None,
            workspace_root.to_path_buf(),
            snapshot.repo.clone(),
        ));
    }
    seen.into_values()
        .map(|(host, cwd, repo)| {
            let github = host
                .as_deref()
                .is_none_or(|h| h.eq_ignore_ascii_case("github.com"));
            let resolved = if github {
                reader(&repo, host.as_deref()).ok_or(NoReader::NoReader)
            } else {
                Err(NoReader::UnsupportedForge)
            };
            RepoTarget {
                repo,
                host,
                cwd,
                reader: resolved,
            }
        })
        .collect()
}

/// What [`cycle`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleOutcome {
    pub started_at: DateTime<Utc>,
    pub report: CycleReport,
    /// The fit hold rule fired: a backfill is in progress and younger than
    /// [`fleet_refresh::FIT_HOLD_HOURS`].
    pub fit_held: bool,
}

/// The raw-event sync seam: `(target, reader, calls left, mode)` →
/// `(rows appended, calls spent, stop)`. Production is [`sync_events`].
pub type EventsSync<'a> =
    dyn FnMut(&RepoTarget, &Reader, u64, SyncMode) -> (u64, u64, Option<StopReason>) + 'a;

/// One cycle over `targets`: gates, snapshots, raw events. Testable with a
/// fake forge and events seam; the fit call and the record emission are the
/// caller's.
pub fn cycle(
    root: &Path,
    targets: &[RepoTarget],
    forge: &mut dyn ForgeRead,
    events: &mut EventsSync<'_>,
    config: &FleetRefreshConfig,
    task: &mut TaskState,
    now: DateTime<Utc>,
) -> CycleOutcome {
    for target in targets {
        if let Err(why) = target.reader {
            if task.warned.insert(target.repo.to_ascii_lowercase()) {
                log::warn!(
                    "eta fleet refresh: {} skipped ({}): no reader App can read it, and this \
                     task never reads as the writer — logged once per daemon lifetime",
                    target.repo,
                    match why {
                        NoReader::NoReader => "no_reader",
                        NoReader::UnsupportedForge => "unsupported_forge",
                    }
                );
            }
        }
    }
    let gate = if task.backoff_until.is_some_and(|until| now < until) {
        Some(StopReason::Backoff)
    } else if forge.breaker_open() {
        Some(StopReason::BreakerOpen)
    } else {
        None
    };
    let report = if let Some(stop) = gate {
        log::info!(
            "eta fleet refresh: {} — no forge calls this cycle",
            if stop == StopReason::Backoff {
                format!("backing off until {:?}", task.backoff_until)
            } else {
                "rate-limit breaker open".to_string()
            }
        );
        // A due backfill skipped whole still holds the fit (#10292).
        fleet_refresh::pend_due_backfills(root, targets, now, config.backfill_days, stop);
        CycleReport {
            repos: targets.iter().map(|t| gated(root, t, stop)).collect(),
            remaining: (config.max_calls_per_cycle, config.backfill_max_calls_per_cycle),
            backfill_in_progress_since: fleet_refresh::backfill_since(root, targets),
            ..CycleReport::default()
        }
    } else {
        let budgets = Budgets {
            refresh: config.max_calls_per_cycle,
            backfill: config.backfill_max_calls_per_cycle,
            reserve: config.reserve_calls,
            backfill_days: config.backfill_days,
        };
        let mut report = fleet_refresh::run_cycle(root, targets, forge, budgets, now);
        sync_all_events(root, targets, events, &mut report);
        report
    };
    if let Some(reset) = report.rate_limited {
        let reset = reset.and_then(|s| Utc.timestamp_opt(s, 0).single());
        let floor = now + chrono::Duration::seconds(config.interval_secs as i64);
        task.backoff_until = Some(reset.map_or(floor, |r| r.max(floor)));
    }
    let fit_held = fleet_refresh::fit_held(report.backfill_in_progress_since, now);
    log_summary(&report);
    CycleOutcome {
        started_at: now,
        report,
        fit_held,
    }
}

fn gated(root: &Path, target: &RepoTarget, stop: StopReason) -> RepoReport {
    let published = fleet::read(&fleet::snapshot_path(root, &target.repo));
    let mut r = RepoReport {
        repo: target.repo.clone(),
        pass: None,
        stop,
        promoted: false,
        prs_read: 0,
        pass_done: 0,
        timelines_incomplete: 0,
        samples_added: 0,
        forge_calls: 0,
        not_modified_calls: 0,
        ratelimit_remaining_min: None,
        reader_app: target.reader.as_ref().ok().map(|r| r.app_id.clone()),
        snapshot_id: published.as_ref().map(|s| s.snapshot_id.clone()),
        as_of: published.as_ref().map(|s| s.as_of),
        raw_events_added: None,
        duration_ms: 0,
    };
    match target.reader {
        Err(NoReader::NoReader) => r.stop = StopReason::NoReader,
        Err(NoReader::UnsupportedForge) => r.stop = StopReason::UnsupportedForge,
        Ok(_) => {}
    }
    r
}

/// Sync each repo's raw event cache from what the matching budget has left.
/// Skipped for a repo whose snapshot pass hit the reserve, a coverage gap or a
/// rate limit, and for every repo once one sync is rate limited.
fn sync_all_events(
    root: &Path,
    targets: &[RepoTarget],
    events: &mut EventsSync<'_>,
    report: &mut CycleReport,
) {
    let halted = report.repos.iter().any(|r| {
        matches!(r.stop, StopReason::RateLimited | StopReason::BreakerOpen | StopReason::Shutdown)
    });
    if halted {
        return;
    }
    let mut reserve_apps: BTreeSet<String> = report
        .repos
        .iter()
        .filter(|r| r.stop == StopReason::Reserve)
        .filter_map(|r| r.reader_app.clone())
        .collect();
    for target in targets {
        let Ok(reader) = &target.reader else { continue };
        if reserve_apps.contains(&reader.app_id) {
            continue;
        }
        let Some(repo_report) = report.repos.iter_mut().find(|r| r.repo == target.repo) else {
            continue;
        };
        if repo_report.stop == StopReason::Coverage {
            continue;
        }
        let cursor =
            EventsCursor::read(&fleet_events::cursor_path(root, &target.repo), &target.repo);
        let complete = cursor
            .endpoints
            .get(&format!(
                "{}:{}",
                fleet_events::SOURCE_FORGE,
                crate::eta::fleet_events_forge::ENDPOINT
            ))
            .is_some_and(|e| e.backfill_complete);
        let (mode, left) = if complete {
            (SyncMode::Refresh, &mut report.remaining.0)
        } else {
            (SyncMode::Backfill, &mut report.remaining.1)
        };
        if *left == 0 {
            continue;
        }
        let (appended, spent, stop) = events(target, reader, *left, mode);
        *left = left.saturating_sub(spent);
        repo_report.raw_events_added = Some(appended);
        repo_report.forge_calls += spent;
        match stop {
            Some(StopReason::RateLimited) => {
                // Same consequence as a snapshot read: end the cycle, back off.
                report.rate_limited = Some(None);
                return;
            }
            Some(StopReason::BreakerOpen) => return,
            Some(StopReason::Reserve) => {
                reserve_apps.insert(reader.app_id.clone());
            }
            _ => {}
        }
    }
}

/// Production raw-event sync for one repo (#10250's resumable cache), through
/// a reader-only source. Returns `(rows appended, calls spent, stop)`.
fn sync_events(
    root: &Path,
    target: &RepoTarget,
    reader: &Reader,
    left: u64,
    mode: SyncMode,
    reserve: u64,
) -> (u64, u64, Option<StopReason>) {
    let mut source = ForgeEventSource::reader_only(
        ForgeEndpoint::IssuesEvents,
        &target.repo,
        &target.cwd,
        reserve,
        reader.clone(),
    );
    let cursor_file = fleet_events::cursor_path(root, &target.repo);
    let mut cursor = EventsCursor::read(&cursor_file, &target.repo);
    let mut log = match EventLog::open(&fleet_events::events_path(root, &target.repo)) {
        Ok(log) => log,
        Err(e) => {
            log::warn!("eta fleet refresh: {}: event log unreadable: {e}", target.repo);
            return (0, 0, Some(StopReason::WriteError));
        }
    };
    let result = fleet_events::sync(&mut source, &mut log, &mut cursor, &cursor_file, mode, left);
    let calls = source.calls();
    match result {
        Ok(report) => {
            let stop = match report.outcome {
                fleet_events::SyncOutcome::Complete => None,
                fleet_events::SyncOutcome::PageBudget => Some(StopReason::Budget),
                fleet_events::SyncOutcome::Stopped(why) => {
                    log::info!("eta fleet refresh: {}: event sync stopped: {why}", target.repo);
                    Some(source.last_stop().unwrap_or(StopReason::ForgeError))
                }
            };
            (report.appended as u64, calls, stop)
        }
        Err(e) => {
            log::warn!("eta fleet refresh: {}: event sync write failed: {e}", target.repo);
            (0, calls, Some(StopReason::WriteError))
        }
    }
}

/// What the end-of-cycle fit check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitCheck {
    /// `autonomous.eta.fit.enabled` is off.
    Disabled,
    /// A backfill is in progress and younger than
    /// [`fleet_refresh::FIT_HOLD_HOURS`] ([`fleet_refresh::fit_held`]).
    Held,
    /// `refit_if_due` had nothing to do (today's file exists, no snapshot, or
    /// stale snapshots before the grace).
    NotDue,
    /// Today's coefficient file was written.
    Wrote(PathBuf),
    /// The fit failed; the next cycle retries.
    Failed(String),
}

/// The daily fit check (#10245's [`run::refit_if_due`]), run at the end of
/// every cycle in the same blocking call, so the fit always sees this cycle's
/// snapshots. With fleet refresh on this replaces #10245's standalone refit
/// task ([`owns_fit`]); `refit_if_due`'s own `due` gate still decides whether
/// today's file is written.
pub fn after_cycle(
    workspace_root: &Path,
    now: DateTime<Utc>,
    held: bool,
    fit_enabled: bool,
    fitter: &Fitter,
) -> FitCheck {
    if !fit_enabled {
        return FitCheck::Disabled;
    }
    if held {
        log::info!(
            "eta fleet refresh: fit held — a backfill is in progress and younger than {}h",
            fleet_refresh::FIT_HOLD_HOURS
        );
        return FitCheck::Held;
    }
    match run::refit_if_due(workspace_root, now, fitter) {
        None => FitCheck::NotDue,
        Some(Ok(report)) => {
            log::info!(
                "eta fit: wrote {} (id={}, data_through={}, dwells={}, dropped missing={} \
                 no_flags={}) after a fleet refresh cycle",
                report.path.display(),
                report.id,
                report.data_through.to_rfc3339(),
                report.dwells,
                report.rows_dropped_missing,
                report.rows_dropped_no_flags
            );
            FitCheck::Wrote(report.path)
        }
        Some(Err(e)) => {
            log::warn!("eta fit: daily refit failed, retrying next cycle: {e:#}");
            FitCheck::Failed(format!("{e:#}"))
        }
    }
}

/// One `eta.fleet_refresh` record per repo.
#[must_use]
pub fn records(
    report: &CycleReport,
    host_id: &str,
    started_at: DateTime<Utc>,
    loom: &Provenance,
) -> Vec<EtaFleetRefreshRecord> {
    let at = crate::telemetry::trace::instant(started_at);
    let cycle_id =
        crate::telemetry::trace::derived_hex(&["loom.eta.fleet_refresh", host_id, &at], 16);
    report
        .repos
        .iter()
        .map(|r| EtaFleetRefreshRecord {
            repo: r.repo.clone(),
            cycle_id: cycle_id.clone(),
            started_at,
            pass: r.pass.map_or("none", |p| p.as_str()).to_string(),
            stop_reason: r.stop.as_str().to_string(),
            promoted: r.promoted,
            prs_read: r.prs_read,
            pass_done: r.pass_done,
            timelines_incomplete: r.timelines_incomplete,
            samples_added: r.samples_added,
            raw_events_added: r.raw_events_added,
            forge_calls: r.forge_calls,
            not_modified_calls: r.not_modified_calls,
            ratelimit_remaining_min: r.ratelimit_remaining_min,
            reader_app: r.reader_app.clone(),
            snapshot_id: r.snapshot_id.clone(),
            as_of: r.as_of,
            duration_ms: r.duration_ms,
            loom: loom.clone(),
        })
        .collect()
}

/// The per-cycle `info` line: repos per stop reason, calls per budget.
fn log_summary(report: &CycleReport) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &report.repos {
        *counts.entry(r.stop.as_str()).or_default() += 1;
    }
    let counts: Vec<String> = counts.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let duration: u64 = report.repos.iter().map(|r| r.duration_ms).sum();
    log::info!(
        "eta fleet refresh: cycle {} repos [{}]; calls refresh={} backfill={} (left {}/{}); {}ms",
        report.repos.len(),
        counts.join(" "),
        report.refresh_calls,
        report.backfill_calls,
        report.remaining.0,
        report.remaining.1,
        duration
    );
}

#[cfg(test)]
#[path = "eta_fleet_refresh_tests.rs"]
mod tests;
