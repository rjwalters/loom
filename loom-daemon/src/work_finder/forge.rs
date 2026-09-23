//! Concrete [`WorkSource`] / [`WorkDispatcher`] implementations that wire the
//! finder to the live forge (`gh`) and the daemon's [`SweepRegistry`].
//!
//! The pure [`super::tick`] logic is exercised in tests via mocks; these
//! adapters are the runtime glue and shell out to `gh` / spawn children, so
//! they are not unit-tested directly (mirroring
//! [`crate::epic_supervisor::forge`]).
//!
//! Extracted from the inline `pub mod forge { … }` block in `work_finder.rs`
//! into its own file (#8572) for the same file-size-ratchet reason as
//! [`super::registry_refresh`]: `work_finder.rs` is over the threshold
//! (`.loom/docs/file-size-policy.md`) and may not grow. Pure move — the
//! module's position in the module tree, and therefore every `super::` path
//! inside it, is unchanged.

use super::{
    read_work_finder_config, resolve_extra_skip_labels_with_config, WorkDispatcher, WorkItem,
    WorkSource,
};
use crate::sweep_registry::SweepRegistry;
use crate::types::{SweepKind, SweepState};
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A forge-backed [`WorkSource`] that lists open `loom:issue` items via
/// `gh`. Mirrors [`crate::epic_supervisor::forge::GhEpicSource`].
pub struct GhWorkSource {
    gh_bin: PathBuf,
    repo: Option<String>,
    /// Working directory the `gh` query runs in. When set (multi-workspace
    /// fan-out, #3928) `gh` auto-detects the repo from that root's git
    /// remote, so each registered workspace is polled against its own repo
    /// without a single machine-global `LOOM_REPO`. `None` keeps today's
    /// behavior (inherit the daemon's cwd).
    cwd: Option<PathBuf>,
}

impl GhWorkSource {
    /// Construct a source using `gh` from `PATH`, honoring `LOOM_REPO` for
    /// the `--repo` flag when set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gh_bin: PathBuf::from("gh"),
            repo: std::env::var("LOOM_REPO").ok(),
            cwd: None,
        }
    }

    /// Construct a source scoped to a specific workspace `root` (#3928): the
    /// `gh` query runs with `current_dir(root)` so it targets that repo's own
    /// remote. `LOOM_REPO`, when set, is still honored as a machine-global
    /// `--repo` override (preserving the single-workspace behavior
    /// byte-for-byte); in a genuine multi-repo deployment it is left unset so
    /// each root's cwd selects its repo.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self {
            gh_bin: PathBuf::from("gh"),
            repo: std::env::var("LOOM_REPO").ok(),
            cwd: Some(root.to_path_buf()),
        }
    }

    /// Override the `gh` binary path (for tests / non-standard installs).
    #[must_use]
    pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
        self.gh_bin = bin;
        self
    }
}

impl Default for GhWorkSource {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkSource for GhWorkSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        // ETag-cached REST listing (#4428): a poll where nothing changed
        // costs zero rate limit (304), replacing the per-tick GraphQL
        // `gh issue list`. REST issue listings include PRs, so filter the
        // `pull_request`-marked rows to keep the pre-#4428 issue-only set.
        let rows = crate::forge_listing::list_issues_cached(
            &self.gh_bin,
            self.cwd.as_deref(),
            self.repo.as_deref(),
            "loom:issue",
            "open",
        )?;
        Ok(rows
            .into_iter()
            .filter(|r| !r.is_pull_request)
            // The REST listing already returns `body` (#4827) — carrying it
            // onto the item costs no extra request and lets dispatch read
            // the `<!-- loom:complexity=... -->` stratum without a
            // per-issue `gh issue view`.
            .map(|r| {
                WorkItem::with_created_at(r.number, r.labels, r.created_at)
                    .with_body(r.body)
                    .with_updated_at(r.updated_at)
            })
            .collect())
    }
}

/// A concrete [`WorkDispatcher`] backed by the daemon [`SweepRegistry`].
///
/// `dispatch()` calls the registry's own `dispatch()` — reusing its
/// idempotency key, `mkdir`-atomic claim lock, and `loom:issue →
/// loom:building` label flip — so the finder never reimplements the race
/// guard. `in_flight()` reads the registry's `Running` / `Pending` entries.
pub struct RegistryDispatcher {
    registry: Arc<Mutex<SweepRegistry>>,
}

impl RegistryDispatcher {
    /// Construct a dispatcher over the shared registry.
    #[must_use]
    pub fn new(registry: Arc<Mutex<SweepRegistry>>) -> Self {
        Self { registry }
    }

    /// The shared registry behind this dispatcher. Test-only seam so a
    /// restart-survivorship test (#6262) can seed the registry the way the
    /// daemon's own startup pass does — through the real registry, not by
    /// injecting an in-flight set the way the `RecordingDispatcher` fake
    /// allows.
    #[cfg(test)]
    #[must_use]
    pub fn registry_for_test(&self) -> Arc<Mutex<SweepRegistry>> {
        self.registry.clone()
    }
}

impl WorkDispatcher for RegistryDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        let mut reg = match self.registry.lock() {
            Ok(r) => r,
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                return HashSet::new();
            }
        };
        // Reap-on-read (Issue #3893): reconcile liveness before seeding
        // occupancy so a sweep whose child has exited does not over-count
        // against the concurrency budget and defer legitimate new dispatch.
        reg.reap_liveness();
        let mut set = HashSet::new();
        for state in [SweepState::Running, SweepState::Pending] {
            for info in reg.list(Some(&state)) {
                if let SweepKind::Issue(n) = info.kind {
                    set.insert(n);
                }
            }
        }
        set
    }

    fn quarantined(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.quarantined_issues(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Issues inside a live per-issue dispatch-backoff window (Issue #4485).
    /// Pure in-memory read of the registry state the reaper maintains — no
    /// forge round trip, mirroring `quarantined()`.
    fn backed_off(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.dispatch_backoff_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// The subset of `backed_off()` whose window was armed by the
    /// open-PR guard rather than a real dispatch failure (Issue #7606).
    /// Pure in-memory read, mirroring `backed_off()`.
    fn pr_open_backed_off(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.open_pr_backoff_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Issues inside a live no-op re-dispatch cooldown window (Issue
    /// #6670). Pure in-memory read of the registry state a
    /// `RecordNoopRelease` call maintains — no forge round trip,
    /// mirroring `backed_off()`.
    fn noop_cooldown(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.noop_cooldown_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Issues inside a live hard-exclusion decline cooldown window (Issue
    /// #7528). Pure in-memory read of the registry state the reaper's
    /// checkpoint-less clean-exit path maintains — no forge round trip,
    /// mirroring `noop_cooldown()`.
    fn declined(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.decline_cooldown_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Whether this workspace is missing `.claude/commands/loom/sweep.md`
    /// (Issue #4027 guard 2.4, quarantined at the work-finder level by
    /// #6440). A cheap `stat` via `SweepRegistryConfig::has_sweep_command`
    /// — no forge round trip, no lock contention beyond the same mutex
    /// every other dispatcher method already takes.
    ///
    /// Mirrors `dispatch_inner`'s own `!self.config.skip_label_flip &&
    /// !self.config.has_sweep_command()` gate exactly: `skip_label_flip`
    /// marks a hermetic unit-test fixture that never installs
    /// `.claude/commands/loom/` on disk, so without this term every such
    /// fixture would read as workspace-commands-missing and this
    /// pre-filter would silently zero out their candidate batches —
    /// unlike the guard in `dispatch()` itself, which they never actually
    /// reach (label flips, and thus this guard, are the thing they're
    /// opting out of).
    fn workspace_commands_missing(&self) -> bool {
        match self.registry.lock() {
            Ok(reg) => !reg.config().skip_label_flip && !reg.config().has_sweep_command(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                false
            }
        }
    }

    /// Discounted occupancy count (Issue #4003): a sweep dispatched longer
    /// than the registry's configured startup-proof grace window with zero
    /// observed startup signal does not count toward the budget — see
    /// `SweepRegistry::occupied_issues`. Reap-on-read first, mirroring
    /// `in_flight()`, so a child whose process already exited never
    /// over-counts either.
    fn occupancy(&self) -> usize {
        let mut reg = match self.registry.lock() {
            Ok(r) => r,
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                return 0;
            }
        };
        reg.reap_liveness();
        reg.occupied_issues().len()
    }

    fn collisions(&self) -> u64 {
        match self.registry.lock() {
            Ok(reg) => reg.collision_count(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                0
            }
        }
    }

    fn peer_claimed(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.peer_claimed_issues(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Additional skip-label list for this workspace (Issue #6685),
    /// resolved fresh each call from `<workspace_root>/.loom/config.json`
    /// via [`read_work_finder_config`] / [`resolve_extra_skip_labels_with_config`]
    /// — a cheap JSON read (mirrors `workspace_commands_missing()`'s own
    /// per-call `stat`), so an operator's `autonomous.workFinder.extraSkipLabels`
    /// edit takes effect on the very next tick with no registry-side
    /// config plumbing or daemon restart required.
    fn extra_skip_labels(&self) -> Vec<String> {
        match self.registry.lock() {
            Ok(reg) => resolve_extra_skip_labels_with_config(&read_work_finder_config(
                &reg.config().workspace_root,
            )),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                Vec::new()
            }
        }
    }

    /// Capabilities this **host** declares it holds (#6893), read fresh each
    /// call from `LOOM_WORKER_CAPABILITIES`.
    ///
    /// Deliberately NOT resolved from `.loom/config.json` the way
    /// [`extra_skip_labels`](Self::extra_skip_labels) above is: a skip-label
    /// list is a repo policy, but "this machine has root / an admin token /
    /// a production cloud profile" is a property of the host and its
    /// credentials, and a file committed to git must not be able to assert
    /// it. See [`crate::capability`].
    fn declared_capabilities(&self) -> std::collections::BTreeSet<String> {
        crate::capability::held_capabilities()
    }

    fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
        log::info!("work_finder: attempting issue #{issue}");
        // Keep the cached complexity stratum, but resolve its model only after
        // runtime admission. A Claude default must not become a native pin.
        // Idempotency key + the registry's claim lock make a re-dispatch of
        // an already-running issue a no-op (`was_new = false`) or a loud
        // lock-collision error.
        let key = format!("workfinder-{issue}");
        // Issue #6688: extends the #6592 begin/poll/finish split (proven
        // for the IPC `DispatchSweep` handler) to this call site, so the
        // account-selection poll no longer holds the registry mutex a
        // concurrent `DaemonStatus`/`health` IPC call's per-root
        // `registry.lock()` (`ipc.rs::build_daemon_status`) needs.
        let outcome = crate::sweep_registry::dispatch_model_releasing_poll_lock(
            &self.registry,
            &SweepKind::Issue(issue),
            Some(key),
            crate::sweep_registry::DispatchModel::Autonomous { complexity },
            None,
            None,
        )?;
        Ok(outcome.was_new)
    }
}
