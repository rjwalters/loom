//! Lock-scope control for the `Issue` begin step (Issue #9573).
//!
//! `begin_prepared_issue_dispatch` used to run its whole guard chain under the
//! per-workspace `Mutex<SweepRegistry>`: roughly twenty sequential `gh` calls
//! plus the lease-order and sole-claim sleeps and the spawn stagger, all
//! serialised against the work finder, reaper and watchdogs of that workspace.
//! The chain's control flow is unchanged (one function, same order, same
//! refusals, same fail-open/closed directions); what changed is *who holds the
//! lock while a step runs*, which is what [`RegistryAccess`] abstracts.
//!
//! # Lock-scope audit (the `Issue` path through `begin_issue_via`)
//!
//! Under the lock (in-memory or local filesystem only):
//! - dedup (1/1b, re-run before the claim), roll gate, runtime admission when
//!   not pre-prepared, workspace-commands (local `stat`) and workspace-hold
//!   guards, the open-PR memo read and its backoff arming;
//! - noop-cooldown, dispatch-backoff, live-claim (`/proc` scan) and peer-claim
//!   guards, disk admission, `acquire_lock` (mkdir) and the in-flight
//!   idempotency insert, peer-claim advertise/retract, `note_label_flip`,
//!   unwind/failure bookkeeping, `reserve_dispatch_stagger`,
//!   `Command::spawn` of the local `spawn-worker.sh` child and the log header.
//!
//! Lock released (forge `gh` calls and sleeps), run on a [`forge_twin`]:
//! - `refuse_closed_issue` (2.5), `dispatch_open_pr_probe` (2.6),
//!   `refuse_recent_landing` (repo resolution), `guard_issue_labels` and
//!   `first_park_label_in` (2.7), `classify_preflip_labels` (4a),
//!   `flip_label_to_building` (4), `write_lease_comment` (4b),
//!   `resolve_lease_order` incl. `confirm_sole_claim` and their sleeps (4c),
//!   the 4d phantom check, label restore and yield comment,
//!   `guard_closed_or_pr` / `remove_building_claim` (4e), the restore after a
//!   spawn failure, and the stagger sleep (5).
//!
//! The local claim (lock dir + in-flight key) is taken under one continuous
//! lock hold that begins with a dedup re-check, so two concurrent dispatches
//! of one issue still yield exactly one spawn: the loser gets the idempotent
//! hand-back (same key) or the `acquire_lock` collision refusal. That hand-back
//! does not mean "spawned": a same-key retry arriving during the flip, the
//! lease-order sleeps or the stagger is handed back an attempt that can still
//! abort (4d yield, 4e release, spawn failure); see `idempotency_dedup`.
//!
//! The roll gate's `admit` and its `MidSpawn` guard (`roll_gate.enter()`) are
//! taken in the same first lock hold, so a roll that closes the gate while
//! this dispatch runs a lock-free forge step still counts it as mid-spawn
//! (#10831 H4).
//!
//! [`forge_twin`]: SweepRegistry::forge_twin

use super::*;
use std::sync::MutexGuard;

/// How the begin step reaches the registry, and when it may drop the lock.
pub(super) trait RegistryAccess {
    /// The registry, locking it first if it is not currently held.
    fn reg(&mut self) -> &mut SweepRegistry;
    /// Run a read-only forge step. `Held` runs it with the lock still held
    /// (the historical shape); `Shared` releases the lock for the duration.
    fn unlocked<R>(&mut self, f: impl FnOnce(&SweepRegistry) -> R) -> R;
    /// Sleep for `wait`, with the lock released where the access allows it.
    fn pause(&mut self, wait: Duration) {
        if !wait.is_zero() {
            self.unlocked(|_| std::thread::sleep(wait));
        }
    }
}

/// The caller already holds the lock: every step runs under it, exactly as
/// before #9573 (reaper/watchdog re-dispatch and direct callers).
pub(super) struct Held<'a>(pub(super) &'a mut SweepRegistry);

impl RegistryAccess for Held<'_> {
    fn reg(&mut self) -> &mut SweepRegistry {
        self.0
    }
    fn unlocked<R>(&mut self, f: impl FnOnce(&SweepRegistry) -> R) -> R {
        f(self.0)
    }
}

/// The caller owns the mutex: the lock is taken lazily by [`reg`] and dropped
/// around every [`unlocked`] step.
///
/// [`reg`]: RegistryAccess::reg
/// [`unlocked`]: RegistryAccess::unlocked
pub(super) struct Shared<'a> {
    mutex: &'a Mutex<SweepRegistry>,
    guard: Option<MutexGuard<'a, SweepRegistry>>,
    twin: SweepRegistry,
}

impl<'a> Shared<'a> {
    pub(super) fn new(mutex: &'a Mutex<SweepRegistry>) -> Self {
        let guard = mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let twin = guard.forge_twin();
        Self {
            mutex,
            guard: Some(guard),
            twin,
        }
    }
}

impl RegistryAccess for Shared<'_> {
    fn reg(&mut self) -> &mut SweepRegistry {
        let mutex = self.mutex;
        self.guard.get_or_insert_with(|| {
            mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
    }
    fn unlocked<R>(&mut self, f: impl FnOnce(&SweepRegistry) -> R) -> R {
        self.guard = None;
        f(&self.twin)
    }
}

impl SweepRegistry {
    /// A registry that shares this one's forge-relevant state, for running the
    /// read-only `&self` forge guards without the lock (Issue #9573).
    ///
    /// Every guard that shells out to `gh` reads only [`SweepRegistryConfig`]
    /// and two caches, the verified-open-PR memo and the resolved `owner/repo`;
    /// both caches are shared `Arc`s here, so a write made through the twin
    /// (the probe seeding the memo, a first `gh repo view` resolve) is seen by
    /// the real registry and vice versa. Nothing else is copied: the twin has
    /// no entries, children or cooldown state and must never be used for
    /// anything but those guards.
    pub(super) fn forge_twin(&self) -> Self {
        let mut twin = Self::new(self.config.clone());
        twin.open_pr_memo = Arc::clone(&self.open_pr_memo);
        twin.owner_repo_cache = Arc::clone(&self.owner_repo_cache);
        #[cfg(test)]
        {
            twin.test_force_rate_limited = self.test_force_rate_limited;
        }
        twin
    }

    /// Reserve this dispatch's slot in the spawn stagger (Issue #3887) and
    /// return how long the caller must sleep before spawning. The lock-free
    /// counterpart of [`Self::apply_dispatch_stagger`]: `last_spawn_at` is set
    /// to the instant the caller will wake (not the instant it finished
    /// sleeping), so a dispatch that arrives during the sleep computes its own
    /// wait from that wake time and the spacing between spawns is the one the
    /// sleeping lock holder used to enforce.
    pub(crate) fn reserve_dispatch_stagger(&mut self) -> Duration {
        let now = Instant::now();
        let wait = match (self.dispatch_stagger.is_zero(), self.last_spawn_at) {
            (false, Some(last)) => (last + self.dispatch_stagger).saturating_duration_since(now),
            _ => Duration::ZERO,
        };
        if !wait.is_zero() {
            log::debug!(
                "sweep_registry: staggering spawn by {}ms to avoid startup race (#3887)",
                wait.as_millis()
            );
        }
        self.last_spawn_at = Some(now + wait);
        wait
    }
}
