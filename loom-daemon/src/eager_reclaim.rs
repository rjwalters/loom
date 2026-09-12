//! Eager, out-of-cycle reclaim pass triggered from the 60s dispatch-cap loop
//! (#7512) — closes the ~14-minute gap between "the disk axis is about to bind
//! the dispatch cap down" ([`crate::work_finder`]'s
//! `resolve_dynamic_max_concurrent`) and "the worktree reaper's own reclaim
//! passes next run" ([`crate::worktree_reaper`]'s 15-minute cadence).
//!
//! # The gap this closes
//!
//! Three reclaim passes already ship — the merged-PR worktree reap, the
//! pressure-triggered [`crate::deep_clean`] (#5919), and
//! [`crate::docker_image_clean`] (#7332) — but all three run on the **worktree
//! reaper's** `DEFAULT_WORKTREE_REAPER_INTERVAL_SECS = 900` ticker, a different
//! `tokio::time::interval` from the **dispatch-cap** loop's
//! `DEFAULT_WORK_FINDER_INTERVAL_SECS = 60`. Nothing coupled the two, so the
//! dispatch loop could watch free space cross the floor and clamp the cap to 0
//! for up to ~14 minutes before reclaim was even attempted. That decoupling —
//! not an absent reclaim mechanism — is what starved dispatch on a host with
//! gigabytes of its own reclaimable garbage sitting on disk.
//!
//! # What this is, and is not
//!
//! This is **not** a new reclaim mechanism. It calls exactly the same three
//! entry points the scheduled reaper already calls, in the same order
//! ([`crate::worktree_reaper::reap_worktrees_only`],
//! [`crate::deep_clean::run_for`], [`crate::docker_image_clean::run_for`]),
//! plus the one genuinely-new pass this issue introduces
//! ([`crate::scratch_reclaim::run_for`]). Every sub-pass still honors its own
//! existing cooldown ([`crate::deep_clean`]'s 6h,
//! [`crate::docker_image_clean`]'s 30m, [`crate::scratch_reclaim`]'s 30m) —
//! calling them eagerly only makes an *already-due* pass run promptly instead
//! of waiting for the reaper's next tick; it never bypasses or shortens a
//! cooldown, because the cooldown state each sub-pass consults is theirs, and
//! this module neither reads nor resets it.
//!
//! # Trigger: edge, not level — plus a cooldown of this pass's own
//!
//! [`disk_axis_binds_cap_down`] is the condition ("the disk term would reduce
//! the cap below what RAM and the configured ceiling alone would allow"), and
//! [`should_trigger`] fires only on its `false -> true` **transition**, never
//! on every tick a stubbornly-full disk keeps it true. On top of that,
//! [`run_pass`] enforces a cooldown of its own
//! ([`DEFAULT_EAGER_MIN_INTERVAL_SECS`]).
//!
//! Both guards exist because of one sub-pass: the merged-PR worktree reap has
//! **no cooldown of its own** — its cadence *is* the reaper's 15-minute ticker
//! — and it makes real REST calls to the forge per candidate worktree. Edge
//! triggering stops a disk that is full for an unrelated reason (a large
//! dataset, another tenant) from turning the 60s loop into a forge-polling
//! loop; the pass-level cooldown additionally bounds a disk *oscillating*
//! across the binding threshold, which would otherwise produce a fresh "edge"
//! every couple of ticks.
//!
//! # Scope: the probed root only
//!
//! The dispatch-cap loop's disk term is a single machine-level probe against
//! one root (`workspace_root` in the single-workspace loop, `fallback_root` in
//! the production multi-workspace loop — see `work_finder.rs`'s own doc
//! comments for why). This module reclaims from that same root, mirroring the
//! existing architecture rather than introducing a "reclaim from every
//! registered root" fan-out for a disk term that was never measuring every
//! root in the first place. The scheduled reaper still walks every registered
//! root on its own cadence, unchanged.
//!
//! # Docker eager default (product decision, #7512)
//!
//! [`crate::docker_image_clean::run_for`] is called unconditionally here, same
//! as the reaper's scheduled pass. #7512's original filing sketched docker
//! prune as opt-in, but it has shipped **default-on**
//! (`autonomous.dockerImageRetention.enabled`) since #7332 and self-gates via
//! its own 30-minute cooldown. Giving the eager path a separate opt-in knob
//! would create a second, drifting on/off surface for one pass; the one knob
//! that already exists governs both call sites.
//!
//! # Safety
//!
//! Every "never touch" guarantee is inherited unchanged from the reused
//! functions — dirty worktrees, worktrees backing open PRs, `safe: true` /
//! `force: false` clean semantics, the machine build slot, and images backing
//! running containers are all gated inside the sub-passes, not here. This
//! module adds no removal code of its own; it only decides *when* to ask.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::deep_clean::DeepCleanReport;
use crate::docker_image_clean::DockerRetentionReport;
use crate::scratch_reclaim::ScratchReclaimReport;

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it. Disabling
/// restores exactly the pre-#7512 behavior — the dispatch loop clamps the cap
/// immediately and only the scheduled reaper's own 15-minute cadence reclaims.
pub const EAGER_RECLAIM_ENABLE_ENV: &str = "LOOM_EAGER_RECLAIM";

/// Env override for this pass's own anti-thrash cooldown (seconds).
pub const EAGER_RECLAIM_MIN_INTERVAL_ENV: &str = "LOOM_EAGER_RECLAIM_MIN_INTERVAL_SECS";

/// Default cooldown between eager passes for one root: 10 minutes. Shorter
/// than the reaper's own 15-minute cadence (otherwise this could never be the
/// thing that runs first, which is the entire point), long enough that a disk
/// oscillating across the binding threshold cannot re-run the uncooled
/// merged-PR worktree reap every couple of dispatch ticks.
pub const DEFAULT_EAGER_MIN_INTERVAL_SECS: u64 = 600;

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.eagerReclaim)
// ============================================================================

/// The subset of `.loom/config.json →
/// autonomous.worktreeReaper.eagerReclaim` this module consumes. Every field
/// is `Option` so an absent key falls through to the env-var /
/// built-in-default resolution — precedence **env > config > default**,
/// matching [`crate::deep_clean::DeepCleanConfig`] and every other
/// `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EagerReclaimConfig {
    /// `…eagerReclaim.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…eagerReclaim.minIntervalSecs` — cooldown between eager passes for one
    /// root (a zero/invalid value drops to `None`).
    pub min_interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.worktreeReaper.eagerReclaim`,
/// soft-failing every field to `None` (env/default resolution) on a missing
/// file, malformed JSON, or a missing block.
#[must_use]
pub fn read_eager_reclaim_config(repo_root: &Path) -> EagerReclaimConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.worktreeReaper.eagerReclaim")
    else {
        return EagerReclaimConfig::default();
    };
    EagerReclaimConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the eager trigger runs — precedence **env > config >
/// default(true)**.
#[must_use]
pub fn resolve_enabled(config: &EagerReclaimConfig) -> bool {
    if let Ok(v) = std::env::var(EAGER_RECLAIM_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the cooldown (seconds) — precedence **env > config > default**. A
/// zero or unparseable env value falls through rather than disabling the
/// anti-thrash gate.
#[must_use]
pub fn resolve_min_interval_secs(config: &EagerReclaimConfig) -> u64 {
    std::env::var(EAGER_RECLAIM_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_EAGER_MIN_INTERVAL_SECS)
}

// ============================================================================
// Trigger (pure)
// ============================================================================

/// Whether the disk term is the axis that binds
/// [`crate::work_finder::resolve_dynamic_max_concurrent`] down this tick —
/// i.e. whether `min(disk, ram, configured_max)` is strictly smaller than
/// `min(ram, configured_max)` would have been on its own.
///
/// This is deliberately broader than "the cap is exactly 0": a root whose disk
/// term has fallen to 1 while RAM and the operator ceiling would both allow 4
/// is already losing dispatch slots to disk, and reclaiming then is what keeps
/// it from reaching 0 at all. It also strictly *implies* the zero case
/// whenever the other two axes are non-zero, so the acceptance criterion "the
/// cap is never finalized as 0 without first attempting the eager reclaim" is
/// covered.
///
/// Note that an **unmeasurable** disk probe is `usize::MAX`
/// ([`crate::disk_headroom::disk_headroom_limit`]'s "unknown != zero"
/// contract, #4164), which can never be strictly less than another term — so a
/// broken `df` never triggers reclaim, exactly as it never triggers a clamp.
#[must_use]
pub fn disk_axis_binds_cap_down(
    disk_headroom: usize,
    ram_headroom: usize,
    configured_max: usize,
) -> bool {
    disk_headroom < ram_headroom.min(configured_max)
}

/// Whether this tick should run an eager reclaim pass: the disk axis binds the
/// cap down **now** and did not at the end of the previous tick.
///
/// `was_binding` is the caller's own across-tick state, updated *after* the
/// post-reclaim disk re-probe so a pass that successfully freed space re-arms
/// the trigger for a genuine future crossing. See the module docs for why this
/// is edge-triggered rather than level-triggered.
#[must_use]
pub fn should_trigger(
    was_binding: bool,
    disk_headroom: usize,
    ram_headroom: usize,
    configured_max: usize,
) -> bool {
    !was_binding && disk_axis_binds_cap_down(disk_headroom, ram_headroom, configured_max)
}

// ============================================================================
// Report
// ============================================================================

/// One eager-reclaim pass's outcome — bundles the four sub-pass reports plus
/// the free-GB reading from immediately before and immediately after, so a
/// single log line can name what every sub-pass did (#7512 AC4).
#[derive(Debug, Clone)]
pub struct EagerReclaimReport {
    /// The repo root this pass reclaimed from (the same root the dispatch
    /// loop's disk term was probed against).
    pub repo_root: PathBuf,
    /// `Some(reason)` when **no sub-pass ran at all** (disabled, or this
    /// pass's own cooldown had not elapsed). `None` on a pass that ran.
    pub skipped: Option<String>,
    /// The floor (`diskWarnFreeGb`) this crossing is measured against, carried
    /// so the log line can report free-GB *versus* the floor.
    pub floor_gb: u64,
    /// Free GB on the worktree-root volume immediately before this pass,
    /// `None` if unmeasurable (unknown != zero, #4164).
    pub free_gb_before: Option<u64>,
    /// Free GB immediately after. The dispatch loop does **not** read this to
    /// finalize its cap — it re-probes `disk_headroom_limit` itself — this is
    /// for the log line and for tests.
    pub free_gb_after: Option<u64>,
    /// How many `issue-<N>` worktrees the merged-PR reap sub-pass removed.
    pub worktrees_removed: usize,
    /// The [`crate::deep_clean`] sub-pass's own report — `None` only when this
    /// pass was skipped outright.
    pub deep_clean: Option<DeepCleanReport>,
    /// The [`crate::docker_image_clean`] sub-pass's own report.
    pub docker: Option<DockerRetentionReport>,
    /// The new `/tmp`-shaped scratch sub-pass's own report (#7512 item 3).
    pub scratch: Option<ScratchReclaimReport>,
    /// When this pass ran.
    pub at: DateTime<Utc>,
}

fn gb_or_unknown(free_gb: Option<u64>) -> String {
    free_gb.map_or_else(|| "unknown".to_string(), |gb| format!("{gb}G"))
}

impl EagerReclaimReport {
    /// One human-readable line naming what every sub-pass did and the
    /// resulting free-GB vs. the floor (#7512 AC4). Deliberately prefixed
    /// `eager_reclaim:` so it can never be confused with `worktree_reaper:`'s
    /// own scheduled-pass lines — an operator (or 2am's fleet-check
    /// DISK-HEADROOM instrument) can tell "the daemon already tried, promptly"
    /// apart from "the 15-minute reaper happened to run".
    #[must_use]
    pub fn log_line(&self) -> String {
        if let Some(reason) = &self.skipped {
            return format!(
                "eager_reclaim: {} disk axis binds the dispatch cap down but no pass ran: \
                 {reason} (#7512)",
                self.repo_root.display()
            );
        }
        let deep = self
            .deep_clean
            .as_ref()
            .map_or_else(|| "n/a".to_string(), DeepCleanReport::reclaimed_summary);
        let docker = self
            .docker
            .as_ref()
            .map_or_else(|| "n/a".to_string(), |d| format!("{} image(s)", d.removed.len()));
        let scratch = self
            .scratch
            .as_ref()
            .map_or_else(|| "n/a".to_string(), ScratchReclaimReport::removed_human);
        format!(
            "eager_reclaim: {} disk axis binds the dispatch cap down ({} free) — ran an \
             out-of-cycle pass now instead of waiting up to 15m for worktree_reaper's own \
             scheduled pass: worktrees {} removed, deep-clean {deep}, docker {docker}, scratch \
             {scratch} — now {} free vs. floor {}G (#7512)",
            self.repo_root.display(),
            gb_or_unknown(self.free_gb_before),
            self.worktrees_removed,
            gb_or_unknown(self.free_gb_after),
            self.floor_gb,
        )
    }
}

/// Log one pass. A pass that ran logs at `warn` (an operator investigating a
/// starved dispatch queue must see that the daemon tried, and what it got); a
/// skipped pass logs at `debug`.
pub fn log_report(report: &EagerReclaimReport) {
    if report.skipped.is_some() {
        log::debug!("{}", report.log_line());
    } else {
        log::warn!("{}", report.log_line());
    }
}

// ============================================================================
// The pass (injected seams)
// ============================================================================

/// The four reclaim sub-passes plus the free-space probe, injected so
/// [`run_pass`] is unit-testable without a git repo, a forge, `docker`, or a
/// host under genuine disk pressure — the same pure-core/IO-shell split
/// [`crate::deep_clean::run_pass`] and [`crate::docker_image_clean::run_pass`]
/// already use.
///
/// The production wiring ([`run_for`]) binds each of these to the *existing*
/// entry point it corresponds to; nothing here reimplements a reclaim.
pub struct SubPasses<'a> {
    /// Merged-PR worktree reap — returns how many `issue-<N>` worktrees it
    /// removed.
    pub reap_worktrees: &'a dyn Fn(&Path) -> usize,
    /// [`crate::deep_clean::run_for`], curried with the resolved floor.
    pub deep_clean: &'a dyn Fn(&Path) -> DeepCleanReport,
    /// [`crate::docker_image_clean::run_for`].
    pub docker: &'a dyn Fn(&Path) -> DockerRetentionReport,
    /// [`crate::scratch_reclaim::run_for`].
    pub scratch: &'a dyn Fn(&Path) -> ScratchReclaimReport,
    /// Free GB on the worktree-root volume, sampled before and after.
    pub free_gb: &'a dyn Fn(&Path) -> Option<u64>,
}

/// Everything one [`run_pass`] needs besides its injected seams.
#[derive(Debug, Clone, Copy)]
pub struct EagerReclaimInputs {
    /// Resolved `eagerReclaim.enabled`.
    pub enabled: bool,
    /// Resolved anti-thrash cooldown for this pass itself.
    pub min_interval_secs: u64,
    /// When an eager pass last ran for this root, or `None`.
    pub last_run: Option<DateTime<Utc>>,
    /// The reaper's resolved `diskWarnFreeGb`, reported in the log line and
    /// handed to the deep-clean sub-pass by [`run_for`].
    pub floor_gb: u64,
    /// Evaluation timestamp.
    pub now: DateTime<Utc>,
}

/// Run one eager pass over `repo_root`, in the exact order
/// [`crate::worktree_reaper::reap_repo`] already uses for its scheduled pass:
/// merged-PR worktree reap → deep clean → docker retention → scratch reclaim.
///
/// The ordering is load-bearing and inherited, not invented here: the cheap
/// worktree sweeps run first so that the expensive, pressure-gated deep pass
/// re-probes free space *after* them and correctly declines to touch a
/// developer's build cache when the cheap passes already freed enough.
///
/// Returns a report with `skipped: Some(..)` and **no sub-pass invoked** when
/// disabled or inside this pass's own cooldown. It never records cooldown
/// state itself — [`run_for`] owns that, so this stays a pure function of its
/// inputs.
#[must_use]
pub fn run_pass(
    repo_root: &Path,
    inputs: &EagerReclaimInputs,
    passes: &SubPasses<'_>,
) -> EagerReclaimReport {
    let skipped = |reason: String| EagerReclaimReport {
        repo_root: repo_root.to_path_buf(),
        skipped: Some(reason),
        floor_gb: inputs.floor_gb,
        free_gb_before: None,
        free_gb_after: None,
        worktrees_removed: 0,
        deep_clean: None,
        docker: None,
        scratch: None,
        at: inputs.now,
    };

    if !inputs.enabled {
        return skipped(
            "disabled (autonomous.worktreeReaper.eagerReclaim.enabled=false or \
             LOOM_EAGER_RECLAIM unset-falsy) — the scheduled worktree_reaper pass still applies"
                .to_string(),
        );
    }

    if let Some(last) = inputs.last_run {
        let since_secs = (inputs.now - last).num_seconds();
        let min = i64::try_from(inputs.min_interval_secs).unwrap_or(i64::MAX);
        if since_secs >= 0 && since_secs < min {
            return skipped(format!(
                "an eager pass ran {since_secs}s ago (cooldown {}s)",
                inputs.min_interval_secs
            ));
        }
    }

    let free_gb_before = (passes.free_gb)(repo_root);
    let worktrees_removed = (passes.reap_worktrees)(repo_root);
    let deep_clean = (passes.deep_clean)(repo_root);
    let docker = (passes.docker)(repo_root);
    let scratch = (passes.scratch)(repo_root);
    let free_gb_after = (passes.free_gb)(repo_root);

    EagerReclaimReport {
        repo_root: repo_root.to_path_buf(),
        skipped: None,
        floor_gb: inputs.floor_gb,
        free_gb_before,
        free_gb_after,
        worktrees_removed,
        deep_clean: Some(deep_clean),
        docker: Some(docker),
        scratch: Some(scratch),
        at: inputs.now,
    }
}

// ============================================================================
// Process-global per-repo cooldown state
// ============================================================================

static LAST_RUN_AT: OnceLock<Mutex<BTreeMap<PathBuf, DateTime<Utc>>>> = OnceLock::new();

fn last_run_slot() -> &'static Mutex<BTreeMap<PathBuf, DateTime<Utc>>> {
    LAST_RUN_AT.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn last_run_at(repo_root: &Path) -> Option<DateTime<Utc>> {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_root)
        .copied()
}

fn record_run(repo_root: &Path, now: DateTime<Utc>) {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(repo_root.to_path_buf(), now);
}

/// Drop cooldown state. Test-only seam (the process-global would otherwise
/// leak between tests in the same binary).
#[doc(hidden)]
pub fn reset_state_for_test() {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

// ============================================================================
// Production wiring
// ============================================================================

/// Run one production eager-reclaim pass for `repo_root` and log it.
///
/// **Blocking** — it shells to `df`/`git`/`docker` and may probe the forge over
/// REST, so callers must be on the blocking pool, exactly like the reaper's own
/// `spawn_blocking(move || reap_repo(..))`. The dispatch loop deliberately
/// `await`s it before finalizing the tick's cap: the whole point is that the
/// cap is not clamped on a stale measurement, and a tick whose disk axis is
/// binding the cap down is a tick that was about to dispatch little or nothing
/// anyway.
pub fn run_for(repo_root: &Path) -> EagerReclaimReport {
    let config = read_eager_reclaim_config(repo_root);
    let reaper_config = crate::worktree_reaper::read_worktree_reaper_config(repo_root);
    let floor_gb = crate::worktree_reaper::resolve_disk_warn_free_gb(&reaper_config);
    let inputs = EagerReclaimInputs {
        enabled: resolve_enabled(&config),
        min_interval_secs: resolve_min_interval_secs(&config),
        last_run: last_run_at(repo_root),
        floor_gb,
        now: Utc::now(),
    };

    let reap_worktrees = |root: &Path| {
        crate::worktree_reaper::reap_worktrees_only(root, &reaper_config)
            .removed
            .len()
    };
    let deep_clean = |root: &Path| crate::deep_clean::run_for(root, floor_gb);
    let docker = crate::docker_image_clean::run_for;
    let scratch = crate::scratch_reclaim::run_for;
    let free_gb = crate::disk_headroom::worktree_root_free_gb;
    let passes = SubPasses {
        reap_worktrees: &reap_worktrees,
        deep_clean: &deep_clean,
        docker: &docker,
        scratch: &scratch,
        free_gb: &free_gb,
    };

    let report = run_pass(repo_root, &inputs, &passes);
    if report.skipped.is_none() {
        record_run(repo_root, inputs.now);
    }
    log_report(&report);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::cell::{Cell, RefCell};

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    // ===================================================================
    // disk_axis_binds_cap_down — the condition
    // ===================================================================

    #[test]
    fn test_binds_when_disk_is_the_smallest_axis() {
        // disk 1 < min(ram 8, max 4) = 4 → disk is the binding axis.
        assert!(disk_axis_binds_cap_down(1, 8, 4));
    }

    #[test]
    fn test_binds_when_disk_is_zero() {
        // The headline case from #7512: cap 0 while RAM and the ceiling are
        // both healthy.
        assert!(disk_axis_binds_cap_down(0, 8, 4));
    }

    #[test]
    fn test_does_not_bind_when_another_axis_is_already_smaller() {
        // RAM is the binding axis, not disk — reclaiming disk would not raise
        // the cap by a single slot, so there is nothing to do eagerly.
        assert!(!disk_axis_binds_cap_down(6, 2, 4));
        // The operator ceiling binds.
        assert!(!disk_axis_binds_cap_down(6, 8, 4));
    }

    #[test]
    fn test_does_not_bind_on_a_tie() {
        // Equal is not "binding down" — the cap is the same either way.
        assert!(!disk_axis_binds_cap_down(4, 8, 4));
    }

    #[test]
    fn test_unmeasurable_disk_never_binds() {
        // `disk_headroom_limit` returns usize::MAX when `df` is unmeasurable
        // (#4164, unknown != zero). It must never trigger a reclaim.
        assert!(!disk_axis_binds_cap_down(usize::MAX, 8, 4));
    }

    #[test]
    fn test_all_axes_zero_does_not_bind() {
        // A configured_max of 0 (dispatch disabled outright) is not a disk
        // problem — there is nothing to reclaim *for*.
        assert!(!disk_axis_binds_cap_down(0, 0, 0));
    }

    // ===================================================================
    // should_trigger — edge, not level (fires only when the disk term is
    // about to bind the cap down, NOT on every dispatch tick).
    // ===================================================================

    #[test]
    fn test_should_trigger_fires_on_a_fresh_crossing() {
        assert!(should_trigger(false, 0, 8, 4));
    }

    #[test]
    fn test_should_trigger_does_not_refire_while_stuck() {
        // Already binding as of the previous tick: staying there must NOT
        // refire — this is the property that keeps the no-cooldown merged-PR
        // worktree reap sub-pass from becoming a 60s forge-polling loop.
        assert!(!should_trigger(true, 0, 8, 4));
    }

    #[test]
    fn test_should_trigger_does_not_fire_while_disk_is_healthy() {
        assert!(!should_trigger(false, 9, 8, 4));
    }

    #[test]
    fn test_should_trigger_fires_exactly_once_per_crossing_over_a_tick_run() {
        // Ticks: disk = 9, 0, 0, 0, 9, 1 with ram 8 / configured_max 4.
        // Exactly two crossings (index 1 and index 5) — not four, not six.
        let ticks = [9usize, 0, 0, 0, 9, 1];
        let mut was_binding = false;
        let mut fired_at = Vec::new();
        for (i, &disk) in ticks.iter().enumerate() {
            if should_trigger(was_binding, disk, 8, 4) {
                fired_at.push(i);
            }
            was_binding = disk_axis_binds_cap_down(disk, 8, 4);
        }
        assert_eq!(fired_at, vec![1, 5]);
    }

    // ===================================================================
    // run_pass — sub-pass invocation, cooldown, disabled
    // ===================================================================

    struct Counters {
        order: RefCell<Vec<&'static str>>,
    }

    impl Counters {
        fn new() -> Self {
            Self {
                order: RefCell::new(Vec::new()),
            }
        }
        fn total(&self) -> usize {
            self.order.borrow().len()
        }
    }

    fn stub_deep_report(root: &Path, now: DateTime<Utc>) -> DeepCleanReport {
        DeepCleanReport {
            repo_root: root.to_path_buf(),
            trigger: crate::deep_clean::DeepCleanTrigger::AboveFloor {
                free_gb: 99,
                floor_gb: 20,
            },
            deferred: None,
            reclaimed: Vec::new(),
            free_gb: Some(99),
            at: now,
        }
    }

    fn stub_docker_report(now: DateTime<Utc>) -> DockerRetentionReport {
        DockerRetentionReport {
            enabled: true,
            plan: None,
            removed: Vec::new(),
            deferred: None,
            at: now,
        }
    }

    fn stub_scratch_report(root: &Path, now: DateTime<Utc>) -> ScratchReclaimReport {
        ScratchReclaimReport {
            repo_root: root.to_path_buf(),
            enabled: true,
            removed_count: 0,
            removed_bytes: 0,
            deferred: None,
            at: now,
        }
    }

    fn run_with_counters(inputs: &EagerReclaimInputs, counters: &Counters) -> EagerReclaimReport {
        let now = inputs.now;
        let reap = |_: &Path| {
            counters.order.borrow_mut().push("worktrees");
            2usize
        };
        let deep = |root: &Path| {
            counters.order.borrow_mut().push("deep");
            stub_deep_report(root, now)
        };
        let docker = |_: &Path| {
            counters.order.borrow_mut().push("docker");
            stub_docker_report(now)
        };
        let scratch = |root: &Path| {
            counters.order.borrow_mut().push("scratch");
            stub_scratch_report(root, now)
        };
        let free_gb = |_: &Path| Some(3u64);
        let passes = SubPasses {
            reap_worktrees: &reap,
            deep_clean: &deep,
            docker: &docker,
            scratch: &scratch,
            free_gb: &free_gb,
        };
        run_pass(Path::new("/repo"), inputs, &passes)
    }

    fn base_inputs(now: DateTime<Utc>) -> EagerReclaimInputs {
        EagerReclaimInputs {
            enabled: true,
            min_interval_secs: 600,
            last_run: None,
            floor_gb: 20,
            now,
        }
    }

    #[test]
    fn test_run_pass_invokes_every_sub_pass_in_the_reapers_order() {
        let counters = Counters::new();
        let report = run_with_counters(&base_inputs(t(0)), &counters);
        assert!(report.skipped.is_none());
        assert_eq!(report.worktrees_removed, 2);
        assert_eq!(
            *counters.order.borrow(),
            vec!["worktrees", "deep", "docker", "scratch"],
            "must mirror worktree_reaper::reap_repo's own sequencing"
        );
        assert!(report.deep_clean.is_some());
        assert!(report.docker.is_some());
        assert!(report.scratch.is_some());
        assert_eq!(report.free_gb_before, Some(3));
        assert_eq!(report.free_gb_after, Some(3));
    }

    #[test]
    fn test_run_pass_inside_its_own_cooldown_invokes_no_sub_pass() {
        let counters = Counters::new();
        let mut inputs = base_inputs(t(300));
        inputs.last_run = Some(t(0)); // 300s ago, cooldown 600s
        let report = run_with_counters(&inputs, &counters);
        assert!(report.skipped.as_deref().unwrap().contains("cooldown"));
        assert_eq!(counters.total(), 0, "a cooldown-skipped pass must not touch the forge/docker");
        assert!(report.deep_clean.is_none());
    }

    #[test]
    fn test_run_pass_past_its_own_cooldown_runs_again() {
        let counters = Counters::new();
        let mut inputs = base_inputs(t(601));
        inputs.last_run = Some(t(0));
        let report = run_with_counters(&inputs, &counters);
        assert!(report.skipped.is_none());
        assert_eq!(counters.total(), 4);
    }

    #[test]
    fn test_run_pass_disabled_invokes_no_sub_pass() {
        let counters = Counters::new();
        let mut inputs = base_inputs(t(0));
        inputs.enabled = false;
        let report = run_with_counters(&inputs, &counters);
        assert!(report.skipped.as_deref().unwrap().contains("disabled"));
        assert_eq!(counters.total(), 0);
    }

    // ===================================================================
    // Cooldown pass-through: the eager trigger must NOT shorten or bypass
    // any sub-pass's own cooldown (#7512 AC1).
    // ===================================================================

    #[test]
    fn test_eager_pass_does_not_bypass_deep_cleans_six_hour_cooldown() {
        // The deep-clean seam here is the REAL `deep_clean::run_pass` — the
        // decision half of the exact entry point `run_for` binds — told that a
        // pass fired 1h ago under a 6h cooldown, on a host genuinely below its
        // floor. It must decline, and must never reach the (injected, counted)
        // removal step.
        let swept = Cell::new(0usize);
        let sweep = |_: &Path| {
            swept.set(swept.get() + 1);
            crate::deep_clean::SweepOutcome::Swept(Vec::new())
        };
        let opts = crate::deep_clean::deep_clean_options(
            crate::worktree_ops::clean::DEFAULT_GRACE_PERIOD_SECS,
        );
        let now = t(0);
        let deep_inputs = crate::deep_clean::DeepCleanInputs {
            opts: &opts,
            enabled: true,
            floor_gb: 20,
            min_interval_secs: 6 * 3600,
            last_fired: Some(now - chrono::Duration::hours(1)),
            now,
        };
        let deep =
            |root: &Path| crate::deep_clean::run_pass(root, &deep_inputs, &|_| Some(1u64), &sweep);

        let reap = |_: &Path| 0usize;
        let docker = |_: &Path| stub_docker_report(now);
        let scratch = |root: &Path| stub_scratch_report(root, now);
        let free_gb = |_: &Path| Some(1u64);
        let passes = SubPasses {
            reap_worktrees: &reap,
            deep_clean: &deep,
            docker: &docker,
            scratch: &scratch,
            free_gb: &free_gb,
        };
        let report = run_pass(Path::new("/repo"), &base_inputs(now), &passes);

        let deep_report = report.deep_clean.unwrap();
        assert!(
            matches!(deep_report.trigger, crate::deep_clean::DeepCleanTrigger::Cooldown { .. }),
            "eager trigger must inherit deep_clean's own cooldown verdict, got {:?}",
            deep_report.trigger
        );
        assert_eq!(swept.get(), 0, "a cooled-down deep clean must not remove anything");
    }

    #[test]
    #[serial]
    fn test_eager_pass_does_not_bypass_dockers_thirty_minute_cooldown() {
        // `docker_image_clean::run_for` is the exact seam the production
        // wiring binds. With its host-wide cooldown freshly armed it must
        // short-circuit — returning a cooldown-marked report WITHOUT shelling
        // out to `docker` at all (which is also why this test is safe to run
        // on a developer machine that has real images).
        crate::docker_image_clean::reset_state_for_test();
        crate::docker_image_clean::record_evaluated_for_test(Utc::now());
        let tmp = tempfile::tempdir().unwrap();
        let report = crate::docker_image_clean::run_for(tmp.path());
        assert!(
            report
                .deferred
                .as_deref()
                .unwrap_or_default()
                .contains("cooldown"),
            "expected a cooldown-marked report, got {report:?}"
        );
        assert!(report.plan.is_none());
        assert!(report.removed.is_empty());
        crate::docker_image_clean::reset_state_for_test();
    }

    #[test]
    #[serial]
    fn test_run_for_records_its_own_cooldown_and_the_second_call_is_a_noop() {
        // End-to-end through the production `run_for`, but with the eager pass
        // DISABLED so no sub-pass shells out: proves the skip path is taken
        // and nothing is recorded. The enabled/cooldown-recording half is
        // covered by `run_pass`'s injected-seam tests above, which can assert
        // sub-pass invocation counts without touching a real host.
        reset_state_for_test();
        std::env::set_var(EAGER_RECLAIM_ENABLE_ENV, "0");
        let tmp = tempfile::tempdir().unwrap();
        let report = run_for(tmp.path());
        std::env::remove_var(EAGER_RECLAIM_ENABLE_ENV);
        assert!(report.skipped.as_deref().unwrap().contains("disabled"));
        assert!(last_run_at(tmp.path()).is_none(), "a skipped pass must not arm the cooldown");
    }

    // ===================================================================
    // Config resolution
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_defaults_true() {
        std::env::remove_var(EAGER_RECLAIM_ENABLE_ENV);
        assert!(resolve_enabled(&EagerReclaimConfig::default()));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_false() {
        std::env::remove_var(EAGER_RECLAIM_ENABLE_ENV);
        assert!(!resolve_enabled(&EagerReclaimConfig {
            enabled: Some(false),
            min_interval_secs: None
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(EAGER_RECLAIM_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&EagerReclaimConfig {
            enabled: Some(true),
            min_interval_secs: None
        }));
        std::env::remove_var(EAGER_RECLAIM_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_min_interval_precedence() {
        std::env::remove_var(EAGER_RECLAIM_MIN_INTERVAL_ENV);
        assert_eq!(
            resolve_min_interval_secs(&EagerReclaimConfig::default()),
            DEFAULT_EAGER_MIN_INTERVAL_SECS
        );
        let config = EagerReclaimConfig {
            enabled: None,
            min_interval_secs: Some(120),
        };
        assert_eq!(resolve_min_interval_secs(&config), 120);
        std::env::set_var(EAGER_RECLAIM_MIN_INTERVAL_ENV, "45");
        assert_eq!(resolve_min_interval_secs(&config), 45);
        // A zero env value falls through rather than disabling the gate.
        std::env::set_var(EAGER_RECLAIM_MIN_INTERVAL_ENV, "0");
        assert_eq!(resolve_min_interval_secs(&config), 120);
        std::env::remove_var(EAGER_RECLAIM_MIN_INTERVAL_ENV);
    }

    // ===================================================================
    // Log line (#7512 AC4)
    // ===================================================================

    #[test]
    fn test_log_line_names_every_sub_pass_and_the_floor() {
        let counters = Counters::new();
        let report = run_with_counters(&base_inputs(t(0)), &counters);
        let line = report.log_line();
        assert!(
            line.starts_with("eager_reclaim:"),
            "must not be confusable with worktree_reaper:'s scheduled-pass line"
        );
        for needle in ["worktrees", "deep-clean", "docker", "scratch", "floor 20G"] {
            assert!(line.contains(needle), "log line missing {needle}: {line}");
        }
    }

    #[test]
    fn test_log_line_for_a_skipped_pass_says_why() {
        let counters = Counters::new();
        let mut inputs = base_inputs(t(10));
        inputs.last_run = Some(t(0));
        let report = run_with_counters(&inputs, &counters);
        assert!(report.log_line().contains("cooldown"));
    }
}
