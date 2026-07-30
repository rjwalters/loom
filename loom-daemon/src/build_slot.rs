//! Machine-wide **build slot** — serializes the rare, genuinely CPU-heavy
//! stages of a sweep instead of throttling every sweep's admission (#4512).
//!
//! # Why this exists (and what it replaced)
//!
//! Admission used to carry a CPU term: the dynamic concurrency cap included
//! `cpu_headroom = (logical_cpus × cpuUtilizationTarget − consumed_cores) /
//! estCoresPerSweep` (#3978/#4031). That priced **every** sweep as if it were
//! a build. The issue→PR pipeline is dominated by API-wait (curator / builder /
//! judge conversations); the sustained-heavy-CPU phases (release builds, full
//! test suites, the build gate) are a small fraction of wall-clock. So the term
//! throttled the ~90% low-CPU majority to defend against the minority case —
//! measurably so: an 8-core worker sitting **95% idle** was capped at 2
//! concurrent sweeps (#4512's evidence).
//!
//! #4512 deleted the CPU term from admission
//! ([`crate::work_finder::resolve_dynamic_max_concurrent`] is now `min(token
//! axis, disk headroom, configured maxConcurrent)`) and moved the protection
//! **to where the load actually is**: N sweeps run concurrently, but at most
//! [`DEFAULT_BUILD_SLOTS`] of them hold the build slot at any moment, so the
//! heavy stages serialize while the rest of every sweep's lifecycle never
//! queues. This is the bounded revival of #4003's "agents check out a slot only
//! for bound work".
//!
//! # Shape: the same `mkdir`-atomic lock as the worktree / claim locks
//!
//! A slot is a lock **directory** created with `mkdir` (POSIX-atomic, works
//! across processes *and* across languages — the Bash half of this,
//! `defaults/scripts/lib/build-slot.sh`, implements the identical protocol so
//! `build-gate.sh` in a sweep worktree serializes against the daemon's own
//! gate). The primitive is [`MkdirLock`], the same one the token-pool state
//! files and the per-issue claim lock (`.loom/locks/issue-<N>`) use; `flock` is
//! deliberately avoided (unavailable on stock macOS).
//!
//! Slots live **machine-wide** at `~/.loom/locks/build-slot/slot-<i>` (override
//! with [`BUILD_SLOT_DIR_ENV`]) — deliberately *not* under a repo's
//! `.loom/locks/`: the host's cores are one machine-level resource shared by
//! every workspace the daemon manages, and #4512's operator note asked
//! explicitly for **one unambiguous home** for machine-tier concurrency state
//! rather than a per-workspace tier.
//!
//! # Three hard safety properties
//!
//! 1. **Never deadlocks.** [`acquire`] waits at most
//!    [`DEFAULT_BUILD_SLOT_WAIT_SECS`] and then **degrades open** — it returns
//!    a lease that holds no slot and lets the caller run unserialized. A slow
//!    or crashed holder therefore costs throughput, never liveness.
//! 2. **Degrades open on any lock-store failure.** An unwritable / missing /
//!    permission-denied lock directory yields
//!    [`LeaseKind::DegradedOpen`] immediately — no spinning, no error
//!    propagation to the caller.
//! 3. **Re-entrant.** A holder exports [`BUILD_SLOT_HELD_ENV`] into its child
//!    environment, so a nested acquire (the daemon's main-health gate holding
//!    the slot around `build-gate.sh`, which then tries to take it too) is a
//!    logged no-op instead of a self-inflicted wait.
//!
//! Because a slot is held for the *duration of a build* (minutes), the stale
//! reaping threshold is deliberately long ([`DEFAULT_STALE_SLOT_SECS`], 1h) —
//! `mkdir` locks carry no heartbeat, and a 30s threshold (the token-pool
//! default) would let a peer reap a perfectly healthy in-progress build. A
//! crashed holder is covered by property 1 (bounded wait → degrade open), not
//! by aggressive reaping.
//!
//! # Telemetry
//!
//! Every state transition is logged once: `info` on acquire (naming the slot
//! and how long it waited), `info` on the first wait of a round (so a queue is
//! visible), `info` on release with the hold duration, and `warn` on
//! degrade-open (naming *why*). The host-distress circuit breaker (#4235)
//! remains the load safety net that makes a hand-tuned `maxConcurrent` safe:
//! a mis-set knob trips the breaker — measured — instead of melting the host.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::tokens_pool::locking::MkdirLock;

// ============================================================================
// Configuration (env-only, like `LOOM_PER_WORKTREE_GB`)
// ============================================================================
//
// Env-only on purpose. The Bash half (`defaults/scripts/lib/build-slot.sh`,
// sourced by `build-gate.sh` inside a sweep worktree) must resolve exactly the
// same values as the daemon, and `disk_headroom`'s `LOOM_PER_WORKTREE_GB` set
// the precedent (#4032 decision): a knob with an independent Bash-side reader
// stays env-only rather than honoring `.loom/config.json` on one path and
// silently ignoring it on the other. A repo that wants a different slot count
// exports it from the daemon's start environment.

/// Number of concurrent build slots on this machine. `0` disables build-slot
/// serialization entirely (every acquire degrades open) — the exact pre-#4512
/// behavior for an operator who wants it back.
pub const BUILD_SLOTS_ENV: &str = "LOOM_BUILD_SLOTS";

/// Default build-slot count: **1**. The heavy stages (`cargo build`/`test`, the
/// build gate) parallelize across essentially every core on their own, so one
/// at a time is the point — #4512 allows "at most 1–2", and 1 is the
/// conservative end.
pub const DEFAULT_BUILD_SLOTS: usize = 1;

/// Bounded wait for a slot, in seconds. On expiry the acquire **degrades open**
/// (never blocks longer, never fails).
pub const BUILD_SLOT_WAIT_SECS_ENV: &str = "LOOM_BUILD_SLOT_WAIT_SECS";

/// Default bounded wait: 300s. Long enough to actually serialize behind a
/// typical build-gate run, short enough that a wedged holder costs one build's
/// worth of throughput rather than stalling a sweep indefinitely.
pub const DEFAULT_BUILD_SLOT_WAIT_SECS: u64 = 300;

/// Age (seconds) at which a slot lock directory is considered abandoned and is
/// reaped. Deliberately long — see the module docs.
pub const BUILD_SLOT_STALE_SECS_ENV: &str = "LOOM_BUILD_SLOT_STALE_SECS";

/// Default stale-slot threshold: 1h.
pub const DEFAULT_STALE_SLOT_SECS: u64 = 3600;

/// Override for the machine-wide slot directory (default
/// `~/.loom/locks/build-slot`). Primarily a test seam; also lets an operator
/// relocate the slots onto a specific filesystem.
pub const BUILD_SLOT_DIR_ENV: &str = "LOOM_BUILD_SLOT_DIR";

/// Re-entrancy sentinel exported into a slot holder's child environment. When
/// set to a truthy value, [`acquire`] is a logged no-op — the caller is already
/// running inside a slot.
pub const BUILD_SLOT_HELD_ENV: &str = "LOOM_BUILD_SLOT_HELD";

/// How often to re-probe the slots while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Resolve the slot count from [`BUILD_SLOTS_ENV`], falling back to
/// [`DEFAULT_BUILD_SLOTS`]. An unparseable value falls back to the default; an
/// explicit `0` is honored (serialization disabled).
#[must_use]
pub fn resolve_slots() -> usize {
    std::env::var(BUILD_SLOTS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_BUILD_SLOTS)
}

/// Resolve the bounded wait from [`BUILD_SLOT_WAIT_SECS_ENV`].
#[must_use]
pub fn resolve_wait() -> Duration {
    Duration::from_secs(
        std::env::var(BUILD_SLOT_WAIT_SECS_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_BUILD_SLOT_WAIT_SECS),
    )
}

/// Resolve the stale-slot threshold from [`BUILD_SLOT_STALE_SECS_ENV`].
#[must_use]
pub fn resolve_stale() -> Duration {
    Duration::from_secs(
        std::env::var(BUILD_SLOT_STALE_SECS_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_STALE_SLOT_SECS),
    )
}

/// Whether this process is already running inside a build slot (a truthy
/// [`BUILD_SLOT_HELD_ENV`]).
#[must_use]
pub fn is_held_here() -> bool {
    std::env::var(BUILD_SLOT_HELD_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// The machine-wide slot directory: [`BUILD_SLOT_DIR_ENV`] when set, else
/// `~/.loom/locks/build-slot`. `None` when neither is resolvable (no home
/// directory) — the caller degrades open.
#[must_use]
pub fn slot_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(BUILD_SLOT_DIR_ENV) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    Some(
        dirs::home_dir()?
            .join(".loom")
            .join("locks")
            .join("build-slot"),
    )
}

// ============================================================================
// The lease
// ============================================================================

/// What a [`BuildSlotLease`] actually obtained — the telemetry-bearing outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseKind {
    /// A slot was acquired (0-based index) after waiting `waited`.
    Held { slot: usize, waited: Duration },
    /// This process is already inside a slot ([`BUILD_SLOT_HELD_ENV`]) — the
    /// acquire was a no-op and nothing is released on drop.
    Reentrant,
    /// No slot is held and the caller should proceed **unserialized**. `reason`
    /// names why (disabled, lock store unusable, or the bounded wait expired).
    DegradedOpen { reason: String },
}

impl LeaseKind {
    /// A stable one-word identifier for logs/metrics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Held { .. } => "held",
            Self::Reentrant => "reentrant",
            Self::DegradedOpen { .. } => "degraded-open",
        }
    }
}

/// RAII lease on a machine-wide build slot. Release (`rmdir`) happens on drop,
/// including on panic/early-return, so a slot is never leaked by a control-flow
/// path the caller forgot about.
///
/// Construction **never fails**: every failure mode collapses into
/// [`LeaseKind::DegradedOpen`], because refusing to run a build because the
/// lock store is broken would be strictly worse than running it unserialized.
pub struct BuildSlotLease {
    kind: LeaseKind,
    /// The held lock, if any. Dropping it removes the slot directory.
    lock: Option<MkdirLock>,
    /// Label of the stage holding the slot, for the release log line.
    label: String,
    /// When the slot was acquired, for the hold-duration log line.
    acquired_at: Instant,
}

impl BuildSlotLease {
    /// The outcome of the acquire.
    #[must_use]
    pub fn kind(&self) -> &LeaseKind {
        &self.kind
    }

    /// Whether this lease actually holds a slot (`false` for a re-entrant or
    /// degraded-open lease). Callers use this to decide whether to export
    /// [`BUILD_SLOT_HELD_ENV`] to children.
    #[must_use]
    pub fn holds_slot(&self) -> bool {
        matches!(self.kind, LeaseKind::Held { .. })
    }

    /// The 0-based index of the held slot, or `None` when no slot is held.
    #[must_use]
    pub fn slot_index(&self) -> Option<usize> {
        match self.kind {
            LeaseKind::Held { slot, .. } => Some(slot),
            _ => None,
        }
    }

    /// Whether children of this process are already covered by a slot — true
    /// when this lease holds one **or** an ancestor already did. Children get
    /// [`BUILD_SLOT_HELD_ENV`] set exactly when this is true, which is what
    /// makes nested acquires no-ops rather than self-waits.
    #[must_use]
    pub fn covers_children(&self) -> bool {
        matches!(self.kind, LeaseKind::Held { .. } | LeaseKind::Reentrant)
    }

    fn degraded(label: &str, reason: String, warn: bool) -> Self {
        if warn {
            log::warn!(
                "build_slot: proceeding WITHOUT a slot for '{label}' — {reason} \
                 (degrading open: high-CPU stages are not serialized this run)"
            );
        } else {
            log::debug!("build_slot: no slot needed for '{label}' — {reason}");
        }
        Self {
            kind: LeaseKind::DegradedOpen { reason },
            lock: None,
            label: label.to_string(),
            acquired_at: Instant::now(),
        }
    }
}

impl Drop for BuildSlotLease {
    fn drop(&mut self) {
        if let LeaseKind::Held { slot, .. } = self.kind {
            // Drop the lock first so the slot is free before we log the release.
            self.lock = None;
            log::info!(
                "build_slot: released slot {slot} after {:.1}s ('{}')",
                self.acquired_at.elapsed().as_secs_f64(),
                self.label
            );
        }
    }
}

// ============================================================================
// acquire
// ============================================================================

/// Acquire a machine-wide build slot for `label`, resolving every knob from the
/// environment. Blocks for at most [`resolve_wait`], then degrades open.
///
/// **Blocking** — call from a synchronous context (or `spawn_blocking`), never
/// inline on a tokio runtime worker.
#[must_use]
pub fn acquire(label: &str) -> BuildSlotLease {
    if is_held_here() {
        log::debug!(
            "build_slot: '{label}' is already inside a build slot ({BUILD_SLOT_HELD_ENV} is set) \
             — re-entrant no-op"
        );
        return BuildSlotLease {
            kind: LeaseKind::Reentrant,
            lock: None,
            label: label.to_string(),
            acquired_at: Instant::now(),
        };
    }
    let slots = resolve_slots();
    if slots == 0 {
        return BuildSlotLease::degraded(
            label,
            format!("{BUILD_SLOTS_ENV}=0 disables build-slot serialization"),
            false,
        );
    }
    let Some(dir) = slot_dir() else {
        return BuildSlotLease::degraded(
            label,
            format!("no home directory to resolve the slot dir (set {BUILD_SLOT_DIR_ENV})"),
            true,
        );
    };
    acquire_in(&dir, slots, resolve_wait(), POLL_INTERVAL, resolve_stale(), label)
}

/// [`acquire`] with every input injected — the testable core (no env reads, no
/// `$HOME`, caller-chosen timings so a test never waits seconds).
#[must_use]
pub fn acquire_in(
    dir: &Path,
    slots: usize,
    wait: Duration,
    poll: Duration,
    stale: Duration,
    label: &str,
) -> BuildSlotLease {
    if slots == 0 {
        return BuildSlotLease::degraded(label, "slot count is 0".to_string(), false);
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return BuildSlotLease::degraded(
            label,
            format!("slot dir {} is unusable: {e}", dir.display()),
            true,
        );
    }

    let started = Instant::now();
    let deadline = started + wait;
    let mut logged_wait = false;
    loop {
        match try_round(dir, slots, stale) {
            Ok(Some((slot, lock))) => {
                let waited = started.elapsed();
                log::info!(
                    "build_slot: acquired slot {slot}/{slots} for '{label}' after {:.1}s wait \
                     ({})",
                    waited.as_secs_f64(),
                    dir.display()
                );
                return BuildSlotLease {
                    kind: LeaseKind::Held { slot, waited },
                    lock: Some(lock),
                    label: label.to_string(),
                    acquired_at: Instant::now(),
                };
            }
            Ok(None) => {}
            Err(e) => {
                // The lock store itself is broken (permissions, vanished
                // parent). Spinning cannot fix that — degrade open now.
                return BuildSlotLease::degraded(
                    label,
                    format!("slot lock at {} is unusable: {e}", dir.display()),
                    true,
                );
            }
        }
        if !logged_wait {
            log::info!(
                "build_slot: all {slots} slot(s) busy — '{label}' waiting up to {}s for a slot",
                wait.as_secs()
            );
            logged_wait = true;
        }
        if Instant::now() >= deadline {
            return BuildSlotLease::degraded(
                label,
                format!(
                    "all {slots} slot(s) still busy after the {}s bounded wait",
                    wait.as_secs()
                ),
                true,
            );
        }
        std::thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// One non-blocking pass over every slot. `Ok(None)` = all busy; `Err` = the
/// lock store is unusable (the caller degrades open instead of spinning).
fn try_round(
    dir: &Path,
    slots: usize,
    stale: Duration,
) -> Result<Option<(usize, MkdirLock)>, String> {
    let mut busy = 0usize;
    let mut last_err = None;
    for slot in 0..slots {
        match MkdirLock::try_acquire(&slot_path(dir, slot), stale) {
            Ok(Some(lock)) => return Ok(Some((slot, lock))),
            Ok(None) => busy += 1,
            Err(e) => last_err = Some(e),
        }
    }
    // Report the store as broken only when NOT ONE slot was merely busy — a
    // single odd slot path must not mask an otherwise healthy, contended set
    // (that case is a normal wait, which the bounded wait already handles).
    if busy == 0 {
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    Ok(None)
}

/// The lock path for slot `i` under `dir`.
#[must_use]
pub fn slot_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("slot-{i}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn fast(dir: &Path, slots: usize, label: &str) -> BuildSlotLease {
        acquire_in(
            dir,
            slots,
            Duration::from_millis(120),
            Duration::from_millis(10),
            Duration::from_secs(3600),
            label,
        )
    }

    // ------------------------------------------------------------------
    // acquire / release
    // ------------------------------------------------------------------

    #[test]
    fn acquires_a_slot_and_releases_it_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        {
            let lease = fast(&dir, 1, "cargo-test");
            assert!(lease.holds_slot(), "an idle machine must hand out a slot");
            assert_eq!(lease.kind().as_str(), "held");
            assert!(slot_path(&dir, 0).is_dir(), "the slot lock dir must exist while held");
            assert!(lease.covers_children());
        }
        assert!(!slot_path(&dir, 0).exists(), "the slot must be released on drop");
    }

    #[test]
    fn creates_the_slot_dir_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("deep").join("nested").join("build-slot");
        let lease = fast(&dir, 1, "gate");
        assert!(lease.holds_slot());
        assert!(dir.is_dir());
    }

    // ------------------------------------------------------------------
    // serialization: a second acquire waits, then degrades open (never
    // deadlocks) — AC "bounded wait + telemetry, never a deadlock"
    // ------------------------------------------------------------------

    #[test]
    fn second_acquire_waits_then_degrades_open_instead_of_blocking_forever() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        let _held = fast(&dir, 1, "first-build");

        let start = Instant::now();
        let second = fast(&dir, 1, "second-build");
        let elapsed = start.elapsed();

        assert!(!second.holds_slot(), "the single slot is taken");
        match second.kind() {
            LeaseKind::DegradedOpen { reason } => {
                assert!(reason.contains("bounded wait"), "reason should name the bound: {reason}");
            }
            other => panic!("expected a degraded-open lease, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(100),
            "must actually wait the bound: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(5), "must not block indefinitely: {elapsed:?}");
    }

    #[test]
    fn releasing_lets_the_next_waiter_in() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        {
            let first = fast(&dir, 1, "first");
            assert!(first.holds_slot());
        }
        let second = fast(&dir, 1, "second");
        assert!(second.holds_slot(), "a released slot must be immediately reusable");
    }

    #[test]
    fn multiple_slots_admit_that_many_concurrent_holders() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        let a = fast(&dir, 2, "a");
        let b = fast(&dir, 2, "b");
        let c = fast(&dir, 2, "c");
        assert!(a.holds_slot());
        assert!(b.holds_slot());
        assert!(!c.holds_slot(), "the third holder exceeds the 2-slot budget");
        // The two holders occupy distinct slots.
        assert_eq!(a.slot_index(), Some(0));
        assert_eq!(b.slot_index(), Some(1));
        assert!(slot_path(&dir, 0).is_dir() && slot_path(&dir, 1).is_dir());
    }

    // ------------------------------------------------------------------
    // degrade-open paths
    // ------------------------------------------------------------------

    #[test]
    fn degrades_open_when_the_slot_dir_cannot_be_created() {
        let tmp = tempfile::tempdir().unwrap();
        // A *file* where the slot directory should be: `create_dir_all` fails.
        let path = tmp.path().join("not-a-dir");
        std::fs::write(&path, b"x").unwrap();
        let lease = fast(&path, 1, "gate");
        assert!(!lease.holds_slot());
        match lease.kind() {
            LeaseKind::DegradedOpen { reason } => assert!(reason.contains("unusable")),
            other => panic!("expected degraded-open, got {other:?}"),
        }
    }

    #[test]
    fn zero_slots_disables_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        let a = fast(&dir, 0, "a");
        let b = fast(&dir, 0, "b");
        assert!(!a.holds_slot() && !b.holds_slot());
        assert_eq!(a.kind().as_str(), "degraded-open");
        assert!(!dir.exists(), "a disabled slot must not even create the dir");
    }

    #[test]
    fn degraded_open_lease_does_not_cover_children() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        let lease = fast(&dir, 0, "a");
        assert!(
            !lease.covers_children(),
            "a lease with no slot must not tell children they are covered"
        );
    }

    // ------------------------------------------------------------------
    // stale reaping — a slot older than the threshold is not a permanent wedge
    // ------------------------------------------------------------------

    #[test]
    fn reaps_a_stale_slot_from_a_crashed_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate a holder that died without releasing.
        std::fs::create_dir(slot_path(&dir, 0)).unwrap();
        std::thread::sleep(Duration::from_millis(30));

        let lease = acquire_in(
            &dir,
            1,
            Duration::from_millis(200),
            Duration::from_millis(10),
            Duration::from_millis(10), // everything older than 10ms is stale
            "after-crash",
        );
        assert!(lease.holds_slot(), "an abandoned slot must be reaped, not waited on forever");
    }

    // ------------------------------------------------------------------
    // re-entrancy — the nested-acquire no-op
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn nested_acquire_is_a_reentrant_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");
        std::env::set_var(BUILD_SLOT_DIR_ENV, &dir);
        std::env::set_var(BUILD_SLOT_HELD_ENV, "1");
        let lease = acquire("nested-gate");
        assert_eq!(lease.kind(), &LeaseKind::Reentrant);
        assert!(!lease.holds_slot());
        assert!(
            lease.covers_children(),
            "an ancestor already holds a slot, so children stay covered"
        );
        assert!(!slot_path(&dir, 0).exists(), "a re-entrant acquire must not take a second slot");
        std::env::remove_var(BUILD_SLOT_HELD_ENV);
        std::env::remove_var(BUILD_SLOT_DIR_ENV);
    }

    #[test]
    #[serial]
    fn is_held_here_parses_truthy_values_only() {
        for truthy in ["1", "true", "YES", "on"] {
            std::env::set_var(BUILD_SLOT_HELD_ENV, truthy);
            assert!(is_held_here(), "{truthy} must be truthy");
        }
        for falsy in ["0", "false", "", "no"] {
            std::env::set_var(BUILD_SLOT_HELD_ENV, falsy);
            assert!(!is_held_here(), "{falsy} must not be truthy");
        }
        std::env::remove_var(BUILD_SLOT_HELD_ENV);
        assert!(!is_held_here());
    }

    // ------------------------------------------------------------------
    // knob resolution
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn resolves_knobs_from_env_with_defaults() {
        std::env::remove_var(BUILD_SLOTS_ENV);
        std::env::remove_var(BUILD_SLOT_WAIT_SECS_ENV);
        std::env::remove_var(BUILD_SLOT_STALE_SECS_ENV);
        assert_eq!(resolve_slots(), DEFAULT_BUILD_SLOTS);
        assert_eq!(resolve_wait(), Duration::from_secs(DEFAULT_BUILD_SLOT_WAIT_SECS));
        assert_eq!(resolve_stale(), Duration::from_secs(DEFAULT_STALE_SLOT_SECS));

        std::env::set_var(BUILD_SLOTS_ENV, "2");
        std::env::set_var(BUILD_SLOT_WAIT_SECS_ENV, "30");
        std::env::set_var(BUILD_SLOT_STALE_SECS_ENV, "60");
        assert_eq!(resolve_slots(), 2);
        assert_eq!(resolve_wait(), Duration::from_secs(30));
        assert_eq!(resolve_stale(), Duration::from_secs(60));

        // `0` is honored for the slot count (opt-out) but not for the stale
        // threshold (which would reap every live slot instantly).
        std::env::set_var(BUILD_SLOTS_ENV, "0");
        std::env::set_var(BUILD_SLOT_STALE_SECS_ENV, "0");
        assert_eq!(resolve_slots(), 0);
        assert_eq!(resolve_stale(), Duration::from_secs(DEFAULT_STALE_SLOT_SECS));

        // Garbage falls back to the defaults.
        std::env::set_var(BUILD_SLOTS_ENV, "many");
        std::env::set_var(BUILD_SLOT_WAIT_SECS_ENV, "soon");
        assert_eq!(resolve_slots(), DEFAULT_BUILD_SLOTS);
        assert_eq!(resolve_wait(), Duration::from_secs(DEFAULT_BUILD_SLOT_WAIT_SECS));

        std::env::remove_var(BUILD_SLOTS_ENV);
        std::env::remove_var(BUILD_SLOT_WAIT_SECS_ENV);
        std::env::remove_var(BUILD_SLOT_STALE_SECS_ENV);
    }

    #[test]
    #[serial]
    fn slot_dir_honors_the_env_override_and_falls_back_to_home() {
        std::env::set_var(BUILD_SLOT_DIR_ENV, "/tmp/loom-slots-test");
        assert_eq!(slot_dir(), Some(PathBuf::from("/tmp/loom-slots-test")));
        // An empty/whitespace override is ignored (falls through to `$HOME`).
        std::env::set_var(BUILD_SLOT_DIR_ENV, "   ");
        if let Some(p) = slot_dir() {
            assert!(
                p.ends_with(PathBuf::from(".loom").join("locks").join("build-slot")),
                "unexpected fallback path: {}",
                p.display()
            );
        }
        std::env::remove_var(BUILD_SLOT_DIR_ENV);
    }

    // ------------------------------------------------------------------
    // cross-process contract: the Bash half must agree on the path shape
    // ------------------------------------------------------------------

    #[test]
    fn slot_path_shape_matches_the_bash_helper() {
        // `defaults/scripts/lib/build-slot.sh` composes "$dir/slot-$i"; if this
        // ever diverges, the daemon and the sweep-side gate stop serializing
        // against each other (silently).
        assert_eq!(slot_path(Path::new("/x"), 0), PathBuf::from("/x/slot-0"));
        assert_eq!(slot_path(Path::new("/x"), 3), PathBuf::from("/x/slot-3"));
    }

    /// Repo-relative path to the Bash half, or `None` when it is not present
    /// (a consumer repo vendoring only the crate).
    fn bash_helper() -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .join("defaults/scripts/lib/build-slot.sh");
        p.is_file().then_some(p)
    }

    #[test]
    fn a_rust_held_slot_blocks_the_bash_half_across_processes() {
        // THE contract that makes #4512 safe: the daemon's gate (Rust) and each
        // sweep worktree's `build-gate.sh` (Bash) are different processes, so the
        // two implementations must serialize against *each other*, not merely
        // each against itself. Path-shape parity (above) is necessary but not
        // sufficient — this exercises the real protocol end to end.
        let Some(helper) = bash_helper() else {
            return; // not a source checkout; nothing to cross-check
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");

        let held = fast(&dir, 1, "rust-side-build");
        assert!(held.holds_slot(), "the Rust half must take slot 0 first");

        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -euo pipefail; source '{}'; loom_build_slot_acquire bash-side-gate; \
                 echo \"PATH=[${{LOOM_BUILD_SLOT_PATH}}]\"",
                helper.display()
            ))
            .env(BUILD_SLOT_DIR_ENV, &dir)
            .env(BUILD_SLOT_WAIT_SECS_ENV, "1")
            .env_remove(BUILD_SLOT_HELD_ENV)
            .output()
            .expect("run the bash half");

        assert!(out.status.success(), "the bash half must degrade open, never fail");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("PATH=[]"),
            "bash must hold NO slot while Rust holds the only one — stdout: {stdout}, \
             stderr: {stderr}"
        );
        assert!(
            stderr.contains("degrading open"),
            "bash must report the bounded-wait degrade — stderr: {stderr}"
        );

        // And once the Rust side releases, the Bash side gets in — proving the
        // block above was contention, not a broken lock store.
        drop(held);
        let out2 = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -euo pipefail; source '{}'; loom_build_slot_acquire bash-side-gate; \
                 echo \"PATH=[${{LOOM_BUILD_SLOT_PATH}}]\"; loom_build_slot_release",
                helper.display()
            ))
            .env(BUILD_SLOT_DIR_ENV, &dir)
            .env(BUILD_SLOT_WAIT_SECS_ENV, "1")
            .env_remove(BUILD_SLOT_HELD_ENV)
            .output()
            .expect("run the bash half again");
        let stdout2 = String::from_utf8_lossy(&out2.stdout);
        assert!(
            stdout2.contains("PATH=[") && !stdout2.contains("PATH=[]"),
            "a released slot must be visible to the other language: {stdout2}"
        );
    }

    #[test]
    fn a_bash_held_slot_blocks_the_rust_half_across_processes() {
        // The mirror direction: a sweep's `build-gate.sh` holding the slot must
        // make the daemon's own gate wait/degrade rather than build alongside it.
        let Some(helper) = bash_helper() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build-slot");

        // Acquire in Bash and *leak* the lock dir (no release) to model a
        // still-running holder; a fresh mtime keeps it non-stale.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -euo pipefail; source '{}'; loom_build_slot_acquire bash-side-gate",
                helper.display()
            ))
            .env(BUILD_SLOT_DIR_ENV, &dir)
            .env(BUILD_SLOT_WAIT_SECS_ENV, "1")
            .env_remove(BUILD_SLOT_HELD_ENV)
            .output()
            .expect("run the bash half");
        assert!(out.status.success());
        assert!(slot_path(&dir, 0).is_dir(), "bash must have taken slot 0");

        let lease = fast(&dir, 1, "rust-side-build");
        assert!(!lease.holds_slot(), "Rust must not build alongside a Bash-held slot");
        assert_eq!(lease.kind().as_str(), "degraded-open");
    }
}
