//! Disk-full dispatch halt (#10973 item 2): the daemon's own terminal
//! self-protection when the worktree-root volume is out of space.
//!
//! # Why
//!
//! On 2026-10-08 a worker's worktree volume filled to 0 GB. The disk axis of
//! the dispatch cap clamped admissions, and the reclaim passes
//! ([`crate::eager_reclaim`], [`crate::deep_clean`],
//! [`crate::worktree_reaper`]) ran, but nothing turned "still full after
//! reclaim" into an explicit, visible halt: the host kept accepting claims and
//! `host.health` reported `dispatch_halted: false`. This module is that
//! terminal state.
//!
//! # Behaviour
//!
//! Each work-finder tick samples [`crate::disk_headroom::worktree_root_free_gb`]
//! (through [`crate::host_breaker::SharedHostBreaker::observe`], so no
//! work-finder edit is needed). A reading `Some(n)` below the floor on
//! [`DEFAULT_SUSTAIN_TICKS`] consecutive ticks trips the halt. The sustain
//! requirement is what makes it "after a reclaim attempt": the eager reclaim
//! pass fires on the tick the disk axis first binds the cap and its result is
//! visible to the next tick's probe, so a pass that frees space never halts.
//!
//! - `None` (unmeasurable probe) never halts (#4164, unknown != zero) and
//!   clears an existing halt rather than latching on missing data.
//! - Hysteresis: once halted, the halt clears only at `free >= resume`
//!   (default twice the floor), so a volume hovering at the floor cannot flap.
//! - The halt is surfaced through the existing `host.health`
//!   `dispatch_halted` / `halt_reason` fields and the same dispatch
//!   suppressors the host breaker feeds; no new wire field.
//!
//! Config: `LOOM_DISK_FULL_HALT_GB` (floor in GB, default
//! [`DEFAULT_FLOOR_GB`]; `0` disables) and `LOOM_DISK_FULL_RESUME_GB`
//! (default `2 * floor`).

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Env var: free-GB floor below which dispatch halts. `0` disables the halt.
pub const FLOOR_GB_ENV: &str = "LOOM_DISK_FULL_HALT_GB";
/// Env var: free GB at/above which a halt clears (default `2 * floor`).
pub const RESUME_GB_ENV: &str = "LOOM_DISK_FULL_RESUME_GB";
/// Default floor (GB free). Below the reaper's `diskWarnFreeGb` so the
/// reclaim passes always get their chance first.
pub const DEFAULT_FLOOR_GB: u64 = 3;
/// Consecutive low ticks required to halt (gives one reclaim pass a tick to
/// show its effect).
pub const DEFAULT_SUSTAIN_TICKS: u32 = 2;

/// Resolved halt parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskHaltConfig {
    /// Halt when free GB is below this; `0` disables.
    pub floor_gb: u64,
    /// Clear a halt when free GB reaches this (always `> floor_gb`).
    pub resume_gb: u64,
    /// Consecutive low ticks required to halt.
    pub sustain_ticks: u32,
}

impl DiskHaltConfig {
    /// Build from a floor and an optional resume level; resume is forced to be
    /// strictly above the floor so the hysteresis band is never empty.
    #[must_use]
    pub fn new(floor_gb: u64, resume_gb: Option<u64>) -> Self {
        let resume = resume_gb.unwrap_or(floor_gb.saturating_mul(2));
        Self {
            floor_gb,
            resume_gb: resume.max(floor_gb.saturating_add(1)),
            sustain_ticks: DEFAULT_SUSTAIN_TICKS,
        }
    }

    /// Resolve from the environment (invalid values fall back to defaults).
    #[must_use]
    pub fn from_env() -> Self {
        let parse = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        Self::new(parse(FLOOR_GB_ENV).unwrap_or(DEFAULT_FLOOR_GB), parse(RESUME_GB_ENV))
    }
}

/// Evolving halt state (input and output of [`step`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskHaltState {
    /// Whether dispatch is currently halted for disk-full.
    pub halted: bool,
    /// Consecutive below-floor ticks while not halted.
    pub low_ticks: u32,
    /// Latest reading (for the reason string).
    pub last_free_gb: Option<u64>,
}

/// A halt-state edge, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Dispatch is now halted.
    Halted,
    /// Dispatch halt cleared.
    Cleared,
}

/// Pure state machine: fold one free-GB reading into `prev`.
#[must_use]
pub fn step(
    prev: &DiskHaltState,
    cfg: &DiskHaltConfig,
    free_gb: Option<u64>,
) -> (DiskHaltState, Option<Edge>) {
    let mut next = DiskHaltState {
        last_free_gb: free_gb,
        ..prev.clone()
    };
    let clear = |mut s: DiskHaltState| {
        let edge = s.halted.then_some(Edge::Cleared);
        s.halted = false;
        s.low_ticks = 0;
        (s, edge)
    };
    // Disabled, or unmeasurable: never halt, and never latch on missing data.
    let Some(n) = free_gb.filter(|_| cfg.floor_gb > 0) else {
        return clear(next);
    };
    if next.halted {
        if n >= cfg.resume_gb {
            return clear(next);
        }
        return (next, None);
    }
    if n < cfg.floor_gb {
        next.low_ticks = next.low_ticks.saturating_add(1);
        if next.low_ticks >= cfg.sustain_ticks {
            next.halted = true;
            return (next, Some(Edge::Halted));
        }
    } else {
        next.low_ticks = 0;
    }
    (next, None)
}

/// The `halt_reason` text for a halted state.
#[must_use]
pub fn reason(state: &DiskHaltState, cfg: &DiskHaltConfig) -> Option<String> {
    if !state.halted {
        return None;
    }
    let n = state.last_free_gb.unwrap_or(0);
    Some(format!(
        "disk_full: {n} GB free (floor {} GB, resumes at {} GB) on the worktree volume after \
         reclaim (#10973)",
        cfg.floor_gb, cfg.resume_gb
    ))
}

/// Thread-safe guard: probes one repo's worktree volume and holds the state.
#[derive(Debug)]
pub struct DiskFullGuard {
    root: PathBuf,
    cfg: DiskHaltConfig,
    state: Mutex<DiskHaltState>,
}

impl DiskFullGuard {
    /// New guard for `root`'s worktree volume.
    #[must_use]
    pub fn new(root: PathBuf, cfg: DiskHaltConfig) -> Self {
        Self {
            root,
            cfg,
            state: Mutex::new(DiskHaltState::default()),
        }
    }

    /// Fold a reading in, logging any edge. Returns whether now halted.
    pub fn observe(&self, free_gb: Option<u64>) -> bool {
        let mut g = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (next, edge) = step(&g, &self.cfg, free_gb);
        match edge {
            Some(Edge::Halted) => log::error!(
                "disk_full_halt: {} — dispatch halted until >= {} GB free",
                reason(&next, &self.cfg).unwrap_or_default(),
                self.cfg.resume_gb
            ),
            Some(Edge::Cleared) => log::info!(
                "disk_full_halt: cleared (free {} GB); dispatch resumes",
                free_gb.map_or_else(|| "unknown".to_string(), |n| n.to_string())
            ),
            None => {}
        }
        *g = next;
        g.halted
    }

    /// Probe the live volume and fold the reading in.
    pub fn sample(&self) -> bool {
        self.observe(crate::disk_headroom::worktree_root_free_gb(&self.root))
    }

    /// Current halt reason, if halted.
    #[must_use]
    pub fn halt_reason(&self) -> Option<String> {
        let g = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reason(&g, &self.cfg)
    }
}

static GLOBAL: OnceLock<DiskFullGuard> = OnceLock::new();

/// Register the process-global guard (first registration wins).
pub fn register_global(root: PathBuf, cfg: DiskHaltConfig) {
    let _ = GLOBAL.set(DiskFullGuard::new(root, cfg));
}

/// Resolve the config from the environment, register the global guard for
/// `root`, and log the resolved parameters once.
pub fn register_from_env(root: &std::path::Path) {
    let cfg = DiskHaltConfig::from_env();
    log::info!(
        "disk_full_halt: floor={}G resume={}G sustain_ticks={}",
        cfg.floor_gb,
        cfg.resume_gb,
        cfg.sustain_ticks
    );
    register_global(root.to_path_buf(), cfg);
}

/// Sample the global guard (no-op when unregistered).
pub fn global_sample() {
    if let Some(g) = GLOBAL.get() {
        g.sample();
    }
}

/// The global halt reason, `None` when unregistered or not halted.
#[must_use]
pub fn global_halt_reason() -> Option<String> {
    GLOBAL.get().and_then(DiskFullGuard::halt_reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DiskHaltConfig {
        DiskHaltConfig::new(3, None) // floor 3, resume 6, sustain 2
    }

    fn feed(g: &DiskFullGuard, readings: &[Option<u64>]) -> Vec<bool> {
        readings.iter().map(|r| g.observe(*r)).collect()
    }

    #[test]
    fn below_floor_sustained_halts_with_disk_full_reason() {
        let g = DiskFullGuard::new(PathBuf::from("/x"), cfg());
        assert_eq!(feed(&g, &[Some(0), Some(0)]), vec![false, true]);
        let r = g.halt_reason().unwrap();
        assert!(r.starts_with("disk_full: 0 GB free"), "{r}");
    }

    #[test]
    fn single_low_tick_after_reclaim_recovery_does_not_halt() {
        // Reclaim freed space between ticks: never halts.
        let g = DiskFullGuard::new(PathBuf::from("/x"), cfg());
        assert_eq!(feed(&g, &[Some(1), Some(20), Some(1), Some(20)]), vec![false; 4]);
        assert!(g.halt_reason().is_none());
    }

    #[test]
    fn unmeasurable_probe_never_halts() {
        let g = DiskFullGuard::new(PathBuf::from("/x"), cfg());
        assert_eq!(feed(&g, &[None, None, None]), vec![false; 3]);
        // A single low reading followed by None resets the streak.
        assert_eq!(feed(&g, &[Some(0), None, Some(0)]), vec![false; 3]);
    }

    #[test]
    fn unmeasurable_probe_does_not_latch_an_existing_halt() {
        let g = DiskFullGuard::new(PathBuf::from("/x"), cfg());
        feed(&g, &[Some(0), Some(0)]);
        assert!(!g.observe(None));
    }

    #[test]
    fn halt_clears_only_at_resume_level_hysteresis() {
        let g = DiskFullGuard::new(PathBuf::from("/x"), cfg());
        let got = feed(&g, &[Some(0), Some(0), Some(3), Some(5), Some(6)]);
        assert_eq!(got, vec![false, true, true, true, false]);
        // And it needs a fresh sustained streak to re-halt.
        assert_eq!(feed(&g, &[Some(2), Some(2)]), vec![false, true]);
    }

    #[test]
    fn floor_zero_disables() {
        let g = DiskFullGuard::new(PathBuf::from("/x"), DiskHaltConfig::new(0, None));
        assert_eq!(feed(&g, &[Some(0), Some(0), Some(0)]), vec![false; 3]);
    }

    #[test]
    fn resume_is_always_above_floor() {
        assert_eq!(DiskHaltConfig::new(5, Some(1)).resume_gb, 6);
        assert_eq!(DiskHaltConfig::new(5, None).resume_gb, 10);
    }

    #[test]
    fn step_reports_edges_once() {
        let c = cfg();
        let s0 = DiskHaltState::default();
        let (s1, e1) = step(&s0, &c, Some(0));
        let (s2, e2) = step(&s1, &c, Some(0));
        let (s3, e3) = step(&s2, &c, Some(0));
        let (_, e4) = step(&s3, &c, Some(50));
        assert_eq!((e1, e2, e3, e4), (None, Some(Edge::Halted), None, Some(Edge::Cleared)));
    }
}
