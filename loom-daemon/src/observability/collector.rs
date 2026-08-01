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
    visibility::derive_visibility, HostHealthRecord, PhaseDuration, RepoVisibility, SweepResult,
    SweepStartedRecord, TelemetryEnvelope, TelemetryRecord, TokenAccountState, TokenSnapshotRecord,
};
use crate::types::{Event, SweepKind};

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
pub fn spawn_task(
    bus: &EventBus,
    queue: Arc<DurableQueue>,
    workspace_root: PathBuf,
    host_id: String,
    snapshot_interval: Duration,
    daemon_started_at: Instant,
) -> tokio::task::JoinHandle<()> {
    let subscription = bus.subscribe(["sweep.global.dispatch", "sweep.issue"]);
    tokio::spawn(run_collector(
        subscription,
        queue,
        workspace_root,
        host_id,
        snapshot_interval,
        daemon_started_at,
    ))
}

async fn run_collector(
    mut subscription: crate::event_bus::Subscription,
    queue: Arc<DurableQueue>,
    workspace_root: PathBuf,
    host_id: String,
    snapshot_interval: Duration,
    daemon_started_at: Instant,
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
                sample_snapshots(&queue, &workspace_root, &host_id, daemon_started_at).await;
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
/// event and push them onto `queue`.
async fn sample_snapshots(
    queue: &DurableQueue,
    workspace_root: &Path,
    host_id: &str,
    daemon_started_at: Instant,
) {
    let token_record = sample_token_snapshot(workspace_root);
    queue.push(TelemetryEnvelope::new(host_id, TelemetryRecord::TokensSnapshot(token_record)));
    let health_record = sample_host_health(workspace_root, daemon_started_at).await;
    queue.push(TelemetryEnvelope::new(host_id, TelemetryRecord::HostHealth(health_record)));
}

/// Read the resolved rotation-ranking file for `workspace_root` into a
/// [`TokenSnapshotRecord`]. `rank` is the account's position in the ranking
/// file (lower = preferred); an unreadable/missing/empty ranking yields an
/// empty `accounts` list rather than an error — mirrors
/// [`crate::capacity::read_ranking_at`]'s soft-fail contract.
fn sample_token_snapshot(workspace_root: &Path) -> TokenSnapshotRecord {
    let pool_dir = crate::tokens_pool::paths::resolve_tokens_dir(workspace_root);
    let ranking_path = pool_dir.join(".ranking");
    let mut accounts = Vec::new();
    if let Ok(contents) = std::fs::read_to_string(&ranking_path) {
        for (index, line) in contents.lines().enumerate() {
            if !line.contains('|') {
                continue;
            }
            let Some((account, status, usage_fraction)) =
                crate::tokens_pool::select::parse_ranking_line(line)
            else {
                continue;
            };
            let exhausted = !crate::capacity::AccountHealth::parse(&status).is_healthy();
            accounts.push(TokenAccountState {
                account,
                rank: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                usage_fraction,
                limit_window_reset_at: None,
                exhausted,
            });
        }
    }
    TokenSnapshotRecord {
        captured_at: Utc::now(),
        accounts,
    }
}

/// Sample host CPU/disk headroom into a [`HostHealthRecord`]. Every measured
/// field is `Option` — an unmeasurable probe stays absent rather than a fake
/// zero (mirrors `cpu_headroom`/`disk_headroom`'s own contract).
async fn sample_host_health(workspace_root: &Path, started_at: Instant) -> HostHealthRecord {
    // CPU idle refresh can block ~1s on macOS (`iostat`) — dispatched through
    // `spawn_blocking` per the exact pattern `work_finder`'s dynamic-cap tick
    // already uses.
    let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
    HostHealthRecord {
        captured_at: Utc::now(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_sec: started_at.elapsed().as_secs(),
        logical_cpus: crate::cpu_headroom::logical_cpu_count(),
        cpu_idle_fraction: crate::cpu_headroom::cached_cpu_idle_fraction(),
        load_per_core: crate::cpu_headroom::load_per_core(),
        worktree_root_free_gb: crate::disk_headroom::worktree_root_free_gb(workspace_root),
    }
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

    #[tokio::test]
    async fn host_health_sample_populates_daemon_version_and_uptime() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let record = sample_host_health(dir.path(), started).await;
        assert_eq!(record.daemon_version, env!("CARGO_PKG_VERSION"));
        assert!(record.logical_cpus >= 1);
    }
}
