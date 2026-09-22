//! Carrying a backstop slot from the tap that was chosen to the child that
//! spends it (Issue #8555).
//!
//! # The gap this closes
//!
//! [`ceiling::admit`](super::ceiling::admit) hands back a
//! [`Reservation`](super::ceiling::Reservation) owned by the **resolving**
//! process and released on drop. That is deliberately the safe default — a
//! caller that resolves and then does not launch cannot leak a metered slot —
//! but it means a launch path that simply drops the [`Decision`](super::Decision)
//! runs **uncounted**, and a ceiling nothing ever counts against is a
//! configuration knob that silently does nothing.
//!
//! Dispatch cannot attach the slot at the moment it is taken, because the PID
//! that will spend it does not exist yet: unlike a native harness spawn (which
//! `exec`s, so selection and run share a PID — see
//! [`crate::api_keys_pool::inflight`]), the daemon **selects, then spawns a
//! child that outlives the selection**. Resolution and `child.id()` are
//! therefore two different places in the code, several guards apart.
//!
//! # Why a thread-keyed park rather than a plumbed-through value
//!
//! The obvious shape — thread the `Reservation` through every intermediate
//! struct and signature from resolution to spawn — costs lines in exactly the
//! files the file-size ratchet freezes (`sweep_registry/dispatch.rs`,
//! `role_runner.rs`), which is what `.loom/docs/file-size-policy.md` says to
//! solve with a **new sibling module plus a small call arm**. This is that
//! module: two one-line calls at the launch sites, and the lifetime logic here.
//!
//! Keying on [`std::thread::ThreadId`] is not a convenience — it is what makes
//! the park race-free without a correlation id. Both launch paths resolve and
//! spawn on **one thread, in one synchronous call chain**:
//!
//! - `sweep_registry::dispatch`: `begin_issue_dispatch` (resolves) →
//!   `poll_and_classify_spawned_child` → `finish_issue_dispatch` (has
//!   `child.id()`). The registry mutex is released between the first and last,
//!   so a *peer dispatch on another thread* can interleave — and does not
//!   collide here, because it parks under its own key.
//! - `role_runner`: `runtime_preflight::check` (resolves) → the role tick's own
//!   spawn, same tick, same thread.
//!
//! A thread can only be inside one such chain at a time, so at most one
//! reservation is ever parked per key.
//!
//! # Failure modes, and why each is safe
//!
//! - **Parked but never attached** (a guard refused after resolution, the spawn
//!   failed, the caller returned early). The slot stays parked until the same
//!   thread's next [`park`] drops it, and the lease ages out on the short
//!   unattached threshold
//!   ([`DEFAULT_RESERVATION_STALE_SECS`](super::ceiling::DEFAULT_RESERVATION_STALE_SECS),
//!   5 minutes) regardless. That **over**-counts for at most that window, which
//!   is the safe direction for a spend ceiling: the worst case is preferring a
//!   free tap when the metered one had room.
//! - **Attached, then the child dies.** Nothing to do: the lease is
//!   PID-governed and the next count reaps it.
//! - **Attach fails** (the lease file vanished under us). The launch proceeds
//!   **uncounted** rather than being killed after the fact — refusing a
//!   dispatch that is already running achieves nothing. Logged at WARN so a
//!   host whose lease store is broken is visible rather than silently
//!   unbounded.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use super::ceiling::Reservation;

fn parked() -> &'static Mutex<HashMap<ThreadId, Reservation>> {
    static PARKED: OnceLock<Mutex<HashMap<ThreadId, Reservation>>> = OnceLock::new();
    PARKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Park `slot` for this thread's in-flight launch, to be claimed by the next
/// [`attach`] on the same thread.
///
/// `None` is not a no-op: it **clears** this thread's park. A resolution that
/// chose a non-governed tap (or that was refused) must not leave the previous
/// launch's stale reservation sitting where this launch's `attach` would pick
/// it up and pin it to the wrong PID.
pub fn park(slot: Option<Reservation>) {
    let Ok(mut map) = parked().lock() else {
        // A poisoned lock means a peer panicked mid-park. Dropping `slot` here
        // releases it, which is the fail-safe direction: an uncounted launch,
        // never a slot pinned forever.
        return;
    };
    let key = std::thread::current().id();
    match slot {
        // The `insert` drops (and so releases) any reservation this thread had
        // parked and never attached — see "Parked but never attached" above.
        Some(slot) => drop(map.insert(key, slot)),
        None => drop(map.remove(&key)),
    }
}

/// Hand this thread's parked slot — if it has one — to the process that will
/// spend it. A no-op when nothing is parked, which is the overwhelmingly
/// common case: no ceiling configured, or a launch that never fell through to
/// a governed tap.
pub fn attach(pid: u32) {
    let Ok(mut map) = parked().lock() else {
        return;
    };
    let Some(slot) = map.remove(&std::thread::current().id()) else {
        return;
    };
    drop(map);
    if let Err(e) = slot.attach(pid) {
        log::warn!(
            "runtime_preference: could not attach the backstop ceiling slot to pid {pid}; this \
             launch runs uncounted against the metered ceiling (#8555): {e}"
        );
    }
}

/// Release this thread's parked slot without attaching it, for a caller that
/// knows the launch is not going to happen. Purely an optimisation over
/// waiting for the unattached lease to age out.
pub fn release() {
    park(None);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::ceiling::{admit, BackstopCeiling, Intent, Verdict, DEFAULT_APPLIES_FROM};
    use super::super::resolve::Tap;
    use super::*;

    struct Store {
        dir: tempfile::TempDir,
        prior: Option<std::ffi::OsString>,
    }

    impl Store {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let prior = std::env::var_os(super::super::ceiling::LEASE_DIR_ENV);
            std::env::set_var(super::super::ceiling::LEASE_DIR_ENV, dir.path().join("backstop"));
            Self { dir, prior }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.path().join("backstop")
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(value) => std::env::set_var(super::super::ceiling::LEASE_DIR_ENV, value),
                None => std::env::remove_var(super::super::ceiling::LEASE_DIR_ENV),
            }
        }
    }

    fn take_slot() -> Reservation {
        let ceiling = BackstopCeiling {
            max_concurrent: Some(4),
            applies_from: DEFAULT_APPLIES_FROM,
            min_complexity: None,
        };
        let Verdict::Admitted(Some(slot)) = admit(
            &ceiling,
            "builder",
            &Tap::runtime("opencode"),
            Some("complex"),
            Intent::Dispatch,
        ) else {
            panic!("expected an admitted reservation");
        };
        slot
    }

    /// The whole point of the module: a slot taken at resolution survives to
    /// the spawn site and is then governed by the spawned PID's liveness.
    #[test]
    #[serial_test::serial]
    fn a_parked_slot_survives_to_the_spawn_site() {
        let store = Store::new();
        let slot = take_slot();
        let path = slot.path().to_path_buf();
        park(Some(slot));
        // The reservation is no longer on the caller's stack, yet the lease is
        // still held — this is exactly the window `Decision` being dropped
        // used to lose.
        assert!(path.exists());
        attach(std::process::id());
        assert!(path.exists(), "attach must keep the lease past the resolving scope");
        assert_eq!(super::super::ceiling::live_count(&store.path()).unwrap(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    /// A launch that resolved but never spawned must not leave a slot behind
    /// for the *next* launch on this thread to inherit and mis-attach.
    #[test]
    #[serial_test::serial]
    fn parking_again_releases_the_slot_the_previous_launch_abandoned() {
        let store = Store::new();
        let abandoned = take_slot();
        let abandoned_path = abandoned.path().to_path_buf();
        park(Some(abandoned));
        assert_eq!(super::super::ceiling::live_count(&store.path()).unwrap(), 1);

        let next = take_slot();
        let next_path = next.path().to_path_buf();
        park(Some(next));
        assert!(!abandoned_path.exists(), "the abandoned slot must be released, not leaked");
        assert_eq!(super::super::ceiling::live_count(&store.path()).unwrap(), 1);

        attach(std::process::id());
        assert!(next_path.exists());
        std::fs::remove_file(&next_path).unwrap();
    }

    /// `park(None)` clears the key, so a resolution that took no slot cannot
    /// let a stale one be attached to this launch's PID.
    #[test]
    #[serial_test::serial]
    fn parking_none_clears_the_key_and_attach_becomes_a_no_op() {
        let store = Store::new();
        park(Some(take_slot()));
        release();
        assert_eq!(super::super::ceiling::live_count(&store.path()).unwrap(), 0);
        attach(std::process::id()); // must not panic, must not resurrect
        assert_eq!(super::super::ceiling::live_count(&store.path()).unwrap(), 0);
    }

    /// The property that makes the thread key race-free rather than merely
    /// convenient: two launches running concurrently on different threads each
    /// get their own slot back, with no correlation id plumbed anywhere.
    #[test]
    #[serial_test::serial]
    fn concurrent_launches_on_different_threads_do_not_steal_each_others_slots() {
        let store = Store::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let paths: Vec<std::path::PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..3)
                .map(|_| {
                    let barrier = std::sync::Arc::clone(&barrier);
                    scope.spawn(move || {
                        let slot = take_slot();
                        let path = slot.path().to_path_buf();
                        park(Some(slot));
                        // Every thread parks before any thread attaches: a
                        // single shared park slot would cross-wire them here.
                        barrier.wait();
                        attach(std::process::id());
                        path
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(paths.len(), 3);
        for path in &paths {
            assert!(path.exists(), "{} was stolen by a peer thread", path.display());
        }
        assert_eq!(super::super::ceiling::live_count(&store.path()).unwrap(), 3);
        for path in &paths {
            std::fs::remove_file(path).unwrap();
        }
    }
}
