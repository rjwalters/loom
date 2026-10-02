//! The **live agent-output producer** (#9764): publishes `session.output`
//! records while a run is still in flight.
//!
//! # Shape
//!
//! Like [`super::collector`] and [`super::daemon_event`] this is a pure
//! [`EventBus`] subscriber plus a timer — it adds no emit call site anywhere
//! else in the daemon. The bus tells it which runs exist
//! (`sweep.global.dispatch` opens one, `sweep.issue.*.exited` / `.crashed`
//! closes it); the timer drives a bounded read of each open run's source.
//!
//! ```text
//!   sweep.global.dispatch ──▶ open run ──▶ resolve canonical loom.repo (gh)
//!                                    │            │
//!                              every tick         ▼
//!                                    ├──▶ claude::discover  (find streams)
//!                                    ├──▶ Cursor::advance   (read the tail)
//!                                    └──▶ sink.push(...)    (OTLP queues)
//!   sweep.issue.N.exited  ──▶ close run, emit coverage=ended
//! ```
//!
//! # Off by default, and it cannot turn anything else on
//!
//! [`resolve_enabled`] defaults to `false` (**env > config > default**, the
//! house precedence). Two further properties make this impossible to enable
//! by accident:
//!
//! - The sink is built **only** from the already-resolved OTLP exporter
//!   queues that [`super::spawn_task`] constructed. It never resolves an
//!   endpoint, never constructs an exporter, and never adds one to the
//!   configured list — no OTLP exporter configured means
//!   [`register_sink`] registers nothing and every emit is a no-op.
//! - `session.output` is declared `native: false`, so even on a host whose
//!   configured sink *is* the managed HTTPS backend, no session text is
//!   offered to it.
//!
//! # Latency budget
//!
//! Source-event → queryable is the sum of: **producer lag** (≤ `intervalMs`,
//! default 2 s, reported per record as
//! `loom.session.output.producer_lag_ms`) + **export lag**
//! (`observability.flushIntervalSecs`, default 30 s — a live-output
//! deployment should set it to ≤ 5 s) + collector and backend ingest. For the
//! #9764 target of p95 ≤ 10 s end to end, the producer/export budget is 2 s +
//! 5 s, leaving ~3 s for the gateway, the backend and a consumer's polling
//! cache. See `defaults/docs/session-output.md`.

pub mod claude;
pub mod latency;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::event_bus::EventBus;
use crate::telemetry::kinds::session_output::{
    Coverage, OutputCategory, RunIdentity, RunState, SessionOutputRecord,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use crate::types::{Event, SweepKind};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};

/// `observability.liveOutput.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_OBSERVABILITY_LIVE_OUTPUT";

/// How often each open run's source is read, when unset at every tier. Also
/// the producer half of the latency budget: a record's `producer_lag_ms`
/// cannot exceed this by more than one pass.
pub const DEFAULT_INTERVAL_MS: u64 = 2_000;

/// How long a run may be silent before a `heartbeat` record is emitted, when
/// unset at every tier. Without it a quiet run and a stalled export look the
/// same to a consumer.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 30;

/// Maximum runs tracked at once, when unset at every tier. A host that
/// somehow exceeds it stops *adding* runs (and says so once) rather than
/// growing its read set without bound.
pub const DEFAULT_MAX_RUNS: usize = 64;

/// Runtimes with a live-output adapter. Everything else gets one explicit
/// `coverage = unsupported` record per run and is never read.
pub const SUPPORTED_RUNTIMES: &[&str] = &["claude"];

/// The `observability.liveOutput` config block, read but not yet resolved
/// against env/defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveOutputConfig {
    pub enabled: Option<bool>,
    pub interval_ms: Option<u64>,
    pub heartbeat_secs: Option<u64>,
    pub max_runs: Option<usize>,
}

/// Read `observability.liveOutput` from `root`'s resolved config, mirroring
/// [`super::read_config`]'s shape.
#[must_use]
pub fn read_config(root: &Path) -> LiveOutputConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "observability.liveOutput") else {
        return LiveOutputConfig::default();
    };
    LiveOutputConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_ms: block
            .get("intervalMs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
        heartbeat_secs: block
            .get("heartbeatSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
        max_runs: block
            .get("maxRuns")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0),
    }
}

/// **env > config > default** (`false`). Live output is opt-in: it is the one
/// telemetry path that carries readable session content, so it never starts
/// because something else was enabled.
#[must_use]
pub fn resolve_enabled(config: &LiveOutputConfig) -> bool {
    std::env::var(ENABLED_ENV)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .or(config.enabled)
        .unwrap_or(false)
}

/// The resolved knobs, after env/config/default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedLiveOutput {
    pub interval: Duration,
    pub heartbeat: Duration,
    pub max_runs: usize,
}

/// Resolve the non-boolean knobs. Env overrides are deliberately not offered
/// for these: the only knob an operator needs at the shell is the on/off
/// switch, and three more env names would be three more ways to have a host
/// silently disagree with its committed config.
#[must_use]
pub fn resolve(config: &LiveOutputConfig) -> ResolvedLiveOutput {
    ResolvedLiveOutput {
        interval: Duration::from_millis(config.interval_ms.unwrap_or(DEFAULT_INTERVAL_MS)),
        heartbeat: Duration::from_secs(config.heartbeat_secs.unwrap_or(DEFAULT_HEARTBEAT_SECS)),
        max_runs: config.max_runs.unwrap_or(DEFAULT_MAX_RUNS),
    }
}

// ============================================================================
// Sink
// ============================================================================

/// The exporter-facing half: wraps one record in a [`TelemetryEnvelope`] and
/// offers it onto the OTLP queue fan-out.
///
/// It also watches the queues' own `dropped_total` so the producer can tell a
/// consumer that delivery was incomplete — see [`Self::newly_dropped`].
#[derive(Clone)]
pub struct SessionOutputSink {
    queue: Arc<dyn QueueSink>,
    /// The concrete queues behind `queue`, kept for drop accounting only.
    queues: Arc<Vec<Arc<DurableQueue>>>,
    host_id: String,
}

impl SessionOutputSink {
    /// Wrap the OTLP queues (and the host id every envelope is stamped with)
    /// in a sink. `None` when there are none — an OTLP-only kind with no OTLP
    /// exporter has nowhere to go, and a queue that is never drained is worse
    /// than no queue at all.
    #[must_use]
    pub fn new(otlp_queues: Vec<Arc<DurableQueue>>, host_id: impl Into<String>) -> Option<Self> {
        if otlp_queues.is_empty() {
            return None;
        }
        let queue: Arc<dyn QueueSink> = Arc::new(FanoutQueue::new(otlp_queues.clone()));
        Some(SessionOutputSink {
            queue,
            queues: Arc::new(otlp_queues),
            host_id: host_id.into(),
        })
    }

    /// The host id this sink stamps on every envelope.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Enqueue one record. Best-effort like every queue producer.
    pub fn push(&self, record: SessionOutputRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::SessionOutput(record),
        ));
    }

    /// Total envelopes the backing queues have discarded for being full, over
    /// their whole lifetime. The caller diffs consecutive readings to turn
    /// "the queue overflowed" into an explicit `gap` record, instead of
    /// letting a consumer infer a complete transcript from an incomplete one.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.queues.iter().map(|q| q.dropped_total()).sum()
    }
}

impl std::fmt::Debug for SessionOutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOutputSink")
            .field("host_id", &self.host_id)
            .field("queues", &self.queues.len())
            .finish_non_exhaustive()
    }
}

static GLOBAL_SINK: OnceLock<SessionOutputSink> = OnceLock::new();

/// Register the process-global sink over the **already-resolved** OTLP
/// queues. Registers nothing when there are none, exactly like
/// [`super::eta::register_sink`].
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if let Some(sink) = SessionOutputSink::new(otlp_queues, host_id) {
        let _ = GLOBAL_SINK.set(sink);
    }
}

/// The registered sink, or `None` when live output is off or no OTLP exporter
/// is configured.
#[must_use]
pub fn global_sink() -> Option<&'static SessionOutputSink> {
    GLOBAL_SINK.get()
}

// ============================================================================
// Run tracking
// ============================================================================

/// One tracked in-flight run.
#[derive(Debug)]
struct Run {
    identity: RunIdentity,
    workspace_root: PathBuf,
    /// Per-transcript cursors, keyed by `stream_id`.
    cursors: HashMap<String, (PathBuf, claude::Cursor)>,
    /// Sequence counter for this run's producer-authored status stream, which
    /// is deliberately separate from any transcript's line numbering.
    status_sequence: u64,
    /// The status stream's id.
    status_stream: String,
    /// When a content record was last emitted, for heartbeat decisions.
    last_content_at: DateTime<Utc>,
    /// Whether any stream has ever been found for this run.
    ever_covered: bool,
    /// Cumulative source events this run could not deliver.
    dropped_events: u64,
    /// When this producer began watching the run. The freshness boundary for
    /// latency sampling: an event older than this is a replayed backlog line
    /// whose age is not a latency (see [`latency`]).
    watch_since: DateTime<Utc>,
    /// Producer-lag distribution over this run's fresh source events.
    lag: latency::LagWindow,
}

impl Run {
    fn supported(&self) -> bool {
        SUPPORTED_RUNTIMES.contains(&self.identity.runtime.as_str())
    }

    /// A status record for this run, carrying the current lag distribution.
    /// Every status record gets it: a heartbeat is where a quiet run's latency
    /// is observable at all, and a gap/coverage row is exactly when a consumer
    /// wants to know whether the pipeline was keeping up.
    fn status(
        &mut self,
        category: OutputCategory,
        at: DateTime<Utc>,
        coverage: Coverage,
        state: RunState,
    ) -> SessionOutputRecord {
        let sequence = self.status_sequence;
        self.status_sequence += 1;
        SessionOutputRecord::status(
            self.identity.clone(),
            category,
            self.status_stream.clone(),
            sequence,
            at,
            coverage,
            state,
        )
        .with_lag(self.lag.snapshot())
    }
}

/// The producer's whole mutable state — one entry per open run, keyed by
/// `(workspace root, issue)` so two managed repos' issue #N never collide.
#[derive(Debug, Default)]
struct Tracker {
    runs: HashMap<(String, u32), Run>,
    /// Attempt counter per `(workspace root, issue)`, so a retry of the same
    /// issue is separable from the original.
    attempts: HashMap<(String, u32), u32>,
    /// Last observed queue drop total, for gap accounting.
    last_dropped_total: u64,
    /// Whether the `max_runs` ceiling has already been logged.
    warned_full: bool,
}

/// Where transcripts live, injectable so tests never touch `$HOME`.
type ProjectsDir = Option<PathBuf>;

impl Tracker {
    /// Open a run, or refresh the identity of one already open.
    fn open(
        &mut self,
        workspace_root: &str,
        issue: u32,
        sweep_id: Option<String>,
        runtime: Option<String>,
        max_runs: usize,
        now: DateTime<Utc>,
    ) -> Option<&mut Run> {
        let key = (workspace_root.to_string(), issue);
        if !self.runs.contains_key(&key) && self.runs.len() >= max_runs {
            if !self.warned_full {
                self.warned_full = true;
                log::warn!(
                    "session.output: tracking {max_runs} runs (the configured maximum); \
                     further runs are not followed until one ends"
                );
            }
            return None;
        }
        let attempt = self.attempts.entry(key.clone()).or_insert(0);
        *attempt += 1;
        let attempt = *attempt;
        let runtime = runtime.unwrap_or_else(|| "unknown".to_string());
        let status_stream = sweep_id
            .clone()
            .unwrap_or_else(|| format!("issue-{issue}-attempt-{attempt}"));
        let run = Run {
            identity: RunIdentity {
                // Resolved asynchronously on the first tick: the bus event
                // carries a filesystem path, never a forge slug, and a
                // basename would be a fabricated identity.
                repo: None,
                // Fail-closed: this producer never asks the forge, so a row
                // never claims `public` on its own authority.
                visibility: crate::telemetry::RepoVisibility::Private,
                issue: Some(issue),
                // A dispatched sweep names its issue by construction. Resolved
                // again from the transcript on first stream sight, which is
                // what covers a role attempt and an unattributed session.
                session_kind: Some(crate::telemetry::SessionKind::Sweep),
                sweep_id,
                session_id: None,
                attempt: Some(attempt),
                runtime,
                role: None,
            },
            workspace_root: PathBuf::from(workspace_root),
            cursors: HashMap::new(),
            status_sequence: 0,
            status_stream,
            last_content_at: now,
            ever_covered: false,
            dropped_events: 0,
            watch_since: now,
            lag: latency::LagWindow::default(),
        };
        self.runs.insert(key.clone(), run);
        self.runs.get_mut(&key)
    }

    fn close(&mut self, workspace_root: &str, issue: u32) -> Option<Run> {
        self.runs.remove(&(workspace_root.to_string(), issue))
    }
}

// ============================================================================
// The task
// ============================================================================

/// Start the live-output producer. `None` — no subscription, no timer, zero
/// syscalls — when live output is disabled or no sink is registered.
pub fn spawn_task(bus: &EventBus, workspace_root: PathBuf) -> Option<tokio::task::JoinHandle<()>> {
    let config = read_config(&workspace_root);
    if !resolve_enabled(&config) {
        return None;
    }
    let Some(sink) = global_sink().cloned() else {
        log::info!(
            "session.output: live output is enabled but no OTLP exporter is configured; \
             nothing is published (the kind is OTLP-only by design)"
        );
        return None;
    };
    let resolved = resolve(&config);
    log::info!(
        "session.output: live output enabled (interval={}ms, heartbeat={}s, max_runs={}, \
         supported runtimes: {})",
        resolved.interval.as_millis(),
        resolved.heartbeat.as_secs(),
        resolved.max_runs,
        SUPPORTED_RUNTIMES.join(", ")
    );
    let subscription = bus.subscribe(["sweep.global.dispatch", "sweep.issue"]);
    Some(tokio::spawn(run_producer(subscription, sink, resolved)))
}

async fn run_producer(
    mut subscription: crate::event_bus::Subscription,
    sink: SessionOutputSink,
    resolved: ResolvedLiveOutput,
) {
    let mut tracker = Tracker::default();
    let mut slug_cache: HashMap<String, String> = HashMap::new();
    let mut ticker = tokio::time::interval(resolved.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = subscription.recv() => match event {
                Ok(event) => handle_event(&mut tracker, &sink, &event, resolved.max_runs),
                Err(crate::event_bus::RecvError::Closed) => {
                    log::debug!("session.output: event bus closed; producer stopping");
                    break;
                }
                Err(_) => {}
            },
            _ = ticker.tick() => {
                tick(&mut tracker, &sink, &mut slug_cache, resolved).await;
            }
        }
    }
}

fn handle_event(tracker: &mut Tracker, sink: &SessionOutputSink, event: &Event, max_runs: usize) {
    let now = Utc::now();
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(issue),
            sweep_id,
            runtime,
            repo,
            ..
        } => {
            let Some(root) = repo.clone() else {
                // Without an owning workspace root there is no transcript
                // directory to read and no way to disambiguate two repos'
                // issue #N. Nothing is published rather than guessed.
                return;
            };
            let Some(run) = tracker.open(
                &root,
                *issue,
                Some(sweep_id.to_string()),
                runtime.clone(),
                max_runs,
                now,
            ) else {
                return;
            };
            let coverage = if run.supported() {
                Coverage::Degraded
            } else {
                Coverage::Unsupported
            };
            let record = run.status(OutputCategory::Coverage, now, coverage, RunState::Running);
            sink.push(record);
        }
        Event::SweepExited { issue, repo, .. } | Event::SweepCrashed { issue, repo, .. } => {
            let Some(root) = repo.clone() else { return };
            if let Some(mut run) = tracker.close(&root, *issue) {
                let record =
                    run.status(OutputCategory::Coverage, now, Coverage::Ended, RunState::Ended);
                sink.push(record);
            }
        }
        _ => {}
    }
}

async fn tick(
    tracker: &mut Tracker,
    sink: &SessionOutputSink,
    slug_cache: &mut HashMap<String, String>,
    resolved: ResolvedLiveOutput,
) {
    // Queue overflow is a delivery gap, and the producer is the only place
    // that can attribute it to the runs it was publishing for. Checked once
    // per tick, before any new record is offered.
    let dropped_total = sink.dropped_total();
    let newly_dropped = dropped_total.saturating_sub(tracker.last_dropped_total);
    tracker.last_dropped_total = dropped_total;

    // Canonical repo resolution: one bounded `gh` call per unseen workspace
    // root, memoized for the process. Never a directory basename.
    let unresolved: Vec<String> = tracker
        .runs
        .values()
        .filter(|run| run.identity.repo.is_none())
        .map(|run| run.workspace_root.display().to_string())
        .collect();
    for root in unresolved {
        if let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, &root).await {
            for run in tracker.runs.values_mut() {
                if run.workspace_root.display().to_string() == root {
                    run.identity.repo = Some(slug.clone());
                }
            }
        }
    }

    let projects: ProjectsDir = crate::transcript_tokens::claude_projects_dir();
    let now = Utc::now();
    let mut records = Vec::new();
    for run in tracker.runs.values_mut() {
        if newly_dropped > 0 {
            run.dropped_events = run.dropped_events.saturating_add(newly_dropped);
            records.push(
                run.status(OutputCategory::Gap, now, Coverage::Degraded, RunState::Running)
                    .with_gap("export_queue_overflow", run.dropped_events),
            );
        }
        if !run.supported() {
            // An unsupported runtime already said so at open time. It is
            // never read, and it never heartbeats as if it were live.
            continue;
        }
        let Some(projects) = projects.as_deref() else {
            continue;
        };
        records.extend(advance_run(run, projects, now));
        if now
            .signed_duration_since(run.last_content_at)
            .to_std()
            .unwrap_or_default()
            >= resolved.heartbeat
        {
            run.last_content_at = now;
            let coverage = if run.ever_covered {
                Coverage::Live
            } else {
                Coverage::Degraded
            };
            records.push(run.status(OutputCategory::Heartbeat, now, coverage, RunState::Idle));
        }
    }
    for record in records {
        sink.push(record);
    }
}

/// Read every stream of one run and produce its records. Pure file I/O over
/// state owned by `run`; extracted so the tests can drive it without a bus,
/// a sink or a tokio runtime.
fn advance_run(run: &mut Run, projects_dir: &Path, now: DateTime<Utc>) -> Vec<SessionOutputRecord> {
    let Some(issue) = run.identity.issue else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let streams = claude::discover(projects_dir, &run.workspace_root, issue);
    if !streams.is_empty() {
        // A source was located: coverage is `live` from here on, even across
        // a quiet interval. Until then heartbeats stay `degraded`, so "we
        // cannot find this run's output" never reads as "this run is quiet".
        run.ever_covered = true;
    }
    for (stream_id, path) in streams {
        if !run.cursors.contains_key(&stream_id) {
            // First sight of this stream — the one place the (relatively
            // expensive) whole-transcript parse runs. Identity comes from
            // #9445's own resolver, so this producer and `session.summary`
            // cannot disagree about whose session this is.
            let context = claude::context_of(&path, now);
            if run.identity.session_id.is_none() {
                run.identity.session_id = Some(stream_id.clone());
            }
            if run.identity.role.is_none() {
                run.identity.role = context.role;
            }
            // The transcript's own resolved slug wins over the tick's `gh`
            // lookup: it is the canonical #9472 path, and it is already here.
            // Neither is ever a directory basename.
            if run.identity.repo.is_none() {
                run.identity.repo = context.repo;
            }
            if run.identity.session_kind.is_none() {
                run.identity.session_kind = context.session_kind;
            }
            run.cursors
                .insert(stream_id.clone(), (path, claude::Cursor::default()));
        }
        let identity = run.identity.clone();
        let Some((path, cursor)) = run.cursors.get_mut(&stream_id) else {
            continue;
        };
        let path = path.clone();
        let pass = cursor.advance(&path, &stream_id, &identity, now);
        if let Some((reason, dropped)) = pass.gap {
            run.dropped_events = run.dropped_events.saturating_add(dropped);
            let record = run
                .status(OutputCategory::Gap, now, Coverage::Degraded, RunState::Running)
                .with_gap(reason, run.dropped_events);
            out.push(record);
        }
        if !pass.records.is_empty() {
            run.last_content_at = now;
            // Latency sampling happens here, over the records actually
            // produced, so the measurement can never disagree with what was
            // published. A replayed backlog line is refused by the window
            // rather than filtered here — the freshness rule lives in one
            // place (`latency::LagWindow::observe`).
            for record in &pass.records {
                run.lag
                    .observe(record.source_at, record.observed_at, run.watch_since);
            }
            out.extend(pass.records);
        }
    }
    out
}

#[cfg(test)]
#[path = "session_output/tests.rs"]
mod tests;

/// Live end-to-end verification, `#[ignore]`d — needs a reachable collector
/// AND the `otlp` feature (the real exporter lives behind it). See the module
/// docs for the invocation.
#[cfg(all(test, feature = "otlp"))]
#[path = "session_output/e2e_tests.rs"]
mod e2e;
