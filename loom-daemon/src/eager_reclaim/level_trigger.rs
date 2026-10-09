//! The eager pass's trigger: edge **and** level (#11192).
//!
//! #7512 fired the eager pass only on the `false -> true` edge of "the disk
//! term binds the dispatch cap down". On loom-worker-1 (2026-10-09) that
//! edge could not recur: with `configured_max = 12` the disk term binds below
//! 96 GB free, so after one pass at 79 GB the trigger stayed disarmed for 7.5
//! hours while free space fell to 0.
//!
//! [`EagerTrigger`] keeps the edge and adds two level conditions:
//!
//! - **Below the floor** (`diskWarnFreeGb`): due on every tick, so a pass runs
//!   once per cooldown window for as long as the pressure lasts.
//! - **Fell another step**: while the disk term keeps binding, free space has
//!   dropped `fallStepGb` (default 10) below the reading the last pass left
//!   behind (or the highest reading since, if space came back in between).
//!
//! The trigger only decides that a pass is *due*. The pass itself
//! ([`super::run_pass`]) still enforces its own cooldown, and every
//! sub-pass its own, so a level that stays true costs one cheap skipped
//! `run_for` per tick between passes, never a forge round trip.

use std::path::Path;

use super::{disk_axis_binds_cap_down, EagerReclaimReport};

/// Why a pass was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    /// The disk term started binding the dispatch cap down this tick (#7512).
    Edge,
    /// Free space is below the floor (#11192).
    BelowFloor,
    /// Free space fell another step below the last pass's reading while the
    /// disk term kept binding (#11192).
    FellSinceLastPass {
        /// The reference reading the fall is measured from.
        from_gb: u64,
    },
}

impl TriggerReason {
    /// The log-line fragment naming the trigger.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Edge => "disk axis binds the dispatch cap down".to_string(),
            Self::BelowFloor => "free space is below the floor".to_string(),
            Self::FellSinceLastPass { from_gb } => format!(
                "free space fell from {from_gb}G since the last pass while the disk axis binds \
                 the dispatch cap down"
            ),
        }
    }
}

/// One dispatch tick's inputs to [`EagerTrigger::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickReading {
    /// The disk term (`free / per_worktree_gb`; `usize::MAX` when unmeasurable).
    pub disk: usize,
    /// The RAM term.
    pub ram: usize,
    /// The configured ceiling.
    pub configured_max: usize,
    /// Free GB on the worktree-root volume, `None` when unmeasurable.
    pub free_gb: Option<u64>,
    /// The reaper's resolved `diskWarnFreeGb`.
    pub floor_gb: u64,
    /// The resolved fall step (GB).
    pub step_gb: u64,
}

/// The dispatch loop's across-tick trigger state for one probe root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EagerTrigger {
    /// Whether the disk term bound the cap down at the end of the last tick.
    was_binding: bool,
    /// Free GB the fall step is measured from: the last pass's closing
    /// reading, raised whenever free space climbs above it. `None` while the
    /// disk term is not binding.
    reference_free_gb: Option<u64>,
}

impl EagerTrigger {
    /// A trigger that has already seen one pass leave `free_gb` free while the
    /// disk term binds. For tests that start mid-incident.
    #[must_use]
    pub fn after_pass(free_gb: u64) -> Self {
        Self {
            was_binding: true,
            reference_free_gb: Some(free_gb),
        }
    }

    /// Whether this tick should ask for a pass, and why. Pure.
    ///
    /// An unmeasurable disk never fires: the disk term is then `usize::MAX`
    /// (no edge), and `free_gb` is `None` (no level).
    #[must_use]
    pub fn evaluate(&self, r: &TickReading) -> Option<TriggerReason> {
        let binds = disk_axis_binds_cap_down(r.disk, r.ram, r.configured_max);
        if binds && !self.was_binding {
            return Some(TriggerReason::Edge);
        }
        let free = r.free_gb?;
        if free < r.floor_gb {
            return Some(TriggerReason::BelowFloor);
        }
        let from_gb = self.reference_free_gb?;
        (binds && r.step_gb > 0 && free.saturating_add(r.step_gb) <= from_gb)
            .then_some(TriggerReason::FellSinceLastPass { from_gb })
    }

    /// Fold in the end-of-tick state: whether the disk term still binds after
    /// any pass, the latest free-GB reading, and the pass if one was asked
    /// for. Called on every tick, pass or not.
    pub fn observe(
        &mut self,
        binds_now: bool,
        free_gb: Option<u64>,
        pass: Option<&EagerReclaimReport>,
    ) {
        self.was_binding = binds_now;
        if !binds_now {
            // The next binding tick is a fresh edge.
            self.reference_free_gb = None;
            return;
        }
        if let Some(report) = pass.filter(|p| p.skipped.is_none()) {
            self.reference_free_gb = report.free_gb_after.or(report.free_gb_before).or(free_gb);
            return;
        }
        if let Some(free) = free_gb {
            self.reference_free_gb = Some(self.reference_free_gb.map_or(free, |r| r.max(free)));
        }
    }

    /// One dispatch tick, production wiring: probe free space and the floor,
    /// run [`super::run_for`] on the blocking pool if a pass is due, and
    /// return the disk term to finalize the cap with (re-probed after a pass
    /// that ran, so the cap is never clamped on a stale reading).
    pub async fn tick(
        &mut self,
        root: &Path,
        disk: usize,
        ram: usize,
        configured_max: usize,
    ) -> usize {
        let probe_root = root.to_path_buf();
        let (free_gb, floor_gb, step_gb) =
            tokio::task::spawn_blocking(move || pressure_probe(&probe_root))
                .await
                .unwrap_or((None, 0, 0));
        let reading = TickReading {
            disk,
            ram,
            configured_max,
            free_gb,
            floor_gb,
            step_gb,
        };
        let mut disk = disk;
        let mut free_now = free_gb;
        let mut pass = None;
        if let Some(reason) = self.evaluate(&reading) {
            let pass_root = root.to_path_buf();
            if let Ok(report) =
                tokio::task::spawn_blocking(move || super::run_for(&pass_root, reason)).await
            {
                if report.skipped.is_none() {
                    disk = crate::disk_headroom::disk_headroom_limit(root);
                    free_now = report.free_gb_after;
                }
                pass = Some(report);
            }
        }
        self.observe(disk_axis_binds_cap_down(disk, ram, configured_max), free_now, pass.as_ref());
        disk
    }
}

/// Free GB on `root`'s worktree volume, the floor, and the fall step.
/// Blocking (`df`, config reads).
fn pressure_probe(root: &Path) -> (Option<u64>, u64, u64) {
    let reaper_config = crate::worktree_reaper::read_worktree_reaper_config(root);
    let floor_gb = crate::worktree_reaper::resolve_disk_warn_free_gb(&reaper_config);
    let step_gb = super::resolve_fall_step_gb(&super::read_eager_reclaim_config(root));
    (crate::disk_headroom::worktree_root_free_gb(root), floor_gb, step_gb)
}
