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
//! records require, so this module tracks a small in-memory `issue ->
//! (sweep_id, started_at)` map ([`DispatchState`]), populated on
//! `sweep.global.dispatch` and consulted (then cleared) on the terminal
//! event. Like every other in-process daemon tracker (e.g.
//! `work_finder`'s per-root state maps), this resets across a daemon
//! restart: a sweep already in flight when the collector starts emits
//! `sweep.phase`/`sweep.completed`/`sweep.outcome` records with a
//! synthesized `unknown-issue-{N}` sweep id and a zero `total_duration_sec`
//! rather than failing to emit at all — a degraded record beats a silently
//! dropped one for a telemetry pipeline.
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
    visibility::derive_visibility, HostHealthRecord, ManagedRepoEntry, PhaseDuration,
    RepoVisibility, SweepResult, SweepStartedRecord, TelemetryEnvelope, TelemetryRecord,
    TokenAccountState, TokenSnapshotRecord,
};
use crate::types::{Event, SweepKind};
use crate::workspace_pool::WorkspacePool;

use super::queue::DurableQueue;

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
}

/// Spawn the collector task on the shared daemon runtime. Subscribes to the
/// frozen `sweep.global.dispatch` / `sweep.issue.*` topics and periodically
/// (every `snapshot_interval`) samples host-level records. `daemon_started_at`
/// is used only to compute `host.health`'s `uptime_sec` (approximated as this
/// task's own uptime — see [`super::spawn_task`]'s doc comment).
///
/// `workspace_pool` (Issue #4955) is this daemon's shared per-workspace
/// registry pool — consulted only at each periodic snapshot tick to populate
/// `host.health`'s `active_sweep_ids` with this host's authoritative
/// in-flight sweep-id set. See [`collect_active_sweep_ids`].
pub fn spawn_task(
    bus: &EventBus,
    queue: Arc<DurableQueue>,
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
    queue: Arc<DurableQueue>,
    workspace_root: PathBuf,
    host_id: String,
    snapshot_interval: Duration,
    daemon_started_at: Instant,
    workspace_pool: Arc<WorkspacePool>,
) {
    let mut dispatches: HashMap<u32, DispatchState> = HashMap::new();
    let mut slug_cache: HashMap<String, String> = HashMap::new();
    let mut snapshot_timer = tokio::time::interval(snapshot_interval);
    snapshot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;

            recv_result = subscription.recv() => {
                match recv_result {
                    Ok(event) => {
                        handle_event(
                            event,
                            &queue,
                            &workspace_root,
                            &host_id,
                            &mut dispatches,
                            &mut slug_cache,
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

            _ = snapshot_timer.tick() => {
                sample_snapshots(
                    &queue,
                    &workspace_root,
                    &host_id,
                    daemon_started_at,
                    &workspace_pool,
                    &mut slug_cache,
                )
                .await;
            }
        }
    }
}

async fn handle_event(
    event: Event,
    queue: &DurableQueue,
    default_workspace_root: &Path,
    host_id: &str,
    dispatches: &mut HashMap<u32, DispatchState>,
    slug_cache: &mut HashMap<String, String>,
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
    for record in map_event_to_records(&event, issue, &slug, visibility, dispatches) {
        queue.push(TelemetryEnvelope::new(host_id, record));
    }
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
    dispatches: &mut HashMap<u32, DispatchState>,
) -> Vec<TelemetryRecord> {
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(_),
            sweep_id,
            ..
        } => {
            let started_at = Utc::now();
            dispatches.insert(
                issue,
                DispatchState {
                    sweep_id: sweep_id.clone(),
                    started_at,
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
            })]
        }
        Event::SweepGlobalDispatch { .. } => Vec::new(),
        Event::SweepPhase { phase, .. } => {
            let sweep_id = dispatches
                .get(&issue)
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
            let dispatch = dispatches.remove(&issue);
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
            let dispatch = dispatches.remove(&issue);
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
    let run = tokio::process::Command::new("gh")
        .arg("repo")
        .arg("view")
        .arg("--json")
        .arg("nameWithOwner")
        .arg("--jq")
        .arg(".nameWithOwner")
        .current_dir(workspace_root)
        .output();
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
/// through `spawn_blocking` per its own doc guidance; a join failure degrades
/// to the private-safe default rather than propagating.
async fn resolve_visibility(slug: &str) -> RepoVisibility {
    let owned = slug.to_string();
    tokio::task::spawn_blocking(move || derive_visibility(&owned))
        .await
        .unwrap_or(RepoVisibility::Private)
}

/// Sample the two host-level record kinds that have no corresponding bus
/// event and push them onto `queue`. `slug_cache` is the same per-workspace
/// slug memoization `handle_event` uses, threaded through so the periodic
/// `managed_repos` roster sample does not re-shell out to `gh repo view` for
/// a workspace root already resolved this run.
async fn sample_snapshots(
    queue: &DurableQueue,
    workspace_root: &Path,
    host_id: &str,
    daemon_started_at: Instant,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    let token_record = sample_token_snapshot(workspace_root);
    queue.push(TelemetryEnvelope::new(host_id, TelemetryRecord::TokensSnapshot(token_record)));
    let health_record =
        sample_host_health(workspace_root, daemon_started_at, workspace_pool, slug_cache).await;
    queue.push(TelemetryEnvelope::new(host_id, TelemetryRecord::HostHealth(health_record)));
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
    let (dispatch_halted, halt_reason) =
        dispatch_halt_from_breaker(crate::host_breaker::global_snapshot());
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
        worktree_root_free_gb: crate::disk_headroom::worktree_root_free_gb(workspace_root),
        active_sweep_ids: collect_active_sweep_ids(workspace_pool),
        dispatch_halted,
        halt_reason,
        managed_repos: collect_managed_repos(workspace_pool, slug_cache).await,
    }
}

/// Derive `host.health`'s `(dispatch_halted, halt_reason)` pair from a
/// [`crate::host_breaker::BreakerSnapshot`] (Issue #4975). Pure — takes the
/// snapshot as a value rather than reading the process-global directly — so
/// it is unit-testable for both the `Open`/`CoolDown` (halted) and
/// `Closed`/unregistered (not halted) cases without mutating the one-shot
/// [`crate::host_breaker::GLOBAL`] handle that every other test in this
/// binary shares.
///
/// `snapshot.suppressed` is already `enabled && phase.suppresses_dispatch()`
/// (`Open` or `CoolDown`) — reused verbatim rather than re-derived from
/// `phase` here, so this can never drift from the breaker's own definition of
/// "refusing work".
fn dispatch_halt_from_breaker(
    snapshot: Option<crate::host_breaker::BreakerSnapshot>,
) -> (bool, Option<String>) {
    match snapshot {
        Some(snapshot) if snapshot.suppressed => (true, snapshot.reason),
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
        // failure degrades to the private-safe default rather than propagating.
        let visibility = tokio::task::spawn_blocking(move || resolve_visibility(&owned))
            .await
            .unwrap_or(RepoVisibility::Private);
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
mod tests {
    use super::*;

    fn dispatch_event(issue: u32, sweep_id: &str) -> Event {
        Event::SweepGlobalDispatch {
            sweep_id: sweep_id.to_string(),
            kind: SweepKind::Issue(issue),
            runtime: None,
            runtime_source: None,
            repo: Some("/repos/loom".to_string()),
        }
    }

    fn phase_event(issue: u32, phase: &str) -> Event {
        Event::SweepPhase {
            issue,
            phase: phase.to_string(),
            pr_number: None,
            repo: Some("/repos/loom".to_string()),
        }
    }

    fn exited_event(issue: u32, exit_code: Option<i32>, duration_sec: i64) -> Event {
        Event::SweepExited {
            issue,
            exit_code,
            duration_sec,
            no_progress: false,
            death_class: None,
            repo: Some("/repos/loom".to_string()),
        }
    }

    fn crashed_event(issue: u32) -> Event {
        Event::SweepCrashed {
            issue,
            checkpoint_phase: Some("builder".to_string()),
            classification: None,
            death_class: None,
            repo: Some("/repos/loom".to_string()),
        }
    }

    #[test]
    fn dispatch_emits_sweep_started_and_tracks_state() {
        let mut dispatches = HashMap::new();
        let records = map_event_to_records(
            &dispatch_event(42, "sweep-issue-42-0"),
            42,
            "rjwalters/loom",
            RepoVisibility::Public,
            &mut dispatches,
        );
        assert_eq!(records.len(), 1);
        match &records[0] {
            TelemetryRecord::SweepStarted(r) => {
                assert_eq!(r.issue, 42);
                assert_eq!(r.sweep_id, "sweep-issue-42-0");
                assert_eq!(r.repo, "rjwalters/loom");
                assert_eq!(r.visibility, RepoVisibility::Public);
            }
            other => panic!("expected SweepStarted, got {other:?}"),
        }
        assert!(dispatches.contains_key(&42));
    }

    #[test]
    fn phase_after_dispatch_carries_the_tracked_sweep_id() {
        let mut dispatches = HashMap::new();
        map_event_to_records(
            &dispatch_event(42, "sweep-issue-42-0"),
            42,
            "rjwalters/loom",
            RepoVisibility::Private,
            &mut dispatches,
        );
        let records = map_event_to_records(
            &phase_event(42, "builder"),
            42,
            "rjwalters/loom",
            RepoVisibility::Private,
            &mut dispatches,
        );
        match &records[0] {
            TelemetryRecord::SweepPhase(r) => {
                assert_eq!(r.sweep_id, "sweep-issue-42-0");
                assert_eq!(r.phase, "builder");
            }
            other => panic!("expected SweepPhase, got {other:?}"),
        }
    }

    #[test]
    fn phase_without_a_tracked_dispatch_uses_a_synthesized_sweep_id() {
        // Simulates a daemon restart mid-sweep: no SweepGlobalDispatch was
        // observed in this process's lifetime for issue 99.
        let mut dispatches = HashMap::new();
        let records = map_event_to_records(
            &phase_event(99, "judge"),
            99,
            "rjwalters/loom",
            RepoVisibility::Private,
            &mut dispatches,
        );
        match &records[0] {
            TelemetryRecord::SweepPhase(r) => assert_eq!(r.sweep_id, "unknown-issue-99"),
            other => panic!("expected SweepPhase, got {other:?}"),
        }
    }

    #[test]
    fn clean_exit_zero_maps_to_success_and_clears_dispatch_state() {
        let mut dispatches = HashMap::new();
        map_event_to_records(
            &dispatch_event(7, "sweep-issue-7-0"),
            7,
            "rjwalters/loom",
            RepoVisibility::Public,
            &mut dispatches,
        );
        let records = map_event_to_records(
            &exited_event(7, Some(0), 120),
            7,
            "rjwalters/loom",
            RepoVisibility::Public,
            &mut dispatches,
        );
        assert_eq!(records.len(), 2, "a terminal event yields completed + outcome");
        match &records[0] {
            TelemetryRecord::SweepCompleted(r) => assert_eq!(r.result, SweepResult::Success),
            other => panic!("expected SweepCompleted, got {other:?}"),
        }
        match &records[1] {
            TelemetryRecord::SweepOutcome(r) => {
                assert_eq!(r.result, SweepResult::Success);
                assert_eq!(r.total_duration_sec, 120);
                assert_eq!(r.sweep_id, "sweep-issue-7-0");
            }
            other => panic!("expected SweepOutcome, got {other:?}"),
        }
        assert!(!dispatches.contains_key(&7), "terminal event must clear tracked state");
    }

    #[test]
    fn nonzero_exit_maps_to_failure() {
        let mut dispatches = HashMap::new();
        let records = map_event_to_records(
            &exited_event(8, Some(1), 30),
            8,
            "rjwalters/loom",
            RepoVisibility::Private,
            &mut dispatches,
        );
        match &records[0] {
            TelemetryRecord::SweepCompleted(r) => assert_eq!(r.result, SweepResult::Failure),
            other => panic!("expected SweepCompleted, got {other:?}"),
        }
    }

    #[test]
    fn crash_maps_to_failure_with_duration_from_tracked_dispatch() {
        let mut dispatches = HashMap::new();
        dispatches.insert(
            5,
            DispatchState {
                sweep_id: "sweep-issue-5-0".to_string(),
                started_at: Utc::now() - chrono::Duration::seconds(60),
            },
        );
        let records = map_event_to_records(
            &crashed_event(5),
            5,
            "rjwalters/loom",
            RepoVisibility::Private,
            &mut dispatches,
        );
        match &records[1] {
            TelemetryRecord::SweepOutcome(r) => {
                assert_eq!(r.result, SweepResult::Failure);
                assert!(r.total_duration_sec >= 59, "duration should reflect elapsed time");
            }
            other => panic!("expected SweepOutcome, got {other:?}"),
        }
        assert!(!dispatches.contains_key(&5));
    }

    #[test]
    fn blocker_event_yields_no_records() {
        let mut dispatches = HashMap::new();
        let event = Event::SweepBlocker {
            issue: 1,
            reason: "human decision".to_string(),
            label_added: "loom:blocked".to_string(),
            repo: None,
        };
        let records = map_event_to_records(
            &event,
            1,
            "rjwalters/loom",
            RepoVisibility::Private,
            &mut dispatches,
        );
        assert!(records.is_empty());
    }

    #[test]
    fn event_issue_ignores_pr_set_dispatch() {
        let event = Event::SweepGlobalDispatch {
            sweep_id: "sweep-prs-0".to_string(),
            kind: SweepKind::PrSet(vec![1, 2]),
            runtime: None,
            runtime_source: None,
            repo: None,
        };
        assert_eq!(event_issue(&event), None);
    }

    #[test]
    fn event_repo_path_reads_the_stamped_workspace_root() {
        let event = phase_event(1, "curator");
        assert_eq!(event_repo_path(&event).as_deref(), Some("/repos/loom"));
    }

    // ------------------------------------------------------------------
    // Host-level snapshot samplers — no `gh` dependency, safe in CI.
    // ------------------------------------------------------------------

    #[test]
    fn token_snapshot_reads_a_ranking_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
        std::fs::write(
            dir.path().join(".loom/tokens/.ranking"),
            "agent-1|available|0.42\nagent-2|exhausted|0.99\n",
        )
        .unwrap();
        let record = sample_token_snapshot(dir.path());
        assert_eq!(record.accounts.len(), 2);
        assert_eq!(record.accounts[0].account, "agent-1");
        assert_eq!(record.accounts[0].rank, Some(0));
        assert!(!record.accounts[0].exhausted);
        assert_eq!(record.accounts[1].account, "agent-2");
        assert!(record.accounts[1].exhausted);
    }

    #[test]
    fn token_snapshot_missing_ranking_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let record = sample_token_snapshot(dir.path());
        assert!(record.accounts.is_empty());
    }

    #[test]
    fn token_snapshot_populates_limit_window_reset_at_from_the_ranking() {
        // Issue #4874: this field was hardcoded `None`, so every exhausted
        // account fleet-wide pushed `limit_window_reset_at: null` and the
        // dashboard's countdown column was permanently `—`. An exhausted
        // account with a reset in the ranking must now report it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
        std::fs::write(
            dir.path().join(".loom/tokens/.ranking"),
            "agent-1|available|0.42\n\
             agent-2|exhausted|0.00|2026-08-02T03:00:00Z\n\
             agent-3|exhausted||2026-08-04T11:00:00Z\n",
        )
        .unwrap();
        let record = sample_token_snapshot(dir.path());
        assert_eq!(record.accounts.len(), 3);

        // No reset field -> unknown, not a fabricated date.
        assert_eq!(record.accounts[0].limit_window_reset_at, None);

        let expected = DateTime::parse_from_rfc3339("2026-08-02T03:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(record.accounts[1].limit_window_reset_at, Some(expected));
        assert!(record.accounts[1].exhausted);
        assert_eq!(record.accounts[1].usage_fraction, Some(0.00));

        // Reset known, utilization unknown: the reset still lands, and the
        // absent utilization is not coerced to 0.0.
        assert_eq!(record.accounts[2].usage_fraction, None);
        assert_eq!(
            record.accounts[2].limit_window_reset_at,
            Some(
                DateTime::parse_from_rfc3339("2026-08-04T11:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn token_snapshot_carries_the_writers_binding_reset_end_to_end() {
        // The whole #4874 chain in one test: the *real* ranking writer renders
        // the file, the collector reads it back, and the instant that lands in
        // `tokens.snapshot` is the one the account is actually waiting on.
        //
        // The rate_limited row is the case that makes this more than plumbing.
        // It is 7d-healthy and 5h-spent, so it returns at the 5h boundary
        // (07:00Z) — writing its 7d reset (a week out) would tell the dashboard
        // the fleet was stalled for six days when it recovers within the hour.
        use crate::tokens_pool::check::{format_ranking_lines, AccountResult, ProbeReport};

        let mut spent = AccountResult::new("agent-1", "exhausted");
        spent.s5h_utilization = Some(0.0);
        spent.s5h_reset = Some("2026-08-01T05:20:00Z".into());
        spent.s7d_reset = Some("2026-08-02T03:00:00Z".into());
        let mut limited = AccountResult::new("agent-2", "rate_limited");
        limited.s5h_utilization = Some(1.0);
        limited.s5h_reset = Some("2026-08-01T07:00:00Z".into());
        limited.s7d_reset = Some("2026-08-07T01:00:00Z".into());

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
        std::fs::write(
            dir.path().join(".loom/tokens/.ranking"),
            format_ranking_lines(&ProbeReport {
                ranked_at: "2026-08-01T05:22:00Z".into(),
                accounts: vec![spent, limited],
            }),
        )
        .unwrap();

        let record = sample_token_snapshot(dir.path());
        let at = |iso: &str| {
            Some(
                DateTime::parse_from_rfc3339(iso)
                    .unwrap()
                    .with_timezone(&Utc),
            )
        };
        assert_eq!(record.accounts[0].limit_window_reset_at, at("2026-08-02T03:00:00Z"));
        assert!(record.accounts[0].exhausted);
        assert_eq!(
            record.accounts[1].limit_window_reset_at,
            at("2026-08-01T07:00:00Z"),
            "a 5h-limited account must report the 5h boundary it actually returns at"
        );
    }

    #[test]
    fn token_snapshot_unparseable_reset_degrades_to_unknown() {
        // The on-disk file is a trust boundary: junk in the reset field must
        // not propagate to the pushed telemetry as a bogus instant.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
        std::fs::write(
            dir.path().join(".loom/tokens/.ranking"),
            "agent-1|exhausted|0.00|not-a-timestamp\n",
        )
        .unwrap();
        let record = sample_token_snapshot(dir.path());
        assert_eq!(record.accounts.len(), 1, "a bad reset must not drop the whole row");
        assert_eq!(record.accounts[0].limit_window_reset_at, None);
        assert!(record.accounts[0].exhausted);
    }

    fn empty_pool() -> WorkspacePool {
        WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current())
    }

    fn empty_slug_cache() -> HashMap<String, String> {
        HashMap::new()
    }

    #[tokio::test]
    async fn host_health_sample_populates_daemon_version_and_uptime() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let pool = empty_pool();
        let record = sample_host_health(dir.path(), started, &pool, &mut empty_slug_cache()).await;
        assert_eq!(record.daemon_version, env!("CARGO_PKG_VERSION"));
        assert!(record.logical_cpus >= 1);
    }

    #[tokio::test]
    async fn host_health_sample_stamps_the_running_binarys_build_identity() {
        let dir = tempfile::tempdir().unwrap();
        let pool = empty_pool();
        let record =
            sample_host_health(dir.path(), Instant::now(), &pool, &mut empty_slug_cache()).await;

        // The sample must carry the SAME commit `loom-daemon --version`
        // prints — that identity is what lets the dashboard tell two
        // same-`daemon_version` builds apart (#4956).
        assert_eq!(record.build_commit, crate::self_update::BUILT_COMMIT);
        assert!(
            !record.build_commit.is_empty(),
            "build_commit must always be populated (\"unknown\" is the no-git fallback)"
        );
        assert_eq!(record.built_at, crate::self_update::built_at());

        // `built_at` is absent only when the compile-time stamp itself is the
        // "unknown" fallback; whenever it IS present it must be a real instant
        // no later than the sample, never a fabricated epoch.
        if let Some(built_at) = record.built_at {
            assert!(
                built_at <= record.captured_at,
                "built_at ({built_at}) must not be after captured_at ({})",
                record.captured_at
            );
        }
    }

    #[test]
    fn built_at_parses_the_compile_time_stamp_or_reports_unknown() {
        // Pins the "unknown != fabricated instant" half of the contract: the
        // helper resolves to `Some` exactly when the raw stamp is parseable.
        let raw = crate::self_update::BUILT_AT_RAW;
        assert_eq!(
            crate::self_update::built_at().is_some(),
            chrono::DateTime::parse_from_rfc3339(raw).is_ok(),
            "built_at() must be Some iff the raw stamp {raw:?} parses"
        );
    }

    #[tokio::test]
    async fn host_health_sample_reports_no_active_sweeps_when_pool_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let pool = empty_pool();
        let record = sample_host_health(dir.path(), started, &pool, &mut empty_slug_cache()).await;
        assert!(
            record.active_sweep_ids.is_empty(),
            "an empty pool has no in-flight sweeps to report"
        );
    }

    #[tokio::test]
    async fn host_health_sample_reports_no_managed_repos_when_pool_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let pool = empty_pool();
        let record = sample_host_health(dir.path(), started, &pool, &mut empty_slug_cache()).await;
        assert!(
            record.managed_repos.is_empty(),
            "an empty pool has no registered repos to report"
        );
    }

    /// A hermetic, `dispatch()`-able registry: `skip_label_flip = true` skips
    /// runtime admission / the workspace-commands guard / every `gh` call
    /// (mirrors `sweep_registry::test_support::fixture_registry`, which is
    /// not reachable from here — `sweep_registry::test_support` is a
    /// private, `#[cfg(test)]`-only module of a sibling module tree). The
    /// fake spawn binary sleeps briefly so the dispatched entry stays
    /// `Running` for the synchronous, single-threaded-until-`.await` body of
    /// the test below.
    fn dispatchable_registry(workspace: &Path) -> crate::sweep_registry::SweepRegistry {
        use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
        use std::os::unix::fs::PermissionsExt;

        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let bin = scripts_dir.join("spawn-worker.sh");
        std::fs::write(&bin, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(bin);
        config.skip_label_flip = true;
        config.journal_path = Some(workspace.join("sweeps-journal.json"));
        config.outcomes_journal_path = Some(workspace.join("sweep-outcomes.jsonl"));
        config.outcome_telemetry_path = Some(workspace.join("sweep-outcome-telemetry.jsonl"));
        SweepRegistry::new(config)
    }

    #[tokio::test]
    async fn collect_active_sweep_ids_reports_running_sweeps_across_every_provisioned_registry() {
        let dir = tempfile::tempdir().unwrap();
        let a_root = dir.path().join("a");
        let b_root = dir.path().join("b");
        std::fs::create_dir_all(&a_root).unwrap();
        std::fs::create_dir_all(&b_root).unwrap();
        let pool = empty_pool();

        // `seed` (not `get_or_provision`) so no reaper/watchdog is spawned
        // for either fixture — nothing races the assertions below.
        pool.seed(a_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&a_root))));
        pool.seed(b_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&b_root))));

        let a_sweep_id = {
            let registry = pool.get_or_provision(&a_root);
            let mut registry = registry.lock().unwrap();
            registry
                .dispatch(&SweepKind::Issue(1), None, None, None, None)
                .expect("dispatch into workspace a")
                .sweep_id
        };
        let b_sweep_id = {
            let registry = pool.get_or_provision(&b_root);
            let mut registry = registry.lock().unwrap();
            registry
                .dispatch(&SweepKind::Issue(2), None, None, None, None)
                .expect("dispatch into workspace b")
                .sweep_id
        };

        let mut ids = collect_active_sweep_ids(&pool);
        ids.sort();
        let mut expected = vec![a_sweep_id, b_sweep_id];
        expected.sort();
        assert_eq!(ids, expected, "in-flight sweeps from BOTH provisioned registries are reported");
    }

    // ------------------------------------------------------------------
    // dispatch_halt_from_breaker (Issue #4975)
    // ------------------------------------------------------------------
    //
    // Exercised as a pure function over a hand-built `BreakerSnapshot` rather
    // than through `host_breaker::register_global` — the breaker's `GLOBAL`
    // handle is a process-wide `OnceLock` shared by every test in this
    // binary, so mutating it here would leak into unrelated tests.

    fn breaker_snapshot(
        phase: crate::host_breaker::BreakerPhase,
        reason: Option<&str>,
    ) -> crate::host_breaker::BreakerSnapshot {
        crate::host_breaker::BreakerSnapshot {
            enabled: true,
            phase,
            suppressed: phase.suppresses_dispatch(),
            reason: reason.map(str::to_string),
            tripped_at: None,
            releases_at: None,
            last_load_per_core: Some(4.24),
            load_per_core_threshold: 2.5,
            sustain_ticks: 3,
            cooldown_secs: 300,
            consecutive_over: 3,
        }
    }

    #[test]
    fn dispatch_halt_from_breaker_reports_not_halted_when_no_breaker_registered() {
        assert_eq!(dispatch_halt_from_breaker(None), (false, None));
    }

    #[test]
    fn dispatch_halt_from_breaker_reports_not_halted_when_closed() {
        let snapshot = breaker_snapshot(crate::host_breaker::BreakerPhase::Closed, None);
        assert_eq!(dispatch_halt_from_breaker(Some(snapshot)), (false, None));
    }

    #[test]
    fn dispatch_halt_from_breaker_reports_halted_with_reason_when_open() {
        let snapshot = breaker_snapshot(
            crate::host_breaker::BreakerPhase::Open,
            Some("load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)"),
        );
        assert_eq!(
            dispatch_halt_from_breaker(Some(snapshot)),
            (
                true,
                Some("load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)".to_string())
            )
        );
    }

    #[test]
    fn dispatch_halt_from_breaker_reports_halted_during_cooldown() {
        let snapshot = breaker_snapshot(
            crate::host_breaker::BreakerPhase::CoolDown,
            Some("load-per-core 1.10 < 2.50; cooling down for 300s"),
        );
        let (halted, reason) = dispatch_halt_from_breaker(Some(snapshot));
        assert!(halted, "CoolDown still suppresses dispatch, so it must count as halted");
        assert!(reason.is_some());
    }

    #[tokio::test]
    async fn collect_managed_repos_reports_every_provisioned_registrys_repo_even_when_idle() {
        // Registered but never dispatched into: this is the exact "idle
        // roster" case #4976 exists for — `collect_active_sweep_ids` would
        // report nothing for either root, but the roster must still list
        // both.
        let dir = tempfile::tempdir().unwrap();
        let a_root = dir.path().join("a");
        let b_root = dir.path().join("b");
        std::fs::create_dir_all(&a_root).unwrap();
        std::fs::create_dir_all(&b_root).unwrap();
        let pool = empty_pool();
        pool.seed(a_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&a_root))));
        pool.seed(b_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&b_root))));

        // Pre-populate the slug cache so slug resolution never shells out to
        // `gh repo view` against these bare tempdirs (which have no git
        // remote at all) — the exact seam `resolve_repo_slug_cached` exists
        // for.
        let mut slug_cache = HashMap::new();
        slug_cache
            .insert(a_root.to_string_lossy().to_string(), "loom-test-fixture/repo-a".to_string());
        slug_cache
            .insert(b_root.to_string_lossy().to_string(), "loom-test-fixture/repo-b".to_string());

        // A deterministic fake visibility resolver — never a real `gh api`
        // call — that reports repo-a public and repo-b private, so the
        // per-entry visibility assertion below is not a coin flip on live
        // forge state.
        fn fake_visibility(slug: &str) -> RepoVisibility {
            if slug == "loom-test-fixture/repo-a" {
                RepoVisibility::Public
            } else {
                RepoVisibility::Private
            }
        }

        let mut repos = collect_managed_repos_with(&pool, &mut slug_cache, fake_visibility).await;
        repos.sort_by(|a, b| a.slug.cmp(&b.slug));
        assert_eq!(
            repos,
            vec![
                ManagedRepoEntry {
                    slug: "loom-test-fixture/repo-a".to_string(),
                    visibility: RepoVisibility::Public,
                },
                ManagedRepoEntry {
                    slug: "loom-test-fixture/repo-b".to_string(),
                    visibility: RepoVisibility::Private,
                },
            ],
            "both registered repos are in the roster, each with its derived visibility, \
             despite neither having a sweep in flight"
        );
    }
}
