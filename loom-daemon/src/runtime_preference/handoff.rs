//! Handing a backstop slot to the child that spends it (Issue #8555).
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
//! # The reservation is carried by value, never parked in shared state
//!
//! The slot rides on the value the caller **already** moves across the
//! resolve→spawn seam, and this module is only the one-line attach arm at the
//! far end:
//!
//! - `sweep_registry::dispatch`: [`DispatchAdmission`](super::DispatchAdmission)
//!   is a field of `PreparedIssueDispatch`, the box `begin_issue_dispatch`
//!   returns and `finish_issue_dispatch` consumes. Whatever happens in between
//!   — a `tokio::task::spawn_blocking(...).await` on the IPC path, a whole
//!   batch of pending resumes queued by the reaper — the slot is wherever the
//!   dispatch it belongs to is.
//! - `role_runner`: `runtime_preflight::check` returns it, the tick carries it
//!   to `run_role_with_timeout`, which attaches it to the child it spawns.
//!
//! **An earlier revision parked the reservation in a process-global map keyed
//! on [`std::thread::ThreadId`] instead, asserting that both launch paths
//! resolve and spawn "on one thread, in one synchronous call chain". That
//! invariant was false, and the failure was not benign:**
//!
//! - `ipc.rs::dispatch_sweep_nonblocking` — the handler behind `DispatchSweep`
//!   and `mcp__loom__dispatch_sweep`, i.e. how work actually arrives — resolves
//!   in phase 1, `.await`s a `spawn_blocking` poll in phase 2, and attaches in
//!   phase 3. Under `#[tokio::main]`'s multi-thread runtime the task resumes on
//!   whichever worker steals it; measured on this code, 16 of 24 dispatches
//!   attached from a different thread than they resolved on.
//! - `reaper.rs::reap_once_releasing_poll_lock` is synchronous and still broke
//!   it, by batching: it calls `begin_issue_dispatch` once per crashed sweep in
//!   one locked pass, then polls and finishes them in a second loop. With K ≥ 2
//!   pending resumes, K-1 parks were overwritten before their own attach ran.
//!
//! Because the map was keyed on a value two live dispatches can share, an
//! `insert` collision did not merely lose the newcomer's accounting — it
//! returned a **peer's still-live** `Reservation` and dropped it, and `Drop for
//! Reservation` deletes the lease file. Colliding dispatches deleted each
//! other's in-flight slots: 12 concurrent dispatches against a ceiling of 64
//! left 4 counted. A correlation token would have fixed the cross-wiring, but
//! the value the token would have been threaded through is the value that can
//! carry the reservation itself, so nothing is gained by the indirection —
//! and an explicitly carried `Reservation` also releases on every early return
//! for free, because dropping it is releasing it.
//!
//! # Failure modes, and why each is safe
//!
//! - **Carried but never attached** (a guard refused after resolution, the
//!   spawn failed, the caller returned early). The value is dropped on that
//!   path, which releases the lease immediately. No window at all, where the
//!   parked design had one bounded by
//!   [`DEFAULT_RESERVATION_STALE_SECS`](super::ceiling::DEFAULT_RESERVATION_STALE_SECS).
//! - **Attached, then the child dies.** Nothing to do: the lease is
//!   PID-governed and the next count reaps it.
//! - **Attach fails** (the lease file vanished under us). The launch proceeds
//!   **uncounted** rather than being killed after the fact — refusing a
//!   dispatch that is already running achieves nothing. Logged at WARN so a
//!   host whose lease store is broken is visible rather than silently
//!   unbounded.

use super::ceiling::Reservation;

/// Hand `slot` — when the resolution took one — to the process that will spend
/// it. A no-op on `None`, which is the overwhelmingly common case: no ceiling
/// configured, or a launch that never fell through to a governed tap.
///
/// Taking the reservation **by value** is the point: there is no shared state
/// to collide on, so no launch can consume, evict, or delete another launch's
/// slot however the two are scheduled.
pub fn attach(slot: Option<Reservation>, pid: u32) {
    let Some(slot) = slot else {
        return;
    };
    if let Err(e) = slot.attach(pid) {
        log::warn!(
            "runtime_preference: could not attach the backstop ceiling slot to pid {pid}; this \
             launch runs uncounted against the metered ceiling (#8555): {e}"
        );
    }
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

    fn take_slot_from(limit: u32) -> Reservation {
        let ceiling = BackstopCeiling {
            max_concurrent: Some(limit),
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

    fn take_slot() -> Reservation {
        take_slot_from(4)
    }

    fn live(store: &Store) -> u32 {
        super::super::ceiling::live_count(&store.path()).unwrap()
    }

    /// A lease that is merely *present* proves nothing: an unattached
    /// reservation looks identical to a count and ages out five minutes later.
    /// What has to hold is that THIS launch's attach reached THIS lease.
    fn assert_attached_to_this_process(path: &std::path::Path) {
        let body = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{} was deleted by a peer: {e}", path.display()));
        let holder: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            holder["attached"],
            serde_json::json!(true),
            "{} was never attached — the launch ran uncounted: {body}",
            path.display()
        );
        assert_eq!(
            holder["pid"],
            serde_json::json!(std::process::id()),
            "{} is pinned to the wrong pid: {body}",
            path.display()
        );
    }

    fn clear(store: &Store) {
        for entry in std::fs::read_dir(store.path())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    /// The whole point of the module: a slot taken at resolution survives to
    /// the spawn site and is then governed by the spawned PID's liveness.
    #[test]
    #[serial_test::serial]
    fn a_carried_slot_survives_to_the_spawn_site() {
        let store = Store::new();
        let slot = take_slot();
        let path = slot.path().to_path_buf();
        // The reservation is no longer on the resolving scope's stack — this is
        // exactly the window a dropped `Decision` used to lose.
        let carried = Some(slot);
        assert!(path.exists());
        attach(carried, std::process::id());
        assert_attached_to_this_process(&path);
        assert_eq!(live(&store), 1);
        clear(&store);
    }

    /// The same, with the attach happening on a thread that never resolved —
    /// deterministically, not at the scheduler's discretion. This is the
    /// property the thread-keyed park could not have: there, the slot would
    /// still be sitting parked under the resolving thread's key, the attach
    /// here would find nothing, and the launch would run uncounted until the
    /// abandoned reservation aged out.
    #[test]
    #[serial_test::serial]
    fn the_attaching_thread_need_not_be_the_resolving_thread() {
        let store = Store::new();
        let slot = take_slot();
        let path = slot.path().to_path_buf();
        std::thread::spawn(move || attach(Some(slot), std::process::id()))
            .join()
            .unwrap();
        assert_attached_to_this_process(&path);
        assert_eq!(live(&store), 1);
        clear(&store);
    }

    /// A launch that resolved and then bailed out drops the value it was
    /// carrying, which releases the lease immediately — no age-out window, and
    /// nothing left behind for a later launch to mis-attach.
    #[test]
    #[serial_test::serial]
    fn dropping_the_carried_slot_releases_it_immediately() {
        let store = Store::new();
        let abandoned = take_slot();
        let path = abandoned.path().to_path_buf();
        assert_eq!(live(&store), 1);
        drop(abandoned);
        assert!(!path.exists(), "an abandoned slot must be released, not leaked");
        assert_eq!(live(&store), 0);
        attach(None, std::process::id()); // must not panic, must not resurrect
        assert_eq!(live(&store), 0);
    }

    /// **Blocker 1 regression test.** The `ipc.rs::dispatch_sweep_nonblocking`
    /// shape: resolve in phase 1, hand the carried value across a
    /// `spawn_blocking(...).await` (phase 2), attach in phase 3 — on whichever
    /// multi-thread-runtime worker happens to poll the continuation. The
    /// thread-keyed park this replaced lost, and actively deleted, the
    /// majority of these.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn a_slot_survives_the_spawn_blocking_seam_onto_another_thread() {
        let store = Store::new();
        let limit = 12;
        let mut tasks = Vec::new();
        for _ in 0..limit {
            tasks.push(tokio::spawn(async move {
                // Phase 1: resolve (takes the slot) — on this worker thread.
                let resolved = std::thread::current().id();
                let slot = take_slot_from(limit);
                let path = slot.path().to_path_buf();
                // Phase 2: the unlocked poll, exactly as `ipc.rs` runs it. The
                // carried value moves with the work.
                let (slot, path) = tokio::task::spawn_blocking(move || (slot, path))
                    .await
                    .unwrap();
                // Phase 3: attach — wherever this continuation got polled.
                let attached_on = std::thread::current().id();
                attach(Some(slot), std::process::id());
                (path, resolved != attached_on)
            }));
        }
        let mut crossed = 0;
        for task in tasks {
            let (path, moved) = task.await.unwrap();
            assert_attached_to_this_process(&path);
            crossed += u32::from(moved);
        }
        // Every dispatch is counted whether or not it changed threads: that is
        // the property. The count is reported so a run where the scheduler
        // happened to keep everything on one thread is not mistaken for proof.
        assert_eq!(live(&store), limit, "{crossed}/{limit} tasks changed thread");
        clear(&store);
    }

    /// **Blocker 2 regression test.** The `reaper.rs::reap_once_impl` shape:
    /// several dispatches are prepared in one locked pass, and only then are
    /// they polled and finished, one after another, on the same thread. Two
    /// parks followed by two attaches is a guaranteed key collision when the
    /// key is the thread — no scheduling luck required.
    #[test]
    #[serial_test::serial]
    fn a_batch_prepared_together_and_finished_later_keeps_every_slot() {
        let store = Store::new();
        let batch = 3;
        // Pass one: prepare every pending resume, carrying each slot with it.
        let prepared: Vec<(Option<Reservation>, std::path::PathBuf)> = (0..batch)
            .map(|_| {
                let slot = take_slot_from(batch);
                let path = slot.path().to_path_buf();
                (Some(slot), path)
            })
            .collect();
        assert_eq!(live(&store), batch, "every prepared resume holds its own slot");
        // Pass two: finish them, in a separate loop, on this same thread.
        for (slot, path) in prepared {
            attach(slot, std::process::id());
            assert_attached_to_this_process(&path);
        }
        assert_eq!(live(&store), batch);
        clear(&store);
    }

    /// Two launches racing on different threads each keep their own slot —
    /// retained from the parked design, since it is still a property worth
    /// pinning, and it now holds by construction.
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
                        let carried = Some(slot);
                        // Every thread resolves before any thread attaches.
                        barrier.wait();
                        attach(carried, std::process::id());
                        path
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(paths.len(), 3);
        for path in &paths {
            assert_attached_to_this_process(path);
        }
        assert_eq!(live(&store), 3);
        clear(&store);
    }
}
