//! The eager pass's trigger: the free-disk floor (#11192, re-keyed by
//! #12406).
//!
//! The pass used to fire when "the disk term binds the dispatch cap down",
//! plus level conditions on top. #12406 removed the disk term from the cap,
//! so the trigger is now the floor alone: while free space on the worktree
//! volume is below `diskWarnFreeGb` a pass is due on every tick. With one
//! condition left there is no trigger state to carry between ticks, so this
//! is two free functions rather than a type (#13837).
//!
//! The trigger only decides that a pass is *due*. The pass itself
//! ([`super::run_pass`]) still enforces its own cooldown, and every
//! sub-pass its own, so a level that stays true costs one cheap skipped
//! `run_for` per tick between passes, never a forge round trip.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One dispatch tick's inputs to [`pass_due`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickReading {
    /// Free GB on the worktree-root volume, `None` when unmeasurable.
    pub free_gb: Option<u64>,
    /// The reaper's resolved `diskWarnFreeGb`.
    pub floor_gb: u64,
}

/// Whether this tick should ask for a pass: free space is below the floor
/// (#11192). Pure. An unmeasurable disk never fires.
#[must_use]
pub fn pass_due(r: &TickReading) -> bool {
    r.free_gb.is_some_and(|free| free < r.floor_gb)
}

/// One dispatch tick, production wiring: probe free space and the floor,
/// and run [`super::run_for`] on the blocking pool if a pass is due.
/// Returns the pass report when one was asked for.
pub async fn tick(root: &Path) -> Option<super::EagerReclaimReport> {
    let probe_root = root.to_path_buf();
    let reading = tokio::task::spawn_blocking(move || pressure_probe(&probe_root))
        .await
        .unwrap_or(TickReading {
            free_gb: None,
            floor_gb: 0,
        });
    if !pass_due(&reading) {
        return None;
    }
    let pass_root = root.to_path_buf();
    tokio::task::spawn_blocking(move || super::run_for(&pass_root))
        .await
        .ok()
}

/// Roots whose detached pass ([`spawn_tick`]) has not finished yet.
static IN_FLIGHT: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Single-flight token: releases `root` when dropped, including when the
/// detached task is cancelled or panics.
struct InFlight(PathBuf);

impl InFlight {
    fn begin(root: &Path) -> Option<Self> {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.to_path_buf())
            .then(|| Self(root.to_path_buf()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// [`tick`], off the caller's critical path (#14560).
///
/// A below-floor pass walks several sub-passes (worktree reap with forge REST
/// calls, build-cache and docker cleanup) and can take many minutes on a
/// starved disk. Awaited from the work-finder tick it held back the tick's
/// span and gauge publication: on one host, ticks whose pass finished 877 s
/// and 623 s after the tick began showed 982 s and 786 s completion gaps,
/// against a 120 s cadence. The pass is hygiene, not admission (it never
/// changes the cap), so the tick only launches it.
///
/// Single-flight per root: while a previous pass is still running no second
/// one is launched (the pass has its own cooldown, but it only starts counting
/// once a pass finishes). Returns whether a pass task was launched. The pass
/// logs its own report. Must be called from inside a Tokio runtime.
pub fn spawn_tick(root: &Path) -> bool {
    let pass_root = root.to_path_buf();
    spawn_single_flight(root, async move {
        let _ = tick(&pass_root).await;
    })
}

/// Spawn `pass` detached unless `root` already has one in flight.
fn spawn_single_flight(
    root: &Path,
    pass: impl std::future::Future<Output = ()> + Send + 'static,
) -> bool {
    let Some(in_flight) = InFlight::begin(root) else {
        return false;
    };
    tokio::spawn(async move {
        let _in_flight = in_flight;
        pass.await;
    });
    true
}

/// Free GB on `root`'s worktree volume and the floor. Blocking (`df`, config
/// reads).
fn pressure_probe(root: &Path) -> TickReading {
    let reaper_config = crate::worktree_reaper::read_worktree_reaper_config(root);
    TickReading {
        free_gb: crate::disk_headroom::worktree_root_free_gb(root),
        floor_gb: crate::worktree_reaper::resolve_disk_warn_free_gb(&reaper_config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_has_one_pass_in_flight_until_its_token_drops() {
        let root = Path::new("/fixture/single-flight-token");
        let first = InFlight::begin(root).expect("first launch");
        assert!(InFlight::begin(root).is_none());
        assert!(InFlight::begin(Path::new("/fixture/other-root")).is_some());
        drop(first);
        assert!(InFlight::begin(root).is_some());
    }

    #[tokio::test]
    async fn a_launch_returns_at_once_and_blocks_a_second_until_the_pass_ends() {
        let root = Path::new("/fixture/spawn-single-flight");
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let (ended, ended_rx) = tokio::sync::oneshot::channel::<()>();

        // The pass is still running when the launch returns (#14560).
        assert!(spawn_single_flight(root, async move {
            let _ = finished.await;
            let _ = ended.send(());
        }));
        assert!(
            !spawn_single_flight(root, async {}),
            "a pass already in flight must not be doubled"
        );

        finish.send(()).unwrap();
        ended_rx.await.unwrap();
        // The slot is released once the task drops its token.
        for _ in 0..200 {
            if spawn_single_flight(root, async {}) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the finished pass never released its slot");
    }
}
