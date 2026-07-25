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
//! dedup and the background reaper across the work-finder and epic supervisor
//! (and the IPC `DispatchSweep` path for the default workspace, which is seeded).
//!
//! # Reaper threading
//!
//! Each provisioned registry gets its own background reaper
//! ([`crate::sweep_registry::spawn_reaper_task`]). The pool is consumed from two
//! different threads — the work-finder runs on the shared daemon Tokio runtime,
//! the epic supervisor on its own dedicated OS thread with a private
//! current-thread runtime. To guarantee every reaper runs on the **shared**
//! daemon runtime regardless of which thread first provisions a workspace, the
//! pool captures a [`tokio::runtime::Handle`] to the shared runtime at
//! construction and enters it (`Handle::enter`) around the reaper spawn.
//!
//! # Scope (phase b)
//!
//! Registry **eviction** on `workspace remove` and `(repo, issue)`-keyed
//! namespacing are explicitly deferred to phase c (#3929). A deregistered
//! workspace's registry + reaper linger harmlessly (the loops simply stop
//! iterating it, since they only fan out over the *current* `effective_roots`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::event_bus::EventBus;
use crate::sweep_registry::{self, SweepRegistry, SweepRegistryConfig};

/// One cached per-workspace registry plus the reaper task keeping it reconciled.
struct PooledRegistry {
    registry: Arc<Mutex<SweepRegistry>>,
    /// The background reaper handle, kept alive for the daemon's lifetime.
    /// `None` for a **seeded** entry (the default workspace) whose reaper is
    /// owned by `main` — the pool must not abort it.
    _reaper: Option<tokio::task::JoinHandle<()>>,
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

        let arc = Arc::new(Mutex::new(registry));
        // Spawn the reaper on the shared daemon runtime regardless of which
        // thread we are called from (the epic supervisor uses its own runtime).
        let reaper = {
            let _guard = self.runtime.enter();
            sweep_registry::spawn_reaper_task(arc.clone())
        };
        log::info!("workspace_pool: provisioned sweep registry for {}", root.display());
        map.insert(
            root.to_path_buf(),
            PooledRegistry {
                registry: arc.clone(),
                _reaper: Some(reaper),
            },
        );
        arc
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
    use super::*;
    use tempfile::tempdir;

    fn pool() -> Arc<WorkspacePool> {
        Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current()))
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
}
