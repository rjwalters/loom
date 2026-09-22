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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::event_bus::{EventBus, RecvError};
use crate::telemetry::{
    visibility::derive_visibility, AdmissionBrakeSummary, HostHealthRecord, HostProtectionSummary,
    ManagedRepoEntry, PhaseDuration, RepoVisibility, RoleTickFailureEntry, RoleTickHealth,
    SweepResult, SweepStartedRecord, TelemetryEnvelope, TelemetryRecord, TokenAccountState,
    TokenSnapshotRecord,
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
    for envelope in correlation::map_envelopes(
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
    ) {
        queue.offer(envelope);
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
fn terminal_records(
    repo: &str,
    visibility: RepoVisibility,
    issue: u32,
    sweep_id: String,
    result: SweepResult,
    total_duration_sec: i64,
    pr_number: Option<u32>,
) -> Vec<TelemetryRecord> {
    let completed_at = Utc::now();
    vec![
        TelemetryRecord::SweepCompleted(crate::telemetry::SweepCompletedRecord {
            repo: repo.to_string(),
            visibility,
            issue,
            sweep_id: sweep_id.clone(),
            completed_at,
            result,
            // Issue #6384: this live event-bus path has no `workspace_root`
            // or sweep-start instant in scope (unlike
            // `sweep_registry::outcome_journal`, which already computes
            // `tokens_in`/`tokens_out` for the same reason those two fields
            // are also hardcoded `None` below) — deferred rather than
            // threading a larger signature change through
            // `map_event_to_records` for this "routine"-scoped issue. The
            // backfill path (`observability::backfill::synthesize_completed`)
            // is the real, non-deferred construction site.
            tokens_by_model: None,
        }),
        TelemetryRecord::SweepOutcome(crate::telemetry::SweepOutcomeRecord {
            repo: repo.to_string(),
            visibility,
            issue,
            sweep_id,
            model: None,
            effort: None,
            config: std::collections::BTreeMap::new(),
            phase_durations: Vec::<PhaseDuration>::new(),
            total_duration_sec,
            result,
            pr_number,
            tokens_in: None,
            tokens_out: None,
            lines_added: None,
            lines_deleted: None,
            tokens_by_model: None,
            failure_class: None,
            models_used: None,
            doctor_cycles: None,
            judge_verdicts: None,
            // Issue #8507: same deferral as `tokens_by_model` above — this
            // live event-bus path has no launch-record log path in scope.
            runtime: None,
            provider: None,
            profile: None,
            complexity: None,
        }),
    ]
}

/// [`fetch_repo_slug`] with a process-lifetime cache keyed by workspace root
/// path (a repo's slug does not change while the daemon runs — same
/// rationale as [`crate::safehouse`]'s own `slug_cache`).
async fn resolve_repo_slug_cached(
    cache: &mut HashMap<String, String>,
    workspace_root: &str,
) -> Option<String> {
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
    let mut cmd = tokio::process::Command::new("gh");
    cmd.arg("repo")
        .arg("view")
        .arg("--json")
        .arg("nameWithOwner")
        .arg("--jq")
        .arg(".nameWithOwner")
        .current_dir(workspace_root);
    // #5431: point this child at the owner-correct credential when
    // `workspace_root` is a cross-owner managed repo (the root-keyed helper
    // takes a `std::process::Command`, so set the env directly here for tokio's
    // command type). A no-op for a single-owner fleet or the root owner's repos.
    if let Some(dir) = crate::credential_preflight::gh_config_dir_for_root(workspace_root) {
        cmd.env("GH_CONFIG_DIR", dir);
    }
    let run = cmd.output();
    let output = tokio::time::timeout(SLUG_FETCH_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
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
async fn resolve_visibility(slug: &str) -> RepoVisibility {
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
    queue.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::TokensSnapshot(token_record)));
    let health_record =
        sample_host_health(workspace_root, daemon_started_at, workspace_pool, slug_cache).await;
    queue.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::HostHealth(health_record)));
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
fn sample_token_snapshot(workspace_root: &Path) -> TokenSnapshotRecord {
    let pool_dir = crate::tokens_pool::paths::resolve_tokens_dir(workspace_root);
    let ranking_path = pool_dir.join(".ranking");
    let mut accounts = Vec::new();
    if let Ok(contents) = std::fs::read_to_string(&ranking_path) {
        for (index, line) in contents.lines().enumerate() {
            if !line.contains('|') {
                continue;
            }
            let Some(row) = crate::tokens_pool::select::parse_ranking_line(line) else {
                continue;
            };
            let exhausted = !crate::capacity::AccountHealth::parse(&row.status).is_healthy();
            accounts.push(TokenAccountState {
                account: row.name,
                provider: AccountProvider::Claude.to_string(),
                rank: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                usage_fraction: row.util_5h,
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
/// fraction. So `rank`/`usage_fraction` stay absent ("unknown, not zero"),
/// `exhausted` is `!is_eligible_at(now)`, and `limit_window_reset_at` is the
/// hold's deadline when there is one — the same "when does this account's
/// constraint lift?" meaning the Claude rows carry. A disabled account is not
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
    // filesystem or point in time they describe.
    let (worktree_root_free_gb, worktree_root_total_gb) =
        crate::disk_headroom::worktree_root_disk_gb(workspace_root);
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
    }
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
/// allowlist (`RECORD_FIELD_ALLOWLIST` in `dashboard/src/redaction.ts`), copied
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
/// below and by `dashboard/test/redactionAdmissionBrake.test.ts`'s
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
    collect_managed_repos_with(workspace_pool, slug_cache, derive_visibility).await
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
) -> Vec<ManagedRepoEntry>
where
    F: Fn(&str) -> RepoVisibility + Copy + Send + Sync + 'static,
{
    // Collect the roots first and drop every registry lock before the async
    // slug/visibility resolution below — never hold a `std::sync::Mutex`
    // guard across an `.await` point.
    let roots: Vec<PathBuf> = workspace_pool
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
        .collect();

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
        entries.push(ManagedRepoEntry { slug, visibility });
    }
    // Deterministic order (the dashboard renders this list directly) and
    // de-duplicated in case two provisioned roots ever resolve to the same
    // forge slug (e.g. two worktrees of one repo).
    entries.sort_by(|a, b| a.slug.cmp(&b.slug));
    entries.dedup_by(|a, b| a.slug == b.slug);
    entries
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
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod provider_accounts_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
