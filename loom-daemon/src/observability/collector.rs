//! Bus-subscriber telemetry collector (Epic #4702, Phase 1 — issue #4705).
//!
//! Mirrors [`crate::safehouse`]'s "subscribe to the existing bus, add no new
//! call sites" design: this module adds **zero** new emit sites anywhere
//! else in the daemon. It subscribes to the already-frozen `sweep.global.
//! dispatch` / `sweep.issue.*` topics (`event_bus.rs`'s taxonomy), maps each
//! relevant event to the [`crate::telemetry`] schema's record types, resolves
//! the emitting repo's `owner/repo` slug + [`RepoVisibility`], and pushes the
//! resulting [`TelemetryEnvelope`]s onto the [`DurableQueue`] for
//! [`super::sender`] to drain. It also periodically samples host-level
//! records (`tokens.snapshot`, `host.health`) that have no corresponding bus
//! event.
//!
//! # Best-effort, in-memory correlation
//!
//! [`Event::SweepPhase`] / [`Event::SweepExited`] / [`Event::SweepCrashed`]
//! carry an `issue` number but not the `sweep_id` the schema's lifecycle
//! records require, so this module tracks a small in-memory `(repo, issue) ->
//! (sweep_id, started_at, trace_context)` map ([`DispatchState`]), populated on
//! `sweep.global.dispatch` and consulted (then cleared) on the terminal
//! event. Like every other in-process daemon tracker (e.g.
//! `work_finder`'s per-root state maps), this resets across a daemon restart.
//!
//! A sweep already in flight when the collector starts therefore has no
//! dispatch to correlate against. Since Issue #8720 the map is re-seeded from
//! the owning registry's own **adoption evidence**
//! ([`crate::sweep_registry::SweepRegistry::tracked_sweep_identity`]) when
//! there is any, so an adopted sweep's `sweep.phase`/`sweep.completed`/
//! `sweep.outcome` records carry the same id this daemon already reports for
//! it in `host.health`'s `active_sweep_ids` and its `sweep.identity` record,
//! and the same start instant adoption admitted it with. Only an event with
//! **no** such evidence — a genuinely untracked issue — still degrades to a
//! synthesized `unknown-issue-{N}` sweep id and a zero `total_duration_sec`
//! rather than failing to emit at all: a degraded record beats a silently
//! dropped one for a telemetry pipeline. Nothing on either path replays a
//! `sweep.started` record for a sweep that started before this process did.
//!
//! # Terminal outcome: `SweepExited`/`SweepCrashed` only
//!
//! The reaper always emits `Event::SweepGlobalCompleted` alongside
//! `SweepExited`/`SweepCrashed` for the same terminal transition
//! (`sweep_registry.rs`) — `SweepGlobalCompleted` carries no `issue` number
//! and would double-emit the same outcome, so this collector does not
//! subscribe to `sweep.global.completed` at all.
//!
//! # `sweep.outcome`: the reaper's outcome journal wins, this path defers
//!
//! This path's own `sweep.outcome` record (built in `terminal_records`) is
//! deliberately impoverished — no `pr_number`, no `config`, no per-phase
//! breakdown — because the reaper's durable outcome journal
//! (`sweep_registry::outcome_journal`) writes the authoritative rich record
//! for the SAME `sweep_id`, synchronously, before it ever emits the bus event
//! this collector reacts to. Since the backend's `sweep.outcome` ingest keeps
//! only the first writer per `sweep_id` (Issue #9477), `handle_event` drops
//! this path's copy for every sweep_id the journal will also cover, and keeps
//! it only for the synthesized `unknown-issue-{N}` fallback the journal never
//! writes under — see [`suppress_journal_owned_outcome`]'s own doc for the
//! full reasoning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::event_bus::{EventBus, RecvError};
use crate::telemetry::{
    is_path_shaped_repo, visibility::derive_visibility, AdmissionBrakeSummary, HostHealthRecord,
    HostProtectionSummary, ManagedRepoEntry, MemoryPressureSummary, PhaseDuration, RepoVisibility,
    RoleTickFailureEntry, RoleTickHealth, SweepResult, SweepStartedRecord, TelemetryEnvelope,
    TelemetryRecord, TokenAccountState, TokenSnapshotRecord,
};
use crate::tokens_pool::{account_inventory, health_snapshot, AccountProvider};
use crate::types::{Event, RoleTickRecord, SweepKind};
use crate::workspace_pool::WorkspacePool;

use super::queue::QueueSink;
mod correlation;
mod identity;
pub(crate) type DispatchKey = (String, u32);

/// Timeout on the `gh repo view` slug lookup — generous but bounded so a
/// wedged `gh` cannot stall the collector loop indefinitely.
const SLUG_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// In-flight sweep state tracked purely in memory (see module docs).
/// `pub(crate)` only because it appears in [`map_event_to_records`]'s signature
/// (#4863) — it is not part of any cross-module contract.
#[derive(Debug, Clone)]
pub(crate) struct DispatchState {
    sweep_id: String,
    started_at: DateTime<Utc>,
    trace_context: Option<crate::telemetry::trace::TraceContext>,
}

/// Spawn the collector task on the shared daemon runtime. Subscribes to the
/// frozen `sweep.global.dispatch` / `sweep.issue.*` topics and periodically
/// (every `snapshot_interval`) samples host-level records. `daemon_started_at`
/// is used only to compute `host.health`'s `uptime_sec` (approximated as this
/// task's own uptime — see [`super::spawn_task`]'s doc comment).
///
/// `queue` is the fan-out sink (Issue #8756): with N exporters configured
/// this is a [`super::queue::FanoutQueue`] cloning every envelope into each
/// per-exporter queue; with one exporter it is that exporter's
/// [`DurableQueue`] directly. The collector never knows the difference.
///
/// `workspace_pool` (Issue #4955) is this daemon's shared per-workspace
/// registry pool — consulted only at each periodic snapshot tick to populate
/// `host.health`'s `active_sweep_ids` with this host's authoritative
/// in-flight sweep-id set. See [`collect_active_sweep_ids`].
pub fn spawn_task(
    bus: &EventBus,
    queue: Arc<dyn QueueSink>,
    workspace_root: PathBuf,
    host_id: String,
    snapshot_interval: Duration,
    daemon_started_at: Instant,
    workspace_pool: Arc<WorkspacePool>,
) -> tokio::task::JoinHandle<()> {
    let subscription = bus.subscribe(["sweep.global.dispatch", "sweep.issue"]);
    tokio::spawn(run_collector(
        subscription,
        queue,
        workspace_root,
        host_id,
        snapshot_interval,
        daemon_started_at,
        workspace_pool,
    ))
}

async fn run_collector(
    mut subscription: crate::event_bus::Subscription,
    queue: Arc<dyn QueueSink>,
    workspace_root: PathBuf,
    host_id: String,
    snapshot_interval: Duration,
    daemon_started_at: Instant,
    workspace_pool: Arc<WorkspacePool>,
) {
    let mut dispatches: HashMap<DispatchKey, DispatchState> = HashMap::new();
    let mut slug_cache: HashMap<String, String> = HashMap::new();
    let mut identities = identity::Sampler::default();
    let mut identity_timer = tokio::time::interval(Duration::from_secs(5));
    identity_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut snapshot_timer = tokio::time::interval(snapshot_interval);
    snapshot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // GitHub quota gauges + breaker-skip flush (Issue #10022), OTLP-only.
    let mut ratelimit_timer = tokio::time::interval(super::ops::ratelimit::TICK);
    ratelimit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // First-boot backfill (Issue #5084): run once immediately, before the
    // first snapshot tick, so a sweep adopted across a daemon restart does
    // not wait a full `snapshot_interval` (5 minutes) for its terminal
    // record to reach the export queue. Blocking here is fine — this is a
    // bounded, local-disk-only read (no network, no `gh`) and runs once at
    // task startup.
    run_backfill(&queue, &workspace_root, &workspace_pool).await;

    loop {
        tokio::select! {
            biased;

            recv_result = subscription.recv() => {
                match recv_result {
                    Ok(event) => {
                        identities.observe(&event, &workspace_root);
                        handle_event(
                            event,
                            queue.as_ref(),
                            &workspace_root,
                            &host_id,
                            &mut dispatches,
                            &mut slug_cache,
                            &workspace_pool,
                        )
                        .await;
                    }
                    Err(RecvError::Closed) => {
                        log::debug!("observability: event bus closed; collector stopping");
                        break;
                    }
                    Err(_) => {}
                }
            }

            _ = identity_timer.tick() => {
                identities.sample(queue.as_ref(), &host_id, &workspace_pool, &mut slug_cache)
                    .await;
            }

            _ = ratelimit_timer.tick() => {
                super::ops::ratelimit::record(&workspace_root).await;
            }

            _ = snapshot_timer.tick() => {
                sample_snapshots(
                    queue.as_ref(),
                    &workspace_root,
                    &host_id,
                    daemon_started_at,
                    &workspace_pool,
                    &mut slug_cache,
                )
                .await;
                // Periodic backfill (Issue #5084), same cadence as the host
                // snapshot: self-heals any terminal record the live
                // event-bus path missed or mis-attributed, not just the
                // first-boot case above (e.g. a sweep whose terminal
                // transition raced this task's own startup).
                run_backfill(&queue, &workspace_root, &workspace_pool).await;
            }
        }
    }
}

/// Run one [`super::backfill::run_backfill_pass_all`] pass, dispatched
/// through `spawn_blocking` (Issue #5084): the backfill path is synchronous
/// disk I/O (journal reads, queue pushes, cursor persistence) with no `.await`
/// points of its own, so running it inline on the collector's async task
/// would block that task's executor thread for the duration — `spawn_blocking`
/// keeps it off the reactor, mirroring how [`sample_host_health`] already
/// dispatches its own blocking CPU probe.
async fn run_backfill(
    queue: &Arc<dyn QueueSink>,
    workspace_root: &Path,
    workspace_pool: &Arc<WorkspacePool>,
) {
    let queue = queue.clone();
    let workspace_root = workspace_root.to_path_buf();
    let workspace_pool = workspace_pool.clone();
    let processed = tokio::task::spawn_blocking(move || {
        super::backfill::run_backfill_pass_all(&workspace_root, &workspace_pool, queue.as_ref())
    })
    .await
    .unwrap_or_else(|error| {
        log::warn!("observability: backfill task panicked: {error}");
        0
    });
    if processed > 0 {
        log::info!("observability: backfill pass enqueued {processed} previously-unexported sweep outcome(s)");
    }
}

async fn handle_event(
    event: Event,
    queue: &dyn QueueSink,
    default_workspace_root: &Path,
    host_id: &str,
    dispatches: &mut HashMap<DispatchKey, DispatchState>,
    slug_cache: &mut HashMap<String, String>,
    workspace_pool: &WorkspacePool,
) {
    let Some(issue) = event_issue(&event) else {
        return;
    };
    let workspace_path = event_repo_path(&event)
        .unwrap_or_else(|| default_workspace_root.to_string_lossy().to_string());
    let Some(slug) = resolve_repo_slug_cached(slug_cache, &workspace_path).await else {
        log::debug!(
            "observability: could not resolve a repo slug for {workspace_path}; dropping \
             record(s) for issue #{issue}"
        );
        return;
    };
    let visibility = resolve_visibility(&slug).await;
    let root = Path::new(&workspace_path);
    let mut envelopes = correlation::map_envelopes(
        &event,
        issue,
        &slug,
        visibility,
        root,
        host_id,
        dispatches,
        // Lazily evaluated: `map_envelopes` only asks when its own correlation
        // map has nothing, so an ordinary dispatched sweep's every phase event
        // costs no registry lock at all.
        &|| registry_evidence(workspace_pool, root, issue),
    );
    attach_outcome_usage(&mut envelopes, root, issue, event_death_class(&event)).await;
    // Issue #9477: drop this path's own `sweep.outcome` for every sweep_id
    // the durable outcome journal will also cover — see
    // `suppress_journal_owned_outcome`'s own doc for why.
    suppress_journal_owned_outcome(&mut envelopes);
    for envelope in envelopes {
        queue.offer(envelope);
    }
}

/// Drop this collector's own `sweep.outcome` envelope for every sweep_id the
/// reaper's durable outcome journal will (or already did) cover — Issue
/// #9477.
///
/// # The race this closes
///
/// The backend ingests `sweep.outcome` with `INSERT OR IGNORE` against a
/// partial `UNIQUE(kind, sweep_id)` index — `idx_records_terminal_sweep_once`,
/// declared in `loom-ui:migrations/0002_idempotent_terminal_records.sql` and
/// applied by `loom-ui:src/index.ts` — so the FIRST writer for a given
/// `sweep_id` wins and every later one is silently absorbed. Two independent
/// paths write that same `(kind="sweep.outcome", sweep_id)` for one terminal
/// sweep transition:
///
/// - **this live event-bus path** (`terminal_records`, below): fires
///   synchronously off `Event::SweepExited`/`SweepCrashed`, with no
///   `pr_number`, empty `phase_durations`, and no `config`/`failure_class`/
///   `judge_verdicts`/`doctor_cycles`/`complexity`;
/// - **the reaper's outcome journal**
///   (`sweep_registry::outcome_journal::append_outcome_telemetry_journal`),
///   drained on a periodic cadence by `super::backfill` — the RICH record,
///   with every one of those fields populated from state the reaper already
///   holds.
///
/// The reaper always calls `append_outcome_journal` synchronously BEFORE it
/// emits the paired `SweepExited`/`SweepCrashed` bus event for the exact same
/// transition (`sweep_registry::reaper`), so by the time this collector's
/// live path even sees the event, the rich journal line already exists on
/// disk — it is simply not exported yet, since the backfill drain that reads
/// it only runs on a periodic tick (first-boot, then every
/// `snapshot_interval`), while this live path pushes to the export queue
/// immediately. The live record therefore reaches the backend first on
/// (almost) every sweep, and the rich journal record loses the `INSERT OR
/// IGNORE` race and is silently discarded.
///
/// # What that costs, measured
///
/// Over the 987 `sweep.outcome` lines in this repo's own outcome-telemetry
/// journal (`.loom/logs/sweep-outcome-telemetry.jsonl{,.1}`), the journal
/// record carries `config` on 100%, `pr_number` on 27.9%, a real classifier
/// `failure_class` on 19.1%, `phase_durations` on 67.8%, `lines_added` on
/// 70.0% and `model` on 95.2%. The live record this path builds for those
/// same 987 `sweep_id`s carries `config: {}`, `pr_number: None`,
/// `phase_durations: []`, `lines_added: None` and `model: None` on 100% of
/// them — by construction, see `terminal_records`. Not one of the 987 journal
/// lines is under a synthesized id, so the live record collides with the rich
/// one on *every* sweep this host reaped. That is the low field-completeness
/// #9441/#9440 measured at the backend, from this side of the wire.
///
/// # Why suppression, not enrichment (Option A over Option C)
///
/// Enriching this live path to carry the same fields would duplicate the
/// journal's own logic (a second source of truth for `config`/
/// `judge_verdicts`/`complexity`/…) for the larger of the two diffs. This
/// path already KNOWS, from its own correlation state, whether the journal
/// will emit under the same id — that is exactly what a tracked
/// [`DispatchState`] (dispatched here, or adopted via registry evidence, see
/// `correlation::recover_adopted_dispatch`) means. So the cheapest sound fix
/// is to drop the redundant, poorer copy here and let the richer one win by
/// simply not having a competitor.
///
/// # The one case this still emits: the synthesized fallback id
///
/// [`unknown_sweep_id`] is used only when this process has NO correlated
/// identity for the terminal transition (a daemon restart with no adoption
/// evidence). The reaper still journals the sweep under its OWN real
/// `sweep_id` in that case — never under `unknown-issue-{N}` — so the two
/// records do not collide at ingest at all; suppressing the fallback record
/// would only lose data, not avoid a race. A record whose own `sweep_id`
/// equals [`unknown_sweep_id`] of its `issue` is therefore never suppressed.
///
/// # What this deliberately does NOT touch: `sweep.completed`
///
/// `sweep.completed` is under the *same* partial unique index, and
/// `super::backfill`'s `synthesize_completed` does rebuild one per journal
/// line, so the live copy wins that race too — but there it is the right
/// winner, and the scope stops here. The synthesized record carries exactly
/// the same fields the live one does (`repo`/`visibility`/`issue`/`sweep_id`/`result`
/// plus `tokens_by_model`), so nothing is lost by the live copy landing
/// first, while `sweep.completed` is the kind the dashboard reads as the
/// *moment* a sweep ended — delaying it by up to a backfill tick would trade
/// away liveness for no completeness gain at all. `sweep.outcome`, the
/// analytics record, has the opposite tradeoff: nobody watches it live, and
/// its fields are the whole point.
///
/// # The accepted tradeoff
///
/// This makes the journal the *sole* source of `sweep.outcome`, which is what
/// `super::backfill`'s own module doc already declares it ("the local outcome-
/// telemetry journal is the queue of record"). The one shape that regresses is
/// a terminal transition whose journal append itself fails — best-effort by
/// contract, logged at `warn` with the path — which previously still produced
/// this path's thin record and now produces none. That is a logged, visible
/// disk failure, and the row it would have saved carried no `pr_number`, no
/// `config` and no phases, so the trade is a rare thin row against a rich row
/// on (almost) every sweep.
fn suppress_journal_owned_outcome(envelopes: &mut Vec<TelemetryEnvelope>) {
    envelopes.retain(|envelope| match &envelope.record {
        TelemetryRecord::SweepOutcome(record) => record.sweep_id == unknown_sweep_id(record.issue),
        _ => true,
    });
}

/// Fill in the token counters and `tokens_status` of any `sweep.outcome`
/// envelope this event produced (Issue #9440).
///
/// # Why this is a post-pass rather than part of the mapping
///
/// [`map_event_to_records`] is a pure, I/O-free function, and its terminal
/// branch hard-coded `tokens_in: None` precisely because it has no workspace
/// root in scope. Keeping it pure is worth more than threading a reader into
/// it: everything the usage read needs is already *on the record it produced*.
/// `total_duration_sec` plus "this terminal transition is happening now" is the
/// same window the durable journal path computes from its registry entry, so
/// this reconstructs rather than re-correlates — see
/// [`crate::sweep_usage::window`].
///
/// The read itself is bounded local-disk work (transcript/session-store scans),
/// so it goes through `spawn_blocking` rather than running on the collector's
/// reactor thread. Terminal events are once-per-sweep, so this adds no
/// per-phase cost at all.
///
/// Fail-open by construction: a panicking or failed blocking hop leaves the
/// envelope exactly as the mapping built it (absent counters, absent status),
/// which is the pre-#9440 shape — never a fabricated zero.
async fn attach_outcome_usage(
    envelopes: &mut [TelemetryEnvelope],
    root: &Path,
    issue: u32,
    death_class: Option<String>,
) {
    // One read per terminal event, applied to both records of the pair — the
    // `sweep.completed` sibling carries the same per-model breakdown, and
    // re-deriving it would be a second scan that could disagree with the first.
    let Some(duration_sec) = envelopes
        .iter()
        .find_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepOutcome(record) => Some(record.total_duration_sec),
            _ => None,
        })
    else {
        return;
    };
    let owned_root = root.to_path_buf();
    let resolved = tokio::task::spawn_blocking(move || {
        // Same usage-source resolution the journal path uses: the
        // `# LOOM_RUNTIME_RESOLVED` marker in the sweep's own log, which a
        // legacy adapter writes even when it writes no launch record.
        let usage_runtime = crate::usage_source::sweep_usage_runtime(None, &owned_root, issue);
        crate::sweep_usage::resolve(
            usage_runtime.as_deref(),
            &owned_root,
            issue,
            crate::sweep_usage::window(None, duration_sec),
            death_class.as_deref(),
        )
    })
    .await;
    let Ok(usage) = resolved else {
        log::warn!(
            "observability: sweep usage read for issue #{issue} panicked; leaving sweep.outcome \
             token counters unset (#9440)"
        );
        return;
    };
    for envelope in envelopes.iter_mut() {
        match &mut envelope.record {
            TelemetryRecord::SweepOutcome(record) => {
                record.tokens_in = usage.tokens_in;
                record.tokens_out = usage.tokens_out;
                record.models_used =
                    crate::sweep_registry::models_used_from(usage.tokens_by_model.as_deref());
                record.tokens_by_model = usage.tokens_by_model.clone();
                record.tokens_status = Some(usage.status);
                record.tokens_status_reason = usage.reason.clone();
                // Issue #9477/#9486: #9440 filled the totals above but left
                // `tokens_unattributed` at `None`, which breaks the schema's
                // own documented invariant — `Σ phase_durations[*].tokens_in
                // + tokens_unattributed.tokens_in == tokens_in`, and the
                // `_out` twin — for every record this path emits, because
                // `terminal_records` builds an EMPTY `phase_durations` and
                // nothing else ever fills it. An empty Σ means the whole
                // measured total is the remainder, which is exactly the case
                // `tokens_unattributed`'s doc calls "the point of the field".
                //
                // Written as the same saturating Σ-and-subtract fold the
                // durable journal path uses
                // (`outcome_journal::append_outcome_telemetry_journal`)
                // rather than as a direct copy of the totals, so the invariant
                // survives a future change that DOES give this path per-phase
                // entries instead of silently becoming a double count.
                //
                // Omitted — never `Some(0, 0)` — when the totals themselves
                // are unknown, matching `tokens_in`/`tokens_out`'s own
                // "unknown != zero" contract; a *measured* zero still yields
                // `Some(0, 0)`, since zero of zero really is accounted for.
                record.tokens_unattributed = phase_remainder(record);
            }
            TelemetryRecord::SweepCompleted(record) => {
                record.tokens_by_model = usage.tokens_by_model.clone();
            }
            _ => {}
        }
    }
}

/// The part of `record`'s sweep totals that no `phase_durations` entry
/// accounts for — [`crate::telemetry::SweepOutcomeRecord::tokens_unattributed`]'s
/// own definition (Issue #9443), computed here for the live path (#9477).
///
/// Deliberately the same shape as the durable journal path's fold in
/// [`crate::sweep_registry::SweepRegistry::append_outcome_telemetry_journal`]:
/// `None` whenever the totals are unknown (there is nothing to take a
/// remainder of, and `0` would falsely claim the phases cover everything), and
/// a saturating subtraction otherwise, so the two emit paths can never disagree
/// about what the remainder means.
fn phase_remainder(
    record: &crate::telemetry::SweepOutcomeRecord,
) -> Option<crate::telemetry::TokenTotals> {
    let (total_in, total_out) = record.tokens_in.zip(record.tokens_out)?;
    let (attributed_in, attributed_out) = record
        .phase_durations
        .iter()
        .filter_map(PhaseDuration::token_split)
        .fold((0u64, 0u64), |(sum_in, sum_out), (tokens_in, tokens_out)| {
            (sum_in.saturating_add(tokens_in), sum_out.saturating_add(tokens_out))
        });
    Some(crate::telemetry::TokenTotals {
        tokens_in: total_in.saturating_sub(attributed_in),
        tokens_out: total_out.saturating_sub(attributed_out),
    })
}

/// The reaper's pre-flight/crash classification carried on a terminal event,
/// when it derived one (Issue #9440) — the signal that separates "never
/// spawned, so truly zero" from every other failure. Same precedence the
/// durable journal uses: `death_class` (the pre-flight classifier) first, then
/// the crash classification.
fn event_death_class(event: &Event) -> Option<String> {
    match event {
        Event::SweepExited { death_class, .. } => death_class.clone(),
        Event::SweepCrashed {
            death_class,
            classification,
            ..
        } => death_class.clone().or_else(|| classification.clone()),
        _ => None,
    }
}

/// This host's authoritative identity for the live sweep of `issue` in the
/// workspace that emitted the event (Issue #8720), or `None` when the owning
/// registry has no unambiguous non-terminal entry for it.
///
/// Looked up by **exact** root path rather than by scanning every provisioned
/// registry: `workspace_root` here is the event's own `repo` stamp, which
/// `SweepRegistry::emit_event` fills from `config().workspace_root.display()`
/// — the same `PathBuf` [`WorkspacePool`] keys that registry under. Scanning
/// (and locking) every other repo's registry to answer a question about this
/// one would only add ways for another repo's same-numbered issue to become a
/// candidate. A workspace with no provisioned registry (an event from a
/// standalone registry, e.g. in tests) simply has no evidence, and the caller
/// keeps today's synthesized-id fallback.
fn registry_evidence(
    workspace_pool: &WorkspacePool,
    workspace_root: &Path,
    issue: u32,
) -> Option<crate::sweep_registry::TrackedSweepIdentity> {
    let registry = workspace_pool.provisioned_registry_for(workspace_root)?;
    let identity = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tracked_sweep_identity(issue);
    identity
}

/// The issue this event concerns, or `None` for an event kind this collector
/// does not translate into a telemetry record (e.g. `SweepBlocker`, a
/// PR-set dispatch).
fn event_issue(event: &Event) -> Option<u32> {
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(issue),
            ..
        } => Some(*issue),
        Event::SweepPhase { issue, .. }
        | Event::SweepExited { issue, .. }
        | Event::SweepCrashed { issue, .. } => Some(*issue),
        _ => None,
    }
}

/// The owning workspace-root path stamped on the event, when present (Issue
/// #3929's `repo` field — a filesystem path, not a forge `owner/repo` slug;
/// [`resolve_repo_slug_cached`] converts it).
fn event_repo_path(event: &Event) -> Option<String> {
    match event {
        Event::SweepGlobalDispatch { repo, .. }
        | Event::SweepPhase { repo, .. }
        | Event::SweepExited { repo, .. }
        | Event::SweepCrashed { repo, .. } => repo.clone(),
        _ => None,
    }
}

/// Pure event -> telemetry-record mapping (no I/O), given an already-resolved
/// `repo` slug and `visibility`. Returns zero, one, or two records — a
/// terminal event yields both a `sweep.completed` record (mirrors the frozen
/// SSE moment) and the richer `sweep.outcome` record.
///
/// `pub(crate)` so the registry's own emit-site tests (#4863) can drive a
/// *genuinely emitted* [`Event::SweepPhase`] through this mapping end-to-end
/// instead of asserting against a hand-built event fixture — the exact gap that
/// let `sweep.phase` be defined, mapped, and unit-tested here while never being
/// published by production code.
pub(crate) fn map_event_to_records(
    event: &Event,
    issue: u32,
    repo: &str,
    visibility: RepoVisibility,
    dispatches: &mut HashMap<DispatchKey, DispatchState>,
) -> Vec<TelemetryRecord> {
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(_),
            sweep_id,
            runtime,
            story_points,
            ..
        } => {
            let started_at = Utc::now();
            dispatches.insert(
                (repo.to_owned(), issue),
                DispatchState {
                    sweep_id: sweep_id.clone(),
                    started_at,
                    trace_context: None,
                },
            );
            vec![TelemetryRecord::SweepStarted(SweepStartedRecord {
                // #9432: the assigned size of the work now in flight, resolved
                // at dispatch from the issue's single `points:*` label. Copied
                // straight through — the collector never reads the forge, so an
                // absent value here IS an absent attribute, never a zero.
                story_points: *story_points,
                repo: repo.to_string(),
                visibility,
                issue,
                sweep_id: sweep_id.clone(),
                started_at,
                model: None,
                effort: None,
                // The dispatch event already names the admitted runtime
                // adapter; carrying it here is what lets the dashboard say
                // *which agent* is working each in-flight sweep.
                runtime: runtime.clone(),
            })]
        }
        Event::SweepGlobalDispatch { .. } => Vec::new(),
        Event::SweepPhase { phase, .. } => {
            let sweep_id = dispatches
                .get(&(repo.to_owned(), issue))
                .map(|d| d.sweep_id.clone())
                .unwrap_or_else(|| unknown_sweep_id(issue));
            vec![TelemetryRecord::SweepPhase(
                crate::telemetry::SweepPhaseRecord {
                    repo: repo.to_string(),
                    visibility,
                    issue,
                    sweep_id,
                    phase: phase.clone(),
                    entered_at: Utc::now(),
                },
            )]
        }
        Event::SweepExited {
            exit_code,
            duration_sec,
            ..
        } => {
            let dispatch = dispatches.remove(&(repo.to_owned(), issue));
            let sweep_id = dispatch
                .as_ref()
                .map(|d| d.sweep_id.clone())
                .unwrap_or_else(|| unknown_sweep_id(issue));
            let result = if *exit_code == Some(0) {
                SweepResult::Success
            } else {
                SweepResult::Failure
            };
            terminal_records(repo, visibility, issue, sweep_id, result, *duration_sec, None)
        }
        Event::SweepCrashed { .. } => {
            let dispatch = dispatches.remove(&(repo.to_owned(), issue));
            let sweep_id = dispatch
                .as_ref()
                .map(|d| d.sweep_id.clone())
                .unwrap_or_else(|| unknown_sweep_id(issue));
            let duration_sec = dispatch
                .as_ref()
                .map(|d| (Utc::now() - d.started_at).num_seconds().max(0))
                .unwrap_or(0);
            terminal_records(
                repo,
                visibility,
                issue,
                sweep_id,
                SweepResult::Failure,
                duration_sec,
                None,
            )
        }
        _ => Vec::new(),
    }
}

fn unknown_sweep_id(issue: u32) -> String {
    format!("unknown-issue-{issue}")
}

/// Build the paired `sweep.completed` + `sweep.outcome` records a terminal
/// event yields.
///
/// Issue #9442: the event's `repo` is the registry's workspace path, which is
/// never written to telemetry. A path-shaped value is resolved locally —
/// `git remote get-url origin` parsed to `owner/name` — and a slug that still
/// will not resolve leaves `repo` absent with `repo_unresolved` set, on both
/// records.
fn terminal_records(
    repo: &str,
    visibility: RepoVisibility,
    issue: u32,
    sweep_id: String,
    result: SweepResult,
    total_duration_sec: i64,
    pr_number: Option<u32>,
) -> Vec<TelemetryRecord> {
    let (repo, repo_unresolved) = if is_path_shaped_repo(repo) {
        crate::init::git::extract_repo_info(Path::new(repo))
            .map(|(o, r)| (Some(format!("{o}/{r}")), false))
            .unwrap_or((None, true))
    } else {
        (Some(repo.to_string()), false)
    };
    let completed_at = Utc::now();
    // Issue #9441: this live path sees only result/duration/PR — no classifier
    // label, no sampled phases, no forge read — so it classifies from exactly
    // those. It still satisfies both invariants: `landed` ⇔ a PR is present,
    // and the classifier hands back the mandatory `failure_class` (a
    // synthesized `unclassified:*` label here, since nothing classified this
    // transition) together with the disposition.
    let (disposition, failure_class) =
        crate::telemetry::classify_disposition(&crate::telemetry::DispositionSignals {
            result,
            pr_number,
            failure_class: None,
            phase_durations: &[],
            total_duration_sec,
            judge_verdicts: None,
            doctor_cycles: None,
            issue_end_state: None,
        });
    vec![
        TelemetryRecord::SweepCompleted(crate::telemetry::SweepCompletedRecord {
            repo: repo.clone(),
            visibility,
            issue,
            sweep_id: sweep_id.clone(),
            completed_at,
            result,
            // Left unset HERE and filled by `attach_outcome_usage` (Issue
            // #9440), which reads usage once for this terminal event and
            // applies it to both records of the pair. This function is
            // deliberately pure — it has no workspace root in scope — so the
            // read happens in the caller, off the reactor thread.
            tokens_by_model: None,
        }),
        TelemetryRecord::SweepOutcome(crate::telemetry::SweepOutcomeRecord {
            repo,
            repo_unresolved,
            visibility,
            issue,
            sweep_id,
            model: None,
            effort: None,
            config: std::collections::BTreeMap::new(),
            phase_durations: Vec::<PhaseDuration>::new(),
            total_duration_sec,
            result,
            disposition,
            pr_number,
            // Token fields (and `tokens_status`) are filled by
            // `attach_outcome_usage` in the caller — see `tokens_by_model` on
            // the paired `sweep.completed` record above (#9440). This path
            // emitted the MAJORITY of the fleet's `sweep.outcome` records and
            // was the larger half of the 10.3% measurement rate #9440 found.
            tokens_in: None,
            tokens_out: None,
            lines_added: None,
            lines_deleted: None,
            tokens_by_model: None,
            // Filled by `attach_outcome_usage` in the caller once the sweep
            // totals are known (Issue #9440/#9477) — this construction site
            // has no workspace root in scope, so there is nothing to take a
            // remainder of yet. `phase_durations` above is always empty on
            // this path, so that fill-in is the WHOLE total, not a partial
            // remainder — see `attach_outcome_usage`'s own comment.
            tokens_unattributed: None,
            failure_class,
            models_used: None,
            doctor_cycles: None,
            judge_verdicts: None,
            // Issue #8507: this live event-bus path has no launch-record log
            // path in scope, so the runtime labels stay deferred to the
            // durable journal path.
            runtime: None,
            provider: None,
            profile: None,
            complexity: None,
            // Issue #9432: like `complexity`, the story-point size is a forge
            // read this pure, reactor-thread mapping never makes. Absent, never
            // `0` — the reaper-side journal path (`outcome_journal`) resolves it
            // from the label list its own single REST read already carries, and
            // the in-flight `sweep.started` record for this same sweep carries
            // the size resolved at dispatch.
            story_points: None,
            tokens_status: None,
            tokens_status_reason: None,
            // Issues #9444/#9465/#9466: terminal facts the reaper-side journal
            // (the real `sweep.outcome`) computes. Absent, never fabricated.
            attempt_index: None,
            previous_sweep_id: None,
            trigger: None,
            rework_events: None,
            pr_numbers: None,
            hw_lines_added: None,
            hw_lines_deleted: None,
            hw_files: None,
            generated_lines: None,
            test_lines: None,
        }),
    ]
}

/// [`fetch_repo_slug`] with a process-lifetime cache keyed by workspace root
/// path (a repo's slug does not change while the daemon runs — same
/// rationale as [`crate::safehouse`]'s own `slug_cache`).
///
/// With repo facts on (W3a) the slug comes from the fingerprint-invalidated
/// repo-facts record instead (`gh repo view` semantics, no forge call when
/// warm), so a moved remote is seen; `Legacy` keeps the path below.
pub(super) async fn resolve_repo_slug_cached(
    cache: &mut HashMap<String, String>,
    workspace_root: &str,
) -> Option<String> {
    if crate::forge_repo_facts::enabled() {
        use crate::forge_repo_facts::{canonical, GhRepoEnv, Lookup};
        let root = std::path::PathBuf::from(workspace_root);
        match tokio::task::spawn_blocking(move || canonical(&root, GhRepoEnv::Ignore)).await {
            Ok(Lookup::Fact(f)) => {
                let slug = f.full_name();
                cache.insert(workspace_root.to_string(), slug.clone());
                return Some(slug);
            }
            // `Unavailable` (a failed verify, or its backoff) keeps the
            // last-known slug or the legacy lookup rather than dropping
            // the record.
            Ok(Lookup::Unavailable | Lookup::Legacy) | Err(_) => {}
        }
    }
    if let Some(slug) = cache.get(workspace_root) {
        return Some(slug.clone());
    }
    let slug = fetch_repo_slug(Path::new(workspace_root)).await?;
    cache.insert(workspace_root.to_string(), slug.clone());
    Some(slug)
}

/// Best-effort `gh repo view --json nameWithOwner --jq .nameWithOwner` lookup
/// for the forge `owner/repo` slug. Every failure (missing/erroring `gh`, a
/// timeout, an empty/malformed answer) degrades to `None` — the caller drops
/// the record rather than emitting a fabricated repo identity.
async fn fetch_repo_slug(workspace_root: &Path) -> Option<String> {
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    // #10089: through the facade (counted, bounded); it also points the child
    // at the owner-correct credential for a cross-owner managed repo (#5431).
    let op = Operation::new("collector.repo_slug");
    let outcome = GhInvocation::new(op, AccessIntent::Read, GhTarget::None, SLUG_FETCH_TIMEOUT)
        .forge_op(crate::forge_call_stats::ops::REPO_VIEW)
        .current_dir(workspace_root)
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .run_async()
        .await;
    let output = outcome.ok_output()?;
    let slug = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!slug.is_empty() && slug.contains('/')).then_some(slug)
}

/// [`derive_visibility`] blocks on a `gh api` probe, so it is dispatched
/// through `spawn_blocking` per its own doc guidance; a join failure (the
/// blocking task panicked) degrades to the private-safe default rather than
/// propagating. This path was previously silent — logged at `warn` (#6039) so
/// it is distinguishable in the daemon log from an ordinary probe failure
/// (which `visibility.rs` itself now logs) rather than looking identical to
/// "repo is actually private".
pub(super) async fn resolve_visibility(slug: &str) -> RepoVisibility {
    let owned = slug.to_string();
    match tokio::task::spawn_blocking(move || derive_visibility(&owned)).await {
        Ok(visibility) => visibility,
        Err(join_error) => {
            log::warn!(
                "observability: visibility probe task for {slug} panicked ({join_error}) — \
                 defaulting to private"
            );
            RepoVisibility::Private
        }
    }
}

/// Sample the two host-level record kinds that have no corresponding bus
/// event and push them onto `queue`. `slug_cache` is the same per-workspace
/// slug memoization `handle_event` uses, threaded through so the periodic
/// `managed_repos` roster sample does not re-shell out to `gh repo view` for
/// a workspace root already resolved this run.
async fn sample_snapshots(
    queue: &dyn QueueSink,
    workspace_root: &Path,
    host_id: &str,
    daemon_started_at: Instant,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    let mut token_record = sample_token_snapshot(workspace_root);
    // The Claude pool comes from `.ranking`; every other provider's pool
    // (`codex`, …) comes from the account registry + provider-health state.
    // Joined here, not inside `sample_token_snapshot`, so the ranking reader
    // stays a pure function of one file (and its tests stay hermetic on a
    // host that has Codex profiles provisioned machine-wide).
    token_record
        .accounts
        .extend(sample_registry_provider_accounts(workspace_root));
    // Token burn + per-provider pool state (Issue #8857), through the
    // OTLP-only ops sink — a no-op (no reads at all) without an OTLP exporter.
    super::ops::quota::record(workspace_root, &token_record.accounts).await;
    queue.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::TokensSnapshot(token_record)));
    // ONE `df -Pk` sample per tick feeds both host.health's GB fields and the
    // ops byte gauges (#8857 — it used to run twice).
    let root = workspace_root.to_path_buf();
    let worktree_volume =
        tokio::task::spawn_blocking(move || crate::disk_headroom::worktree_root_disk_bytes(&root))
            .await
            .unwrap_or((None, None));
    let health_record = sample_host_health(
        workspace_root,
        daemon_started_at,
        workspace_pool,
        slug_cache,
        worktree_volume,
    )
    .await;
    queue.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::HostHealth(health_record)));
    // Memory/swap/worktree-volume gauges (Issue #8860), same cadence, through
    // the OTLP-only ops sink — a no-op when no OTLP exporter is running.
    super::ops::host::record(worktree_volume).await;
    // The work finder's ranked ready queue (Issue #8852, phase 2) — native
    // HTTPS only, and only when the work finder has ticked since last time.
    super::queue_snapshot::record(workspace_pool, slug_cache).await;
    // Forge label-stage dwell (Issue #8929), OTLP-only: ETag-cached stage
    // listings plus a bounded per-item budget; a no-op without the ops sink.
    super::ops::stage_dwell::record(workspace_pool, slug_cache).await;
    // Merge-chain re-date pressure (Issue #10163), OTLP-only: local `git log`
    // reads, no forge call; a no-op without the ops sink.
    super::ops::redate_chain::record(workspace_pool).await;
    // Per-issue dispatch disposition spans (Issue #9222), OTLP-only: a no-op
    // without the ops sink, and without a new work-finder tick since the last
    // export pass.
    super::ops::disposition::record(slug_cache).await;
    // ETA (Issue #9289): review listings, outcome checks and re-estimates,
    // after `stage_dwell` so the ETag-cached listings are warm.
    super::eta::record(workspace_root, workspace_pool, slug_cache).await;
    // #10414: the ETA pass finished; a no-op unless ETA registered it.
    crate::task_liveness::beat_if_registered(crate::task_liveness::ETA_PASS);
    // This host's live estimate set (Issue #9329) — native HTTPS only, and
    // only when the set changed. After `eta::record` so it carries this
    // pass's estimates rather than the previous pass's.
    super::eta_snapshot::record().await;
    // ETA pipeline health gauges (Issue #10391), OTLP-only: local state and
    // file reads, no forge call; a no-op without the ops sink. After the
    // snapshot so it reports this pass's built snapshot.
    super::ops::eta_health::record(workspace_root).await;
}

/// Parse a `.ranking` row's binding-window reset text into the typed instant
/// `tokens.snapshot` carries (issue #4874). The writers emit a canonical
/// `%Y-%m-%dT%H:%M:%SZ` instant, but this is the trust boundary between an
/// on-disk file and the pushed telemetry, so an unparseable value degrades to
/// `None` ("unknown") rather than propagating junk to the dashboard's
/// countdown — the same "unknown, not a fabricated date" contract the
/// host-detail view already holds on the rendering side.
fn parse_reset_instant(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Read the resolved rotation-ranking file for `workspace_root` into a
/// [`TokenSnapshotRecord`]. `rank` is the account's position in the ranking
/// file (lower = preferred); an unreadable/missing/empty ranking yields an
/// empty `accounts` list rather than an error — mirrors
/// [`crate::capacity::read_ranking_at`]'s soft-fail contract.
///
/// `limit_window_reset_at` comes from the ranking row's optional reset field
/// (issue #4874) — the instant the window *currently gating that account*
/// rolls over (7d for an `exhausted` account, 5h otherwise; the writer resolves
/// this via [`crate::tokens_pool::check::limit_reset`]). Before that field
/// existed this was hardcoded `None`, which is why every exhausted account
/// fleet-wide reported no reset instant and the dashboard's countdown column
/// was permanently `—`.
///
/// `usage_fraction_weekly` (issue #9005) comes from the `.ranking.weekly.json`
/// sidecar the same `tokens check --ranking` run writes beside `.ranking`
/// ([`crate::tokens_pool::ranking_weekly`]) — `.ranking` itself has no room
/// for a fifth column. A row the sidecar does not name, or a missing/stale
/// sidecar, leaves the weekly axis absent rather than `0`.
fn sample_token_snapshot(workspace_root: &Path) -> TokenSnapshotRecord {
    let pool_dir = crate::tokens_pool::paths::resolve_tokens_dir(workspace_root);
    let ranking_path = pool_dir.join(".ranking");
    let mut accounts = Vec::new();
    if let Ok(contents) = std::fs::read_to_string(&ranking_path) {
        let weekly = crate::tokens_pool::ranking_weekly::read_weekly_utilization_sidecar(&pool_dir);
        for (index, line) in contents.lines().enumerate() {
            if !line.contains('|') {
                continue;
            }
            let Some(row) = crate::tokens_pool::select::parse_ranking_line(line) else {
                continue;
            };
            let exhausted = !crate::capacity::AccountHealth::parse(&row.status).is_healthy();
            let usage_fraction_weekly = weekly.get(&row.name).copied();
            accounts.push(TokenAccountState {
                account: row.name,
                provider: AccountProvider::Claude.to_string(),
                rank: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                usage_fraction: row.util_5h,
                usage_fraction_weekly,
                limit_window_reset_at: parse_reset_instant(row.limit_reset.as_deref()),
                exhausted,
            });
        }
    }
    TokenSnapshotRecord {
        captured_at: Utc::now(),
        accounts,
    }
}

/// The non-Claude providers' accounts (`codex`, …) from the multi-provider
/// account registry, each with the account-wide eligibility verdict the
/// daemon's own selector would give it right now.
///
/// These pools have no `.ranking` file: the registry knows *which* accounts
/// exist and the provider-health state file knows whether each is currently
/// held (`cooldown_until`, `ReauthRequired`), but neither measures a usage
/// fraction. So `rank`/`usage_fraction`/`usage_fraction_weekly` stay absent
/// ("unknown, not zero"), `exhausted` is `!is_eligible_at(now)`, and
/// `limit_window_reset_at` is the hold's deadline when there is one — the same
/// "when does this account's constraint lift?" meaning the Claude rows carry. A disabled account is not
/// part of the usable pool and is not reported; an unreadable registry or
/// health file degrades to no rows for that provider rather than an error,
/// matching the `.ranking` soft-fail above.
fn sample_registry_provider_accounts(workspace_root: &Path) -> Vec<TokenAccountState> {
    // One consistent read for the whole sample. A failed read is unknown
    // capacity; only a successfully read snapshot can imply no health hold.
    let Ok(health) = health_snapshot(workspace_root) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut accounts = Vec::new();
    for provider in AccountProvider::ALL {
        if provider == AccountProvider::Claude {
            continue;
        }
        let Ok(inventory) = account_inventory(workspace_root, provider) else {
            continue;
        };
        for descriptor in inventory.into_iter().filter(|account| account.enabled) {
            let account_health = health.get(&descriptor.id);
            let exhausted = account_health.is_some_and(|entry| !entry.is_eligible_at(now));
            let limit_window_reset_at = account_health
                .filter(|_| exhausted)
                .and_then(|entry| entry.cooldown_until)
                .and_then(|deadline| {
                    DateTime::<Utc>::from_timestamp(i64::try_from(deadline).ok()?, 0)
                });
            accounts.push(TokenAccountState {
                account: descriptor.id.name,
                provider: provider.to_string(),
                rank: None,
                usage_fraction: None,
                usage_fraction_weekly: None,
                limit_window_reset_at,
                exhausted,
            });
        }
    }
    accounts
}

/// Sample host CPU/disk headroom into a [`HostHealthRecord`], stamped with the
/// running binary's build identity. Every measured field is `Option` — an
/// unmeasurable probe stays absent rather than a fake zero (mirrors
/// `cpu_headroom`/`disk_headroom`'s own contract).
async fn sample_host_health(
    workspace_root: &Path,
    started_at: Instant,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
    worktree_volume_bytes: (Option<u64>, Option<u64>),
) -> HostHealthRecord {
    // CPU idle refresh can block ~1s on macOS (`iostat`) — dispatched through
    // `spawn_blocking` per the exact pattern `work_finder`'s dynamic-cap tick
    // already uses.
    let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
    // The host-distress breaker (#4235) is the daemon's own authoritative
    // "is this host refusing new work right now" signal — reused rather than
    // re-derived (#4975). `None` (no work-finder loop ever registered a
    // breaker on this host) reads as "not known to be halted", matching every
    // other unmeasurable field's "unknown != zero" contract.
    // #8478: the admission brake is the SECOND way this host can be refusing
    // dispatch, and the incident behind #8478 was invisible fleet-wide because
    // only the breaker fed `dispatch_halted`. Sampled before the halt pair so
    // the latter can fold it in.
    let admission_brake = sample_admission_brake(crate::admission_brake::global_snapshot()).await;
    let (dispatch_halted, halt_reason) = dispatch_halt_from_breaker(
        crate::host_breaker::global_snapshot(),
        admission_brake.as_ref(),
    );
    // Free AND total (#5356) come from the SAME `df -Pk` sample — one
    // subprocess spawn, not two — so the pair can never disagree about which
    // filesystem or point in time they describe. The caller took that sample
    // in bytes (#8857) so the ops gauges share it; GB here is the same integer
    // floor `disk_headroom::parse_df_*_gb` applies (`kb / 1024 / 1024`).
    let (worktree_root_free_gb, worktree_root_total_gb) = (
        worktree_volume_bytes
            .0
            .map(crate::disk_headroom::bytes_to_whole_gb),
        worktree_volume_bytes
            .1
            .map(crate::disk_headroom::bytes_to_whole_gb),
    );
    // Fleet captain (#8848): `is_captain` is this host's gate outcome against
    // `root`'s declared `fleet.captain`, and `armed_singleton_jobs` (#8901)
    // merges this process's own in-daemon registry with the durable
    // shell-arm registry a `loom-daemon fleet-captain <job>` invocation
    // writes — see `crate::fleet_captain`'s module doc, "Two arm registries".
    let captain_gate = crate::fleet_captain::resolve_gate_for_root(
        workspace_root,
        &crate::sweep_registry::host_identity(),
    );
    // Host memory-pressure state (the "what was the host doing when a role
    // tick died" slice). The probe shells to `vm_stat`/`sysctl` or reads
    // `/proc`, so it runs off the async runtime exactly like the disk probe
    // above; a failed sample degrades to all-`None` (host_pressure::sample
    // never panics), which the wire fields then omit — unknown != zero.
    let pressure = tokio::task::spawn_blocking(crate::host_pressure::sample)
        .await
        .unwrap_or_default();
    let (swap_in_bytes_per_sec, swap_out_bytes_per_sec) = swap_sample_rates(&pressure);
    // Export coverage (#10196): which exporters this process actually started
    // and which record kinds those exporters carry, so a replay reader can
    // tell "this host reported nothing" from "this host was not reporting".
    // Misconfigured/never-started entries are excluded by `export_coverage`.
    let (exporters, exported_kinds) = crate::telemetry::export_coverage(
        &crate::observability::global_export_statuses(),
        Utc::now(),
    );
    HostHealthRecord {
        captured_at: Utc::now(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        // The build identity of the RUNNING binary, straight from the same
        // compile-time stamps `loom-daemon --version` prints (#4956).
        // `daemon_version` alone only moves once per release, so it cannot
        // distinguish a day-stale binary from current `main`.
        build_commit: crate::self_update::BUILT_COMMIT.to_string(),
        built_at: crate::self_update::built_at(),
        uptime_sec: started_at.elapsed().as_secs(),
        logical_cpus: crate::cpu_headroom::logical_cpu_count(),
        cpu_idle_fraction: crate::cpu_headroom::cached_cpu_idle_fraction(),
        load_per_core: crate::cpu_headroom::load_per_core(),
        worktree_root_free_gb,
        worktree_root_total_gb,
        active_sweep_ids: collect_active_sweep_ids(workspace_pool),
        dispatch_halted,
        halt_reason,
        managed_repos: collect_managed_repos(workspace_pool, slug_cache).await,
        roles: sample_role_tick_health(&crate::role_runner::role_tick_records()),
        protection: sample_host_protection().await,
        admission_brake,
        is_captain: captain_gate.is_captain_flag(),
        armed_singleton_jobs: crate::fleet_captain::armed_singleton_job_names_for_host(
            workspace_root,
        ),
        captainless_singleton_jobs: crate::fleet_captain::captainless_singleton_job_names(),
        exported_kinds,
        exporters,
        // Memory/pressure slice ("deferred vs killed vs timed out"): the
        // whole object is omitted when nothing was measured — never a flat
        // zero — the same absence contract `protection`/`admission_brake`
        // follow. Swap rates come from the process-global sample history.
        memory: memory_summary(pressure, swap_in_bytes_per_sec, swap_out_bytes_per_sec),
    }
}

/// Process-global previous swap-counter sample, kept so successive
/// `host.health` samples can turn the two cumulative totals into rates. A plain
/// `Mutex` (not async): the critical section is a few nanoseconds and the
/// collector is the only writer. Poison is recovered rather than propagated —
/// the daemon must keep sampling if a past holder ever panicked (a dead rate
/// is a `None`, never a wedged health loop).
static SWAP_RATE_HISTORY: Mutex<Option<crate::host_pressure::SwapCounterSample>> = Mutex::new(None);

/// Fold one fresh sample and its computed rates into the wire
/// [`MemoryPressureSummary`], returning `None` when nothing at all was
/// measured (the wire field's absence contract — an all-`None` summary must
/// not be serialized as an empty-looking object).
fn memory_summary(
    pressure: crate::host_pressure::HostPressure,
    swap_in_bytes_per_sec: Option<f64>,
    swap_out_bytes_per_sec: Option<f64>,
) -> Option<MemoryPressureSummary> {
    let summary = MemoryPressureSummary {
        mem_total_bytes: pressure.mem_total_bytes,
        mem_available_bytes: pressure.mem_available_bytes,
        mem_compressed_bytes: pressure.mem_compressed_bytes,
        swap_total_bytes: pressure.swap_total_bytes,
        swap_used_bytes: pressure.swap_used_bytes,
        swap_in_bytes_total: pressure.swap_in_bytes_total,
        swap_out_bytes_total: pressure.swap_out_bytes_total,
        swap_in_bytes_per_sec,
        swap_out_bytes_per_sec,
        memory_pressure: pressure.memory_pressure,
        oom_kill_total: pressure.oom_kill_total,
    };
    (summary != MemoryPressureSummary::default()).then_some(summary)
}

/// Turn one fresh [`crate::host_pressure::HostPressure`] sample into the
/// `swap_*_bytes_per_sec` wire pair, remembering the counters for the next
/// cycle.
///
/// The rate is only honest when **both** cumulative totals were measured on
/// this side and the last — a partially-measured sample (e.g. `vm_stat` read
/// but the headers were malformed) is dropped for rate purposes rather than
/// blended, and the history keeps the last fully-measured pair. The pure
/// per-side rules (first sample, counter rollback, zero elapsed) live in
/// [`crate::host_pressure::swap_rates`].
fn swap_sample_rates(pressure: &crate::host_pressure::HostPressure) -> (Option<f64>, Option<f64>) {
    let mut history = SWAP_RATE_HISTORY.lock().unwrap_or_else(|p| p.into_inner());
    let rates = crate::host_pressure::swap_rates(
        *history,
        pressure.swap_in_bytes_total,
        pressure.swap_out_bytes_total,
    );
    if let (Some(in_now), Some(out_now)) =
        (pressure.swap_in_bytes_total, pressure.swap_out_bytes_total)
    {
        *history = Some(crate::host_pressure::SwapCounterSample {
            at: std::time::Instant::now(),
            swap_in_bytes_total: in_now,
            swap_out_bytes_total: out_now,
        });
    }
    rates
}

/// Project this host's [`crate::admission_brake::BrakeSnapshot`] onto the wire
/// [`AdmissionBrakeSummary`] (Issue #8478), attaching foreign-load attribution
/// only when the host is actually starving past its own warn threshold.
///
/// The `ps` shellout is the reason this is `async` and split from the pure
/// [`brake_summary_from_snapshot`] below: it runs under `spawn_blocking` (the
/// same pattern `sample_host_protection` uses for `launchctl`/`systemctl`) and
/// **only** on a host whose dispatch is already suppressed by load Loom does not
/// own. A healthy host pays nothing — no subprocess, no `ps` — on every
/// `host.health` sample, which is the whole reason the gate is on
/// `dispatch_suppressed_by_foreign_load` rather than on `held`.
async fn sample_admission_brake(
    snapshot: Option<crate::admission_brake::BrakeSnapshot>,
) -> Option<AdmissionBrakeSummary> {
    let mut summary = brake_summary_from_snapshot(snapshot, Utc::now())?;
    if summary.dispatch_suppressed_by_foreign_load {
        summary.top_cpu_consumers = tokio::task::spawn_blocking(|| {
            let clause = crate::foreign_load::attribution_clause();
            // `attribution_clause` returns "" on ANY probe failure; an absent
            // attribution must stay absent rather than becoming an empty
            // string a consumer would render as a blank answer.
            (!clause.is_empty()).then_some(clause)
        })
        .await
        .ok()
        .flatten();
    }
    Some(summary)
}

/// Pure projection of a brake snapshot onto the wire summary — split out of
/// [`sample_admission_brake`] so the duration arithmetic and the
/// suppressed-by-foreign-load verdict are unit-testable with a fixed `now` and
/// no process-global brake registration (the `OnceLock` every other test in
/// this binary shares can only be set once).
///
/// `now` is the emitting host's own clock, so `starving_secs` is computed
/// locally and a consumer never subtracts a remote timestamp from its own.
/// Clamped at `0`: a snapshot whose `starving_since` is momentarily ahead of
/// `now` (a clock adjustment mid-tick) must report "just started", never a
/// negative duration.
fn brake_summary_from_snapshot(
    snapshot: Option<crate::admission_brake::BrakeSnapshot>,
    now: DateTime<Utc>,
) -> Option<AdmissionBrakeSummary> {
    let snapshot = snapshot?;
    let starving_secs = snapshot
        .starving_since
        .map(|since| (now - since).num_seconds().max(0));
    // Held AND starving at least as long as THIS host considers alarming. Both
    // conjuncts matter: `starving_since` is only ever set on a held tick, but
    // stating `held` explicitly keeps the field honest if that ever changes.
    let dispatch_suppressed_by_foreign_load =
        snapshot.held && starving_secs.is_some_and(|secs| secs >= snapshot.starvation_warn_secs);
    Some(AdmissionBrakeSummary {
        held: snapshot.held,
        starving_since: snapshot.starving_since,
        starving_secs,
        starvation_warn_secs: snapshot.starvation_warn_secs,
        escape_hatch_grants: snapshot.escape_hatch_grants,
        dispatch_suppressed_by_foreign_load,
        // Filled in by `sample_admission_brake` only when suppressed.
        top_cpu_consumers: None,
    })
}

/// Sample this host's watchdog/crash-protection state (Issue #5352) via
/// [`crate::daemon_install_state::probe_protection`] — the exact same
/// classification `loom-daemon status`'s `Protection:` line and `--json`'s
/// `protection` object already compute, reused rather than re-derived so the
/// telemetry pipeline can never disagree with the host-local CLI verdict.
///
/// The probe shells out to `launchctl`/`systemctl` (blocking I/O), so it is
/// dispatched through `spawn_blocking` — the same pattern this module already
/// uses for the CPU-idle refresh above. A join failure (the executor
/// shutting down) degrades to `None` ("not reported"), never a fabricated
/// verdict.
async fn sample_host_protection() -> Option<HostProtectionSummary> {
    tokio::task::spawn_blocking(crate::daemon_install_state::probe_protection)
        .await
        .ok()
        .flatten()
        .map(protection_summary_from_report)
}

/// Pure projection of a host-local [`crate::daemon_install_state::ProtectionReport`]
/// onto the wire-level [`HostProtectionSummary`] — split out of
/// [`sample_host_protection`] so the field mapping is unit-testable without
/// touching the environment or shelling out to `launchctl`/`systemctl`.
fn protection_summary_from_report(
    report: crate::daemon_install_state::ProtectionReport,
) -> HostProtectionSummary {
    HostProtectionSummary {
        state: report.state.as_str().to_string(),
        watchdog_provisioned: report.watchdog_provisioned,
    }
}

/// Sample this host's role-tick health (Issue #5022) from the process-global
/// ring [`crate::role_runner::role_tick_records`] maintains. Reuses
/// [`crate::health::summarize_role_ticks`] — the exact transient-vs-persistent
/// classifier `loom-daemon health`'s `roles` section already applies — rather
/// than inventing a second one, so the two can never disagree about which
/// `(root, role)` pairs are persistently failing.
///
/// Unlike `loom-daemon health --since`, this samples the **entire** ring
/// ([`DateTime::<Utc>::MIN_UTC`] as the window start) rather than a caller-
/// chosen window: the ring itself is already bounded
/// ([`crate::role_runner::ROLE_TICK_RING_CAPACITY`]), so a periodic
/// `host.health` push has no separate window concept to apply — it always
/// reports the freshest picture the ring holds.
fn sample_role_tick_health(records: &[RoleTickRecord]) -> RoleTickHealth {
    let summary = crate::health::summarize_role_ticks(records, DateTime::<Utc>::MIN_UTC);
    RoleTickHealth {
        total: summary.total,
        ok: summary.ok,
        persistent: summary
            .persistent
            .into_iter()
            .map(|failure| RoleTickFailureEntry {
                root: failure.root,
                role: failure.role,
                failures: failure.failures,
                last_at: failure.last_at,
                detail: failure.detail,
            })
            .collect(),
    }
}

/// Derive `host.health`'s `(dispatch_halted, halt_reason)` pair from a
/// [`crate::host_breaker::BreakerSnapshot`] (Issue #4975) **or** a
/// sustained-starving admission brake (Issue #8478). Pure — takes both as
/// values rather than reading the process-globals directly — so it is
/// unit-testable for every combination without mutating the one-shot
/// [`crate::host_breaker::GLOBAL`] handle that every other test in this
/// binary shares.
///
/// `snapshot.suppressed` is already `enabled && phase.suppresses_dispatch()`
/// (`Open` or `CoolDown`) — reused verbatim rather than re-derived from
/// `phase` here, so this can never drift from the breaker's own definition of
/// "refusing work".
///
/// # Why the brake belongs in the same pair (#8478)
///
/// `dispatch_halted` is consumed as *the* "is this host refusing new work"
/// signal — the dashboard's `distressReason` renders it generically as
/// `dispatch halted: <reason>`, never breaker-specifically. But before #8478
/// only the breaker fed it, so the 2026-09-20 incident — 12 hours of held
/// admission from foreign load, with the breaker never tripping — presented
/// fleet-wide as a *healthy idle host*. Folding the brake in makes that host
/// render as degraded through the consumer path that already exists, with no
/// dashboard change; [`AdmissionBrakeSummary`] then carries the duration and
/// the attribution that this boolean-plus-prose pair structurally cannot.
///
/// The breaker keeps priority when both fire: it is the stickier, more severe
/// condition (sustained distress across a cool-down vs. a point-in-time hold),
/// so its reason is the more actionable one to lead with. Only a brake that has
/// passed its own `starvation_warn_secs` counts here — a brake holding while
/// sweeps genuinely drain is healthy backpressure, and reporting *that* as a
/// halt would flag every busy host in the fleet.
///
/// # Why `halt_reason` carries scalars ONLY — never the attribution
///
/// `halt_reason` is an unconditional member of `host.health`'s **public**
/// allowlist (`RECORD_FIELD_ALLOWLIST` in `loom-ui:src/redaction.ts`), copied
/// verbatim into every unauthenticated fleet response. `top_cpu_consumers` is
/// deliberately **not** in that allowlist — `redactAdmissionBrakeRow` drops it,
/// because a list of executable basenames is workload detail that has no safe
/// truncation (a command name IS the payload).
///
/// Interpolating the attribution into this free-text reason would therefore
/// re-emit, byte for byte, the exact data the row redaction just removed —
/// defeating the boundary through the other field. So the reason states only
/// the duration, this host's own threshold, and the non-attributing verdict
/// "suppressed by load Loom does not own"; [`AdmissionBrakeSummary`]'s
/// `top_cpu_consumers` is the **sole** carrier of process attribution, and it
/// stays behind the Access gate where an authenticated `/api/*` viewer reads it
/// unchanged. The host-local `admission_brake` starvation log line
/// ([`crate::admission_brake::global_observe`]) is unaffected — it never leaves
/// the host, so it keeps the full clause.
///
/// Pinned end-to-end by
/// `the_halt_reason_never_carries_process_attribution_past_the_public_boundary`
/// below and by `loom-ui:test/redactionAdmissionBrake.test.ts`'s
/// "no process name survives" case, which assert the two halves of the same
/// boundary (Judge finding on PR #8547).
fn dispatch_halt_from_breaker(
    snapshot: Option<crate::host_breaker::BreakerSnapshot>,
    brake: Option<&AdmissionBrakeSummary>,
) -> (bool, Option<String>) {
    match snapshot {
        Some(snapshot) if snapshot.suppressed => return (true, snapshot.reason),
        _ => {}
    }
    match brake {
        Some(brake) if brake.dispatch_suppressed_by_foreign_load => (
            true,
            Some(format!(
                "admission brake STARVING for {}s with 0 sweeps in flight (\u{2265} this host's \
                 starvationWarnSecs {}); dispatch is suppressed by load Loom does not own \
                 (#8478)",
                brake.starving_secs.unwrap_or_default(),
                brake.starvation_warn_secs,
            )),
        ),
        _ => (false, None),
    }
}

/// This host's currently managed repository roster (Issue #4976): every
/// workspace root [`WorkspacePool::provisioned_registries`] currently tracks,
/// resolved to its forge `owner/repo` slug and [`RepoVisibility`] — the same
/// derivation [`handle_event`] uses for a sweep record. Feeds `host.health`'s
/// `managed_repos` so the Phase-2 dashboard can show a host's roster even
/// when a registered repo has no sweeps at all (`active_sweep_ids` alone
/// cannot answer "is this repo idle or simply not registered here").
///
/// Best-effort per repo: a workspace whose slug cannot be resolved (no `gh`,
/// not a git remote, a timed-out probe) is silently dropped from the roster
/// rather than failing the whole `host.health` sample — mirrors every other
/// best-effort probe this collector already makes.
async fn collect_managed_repos(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) -> Vec<ManagedRepoEntry> {
    // Each registered workspace's dispatch priority (#9244, loom-ui#153).
    // Best-effort: an unreadable registry just omits `priority`.
    //
    // Deliberately re-read per collection rather than cached (Issue #9314).
    // This runs once per `snapshot_interval` (5 minutes by default), from the
    // collector task, and reads ONE small local JSON file
    // (`~/.loom/workspaces.json`) — the same read a dozen other daemon paths
    // make per tick. Freshness is the point: an operator's `loom-daemon
    // workspace priority` edit must show up in the next `host.health` sample
    // without a daemon restart, and `managed_repos` is how the dashboard
    // renders it. Caching it would trade that for no measurable saving and add
    // a staleness class (a cached priority outliving the edit) to a field whose
    // whole job is to report current configuration.
    let priorities: HashMap<PathBuf, u32> =
        crate::workspace_registry::WorkspaceRegistry::load_default()
            .map(|r| {
                r.workspaces
                    .into_iter()
                    .map(|w| (w.root, w.priority))
                    .collect()
            })
            .unwrap_or_default();
    collect_managed_repos_with(workspace_pool, slug_cache, derive_visibility, &priorities).await
}

/// Testable core of [`collect_managed_repos`]: `resolve_visibility` (a plain
/// sync fn, dispatched through `spawn_blocking` exactly like the standalone
/// [`resolve_visibility`] helper does) is injected so a unit test can
/// substitute a deterministic fake for the real `gh api` probe — the same
/// seam [`crate::telemetry::visibility::refresh_visibility_cache_with`] gives
/// its own tests, so this roster helper never has to make a live network call
/// to be exercised hermetically.
async fn collect_managed_repos_with<F>(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
    resolve_visibility: F,
    priorities: &HashMap<PathBuf, u32>,
) -> Vec<ManagedRepoEntry>
where
    F: Fn(&str) -> RepoVisibility + Copy + Send + Sync + 'static,
{
    // Collect the roots first and drop every registry lock before the async
    // slug/visibility resolution below — never hold a `std::sync::Mutex`
    // guard across an `.await` point.
    let roots = provisioned_roots(workspace_pool);

    let mut entries = Vec::with_capacity(roots.len());
    for root in roots {
        let root_str = root.to_string_lossy().to_string();
        let Some(slug) = resolve_repo_slug_cached(slug_cache, &root_str).await else {
            log::debug!("observability: could not resolve a repo slug for {root_str}; dropping it from the roster");
            continue;
        };
        let owned = slug.clone();
        // Mirrors `resolve_visibility`'s own contract: a `spawn_blocking` join
        // failure degrades to the private-safe default rather than
        // propagating — logged at `warn` (#6039), the same as the standalone
        // `resolve_visibility` helper, rather than silently swallowed.
        let visibility = match tokio::task::spawn_blocking(move || resolve_visibility(&owned)).await
        {
            Ok(visibility) => visibility,
            Err(join_error) => {
                log::warn!(
                    "observability: managed_repos visibility probe task for {slug} panicked \
                     ({join_error}) — defaulting to private"
                );
                RepoVisibility::Private
            }
        };
        let priority = priorities.get(&root).copied();
        entries.push(ManagedRepoEntry {
            slug,
            visibility,
            priority,
        });
    }
    // Deterministic order (the dashboard renders this list directly) and
    // de-duplicated in case two provisioned roots ever resolve to the same
    // forge slug (e.g. two worktrees of one repo).
    entries.sort_by(|a, b| a.slug.cmp(&b.slug));
    entries.dedup_by(|a, b| a.slug == b.slug);
    entries
}

/// Every workspace root [`WorkspacePool::provisioned_registries`] currently
/// tracks. Each registry lock is released before this returns.
pub(super) fn provisioned_roots(workspace_pool: &WorkspacePool) -> Vec<PathBuf> {
    workspace_pool
        .provisioned_registries()
        .into_iter()
        .map(|registry| {
            registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .config()
                .workspace_root
                .clone()
        })
        .collect()
}

/// This host's currently in-flight (`Pending`/`Running`, i.e. non-terminal)
/// sweep IDs, across every repo [`WorkspacePool::provisioned_registries`]
/// currently tracks (Issue #4955). Feeds `host.health`'s `active_sweep_ids`
/// so the Phase-2 dashboard's `FleetState` Durable Object can reconcile its
/// live `sweep:` entries against this daemon's own authoritative registry
/// view rather than relying solely on a (sometimes lost) `sweep.completed`
/// record to know a sweep is done.
fn collect_active_sweep_ids(workspace_pool: &WorkspacePool) -> Vec<String> {
    let mut ids = Vec::new();
    for registry in workspace_pool.provisioned_registries() {
        let registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ids.extend(
            registry
                .list(None)
                .into_iter()
                .filter(|info| !info.state.is_terminal())
                .map(|info| info.sweep_id),
        );
    }
    ids
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod admission_brake_tests;
// Issue #9477: `sweep.outcome` live-vs-journal precedence.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod outcome_precedence_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod provider_accounts_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod shell_arm_registry_tests;
// Issue #9432: `sweep.started`'s story-point pass-through.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod story_points_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod terminal_records_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
// Issue #9440: the terminal records' token counters + `tokens_status`.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tokens_status_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod weekly_utilization_tests;
