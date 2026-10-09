//! #11192: the level trigger, tier escalation, the empty-pass alert and
//! backoff, and the cross-root idle-target pass on the scheduled path. A
//! sibling of `eager_reclaim.rs`'s own `tests` module so that file stays
//! under the file-size threshold.

#![allow(clippy::unwrap_used)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;

use chrono::{DateTime, Utc};
use serial_test::serial;

use super::*;

const FLOOR: u64 = 20;
const STEP: u64 = 10;
const CEILING: usize = 12;
const RAM: usize = 64;
const COOLDOWN: u64 = 600;

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
}

/// The disk term the work finder computes at the default 8 GB/worktree.
fn disk_term(free: Option<u64>) -> usize {
    free.map_or(usize::MAX, |gb| usize::try_from(gb / 8).unwrap())
}

fn reading(free: Option<u64>) -> TickReading {
    TickReading {
        disk: disk_term(free),
        ram: RAM,
        configured_max: CEILING,
        free_gb: free,
        floor_gb: FLOOR,
        step_gb: STEP,
    }
}

// ===================================================================
// Stub sub-pass reports
// ===================================================================

fn deep_report(root: &Path, now: DateTime<Utc>) -> DeepCleanReport {
    DeepCleanReport {
        repo_root: root.to_path_buf(),
        trigger: crate::deep_clean::DeepCleanTrigger::AboveFloor {
            free_gb: 99,
            floor_gb: FLOOR,
        },
        deferred: None,
        reclaimed: Vec::new(),
        free_gb: Some(99),
        at: now,
    }
}

fn docker_report(now: DateTime<Utc>) -> DockerRetentionReport {
    DockerRetentionReport {
        enabled: true,
        plan: None,
        removed: Vec::new(),
        deferred: None,
        at: now,
    }
}

fn scratch_report(root: &Path, now: DateTime<Utc>, removed: usize) -> ScratchReclaimReport {
    ScratchReclaimReport {
        repo_root: root.to_path_buf(),
        enabled: true,
        removed_count: removed,
        removed_bytes: 0,
        deferred: None,
        at: now,
    }
}

fn git_tmp_report(root: &Path, now: DateTime<Utc>) -> GitTmpReclaimReport {
    GitTmpReclaimReport {
        repo_root: root.to_path_buf(),
        enabled: true,
        dry_run: false,
        totals: crate::git_tmp_reclaim::SweepTotals::default(),
        skipped: None,
        at: now,
    }
}

fn orphan_report(root: &Path) -> TargetOrphanReport {
    TargetOrphanReport {
        repo_root: root.to_path_buf(),
        enabled: true,
        ..TargetOrphanReport::default()
    }
}

// ===================================================================
// A whole dispatch loop, simulated: trigger -> run_pass -> ledger ->
// observe, on an injected clock, mirroring `EagerTrigger::tick`.
// ===================================================================

struct Sim {
    trigger: EagerTrigger,
    ledger: PassLedger,
    /// Passes that ran (not cooldown-skipped): (seconds, reason, report).
    ran: Vec<(i64, TriggerReason, EagerReclaimReport)>,
    worktree_reaps: Cell<usize>,
    idle_runs: Cell<usize>,
    /// Whether the scratch sub-pass finds something to remove.
    reclaimable: bool,
}

impl Sim {
    fn new(trigger: EagerTrigger) -> Self {
        Self {
            trigger,
            ledger: PassLedger::default(),
            ran: Vec::new(),
            worktree_reaps: Cell::new(0),
            idle_runs: Cell::new(0),
            reclaimable: false,
        }
    }

    fn tick(&mut self, secs: i64, free: Option<u64>) {
        let now = t(secs);
        let r = reading(free);
        let mut free_now = free;
        let mut disk = r.disk;
        let mut pass = None;
        if let Some(reason) = self.trigger.evaluate(&r) {
            let inputs = EagerReclaimInputs {
                enabled: true,
                min_interval_secs: COOLDOWN,
                ledger: self.ledger,
                floor_gb: FLOOR,
                reason,
                now,
            };
            let removed = usize::from(self.reclaimable);
            let reap = |_: &Path| {
                self.worktree_reaps.set(self.worktree_reaps.get() + 1);
                0usize
            };
            let deep = |root: &Path| deep_report(root, now);
            let docker = |_: &Path| docker_report(now);
            let scratch = |root: &Path| scratch_report(root, now, removed);
            let orphans = |root: &Path| orphan_report(root);
            let free_gb = |_: &Path| free;
            let git_tmp = |root: &Path| git_tmp_report(root, now);
            let idle_body = |_: &Path, _: u64| {
                self.idle_runs.set(self.idle_runs.get() + 1);
                ReclaimReport::default()
            };
            let idle =
                |root: &Path| run_idle_targets_if_due(root, FLOOR, now, COOLDOWN, &idle_body);
            let passes = SubPasses {
                reap_worktrees: &reap,
                deep_clean: &deep,
                docker: &docker,
                scratch: &scratch,
                target_orphans: &orphans,
                free_gb: &free_gb,
                git_tmp: &git_tmp,
                idle_targets: &idle,
            };
            let report = run_pass(Path::new("/repo"), &inputs, &passes);
            self.ledger.record(&report);
            if report.skipped.is_none() {
                free_now = report.free_gb_after;
                disk = disk_term(free_now);
                self.ran.push((secs, reason, report.clone()));
            }
            pass = Some(report);
        }
        self.trigger
            .observe(disk_axis_binds_cap_down(disk, RAM, CEILING), free_now, pass.as_ref());
    }
}

// ===================================================================
// AC1: re-arms while the disk term keeps binding
// ===================================================================

#[test]
#[serial]
fn test_rearms_while_disk_keeps_binding_and_free_space_falls() {
    // loom-worker-1, 2026-10-09: one pass at 08:59 left 79G free; the disk
    // term (free/8 < ceiling 12) then bound on every tick. Edge-only
    // triggering ran nothing more. Ticks 10+ minutes apart.
    reset_idle_state_for_test();
    let mut sim = Sim::new(EagerTrigger::after_pass(79));
    for (i, free) in [79u64, 60, 40, 19].into_iter().enumerate() {
        sim.tick(i64::try_from(i).unwrap() * 660, Some(free));
    }
    let fired: Vec<(i64, TriggerReason)> = sim.ran.iter().map(|(s, r, _)| (*s, *r)).collect();
    assert_eq!(
        fired,
        vec![
            (660, TriggerReason::FellSinceLastPass { from_gb: 79 }),
            (1320, TriggerReason::FellSinceLastPass { from_gb: 60 }),
            (1980, TriggerReason::BelowFloor),
        ]
    );
    // The edge-only rule this replaces would have fired none of them.
    assert!(!should_trigger(true, disk_term(Some(19)), RAM, CEILING));
}

#[test]
#[serial]
fn test_below_floor_fires_even_when_the_disk_term_does_not_bind() {
    // A host whose ceiling is 2 never has the disk term bind above 16G free,
    // but below the floor the pass is still due.
    let trigger = EagerTrigger::default();
    let r = TickReading {
        disk: 2,
        ram: RAM,
        configured_max: 2,
        free_gb: Some(17),
        floor_gb: FLOOR,
        step_gb: STEP,
    };
    assert_eq!(trigger.evaluate(&r), Some(TriggerReason::BelowFloor));
}

#[test]
fn test_small_wobbles_do_not_fire() {
    let trigger = EagerTrigger::after_pass(79);
    assert_eq!(trigger.evaluate(&reading(Some(72))), None, "7G < the 10G step");
    assert_eq!(
        trigger.evaluate(&reading(Some(69))),
        Some(TriggerReason::FellSinceLastPass { from_gb: 79 })
    );
    let no_step = TickReading {
        step_gb: 0,
        ..reading(Some(30))
    };
    assert_eq!(trigger.evaluate(&no_step), None, "a zero step disables the fall rule");
}

#[test]
fn test_unmeasurable_disk_never_fires() {
    let trigger = EagerTrigger::after_pass(79);
    assert_eq!(trigger.evaluate(&reading(None)), None);
}

#[test]
fn test_fall_reference_is_a_high_water_mark_and_resets_off_binding() {
    let mut trigger = EagerTrigger::after_pass(50);
    // Space came back to 70 without a pass (a sweep finished): the fall is
    // measured from 70 now, not from 50.
    trigger.observe(true, Some(70), None);
    assert_eq!(trigger.evaluate(&reading(Some(62))), None);
    assert_eq!(
        trigger.evaluate(&reading(Some(60))),
        Some(TriggerReason::FellSinceLastPass { from_gb: 70 })
    );
    // The disk term stops binding: the next binding tick is an edge again.
    trigger.observe(false, Some(120), None);
    assert_eq!(trigger.evaluate(&reading(Some(90))), Some(TriggerReason::Edge));
}

// ===================================================================
// AC2: the 10-minute pass cooldown still holds
// ===================================================================

#[test]
#[serial]
fn test_pinned_below_floor_runs_at_most_one_pass_per_cooldown_window() {
    reset_idle_state_for_test();
    let mut sim = Sim::new(EagerTrigger::default());
    sim.reclaimable = true; // keep the backoff out of this test
    for tick in 0..60 {
        sim.tick(tick * 60, Some(15));
    }
    let at: Vec<i64> = sim.ran.iter().map(|(s, _, _)| *s).collect();
    assert_eq!(at, vec![0, 600, 1200, 1800, 2400, 3000], "one pass per 600s window");
    for pair in at.windows(2) {
        assert!(pair[1] - pair[0] >= 600);
    }
}

// ===================================================================
// Tier escalation
// ===================================================================

/// Run one pass whose successive free-space probes read `probes` (before,
/// after tier 1, after tier 2, …; the last value repeats), returning the
/// sub-passes it called in order.
fn run_tiered(probes: &[Option<u64>]) -> (EagerReclaimReport, Vec<&'static str>) {
    let order = RefCell::new(Vec::new());
    let queue = RefCell::new(probes.iter().copied().collect::<VecDeque<_>>());
    let now = t(0);
    let free_gb = |_: &Path| {
        let mut q = queue.borrow_mut();
        if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            *q.front().unwrap()
        }
    };
    let reap = |_: &Path| {
        order.borrow_mut().push("worktrees");
        0usize
    };
    let git_tmp = |root: &Path| {
        order.borrow_mut().push("git_tmp");
        git_tmp_report(root, now)
    };
    let scratch = |root: &Path| {
        order.borrow_mut().push("scratch");
        scratch_report(root, now, 0)
    };
    let orphans = |root: &Path| {
        order.borrow_mut().push("target_orphans");
        orphan_report(root)
    };
    let idle = |_: &Path| {
        order.borrow_mut().push("idle_targets");
        Some(ReclaimReport::default())
    };
    let deep = |root: &Path| {
        order.borrow_mut().push("deep");
        deep_report(root, now)
    };
    let docker = |_: &Path| {
        order.borrow_mut().push("docker");
        docker_report(now)
    };
    let passes = SubPasses {
        reap_worktrees: &reap,
        deep_clean: &deep,
        docker: &docker,
        scratch: &scratch,
        target_orphans: &orphans,
        free_gb: &free_gb,
        git_tmp: &git_tmp,
        idle_targets: &idle,
    };
    let inputs = EagerReclaimInputs {
        enabled: true,
        min_interval_secs: COOLDOWN,
        ledger: PassLedger::default(),
        floor_gb: FLOOR,
        reason: TriggerReason::BelowFloor,
        now,
    };
    let report = run_pass(Path::new("/repo"), &inputs, &passes);
    let called = order.borrow().clone();
    (report, called)
}

const TIER1: [&str; 4] = ["worktrees", "git_tmp", "scratch", "target_orphans"];

#[test]
fn test_tier_one_only_when_already_above_the_floor() {
    // An edge at 79G free: nothing invasive is needed.
    let (report, called) = run_tiered(&[Some(79)]);
    assert_eq!(report.tiers_run, 1);
    assert_eq!(called, TIER1.to_vec());
    assert!(report.idle_targets.is_none() && report.deep_clean.is_none());
    assert!(report.log_line().contains("not needed (above the floor)"));
}

#[test]
fn test_stops_after_tier_one_when_it_freed_enough() {
    let (report, called) = run_tiered(&[Some(15), Some(25)]);
    assert_eq!(report.tiers_run, 1);
    assert_eq!(called, TIER1.to_vec());
    assert_eq!(report.free_gb_before, Some(15));
    assert_eq!(report.free_gb_after, Some(25));
}

#[test]
fn test_escalates_to_idle_caches_then_stops() {
    let (report, called) = run_tiered(&[Some(15), Some(15), Some(25)]);
    assert_eq!(report.tiers_run, 2);
    let mut want = TIER1.to_vec();
    want.push("idle_targets");
    assert_eq!(called, want);
    assert!(report.deep_clean.is_none() && report.docker.is_none());
}

#[test]
fn test_escalates_to_the_most_invasive_tier_while_pressure_persists() {
    let (report, called) = run_tiered(&[Some(15)]);
    assert_eq!(report.tiers_run, 3);
    let mut want = TIER1.to_vec();
    want.extend(["idle_targets", "deep", "docker"]);
    assert_eq!(called, want);
}

#[test]
fn test_unmeasurable_free_space_runs_every_tier() {
    // Unknown != zero (#4164), but also unknown != healthy: the sub-passes'
    // own gates decide, exactly as before tiers existed.
    let (report, _) = run_tiered(&[None]);
    assert_eq!(report.tiers_run, 3);
}

// ===================================================================
// The cross-root idle-target pass on the scheduled path (AC3)
// ===================================================================

#[test]
#[serial]
fn test_scheduled_path_runs_the_cross_root_pass_under_pressure_without_eager_reclaim() {
    // No eager pass at all: only the reaper's 15-minute ticks, below the
    // floor for two hours. The cross-root pass must run on every one.
    reset_idle_state_for_test();
    let calls = RefCell::new(Vec::new());
    let idle = |root: &Path, floor: u64| {
        calls.borrow_mut().push((root.to_path_buf(), floor));
        ReclaimReport::default()
    };
    let below = |_: &Path| Some(5u64);
    for tick in 0..8 {
        let ran = scheduled_idle_pass_with(
            Path::new("/home/u/GitHub/loom"),
            FLOOR,
            t(tick * 900),
            COOLDOWN,
            &below,
            &idle,
        );
        assert!(ran.is_some(), "reaper tick {tick} below the floor must run the pass");
    }
    assert_eq!(calls.borrow().len(), 8);
    assert_eq!(calls.borrow()[0], (Path::new("/home/u/GitHub/loom").to_path_buf(), FLOOR));
}

#[test]
#[serial]
fn test_scheduled_path_skips_above_the_floor_or_unmeasured() {
    reset_idle_state_for_test();
    let calls = Cell::new(0usize);
    let idle = |_: &Path, _: u64| {
        calls.set(calls.get() + 1);
        ReclaimReport::default()
    };
    let root = Path::new("/repo");
    assert!(
        scheduled_idle_pass_with(root, FLOOR, t(0), COOLDOWN, &|_| Some(FLOOR), &idle).is_none()
    );
    assert!(scheduled_idle_pass_with(root, FLOOR, t(0), COOLDOWN, &|_| None, &idle).is_none());
    assert_eq!(calls.get(), 0);
}

#[test]
#[serial]
fn test_both_paths_share_one_window() {
    // A multi-root reaper tick right after an eager pass ran the cross-root
    // pass does not walk every root again; once the window passes it does.
    reset_idle_state_for_test();
    let calls = Cell::new(0usize);
    let idle = |_: &Path, _: u64| {
        calls.set(calls.get() + 1);
        ReclaimReport::default()
    };
    let below = |_: &Path| Some(5u64);
    assert!(run_idle_targets_if_due(Path::new("/probe"), FLOOR, t(0), COOLDOWN, &idle).is_some());
    for root in ["/a", "/b", "/c"] {
        let ran = scheduled_idle_pass_with(Path::new(root), FLOOR, t(120), COOLDOWN, &below, &idle);
        assert!(ran.is_none());
    }
    assert!(
        scheduled_idle_pass_with(Path::new("/a"), FLOOR, t(600), COOLDOWN, &below, &idle).is_some()
    );
    assert_eq!(calls.get(), 2);
}

#[test]
fn test_idle_pass_due() {
    assert!(idle_pass_due(None, t(0), COOLDOWN));
    assert!(!idle_pass_due(Some(t(0)), t(599), COOLDOWN));
    assert!(idle_pass_due(Some(t(0)), t(600), COOLDOWN));
    assert!(idle_pass_due(Some(t(600)), t(0), COOLDOWN), "a clock step back counts as due");
}

// ===================================================================
// Nothing reclaimable: alert, and no thrash
// ===================================================================

#[test]
#[serial]
fn test_no_thrash_when_nothing_is_reclaimable() {
    // Four hours pinned below the floor with nothing the daemon can remove.
    reset_idle_state_for_test();
    let mut sim = Sim::new(EagerTrigger::default());
    for tick in 0..240 {
        sim.tick(tick * 60, Some(12));
    }
    // The pass cooldown holds: one pass per 10-minute window, no more.
    assert_eq!(sim.ran.len(), 24);
    // The local cross-root pass still runs once per window (AC3)...
    assert_eq!(sim.idle_runs.get(), 24);
    // ...but the forge-polling merged-PR reap backs off: 0, then 2x/4x/8x
    // the cooldown (8x = 80 min is the cap) — at 0, 4800 and 9600s.
    assert_eq!(sim.worktree_reaps.get(), 3);
    // Every pass that came back empty raised its own alert.
    for (i, (_, _, report)) in sim.ran.iter().enumerate() {
        let streak = u32::try_from(i + 1).unwrap();
        let alert = report.empty_pass_alert(streak).unwrap();
        assert!(alert.starts_with("eager_reclaim: ALERT"), "{alert}");
        assert!(alert.contains(&format!("{streak} in a row")));
    }
    assert_eq!(sim.ledger.empty_streak, 24);
}

#[test]
fn test_backoff_resets_once_a_pass_reclaims_something() {
    let mut ledger = PassLedger {
        last_run: Some(t(0)),
        last_worktree_reap: Some(t(0)),
        empty_streak: 3,
    };
    assert!(!worktree_reap_due(&ledger, t(600), COOLDOWN));
    assert!(worktree_reap_due(&ledger, t(4800), COOLDOWN));
    let (mut report, _) = run_tiered(&[Some(15), Some(25)]);
    report.worktrees_removed = 1;
    assert!(report.reclaimed_anything());
    assert!(report.empty_pass_alert(1).is_none());
    ledger.record(&report);
    assert_eq!(ledger.empty_streak, 0);
    assert!(worktree_reap_due(&ledger, t(1), COOLDOWN));
}

#[test]
fn test_a_skipped_pass_neither_alerts_nor_moves_the_ledger() {
    let (mut report, _) = run_tiered(&[Some(15)]);
    report.skipped = Some("cooldown".to_string());
    assert!(report.empty_pass_alert(1).is_none());
    let mut ledger = PassLedger::default();
    ledger.record(&report);
    assert_eq!(ledger, PassLedger::default());
}

#[test]
fn test_log_line_names_the_trigger_and_the_tier() {
    let (report, _) = run_tiered(&[Some(15)]);
    let line = report.log_line();
    assert!(line.starts_with("eager_reclaim:"), "{line}");
    assert!(line.contains("free space is below the floor"), "{line}");
    assert!(line.contains("tier 3 of 3"), "{line}");
}

// ===================================================================
// Config
// ===================================================================

#[test]
#[serial]
fn test_resolve_fall_step_precedence() {
    std::env::remove_var(EAGER_RECLAIM_FALL_STEP_ENV);
    assert_eq!(resolve_fall_step_gb(&EagerReclaimConfig::default()), DEFAULT_EAGER_FALL_STEP_GB);
    let config = EagerReclaimConfig {
        fall_step_gb: Some(25),
        ..EagerReclaimConfig::default()
    };
    assert_eq!(resolve_fall_step_gb(&config), 25);
    std::env::set_var(EAGER_RECLAIM_FALL_STEP_ENV, "5");
    assert_eq!(resolve_fall_step_gb(&config), 5);
    std::env::set_var(EAGER_RECLAIM_FALL_STEP_ENV, "0");
    assert_eq!(resolve_fall_step_gb(&config), 25, "zero falls through");
    std::env::remove_var(EAGER_RECLAIM_FALL_STEP_ENV);
}
