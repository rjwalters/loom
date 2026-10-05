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
//! Since #10414 each cycle is awaited for at most one interval
//! ([`super::cycle_guard`]). A cycle that runs longer is logged and counted as a
//! `loom.daemon.task_faults{reason=overrun}` fault. No second cycle starts while
//! it is still running. Every finished cycle beats the
//! `task_alive{task=eta_fleet_refresh}` liveness gauge, so a wedged cycle shows
//! as a `0`.
//!
//! # One refresher: the fleet captain (#10329)
//!
//! The snapshots and raw caches are byte-deterministic and host-independent,
//! and every host resolves the same reader installations, so N refreshers buy
//! nothing and spend N times the shared reader budgets. Each tick therefore
//! passes the `fleet.captain` gate (#8848) first ([`gate_tick`], re-read every
//! tick, so an edit needs no restart):
//!
//! - **This host is the captain**: `eta-fleet-refresh` is armed
//!   ([`SINGLETON_JOB_NAME`], `host.health.armed_singleton_jobs`) and the
//!   cycle runs.
//! - **Another host is**: no forge call at all (snapshots, backfill, raw
//!   events) and no record, but the fit check still runs, on whatever
//!   snapshots this host has — the captain's, when `LOOM_ETA_FLEET_SNAPSHOT_DIR`
//!   points at a directory shared with it; otherwise only this host's own
//!   older ones, or none (logged when the gate changes). Publishing the
//!   captain's fit to the other hosts is not this task's job.
//! - **No captain declared**: the cycle runs, unarmed — deliberately
//!   **fail-open**, unlike the gate's fail-closed contract for alerting jobs.
//!   A duplicate refresh costs budget, never correctness (deterministic
//!   output, atomic last-writer-wins), and the task is on by default: a
//!   single-host install must not lose its fit for want of a captain. Logged
//!   once, with the hint to declare one.
//!
//! `should_spawn` / `owns_fit` stay config-only, so the fit's owner never
//! flips with the gate.
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
//! 4. **Raw events** (#10250, #10298): each repo's raw cache is synced
//!    in-process from every repo-wide listing ([`event_endpoints`]: issue
//!    events, then pulls), each through its own reader-only source and its own
//!    cursor, from what the matching shared budget has left. After the
//!    snapshots, and never ahead of the fit.
//! 5. **Fit** ([`after_cycle`]): #10245's daily fit check, in the same blocking
//!    call, so the fit always sees this cycle's snapshots. It is held while a
//!    backfill younger than six hours is in progress, and skipped when
//!    `autonomous.eta.fit.enabled` is off. With this task on, #10245's
//!    standalone refit task is not spawned ([`owns_fit`]).
//! 6. **Telemetry**: one `eta.fleet_refresh` record per repo, and one `info`
//!    line with the stop-reason counts and the calls spent.
//! 7. **SigNoz in-sweep half** (#9758, `fleetRefresh.signoz.enabled`, default
//!    off): each repo's fleet `sweep.outcome` records are re-read from the
//!    telemetry store into its SigNoz snapshot ([`signoz_cycle`]); a failed
//!    walk keeps the last valid snapshot and logs why.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};

use super::cycle_guard::{CycleGuard, CycleTick};
pub use super::eta_fit::FitCheck;
use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::config::{EtaConfig, FleetRefreshConfig};
use crate::eta::fit::{run, Fitter};
use crate::eta::fleet;
use crate::eta::fleet_events::{self, EventLog, EventsCursor, SyncMode};
use crate::eta::fleet_events_forge::{ForgeEndpoint, ForgeEventSource};
use crate::eta::fleet_fetch::{ForgeRead, Installation, NoReader, Reader, ReaderForge, RepoTarget};
use crate::eta::fleet_refresh::{self, Budgets, CycleReport, PassKind, RepoReport, StopReason};
use crate::eta::Provenance;
use crate::task_liveness::ETA_FLEET_REFRESH;
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
    /// The last tick's captain gate, so a change is logged once (#10329).
    gate: Option<RefreshGate>,
}

/// The singleton job name this task arms under `fleet.captain` (#10329).
pub const SINGLETON_JOB_NAME: &str = "eta-fleet-refresh";

/// One tick's fleet-captain decision (#10329; see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshGate {
    /// This host is the declared captain: armed, and it refreshes.
    Captain,
    /// No `fleet.captain` declared: it refreshes, unarmed (fail-open).
    NoCaptain,
    /// Another host is the captain: no forge call, no record; the fit runs.
    StandDown { captain: String },
}

impl RefreshGate {
    /// Whether this tick makes the cycle's forge calls.
    #[must_use]
    pub fn refreshes(&self) -> bool {
        !matches!(self, Self::StandDown { .. })
    }
}

/// Resolve this tick's [`RefreshGate`] from `root`'s `fleet.captain`, keep the
/// armed-singleton registry in step, and log the gate when it changes.
pub fn gate_tick(root: &Path, host_id: &str, task: &mut TaskState) -> RefreshGate {
    use crate::fleet_captain::{self as captain, CaptainGate};
    let gate = match captain::resolve_gate_for_root(root, host_id) {
        CaptainGate::Armed { captain: name } => {
            match captain::arm_singleton_job(SINGLETON_JOB_NAME, root, host_id) {
                Ok(()) => RefreshGate::Captain,
                // `fleet.captain` changed between the two reads: sit this tick
                // out; the next one re-reads it.
                Err(_) => RefreshGate::StandDown { captain: name },
            }
        }
        CaptainGate::Refused { captain: name, .. } => {
            captain::disarm_singleton_job(SINGLETON_JOB_NAME);
            RefreshGate::StandDown { captain: name }
        }
        // Not `arm_singleton_job`: that would list the job as captainless
        // (stopped for want of a captain), and this one fails open instead.
        CaptainGate::NoCaptainDeclared => {
            captain::disarm_singleton_job(SINGLETON_JOB_NAME);
            RefreshGate::NoCaptain
        }
    };
    if task.gate.as_ref() != Some(&gate) {
        log_gate(root, host_id, &gate);
        task.gate = Some(gate.clone());
    }
    gate
}

fn log_gate(root: &Path, host_id: &str, gate: &RefreshGate) {
    match gate {
        RefreshGate::Captain => log::info!(
            "eta fleet refresh: this host ({host_id}) is the fleet captain — it refreshes the \
             fleet snapshots for every host (#10329)"
        ),
        RefreshGate::NoCaptain => log::info!(
            "eta fleet refresh: no fleet.captain declared; every host with a reader refreshes. \
             Declare one on a multi-host fleet (`fleet.captain` in .loom/config.json) so only \
             one host spends the shared reader budgets (#10329)"
        ),
        RefreshGate::StandDown { captain } => {
            let dir = fleet::snapshot_dir(root);
            let present = fleet::load_all(root).len();
            log::info!(
                "eta fleet refresh: standing down — the fleet captain is {captain}, this host is \
                 {host_id}: no forge calls and no eta.fleet_refresh records here (#10329). The \
                 daily fit still runs on the {present} snapshot(s) under {}{}",
                dir.display(),
                if present == 0 {
                    ": none, so nothing to fit until the captain's snapshots are shared here \
                     (LOOM_ETA_FLEET_SNAPSHOT_DIR)"
                } else {
                    ""
                }
            );
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    pub gate: RefreshGate,
    /// The cycle, or `None` when the host stood down (no call, no record).
    pub outcome: Option<CycleOutcome>,
    /// The hold for this tick's [`after_cycle`].
    pub fit_held: bool,
}

/// One tick: the captain gate, then `run` (the cycle) only when this host
/// refreshes. A standing-down host still computes the fit hold, from the
/// refresh state beside its snapshots ([`backfill_in_progress_since`]), so a
/// host sharing the captain's snapshot directory honours its backfill hold.
pub fn tick(
    root: &Path,
    host_id: &str,
    task: &mut TaskState,
    now: DateTime<Utc>,
    run: impl FnOnce(&mut TaskState) -> CycleOutcome,
) -> Tick {
    let gate = gate_tick(root, host_id, task);
    if gate.refreshes() {
        let outcome = run(task);
        Tick {
            gate,
            fit_held: outcome.fit_held,
            outcome: Some(outcome),
        }
    } else {
        Tick {
            gate,
            outcome: None,
            fit_held: fleet_refresh::fit_held(backfill_in_progress_since(root), now),
        }
    }
}

/// The oldest in-progress backfill among every refresh state under `root`'s
/// snapshot directory — [`fleet_refresh::backfill_since`] without a repo set.
#[must_use]
pub fn backfill_in_progress_since(root: &Path) -> Option<DateTime<Utc>> {
    let entries = std::fs::read_dir(fleet_refresh::refresh_dir(root)).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.ends_with(".json") && !name.ends_with(".staging.json")
        })
        .filter_map(|p| fleet_refresh::read_state(&p)?.pass)
        .filter(|pass| pass.kind == PassKind::Backfill)
        .map(|pass| pass.listed_at)
        .min()
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
    let every = Duration::from_secs(config.interval_secs);
    // #10414: liveness beats once per finished cycle; the first lands after
    // FIRST_CYCLE_DELAY, and a cycle may take up to one interval.
    crate::task_liveness::register(
        ETA_FLEET_REFRESH,
        every,
        crate::task_liveness::default_stale_after(every).saturating_add(FIRST_CYCLE_DELAY),
    );
    Some(tokio::spawn(async move {
        tokio::time::sleep(FIRST_CYCLE_DELAY).await;
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // #10414: a cycle that runs past one interval is reported, and never
        // gets a second cycle stacked beside it on the same `task` lock.
        let mut guard = CycleGuard::new(cycle_bound(every));
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
            let start = move || {
                tokio::task::spawn_blocking(move || {
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
                    .is_ok()
                })
            };
            report_tick(guard.tick(start).await);
        }
    }))
}

/// How long one cycle may run before it counts as an overrun: one interval.
#[must_use]
pub fn cycle_bound(interval: Duration) -> Duration {
    interval
}

/// Log one guarded tick, count its faults, and beat liveness when a cycle
/// finished (#10414). A wedged cycle beats nothing, so `task_alive` drops.
pub fn report_tick(tick: CycleTick<bool>) {
    use super::ops::liveness::{fault, Fault};
    match tick {
        CycleTick::Finished(Ok(true)) => {
            crate::task_liveness::beat_if_registered(ETA_FLEET_REFRESH)
        }
        CycleTick::Finished(Ok(false)) => {
            log::warn!("eta fleet refresh: cycle panicked; retrying next tick");
            fault(ETA_FLEET_REFRESH, Fault::Panic);
            crate::task_liveness::beat_if_registered(ETA_FLEET_REFRESH);
        }
        CycleTick::Finished(Err(e)) => {
            log::warn!("eta fleet refresh: cycle failed to run: {e}; retrying next tick");
            fault(ETA_FLEET_REFRESH, Fault::Panic);
        }
        CycleTick::Overran { running_for } => {
            log::warn!(
                "eta fleet refresh: cycle still running after {}s (past its one-interval bound) \
                 — likely stuck in a forge read, the SigNoz walk or a lock; no new cycle starts \
                 until it returns (#10414)",
                running_for.as_secs()
            );
            fault(ETA_FLEET_REFRESH, Fault::Overrun);
        }
        CycleTick::StillRunning { running_for } => log::warn!(
            "eta fleet refresh: skipping this tick — the cycle started {}s ago is still running \
             (#10414)",
            running_for.as_secs()
        ),
    }
}

/// One production tick: the captain gate ([`tick`]), then the cycle over the
/// real repo set, reader resolution and forge, then the fit check.
fn run_production_cycle(
    root: &Path,
    roots: &[PathBuf],
    config: &FleetRefreshConfig,
    host_id: &str,
    sink: Option<&dyn QueueSink>,
    task: &mut TaskState,
    (fit_enabled, fitter): (bool, &Fitter),
) {
    let now = Utc::now();
    // Re-resolved every tick, like `ci_telemetry`'s gate: a host-identity
    // change takes effect on the next tick too (#8848).
    let gate_host = crate::sweep_registry::host_identity();
    // Local only (git remotes, reader token files): no forge call, so the
    // repo set is resolved on every host, for the SigNoz half below too.
    let targets = repo_targets(
        root,
        roots,
        |r| crate::forge_etag_store::remote_identity(r),
        |repo, host| {
            crate::forge_identity::read_credential(repo, host)
                .map(|(dir, app_id)| Reader { app_id, dir })
        },
    );
    let ticked = tick(root, &gate_host, task, now, |task| {
        let mut forge = ReaderForge::new();
        let mut events = |target: &RepoTarget,
                          reader: &Reader,
                          endpoint: ForgeEndpoint,
                          left: u64,
                          mode: SyncMode| {
            sync_events(root, target, reader, endpoint, left, mode, config.reserve_calls)
        };
        cycle(root, &targets, &mut forge, &mut events, config, task, now)
    });
    let loom = Provenance::current();
    let report = ticked.outcome.as_ref().map(|o| (&o.report, o.started_at));
    for record in report.map_or_else(Vec::new, |(r, at)| records(r, host_id, at, &loom)) {
        if !record.has_provenance() {
            log::warn!("eta fleet refresh: dropped record for {}: invalid provenance", record.repo);
            continue;
        }
        if let Some(sink) = sink {
            sink.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::EtaFleetRefresh(record)));
        }
    }
    // #9758: the SigNoz in-sweep half, same cadence, its own backend. Before
    // the fit only by position; the fit reads forge snapshots alone. Not
    // captain-gated (#10329): it spends no reader budget, and it is a
    // per-host opt-in with its own credential.
    if config.signoz.enabled {
        let repos: Vec<String> = targets.iter().map(|t| t.repo.clone()).collect();
        signoz_cycle(root, &repos, &config.signoz, Utc::now());
    }
    let cycle = cycle_state(&ticked, now, config.interval_secs);
    crate::eta::health::write_refresh_cycle(root, &cycle);
    super::ops::eta_health::note_tick(&cycle);
    // After the records: a fit that panics must not cost the cycle's telemetry.
    let fit_started = Utc::now();
    let began = std::time::Instant::now();
    let check = after_cycle(root, fit_started, ticked.fit_held, fit_enabled, fitter);
    super::eta_fit::finish(
        root,
        sink,
        host_id,
        super::eta_fit::Trigger::FleetRefresh,
        fit_started,
        began.elapsed(),
        &check,
    );
}

/// Refresh every repo's SigNoz in-sweep snapshot (#9758) through the
/// configured ClickHouse endpoint. A missing endpoint is a warning and a
/// no-op: nothing is fetched and every published snapshot stays as it is.
pub fn signoz_cycle(
    root: &Path,
    repos: &[String],
    config: &crate::eta::config::FleetSignozConfig,
    now: DateTime<Utc>,
) -> Vec<crate::eta::fleet_signoz_refresh::FetchReport> {
    use crate::eta::fleet_signoz_refresh::{run_cycle, ClickhouseHttp, Limits};
    let Some(endpoint) = config.endpoint.clone() else {
        log::warn!(
            "eta fleet signoz: enabled but no endpoint configured \
             (autonomous.eta.fleetRefresh.signoz.endpoint); skipped"
        );
        return Vec::new();
    };
    let mut reader = ClickhouseHttp {
        endpoint,
        user: config.user.clone(),
        credential_file: config.credential_file.clone(),
        timeout: Duration::from_secs(60),
    };
    let limits = Limits {
        page_size: config.page_size,
        max_pages: config.max_pages,
    };
    run_cycle(root, repos, &mut reader, now, limits)
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

/// The raw-event sync seam: `(target, reader, endpoint, calls left, mode)` →
/// `(rows appended, calls spent, stop)`. Production is [`sync_events`].
pub type EventsSync<'a> = dyn FnMut(&RepoTarget, &Reader, ForgeEndpoint, u64, SyncMode) -> (u64, u64, Option<StopReason>)
    + 'a;

/// The listings the daemon syncs, in [`ForgeEndpoint::ALL`]'s order: every
/// repo-wide one (issue events, then pulls — #10298). The per-PR walks
/// (reviews, check runs) are not cursor-complete listings and stay CLI-only.
pub fn event_endpoints() -> impl Iterator<Item = ForgeEndpoint> {
    ForgeEndpoint::ALL
        .into_iter()
        .filter(|e| e.per_pr().is_none())
}

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

/// Sync each repo's raw event cache, one [`event_endpoints`] listing at a
/// time, from what the matching budget has left. Each endpoint picks refresh or
/// backfill from its own cursor key; the budgets are shared by every endpoint
/// and repo. Skipped for a repo whose snapshot pass hit a coverage gap, for
/// every repo on a reader installation (App and owner, #10329) that hit the
/// reserve, and for everything left
/// once one sync is rate limited or meets the open breaker.
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
    let mut reserve_installs: BTreeSet<Installation> = targets
        .iter()
        .filter(|t| {
            report
                .repos
                .iter()
                .any(|r| r.repo == t.repo && r.stop == StopReason::Reserve)
        })
        .filter_map(|t| t.reader.as_ref().ok().map(|r| r.installation(&t.repo)))
        .collect();
    for target in targets {
        let Ok(reader) = &target.reader else { continue };
        let Some(repo_report) = report.repos.iter_mut().find(|r| r.repo == target.repo) else {
            continue;
        };
        if repo_report.stop == StopReason::Coverage {
            continue;
        }
        let cursor_file = fleet_events::cursor_path(root, &target.repo);
        for endpoint in event_endpoints() {
            if reserve_installs.contains(&reader.installation(&target.repo)) {
                break;
            }
            // Re-read per endpoint: never infer one listing's completeness
            // from another's (#10298).
            let complete = EventsCursor::read(&cursor_file, &target.repo)
                .endpoints
                .get(&format!("{}:{}", fleet_events::SOURCE_FORGE, endpoint.name()))
                .is_some_and(|e| e.backfill_complete);
            let (mode, left) = if complete {
                (SyncMode::Refresh, &mut report.remaining.0)
            } else {
                (SyncMode::Backfill, &mut report.remaining.1)
            };
            if *left == 0 {
                continue;
            }
            let (appended, spent, stop) = events(target, reader, endpoint, *left, mode);
            *left = left.saturating_sub(spent);
            repo_report.raw_events_added =
                Some(repo_report.raw_events_added.unwrap_or(0) + appended);
            repo_report.forge_calls += spent;
            match stop {
                Some(StopReason::RateLimited) => {
                    // Same consequence as a snapshot read: end the cycle, back off.
                    report.rate_limited = Some(None);
                    return;
                }
                Some(StopReason::BreakerOpen) => return,
                Some(StopReason::Reserve) => {
                    reserve_installs.insert(reader.installation(&target.repo));
                }
                // The reader was withdrawn for this repo: its other listings
                // would only fail the same way.
                Some(StopReason::Coverage) => break,
                _ => {}
            }
        }
    }
}

/// Production raw-event sync of one repo's `endpoint` listing (#10250's
/// resumable cache), through a reader-only source.
/// Returns `(rows appended, calls spent, stop)`.
fn sync_events(
    root: &Path,
    target: &RepoTarget,
    reader: &Reader,
    endpoint: ForgeEndpoint,
    left: u64,
    mode: SyncMode,
    reserve: u64,
) -> (u64, u64, Option<StopReason>) {
    let source = reader_source(target, reader, endpoint, reserve);
    sync_events_from(root, target, source, left, mode)
}

/// The daemon's source for one listing: reader-only, for every endpoint —
/// [`crate::forge_etag_store::fetch_with_reader`], never the writer-falling-back
/// `fetch_conditional` the CLI's [`ForgeEventSource::new`] uses.
fn reader_source(
    target: &RepoTarget,
    reader: &Reader,
    endpoint: ForgeEndpoint,
    reserve: u64,
) -> ForgeEventSource {
    ForgeEventSource::reader_only(endpoint, &target.repo, &target.cwd, reserve, reader.clone())
}

/// [`sync_events`] over an already-built source (tests point it at a stub
/// `gh`).
fn sync_events_from(
    root: &Path,
    target: &RepoTarget,
    mut source: ForgeEventSource,
    left: u64,
    mode: SyncMode,
) -> (u64, u64, Option<StopReason>) {
    let cursor_file = fleet_events::cursor_path(root, &target.repo);
    let mut cursor = EventsCursor::read(&cursor_file, &target.repo);
    let mut log = match EventLog::open(&fleet_events::events_path(root, &target.repo)) {
        Ok(log) => log,
        Err(e) => {
            log::warn!("eta fleet refresh: {}: event log unreadable: {e}", target.repo);
            return (0, 0, Some(StopReason::WriteError));
        }
    };
    let key = fleet_events::RawEventSource::cursor_key(&source);
    let result = fleet_events::sync(&mut source, &mut log, &mut cursor, &cursor_file, mode, left);
    let calls = source.calls();
    match result {
        Ok(report) => {
            let stop = match report.outcome {
                fleet_events::SyncOutcome::Complete => None,
                fleet_events::SyncOutcome::PageBudget => Some(StopReason::Budget),
                fleet_events::SyncOutcome::Stopped(why) => {
                    log::info!("eta fleet refresh: {}: {key} sync stopped: {why}", target.repo);
                    Some(source.last_stop().unwrap_or(StopReason::ForgeError))
                }
            };
            (report.appended as u64, calls, stop)
        }
        Err(e) => {
            log::warn!("eta fleet refresh: {}: {key} sync write failed: {e}", target.repo);
            (0, calls, Some(StopReason::WriteError))
        }
    }
}

/// The daily fit check (#10245's [`run::refit_check`]), run at the end of
/// every cycle in the same blocking call, so the fit always sees this cycle's
/// snapshots. With fleet refresh on this replaces #10245's standalone refit
/// task ([`owns_fit`]); `refit_check`'s own `due` gate still decides whether
/// today's file is written. The caller turns the outcome into the cycle's one
/// `eta.fit` record ([`super::eta_fit::finish`], #10391).
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
    let check = super::eta_fit::run_check(workspace_root, now, fitter);
    match &check {
        FitCheck::Wrote(report) => log::info!(
            "eta fit: wrote {} (id={}, data_through={}, dwells={}, dropped missing={} \
             no_flags={}) after a fleet refresh cycle",
            report.path.display(),
            report.id,
            report.data_through.to_rfc3339(),
            report.dwells,
            report.rows_dropped_missing,
            report.rows_dropped_no_flags
        ),
        FitCheck::Failed(e) => log::warn!("eta fit: daily refit failed, retrying next cycle: {e}"),
        FitCheck::Panicked => {
            log::warn!("eta fit: daily refit panicked, retrying next cycle (ETA fit only)");
        }
        _ => {}
    }
    check
}

/// The tick's `refresh-cycle.json` state (#10391): written on every tick,
/// stand-down included.
#[must_use]
pub fn cycle_state(
    ticked: &Tick,
    started_at: DateTime<Utc>,
    interval_secs: u64,
) -> crate::eta::health::RefreshCycleState {
    use crate::eta::health::{RefreshCycleState, RefreshRepo};
    let (gate, captain) = match &ticked.gate {
        RefreshGate::Captain => ("captain", None),
        RefreshGate::NoCaptain => ("no_captain", None),
        RefreshGate::StandDown { captain } => ("stand_down", Some(captain.clone())),
    };
    let mut stop_reasons: BTreeMap<String, u64> = BTreeMap::new();
    let mut repos = Vec::new();
    if let Some(outcome) = &ticked.outcome {
        for r in &outcome.report.repos {
            *stop_reasons.entry(r.stop.as_str().to_string()).or_default() += 1;
            repos.push(RefreshRepo {
                repo: r.repo.clone(),
                stop_reason: r.stop.as_str().to_string(),
                as_of: r.as_of,
            });
        }
    }
    RefreshCycleState {
        started_at,
        gate: gate.to_string(),
        captain,
        interval_secs,
        stop_reasons,
        repos,
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

#[cfg(test)]
#[path = "eta_fleet_refresh_gate_tests.rs"]
mod gate_tests;

#[cfg(test)]
#[path = "eta_fleet_refresh_watchdog_tests.rs"]
mod watchdog_tests;
