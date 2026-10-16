//! The eager pass's trigger: the free-disk floor (#11192, re-keyed by
//! #12406).
//!
//! The pass used to fire when "the disk term binds the dispatch cap down",
//! plus level conditions on top. #12406 removed the disk term from the cap,
//! so the trigger is now the floor alone: while free space on the worktree
//! volume is below `diskWarnFreeGb` a pass is due on every tick.
//!
//! The trigger only decides that a pass is *due*. The pass itself
//! ([`super::run_pass`]) still enforces its own cooldown, and every
//! sub-pass its own, so a level that stays true costs one cheap skipped
//! `run_for` per tick between passes, never a forge round trip.

use std::path::Path;

/// Why a pass was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    /// Free space is below the floor (#11192).
    BelowFloor,
}

impl TriggerReason {
    /// The log-line fragment naming the trigger.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::BelowFloor => "free space is below the floor".to_string(),
        }
    }
}

/// One dispatch tick's inputs to [`EagerTrigger::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickReading {
    /// Free GB on the worktree-root volume, `None` when unmeasurable.
    pub free_gb: Option<u64>,
    /// The reaper's resolved `diskWarnFreeGb`.
    pub floor_gb: u64,
}

/// The dispatch loop's eager-reclaim trigger for one probe root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EagerTrigger;

impl EagerTrigger {
    /// Whether this tick should ask for a pass, and why. Pure. An
    /// unmeasurable disk never fires.
    #[must_use]
    pub fn evaluate(&self, r: &TickReading) -> Option<TriggerReason> {
        let free = r.free_gb?;
        (free < r.floor_gb).then_some(TriggerReason::BelowFloor)
    }

    /// One dispatch tick, production wiring: probe free space and the floor,
    /// and run [`super::run_for`] on the blocking pool if a pass is due.
    /// Returns the pass report when one was asked for.
    pub async fn tick(&mut self, root: &Path) -> Option<super::EagerReclaimReport> {
        let probe_root = root.to_path_buf();
        let reading = tokio::task::spawn_blocking(move || pressure_probe(&probe_root))
            .await
            .unwrap_or(TickReading {
                free_gb: None,
                floor_gb: 0,
            });
        let reason = self.evaluate(&reading)?;
        let pass_root = root.to_path_buf();
        tokio::task::spawn_blocking(move || super::run_for(&pass_root, reason))
            .await
            .ok()
    }
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
