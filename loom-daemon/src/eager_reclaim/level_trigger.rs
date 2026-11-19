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

use std::path::Path;

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

/// Free GB on `root`'s worktree volume and the floor. Blocking (`df`, config
/// reads).
fn pressure_probe(root: &Path) -> TickReading {
    let reaper_config = crate::worktree_reaper::read_worktree_reaper_config(root);
    TickReading {
        free_gb: crate::disk_headroom::worktree_root_free_gb(root),
        floor_gb: crate::worktree_reaper::resolve_disk_warn_free_gb(&reaper_config),
    }
}
