//! Per-workspace sweep-registry pool (Issue #3928 — phase b of #3835/#3926).
//!
//! Phase 1 (#3926) shipped the machine-level [`WorkspaceRegistry`] but wired
//! *nothing* in the daemon's autonomous loops to consume it. Phase b (this
//! issue) is that consumption: the work-finder and epic supervisor now fan out
//! over [`WorkspaceRegistry::effective_roots`] and dispatch into **each**
//! registered repo's working tree.
//!
//! The single-workspace assumption ran deep: both loops were constructed with
//! **one** [`SweepRegistry`] whose `workspace_root` is baked in at construction
//! and determines the spawned child's `current_dir`, the `.loom/locks` claim
//! directory, the `.loom/logs` sweep logs, and the `.loom/sweep-checkpoint`
//! directory (`SweepRegistryConfig::new(root)`). A single registry can only ever
//! dispatch correctly into **one** repo. So multi-repo dispatch genuinely needs
//! **N independent registries** — one per registered root.
//!
//! [`WorkspacePool`] owns those registries: a lazily-populated, hot-reconciled
//! cache keyed by workspace root. Both autonomous loops share **one** pool so a
//! given repo has exactly **one** registry instance — unifying the in-flight
//! dedup and the background reaper + watchdog across the work-finder and epic
//! supervisor (and the IPC `DispatchSweep` path for the default workspace,
//! which is seeded).
//!
//! # Reaper + watchdog threading
//!
//! Each provisioned registry gets its own background reaper
//! ([`crate::sweep_registry::spawn_reaper_task`]) **and** its own watchdog
//! ([`crate::sweep_registry::spawn_watchdog_task`], Issue #4124) — the
//! startup-hang, mid-build-death, and review-stall self-healing backstops that,
//! before #4124, ran only for the default workspace via `main`. The pool is
//! consumed from two different threads — the work-finder runs on the shared
//! daemon Tokio runtime, the epic supervisor on its own dedicated OS thread
//! with a private current-thread runtime. To guarantee every reaper and
//! watchdog runs on the **shared** daemon runtime regardless of which thread
//! first provisions a workspace, the pool captures a [`tokio::runtime::Handle`]
//! to the shared runtime at construction and enters it (`Handle::enter`)
//! around both spawns.
//!
//! # Eviction (phase c, #3929)
//!
//! [`WorkspacePool::evict`] removes a provisioned registry when its workspace is
//! deregistered (`workspace remove` / [`Request::DeregisterWorkspace`]), aborting
//! its background reaper and watchdog tasks so neither leaks for the daemon's
//! lifetime. The **seeded default workspace** is guarded — its registry,
//! reaper, and watchdog are owned by `main` and continue serving
//! default-workspace IPC requests, so `evict` is a no-op for it (identified by
//! `_reaper: None`).
//!
//! [`Request::DeregisterWorkspace`]: crate::types::Request::DeregisterWorkspace

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::event_bus::EventBus;
use crate::peer_claims::{self, ClaimAd, PeerClaimView};
use crate::safehouse::{self, InboundEventSink, PeerClaimSink};
use crate::sweep_registry::{self, host_identity, SweepRegistry, SweepRegistryConfig};

/// The daemon-wide safehouse peer-claim coordination context (Issue #4028): one
/// shared inbound view + one outbound publisher + the single coordination task
/// that bridges them to the room. Created lazily by
/// [`WorkspacePool::start_peer_coordination`] when `safehouse.enabled` is true;
/// every provisioned registry gets clones of the publisher + view so a soft
/// claim advertised by any repo's dispatch reaches the room and every repo's
/// work-finder sees peer claims. `None` ⇒ byte-for-byte no-op.
struct PeerCoordination {
    /// Bounded, non-blocking outbound channel to the coordination task.
    publisher: tokio::sync::mpsc::Sender<ClaimAd>,
    /// The shared inbound view the coordination task feeds and every
    /// dispatcher queries.
    view: Arc<Mutex<PeerClaimView>>,
    /// The coordination task handle, kept alive for the daemon's lifetime.
    _task: Option<tokio::task::JoinHandle<()>>,
}

/// One cached per-workspace registry plus the background tasks keeping it
/// reconciled and self-healing.
struct PooledRegistry {
    registry: Arc<Mutex<SweepRegistry>>,
    /// The background reaper handle, kept alive for the daemon's lifetime.
    /// `None` for a **seeded** entry (the default workspace) whose reaper is
    /// owned by `main` — the pool must not abort it. This is also the
    /// structural discriminator `evict` uses to detect the seeded default
    /// workspace (`_reaper.is_none()`) — see the doc comment on `evict`.
    _reaper: Option<tokio::task::JoinHandle<()>>,
    /// The background watchdog handle (Issue #4124), kept alive for the
    /// daemon's lifetime. `None` for a **seeded** entry (the default
    /// workspace) whose watchdog is owned by `main` — the pool must not
    /// abort it.
    _watchdog: Option<tokio::task::JoinHandle<()>>,
}

/// A lazily-populated, thread-safe cache of one [`SweepRegistry`] per workspace
/// root, shared by the work-finder and epic supervisor so each repo has exactly
/// one registry instance (unified dedup + reaper).
pub struct WorkspacePool {
    event_bus: Arc<EventBus>,
    /// Handle to the **shared** daemon runtime, used so reapers always run there
    /// even when a workspace is first provisioned from the epic supervisor's
    /// dedicated OS thread.
    runtime: tokio::runtime::Handle,
    inner: Mutex<HashMap<PathBuf, PooledRegistry>>,
    /// The daemon-wide safehouse peer-claim coordination context (Issue #4028),
    /// or `None` when `safehouse.enabled` is false. Set once by
    /// [`start_peer_coordination`](Self::start_peer_coordination); injected into
    /// every registry [`get_or_provision`](Self::get_or_provision) builds.
    peer_coord: Mutex<Option<PeerCoordination>>,
    /// The daemon-wide live safehouse connection-state cell (Issue #4345),
    /// updated by both [`start_safehouse_narration`](Self::start_safehouse_narration)'s
    /// sink and [`start_peer_coordination`](Self::start_peer_coordination)'s
    /// coordination task. Read back by [`safehouse_status`](Self::safehouse_status)
    /// for `loom-daemon status` — see `crate::safehouse::SafehouseState`.
    safehouse_state: safehouse::SharedSafehouseState,
}

impl WorkspacePool {
    /// Construct an empty pool that provisions registries on demand, spawning
    /// each registry's reaper on `runtime` (the shared daemon runtime).
    #[must_use]
    pub fn new(event_bus: Arc<EventBus>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            event_bus,
            runtime,
            inner: Mutex::new(HashMap::new()),
            peer_coord: Mutex::new(None),
            safehouse_state: safehouse::new_shared_state(),
        }
    }

    /// Start the optional safehouse fleet-comms narration sink (#3997) for
    /// `repo_root`. The sink subscribes the pool's shared [`Arc<EventBus>`] —
    /// the single place the bus is owned — and spawns on the daemon runtime, so
    /// every daemon-dispatched sweep's lifecycle transitions narrate into the
    /// safehouse room with no per-role changes. **Byte-for-byte no-op** when
    /// `safehouse.enabled` is false/absent: [`crate::safehouse::spawn_sink`]
    /// returns without subscribing or touching a socket. Best-effort by
    /// contract — a missing/refused peer degrades to a `warn`, never a sweep
    /// failure. The handle is detached (daemon-lifetime).
    ///
    /// `activity_db` (#4497) is threaded to the completion emit point purely so
    /// its public-feed `meta` can carry a best-effort per-issue `tokens` total;
    /// `None` omits that field and changes nothing else.
    pub fn start_safehouse_narration(
        &self,
        repo_root: &Path,
        activity_db: Option<Arc<Mutex<crate::activity::ActivityDb>>>,
    ) {
        let config = crate::safehouse::resolve_config(repo_root);
        let _ = crate::safehouse::spawn_sink(
            config,
            &self.event_bus,
            &self.runtime,
            self.safehouse_state.clone(),
            activity_db,
        );
    }

    /// Snapshot the live safehouse connection state (Issue #4345) for
    /// `loom-daemon status`: `not_configured` / `unreachable` / `connected`,
    /// replacing the pre-#4345 silence all three states shared. See
    /// `.loom/docs/safehouse.md` "New-host onboarding".
    #[must_use]
    pub fn safehouse_status(&self) -> crate::types::SafehouseStatus {
        safehouse::snapshot_state(&self.safehouse_state).to_status()
    }

    /// Start the daemon-wide safehouse **peer-claim coordination** (Issue #4028)
    /// keyed off `repo_root`'s config. Creates the shared inbound
    /// [`PeerClaimView`], the bounded outbound advertiser channel, and the single
    /// coordination task that bridges them to the room, then stores them so every
    /// registry [`get_or_provision`](Self::get_or_provision) builds — and the
    /// seeded default registry via [`inject_peer_coordination`](Self::inject_peer_coordination)
    /// — advertises and consumes soft claims.
    ///
    /// **Byte-for-byte no-op** when `safehouse.enabled` is false/absent: no view,
    /// no channel, no task, no socket. Idempotent — a second call is a no-op once
    /// coordination is established.
    pub fn start_peer_coordination(&self, repo_root: &Path) {
        let config = safehouse::resolve_config(repo_root);
        if !config.enabled {
            // Mirrors spawn_sink's own disabled handling — set here too since
            // this early return means spawn_peer_coordination (which would
            // otherwise set it) is never reached (#4345).
            safehouse::set_not_configured(&self.safehouse_state);
            return; // disabled ⇒ no coordination, byte-for-byte no-op
        }
        let mut slot = self
            .peer_coord
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return; // already established
        }
        let ttl = peer_claims::resolve_peer_claim_ttl(repo_root);
        let view = Arc::new(Mutex::new(PeerClaimView::new(host_identity(), ttl)));
        let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(safehouse::PEER_CLAIM_CHANNEL_CAP);
        let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view.clone()));
        let task = safehouse::spawn_peer_coordination(
            config,
            sink,
            rx,
            &self.runtime,
            self.safehouse_state.clone(),
        );
        if task.is_none() {
            // Enabled but no socket resolved: leave coordination unestablished so
            // registries stay in the no-op path (the outbound sender would have
            // no consumer). spawn_peer_coordination already logged the reason.
            return;
        }
        log::info!(
            "workspace_pool: safehouse peer-claim coordination started (ttl={}s)",
            ttl.as_secs()
        );
        *slot = Some(PeerCoordination {
            publisher: tx,
            view,
            _task: task,
        });
    }

    /// Inject the peer-claim publisher + view into `registry` when coordination
    /// is established (Issue #4028). A no-op when `safehouse.enabled` is false, so
    /// a registry with no coordination behaves byte-for-byte as before. Called by
    /// [`get_or_provision`](Self::get_or_provision) and by `main.rs` for the
    /// seeded default registry.
    pub fn inject_peer_coordination(&self, registry: &mut SweepRegistry) {
        let slot = self
            .peer_coord
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(coord) = slot.as_ref() {
            registry.set_peer_claim_publisher(coord.publisher.clone());
            registry.set_peer_claims(coord.view.clone());
        }
    }

    /// Seed the pool with a pre-built registry for `root` — used for the default
    /// workspace (the daemon's `sweep_workspace`), whose registry + reaper are
    /// already constructed and owned by `main` and are also used by the IPC
    /// `DispatchSweep` path. A `get_or_provision(root)` for the same root returns
    /// this shared instance rather than building a second one, preserving the
    /// pre-registry single-workspace behavior byte-for-byte.
    pub fn seed(&self, root: PathBuf, registry: Arc<Mutex<SweepRegistry>>) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(root).or_insert(PooledRegistry {
            registry,
            _reaper: None,
            _watchdog: None,
        });
    }

    /// Return the registry for `root`, provisioning it (and its reaper) on first
    /// access. Idempotent: repeated calls for the same root return the same
    /// shared [`Arc`].
    ///
    /// Provisioning mirrors the default-workspace construction in `main`:
    /// [`SweepRegistry::with_event_bus`], `reconstruct()`, and the #3887 dispatch
    /// stagger resolved from the workspace's own `.loom/config.json`.
    #[must_use]
    pub fn get_or_provision(&self, root: &Path) -> Arc<Mutex<SweepRegistry>> {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = map.get(root) {
            return existing.registry.clone();
        }

        let config = SweepRegistryConfig::new(root.to_path_buf());
        let mut registry = SweepRegistry::with_event_bus(config, self.event_bus.clone());
        match registry.reconstruct() {
            Ok(0) => {}
            Ok(n) => log::info!(
                "workspace_pool: reconstructed {n} sweep entr{} for {}",
                if n == 1 { "y" } else { "ies" },
                root.display()
            ),
            Err(e) => {
                log::warn!("workspace_pool: reconstruction failed for {}: {e}", root.display())
            }
        }
        let startup = sweep_registry::read_startup_race_config(root);
        registry.set_dispatch_stagger(sweep_registry::resolve_dispatch_stagger(&startup));
        // Startup-proof occupancy grace (#4003): resolve env > config > default
        // for this workspace, mirroring the dispatch stagger above.
        registry.set_startup_proof_grace(sweep_registry::resolve_startup_proof_grace(&startup));
        // Insta-crash quarantine (#3939): resolve env > config > default for this
        // workspace so the reaper quarantines a repeatedly-insta-crashing issue
        // instead of letting it be re-dispatched every tick.
        registry.set_quarantine_config(sweep_registry::resolve_quarantine_config(root));
        // Per-issue dispatch backoff (#4485): resolve env > config > default for
        // this workspace so a failing issue's re-dispatch cadence is bounded
        // even when its deaths land in a quarantine carve-out.
        registry.set_dispatch_backoff_config(sweep_registry::resolve_dispatch_backoff_config(root));
        // Claude-wrapper pre-flight-death workspace tripwire (#4386): resolve
        // env > config > default for this workspace, mirroring the
        // insta-crash quarantine config above.
        registry
            .set_preflight_tripwire_config(sweep_registry::resolve_preflight_tripwire_config(root));
        // Cross-host collision detection (#4085): resolve env > config >
        // default(off) so a shared-backlog deployment measures the baseline
        // duplicate-dispatch rate. Detection only — never changes dispatch.
        registry.set_collision_detection(sweep_registry::resolve_collision_detection(root));
        // Cross-host soft claim (#4028): attach the shared peer-claim publisher +
        // view when safehouse coordination is established. A no-op otherwise.
        self.inject_peer_coordination(&mut registry);

        let arc = Arc::new(Mutex::new(registry));
        // Spawn the reaper and watchdog on the shared daemon runtime
        // regardless of which thread we are called from (the epic supervisor
        // uses its own runtime) — both spawns must stay inside this guard or
        // they panic ("must be called from the context of a Tokio 1.x
        // runtime") when provisioned from the epic supervisor's own
        // current-thread runtime.
        let (reaper, watchdog) = {
            let _guard = self.runtime.enter();
            let reaper = sweep_registry::spawn_reaper_task(arc.clone());
            // Sweep watchdog (Issue #4124): every provisioned (pooled)
            // workspace now gets the same startup / mid-build-death /
            // review-stall self-healing backstops the default workspace has
            // had since #3887/#3895/#3910 — previously only the reaper was
            // wired up here. Reuses the `startup` config already resolved
            // above so per-workspace env > config > default precedence comes
            // out correct by construction (mirrors `set_dispatch_stagger` /
            // `set_startup_proof_grace` above).
            let watchdog = if sweep_registry::resolve_watchdog_enabled(&startup) {
                let timeout = sweep_registry::resolve_watchdog_timeout(&startup);
                let interval = sweep_registry::resolve_watchdog_interval(&startup);
                let review_stall_timeout = if sweep_registry::resolve_review_stall_enabled(&startup)
                {
                    Some(sweep_registry::resolve_review_stall_timeout(&startup))
                } else {
                    None
                };
                Some(sweep_registry::spawn_watchdog_task(
                    arc.clone(),
                    timeout,
                    interval,
                    review_stall_timeout,
                ))
            } else {
                log::info!("workspace_pool: watchdog disabled for {} (#3887)", root.display());
                None
            };
            (reaper, watchdog)
        };
        log::info!("workspace_pool: provisioned sweep registry for {}", root.display());
        map.insert(
            root.to_path_buf(),
            PooledRegistry {
                registry: arc.clone(),
                _reaper: Some(reaper),
                _watchdog: watchdog,
            },
        );
        arc
    }

    /// Evict the provisioned registry for `root`, aborting its background reaper
    /// task so it does not leak (Issue #3929). Called from the
    /// [`Request::DeregisterWorkspace`](crate::types::Request::DeregisterWorkspace)
    /// handler so `workspace remove` also drops the in-memory pool entry.
    ///
    /// Returns `true` when an entry was removed, `false` when `root` was not
    /// pooled **or** was the seeded default workspace (which `main` owns — its
    /// reaper must never be aborted from this path, since the daemon keeps
    /// serving default-workspace IPC requests). The default-workspace guard is
    /// structural: seeded entries carry `_reaper: None`.
    ///
    /// A live sweep child in the evicted registry is **not** killed — only the
    /// in-memory tracking + reaper go away. The child keeps running and its lock
    /// / log files are untouched; its terminal state simply becomes unobservable
    /// via IPC after eviction (an accepted consequence of an explicit operator
    /// `workspace remove`).
    ///
    /// Dropping a bare [`tokio::task::JoinHandle`] only *detaches* the task —
    /// it does **not** cancel it — so we `.abort()` explicitly before dropping.
    pub fn evict(&self, root: &Path) -> bool {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Guard the seeded default workspace: it has no owned reaper here
        // (`_reaper: None`) and `main` continues to serve its IPC requests.
        if map.get(root).is_some_and(|p| p._reaper.is_none()) {
            log::debug!(
                "workspace_pool: refusing to evict seeded default workspace {}",
                root.display()
            );
            return false;
        }
        match map.remove(root) {
            Some(pooled) => {
                // Release any outstanding quarantine labels before the
                // registry's reaper (the only thing that would otherwise
                // retry a failed release) is aborted (Issue #4110) — without
                // this, an evicted workspace's quarantined issues sit at
                // `loom:blocked` until the *next* full daemon restart's
                // startup reconciliation pass
                // ([`crate::quarantine_reconciliation`]).
                let flushed = pooled
                    .registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .flush_quarantines_for_eviction();
                if flushed > 0 {
                    log::info!(
                        "workspace_pool: flushed {flushed} quarantine release(s) for {} before \
                         eviction (#4110)",
                        root.display()
                    );
                }
                if let Some(reaper) = pooled._reaper {
                    reaper.abort();
                }
                if let Some(watchdog) = pooled._watchdog {
                    watchdog.abort();
                }
                log::info!("workspace_pool: evicted sweep registry for {}", root.display());
                true
            }
            None => false,
        }
    }

    /// Number of registries currently cached (test/observability aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether the pool has no cached registries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::*;
    use tempfile::tempdir;

    fn pool() -> Arc<WorkspacePool> {
        Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current()))
    }

    /// Write a `.loom/config.json` under `root` with the given JSON body
    /// (mirrors `sweep_registry::tests::write_config`, reimplemented locally
    /// since cross-module test helpers aren't shared).
    fn write_config(root: &Path, body: &str) {
        let loom = root.join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("config.json"), body).unwrap();
    }

    /// Drop the safehouse **env layer** so the `.loom/config.json` written by
    /// [`write_config`] is authoritative.
    ///
    /// `safehouse::apply_env_overrides` lets `$LOOM_SAFEHOUSE_ENABLED` /
    /// `$LOOM_SAFEHOUSE_SOCKET` / `$LOOM_SAFEHOUSE_ROOM` win over config, and
    /// `resolve_socket` falls back to `$SAFEHOUSED_SOCKET`. Any agent session
    /// spawned by a running `loom-daemon` with safehouse narration on exports
    /// exactly those, so a test that only writes a config file silently asserts
    /// against the *host's* socket there instead of its own tempdir (#4385).
    /// These tests passed under `cargo test` only because a sibling test in the
    /// same process happened to clear the vars first — the class of accidental
    /// dependency that per-test process isolation makes visible.
    fn clear_safehouse_env() {
        for key in [
            "LOOM_SAFEHOUSE_ENABLED",
            "LOOM_SAFEHOUSE_SOCKET",
            "LOOM_SAFEHOUSE_ROOM",
            "LOOM_SAFEHOUSE_PERSONA",
            "SAFEHOUSED_SOCKET",
        ] {
            std::env::remove_var(key);
        }
    }

    #[tokio::test]
    async fn provision_is_idempotent_per_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let a = pool.get_or_provision(&root);
        let b = pool.get_or_provision(&root);
        assert!(Arc::ptr_eq(&a, &b), "same root returns the same registry");
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn distinct_roots_get_distinct_registries() {
        let dir = tempdir().unwrap();
        let a_root = dir.path().join("a");
        let b_root = dir.path().join("b");
        std::fs::create_dir_all(&a_root).unwrap();
        std::fs::create_dir_all(&b_root).unwrap();
        let pool = pool();

        let a = pool.get_or_provision(&a_root);
        let b = pool.get_or_provision(&b_root);
        assert!(!Arc::ptr_eq(&a, &b), "distinct roots ⇒ distinct registries");
        assert_eq!(pool.len(), 2);
    }

    #[tokio::test]
    async fn seeded_registry_is_reused_not_reprovisioned() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let seeded =
            Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(root.clone()))));
        pool.seed(root.clone(), seeded.clone());
        assert_eq!(pool.len(), 1);

        let got = pool.get_or_provision(&root);
        assert!(Arc::ptr_eq(&got, &seeded), "seeded instance is returned as-is");
        assert_eq!(pool.len(), 1, "no second registry provisioned");
    }

    #[tokio::test]
    async fn empty_pool_reports_empty() {
        let pool = pool();
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
    }

    // ---- safehouse connection-state wiring (#4345) ----

    #[tokio::test]
    async fn safehouse_status_defaults_to_not_configured_before_any_start_call() {
        let pool = pool();
        assert_eq!(pool.safehouse_status().state, "not_configured");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn start_safehouse_narration_surfaces_unreachable_for_enabled_unresolved_socket() {
        clear_safehouse_env();
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let socket = dir.path().join("nope.sock"); // never bound
        write_config(
            &root,
            &format!(
                r#"{{"safehouse": {{"enabled": true, "socket": {:?}}}}}"#,
                socket.display().to_string()
            ),
        );
        let pool = pool();

        pool.start_safehouse_narration(&root, None);
        // The sink reports "configured, not yet connected" immediately on
        // spawn (before any connect attempt) — poll briefly rather than
        // assuming the spawned task has already run once.
        let mut status = pool.safehouse_status();
        for _ in 0..50 {
            if status.state != "not_configured" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = pool.safehouse_status();
        }
        assert_eq!(status.state, "unreachable");
        assert_eq!(status.socket, Some(socket));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn start_peer_coordination_disabled_reports_not_configured_even_after_prior_state() {
        clear_safehouse_env();
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let socket = dir.path().join("nope.sock");
        write_config(
            &root,
            &format!(
                r#"{{"safehouse": {{"enabled": true, "socket": {:?}}}}}"#,
                socket.display().to_string()
            ),
        );
        let pool = pool();

        // Drive the cell to a non-default state first via the narration sink.
        pool.start_safehouse_narration(&root, None);
        let mut status = pool.safehouse_status();
        for _ in 0..50 {
            if status.state != "not_configured" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = pool.safehouse_status();
        }
        assert_eq!(status.state, "unreachable", "precondition: cell must be non-default");

        // Now disable and re-resolve via the peer-coordination entry point's
        // own disabled fast path (which returns before ever calling
        // spawn_peer_coordination — the code path #4345 had to patch
        // explicitly).
        write_config(&root, r#"{"safehouse": {"enabled": false}}"#);
        pool.start_peer_coordination(&root);
        assert_eq!(pool.safehouse_status().state, "not_configured");
    }

    #[tokio::test]
    async fn evict_removes_provisioned_root_and_reprovisions_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let first = pool.get_or_provision(&root);
        assert_eq!(pool.len(), 1);

        assert!(pool.evict(&root), "evicting a provisioned root returns true");
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());

        // A subsequent get_or_provision re-provisions a *new* registry instance
        // (not the evicted Arc).
        let second = pool.get_or_provision(&root);
        assert!(!Arc::ptr_eq(&first, &second), "re-provisioned registry is a new instance");
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn evict_absent_root_is_false() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();
        assert!(!pool.evict(&root), "evicting a never-provisioned root is a no-op false");
        assert_eq!(pool.len(), 0);
    }

    #[tokio::test]
    async fn evict_seeded_default_workspace_is_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let seeded =
            Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(root.clone()))));
        pool.seed(root.clone(), seeded.clone());
        assert_eq!(pool.len(), 1);

        // The seeded default workspace is owned by `main`; evicting it here must
        // be a no-op so its reaper/IPC lifecycle is never disturbed.
        assert!(!pool.evict(&root), "seeded default workspace is not evictable");
        assert_eq!(pool.len(), 1, "seeded entry survives the evict attempt");

        let got = pool.get_or_provision(&root);
        assert!(Arc::ptr_eq(&got, &seeded), "seeded instance is still the pooled one");
    }

    // ========================================================================
    // Sweep watchdogs for pooled workspaces (Issue #4124)
    //
    // Before this fix, `get_or_provision` spawned only the reaper
    // (`spawn_reaper_task`) — the startup / mid-build-death / review-stall
    // self-healing watchdog (`spawn_watchdog_task`) ran only for the default
    // workspace, wired up once in `main.rs`. These tests cover: (1) every
    // pooled workspace now gets a watchdog alongside its reaper, (2) the
    // seeded default workspace does NOT get a second one (main already gives
    // it exactly one), (3) `evict` aborts the watchdog too (no orphan task),
    // and (4) per-workspace watchdog config is not collapsed to a shared /
    // default value.
    // ========================================================================

    #[tokio::test]
    async fn provision_spawns_watchdog_alongside_reaper() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let _ = pool.get_or_provision(&root);

        let map = pool.inner.lock().unwrap();
        let entry = map.get(&root).expect("root was just provisioned");
        assert!(entry._reaper.is_some(), "reaper is spawned for a pooled workspace");
        assert!(
            entry._watchdog.is_some(),
            "watchdog must also be spawned for a pooled workspace (#4124)"
        );
    }

    #[tokio::test]
    async fn seeded_default_workspace_is_not_double_watchdogged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let seeded =
            Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(root.clone()))));
        pool.seed(root.clone(), seeded.clone());

        {
            let map = pool.inner.lock().unwrap();
            let entry = map.get(&root).unwrap();
            assert!(entry._reaper.is_none(), "seeded entry owns no reaper (main owns it)");
            assert!(entry._watchdog.is_none(), "seeded entry owns no watchdog (main owns it)");
        }

        // get_or_provision for the same (seeded) root must return the
        // existing entry byte-for-byte, spawning nothing new — so the
        // default workspace still has exactly the one watchdog `main.rs`
        // gives it, never two.
        let got = pool.get_or_provision(&root);
        assert!(Arc::ptr_eq(&got, &seeded));
        let map = pool.inner.lock().unwrap();
        let entry = map.get(&root).unwrap();
        assert!(
            entry._reaper.is_none(),
            "get_or_provision must not spawn a reaper for the seeded root"
        );
        assert!(
            entry._watchdog.is_none(),
            "get_or_provision must not spawn a second watchdog for the seeded (default) root"
        );
    }

    #[tokio::test]
    async fn evict_aborts_watchdog_alongside_reaper() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pool = pool();

        let _ = pool.get_or_provision(&root);
        {
            let map = pool.inner.lock().unwrap();
            let entry = map.get(&root).unwrap();
            assert!(entry._reaper.is_some());
            assert!(entry._watchdog.is_some());
        }

        assert!(pool.evict(&root), "evicting a provisioned root returns true");
        assert!(pool.is_empty(), "evict removes the entry (and, internally, aborts both tasks)");

        // Refresh guard: eviction of the seeded default workspace must still
        // be refused even after pooled watchdogs are in the mix.
        let seeded_root = dir.path().join("seeded");
        std::fs::create_dir_all(&seeded_root).unwrap();
        let seeded =
            Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(seeded_root.clone()))));
        pool.seed(seeded_root.clone(), seeded);
        assert!(
            !pool.evict(&seeded_root),
            "seeded default workspace remains un-evictable (structural _reaper.is_none() guard)"
        );
    }

    #[tokio::test]
    async fn per_workspace_watchdog_config_is_not_collapsed_to_default() {
        // Two distinct roots, each with its own `.loom/config.json` setting a
        // *different* `autonomous.watchdog.timeoutSecs` / `intervalSecs`.
        // `get_or_provision` resolves this per-root via
        // `sweep_registry::read_startup_race_config(root)` — reproduce that
        // exact call here (rather than spying on the spawned task, which
        // bakes the resolved `Duration`s into opaque closure state) to prove
        // resolution is genuinely per-workspace, not a single shared value.
        let dir = tempdir().unwrap();
        let root_a = dir.path().join("a");
        let root_b = dir.path().join("b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();

        write_config(
            &root_a,
            r#"{"autonomous":{"watchdog":{"timeoutSecs":111,"intervalSecs":22}}}"#,
        );
        write_config(
            &root_b,
            r#"{"autonomous":{"watchdog":{"timeoutSecs":333,"intervalSecs":44}}}"#,
        );

        let cfg_a = sweep_registry::read_startup_race_config(&root_a);
        let cfg_b = sweep_registry::read_startup_race_config(&root_b);

        assert_eq!(sweep_registry::resolve_watchdog_timeout(&cfg_a), Duration::from_secs(111));
        assert_eq!(sweep_registry::resolve_watchdog_interval(&cfg_a), Duration::from_secs(22));
        assert_eq!(sweep_registry::resolve_watchdog_timeout(&cfg_b), Duration::from_secs(333));
        assert_eq!(sweep_registry::resolve_watchdog_interval(&cfg_b), Duration::from_secs(44));

        assert_ne!(
            sweep_registry::resolve_watchdog_timeout(&cfg_a),
            sweep_registry::resolve_watchdog_timeout(&cfg_b),
            "each workspace's watchdog timeout must resolve independently, not collapse to one \
             shared/default value"
        );
    }

    /// Install a fake `spawn-claude.sh` that starts and then hangs (writes
    /// only the spawn-header line, never a checkpoint) — mirrors
    /// `sweep_registry::tests::hung_child_registry` / `lifecycle_registry`
    /// (private to that module, reimplemented locally here).
    fn hung_watchdog_config(root: &Path) -> SweepRegistryConfig {
        let scripts_dir = root.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        std::fs::write(
            &fake_bin,
            "#!/usr/bin/env bash\n\
             echo \"spawn-claude: using OAuth account 'faketok' (mode=random)\"\n\
             sleep 30\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();
        let mut config = SweepRegistryConfig::new(root.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        // Never touch a real `gh` — a pooled-workspace registry inside
        // `get_or_provision` always resolves `skip_label_flip = false` (real
        // workspaces need real label flips), but that would fire real `gh
        // issue edit` calls against this crate's own real GitHub issues in a
        // test. This test proves the reclaim *mechanism* — the same
        // `spawn_watchdog_task` `get_or_provision` wires up — is live and
        // functional when driven by a real async tick.
        config.skip_label_flip = true;
        config.journal_path = Some(root.join("test-sweeps-journal.json"));
        config
    }

    async fn wait_for_async(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= timeout {
                return cond();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn pooled_workspace_watchdog_reclaims_a_hung_sweep() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let config = hung_watchdog_config(&root);
        let mut registry = SweepRegistry::new(config);

        let out = registry
            .dispatch(&crate::types::SweepKind::Issue(999_001), None, None, None, None)
            .unwrap();
        // Assert reclaim on the child PID, not the sweep_id: `generate_sweep_id`
        // mints IDs at second granularity (`Utc::now().timestamp()`), so this
        // fast dispatch→watchdog→re-dispatch cycle frequently completes within
        // one wall-clock second and the re-dispatched sweep gets an id identical
        // to the original (#4124 flaky test). A freshly spawned child always has
        // a distinct OS PID, so PID inequality is collision-proof.
        let original_pid = out.pid;

        let arc = Arc::new(Mutex::new(registry));
        // Short timeout/interval so the test doesn't wait the production
        // 300s/30s defaults; the watchdog's first real tick (after the
        // immediately-skipped boot tick) fires at ~interval, by which point
        // elapsed-since-dispatch already exceeds timeout.
        let watchdog = sweep_registry::spawn_watchdog_task(
            arc.clone(),
            Duration::from_millis(150),
            Duration::from_millis(200),
            None,
        );

        let reclaimed = wait_for_async(Duration::from_secs(10), || {
            let reg = arc.lock().unwrap();
            reg.list(Some(&crate::types::SweepState::Running))
                .iter()
                .any(|i| {
                    matches!(i.kind, crate::types::SweepKind::Issue(n) if n == 999_001)
                        && i.pid != original_pid
                })
        })
        .await;
        assert!(
            reclaimed,
            "watchdog should auto-cancel + re-dispatch the hung pooled-workspace sweep (#4124)"
        );

        watchdog.abort();

        // Cleanup: cancel any lingering hung child(ren) for the issue so the
        // fixture process doesn't outlive the test.
        let mut reg = arc.lock().unwrap();
        let ids: Vec<String> = reg
            .list(None)
            .into_iter()
            .filter(|i| {
                matches!(i.kind, crate::types::SweepKind::Issue(n) if n == 999_001)
                    && matches!(
                        i.state,
                        crate::types::SweepState::Running | crate::types::SweepState::Pending
                    )
            })
            .map(|i| i.sweep_id)
            .collect();
        for id in ids {
            let _ = reg.cancel(&id, Duration::from_millis(500));
        }
    }
}
