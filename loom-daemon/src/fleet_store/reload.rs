//! Classify a changed fleet-config dotted path as live-reloadable or
//! restart-required (Issue #9597).
//!
//! This table is **code**, not `defaults/docs/fleet-config-lifecycle.md`'s
//! narrative table — the whole point of #9597 is that `render`'s per-key
//! classification cannot silently drift out of sync with what the daemon
//! actually does, the way the doc table already has (`autonomous.workFinder`.
//! `maxConcurrent` / `maxConcurrentPerRepo` / `extraSkipLabels` all went live
//! in #9060/#9090/pre-existing work without that doc ever being updated —
//! exactly the failure this module exists to stop happening again).
//!
//! # The mechanical test
//!
//! A knob is [`Reloadability::Live`] when its `read_*_config` call sits
//! inside a loop/tick body (or is otherwise re-read fresh on every use) —
//! there is no daemon-side caching to invalidate, so a config-file edit is
//! picked up on the very next iteration with zero extra work. It is
//! [`Reloadability::RestartRequired`] when that call runs once at daemon
//! bring-up, before any loop starts, and its result is captured into a
//! local/registered handle for the rest of the process's life. See each
//! table entry's comment for the exact call site that was checked.
//!
//! An entry not covered here defaults conservatively to
//! [`Reloadability::RestartRequired`] — "unknown" must never be reported as
//! "safe to skip the restart", only the reverse.

use std::fmt;

/// Whether a changed config path takes effect on a running daemon without a
/// restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reloadability {
    /// The consuming code re-reads this path fresh on every use; a config
    /// edit is picked up with no daemon-side signal required.
    Live,
    /// The consuming code resolves this path once at daemon bring-up; a
    /// change needs the daemon to end and a new process to start in its
    /// place before it takes effect.
    RestartRequired,
}

impl Reloadability {
    /// Short, stable label used in `render`'s output and the pending-restart
    /// marker — never changes shape across a release, since scripts may grep
    /// for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Reloadability::Live => "live",
            Reloadability::RestartRequired => "restart-required",
        }
    }
}

impl fmt::Display for Reloadability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `(dotted-path prefix, classification)`. Longest-matching prefix wins, so a
/// specific knob (`autonomous.workFinder.maxConcurrent`) can override its
/// parent block's default (`autonomous.workFinder`) — declaration order in
/// this slice does not matter.
const TABLE: &[(&str, Reloadability)] = &[
    // Re-read every role-runner tick, per registered root — no daemon-side
    // caching at all. `role_runner::read_role_runner_config` is called
    // inside the tick loop body (`loom-daemon/src/role_runner.rs`), and
    // `loom-daemon status` already reports the live per-root value.
    ("autonomous.roleRunner", Reloadability::Live),
    // Per-repo approval/merge policy (#11674): `merge_policy::declared` is
    // read from the effective config on every `fleet.state` export.
    ("repos", Reloadability::Live),
    // Re-read every multi-workspace work-finder tick since #9060 (the
    // operator ceiling) / #9090 (the per-repo cap) —
    // `ConfiguredMaxReloader::refresh()` in
    // `loom-daemon/src/work_finder/configured_max.rs`, consumed from
    // `spawn_multi_work_finder_task`'s tick body.
    ("autonomous.workFinder.maxConcurrent", Reloadability::Live),
    ("autonomous.workFinder.maxConcurrentPerRepo", Reloadability::Live),
    // #12090: the per-tick admission (ramp) cap rides the same per-tick read
    // (`ConfiguredMaxReloader::max_admissions_per_tick`), as does its
    // `hyperparameters.dispatch` home (`apply_hyperparams` overlays it, and
    // `maxConcurrent`, inside that same `read_work_finder_config` call).
    ("autonomous.workFinder.maxAdmissionsPerTick", Reloadability::Live),
    ("hyperparameters.dispatch.maxAdmissionsPerTick", Reloadability::Live),
    ("hyperparameters.dispatch.maxConcurrent", Reloadability::Live),
    // #12090: the brake's and breaker's thresholds are pushed into their
    // process-global handles every tick by `ConfiguredMaxReloader::refresh()`
    // (`admission_brake::global_reload`, `host_breaker::global_reload`).
    ("autonomous.workFinder.saturationBrake", Reloadability::Live),
    ("autonomous.hostBreaker", Reloadability::Live),
    // Re-read fresh on every candidate-list call —
    // `WorkDispatcher::extra_skip_labels` in
    // `loom-daemon/src/work_finder/forge.rs` reads
    // `.loom/config.json` directly each time, never cached.
    ("autonomous.workFinder.extraSkipLabels", Reloadability::Live),
    // Read from config each time a red-main fix is waiting (#10118) —
    // `main_red_fix::escalate_after`, never cached.
    ("autonomous.workFinder.redFixEscalateAfterSecs", Reloadability::Live),
    // Read from config at the start of every work-finder tick (#12074) —
    // `step_dispatch::gather_budget::begin_tick`, never cached.
    ("autonomous.workFinder.tickDeadlineSecs", Reloadability::Live),
    // Read from config at every work-finder tick (#12256) —
    // `step_dispatch::gather_budget::gather_concurrency`, never cached.
    ("autonomous.workFinder.gatherConcurrency", Reloadability::Live),
    // Read from config at every work-finder tick (#12306) —
    // `discovery::BackendKind::for_root`, never cached.
    ("autonomous.workFinder.discovery", Reloadability::Live),
    // Read from config at every work-finder tick (#12298) —
    // `step_dispatch::gather_budget::spawn_concurrency`, never cached.
    ("autonomous.workFinder.spawnConcurrency", Reloadability::Live),
    // Read from the workspace's effective config on every landed pre-filter
    // call (#12205) — `work_finder::landed_prefilter::prefilter_enabled`, never cached.
    ("autonomous.workFinder.landedPrefilter", Reloadability::Live),
    // #12203: the stage-dwell sampler's switch and spacing are read from the
    // effective config on every forge-gather pass
    // (`collector::forge_gathers` → `stage_dwell::Settings::from_effective`),
    // never cached.
    ("observability.stageDwell", Reloadability::Live),
    // #12246: the per-agent-scope `MemoryMax` knobs are resolved by
    // `loom-daemon ram-scope-limit`, a fresh process per agent spawn
    // (`ram_scope_limit::resolve`), never cached by the daemon.
    ("autonomous.agentMemoryMax", Reloadability::Live),
    ("autonomous.agentMemoryMaxPct", Reloadability::Live),
    ("autonomous.agentMemoryMaxMinMb", Reloadability::Live),
    // Everything else under `autonomous.workFinder` is resolved once at
    // daemon bring-up in `daemon_service.rs`'s work-finder startup block,
    // before the loop is spawned: `enabled` (gates whether the loop is
    // spawned at all) and `intervalSecs` (the ticker's period, which also
    // seeds the forge-event poll gate's base cadence and the armed early-tick
    // registration — re-arming only the ticker would leave those stale, so it
    // stays restart-required until they can follow it; #12090).
    ("autonomous.workFinder", Reloadability::RestartRequired),
    // Resolved once at startup, before any loop is spawned
    // (`rate_limit_breaker::register_global`, `daemon_service.rs`).
    ("autonomous.rateLimitBreaker", Reloadability::RestartRequired),
    // Resolved once at startup from the primary workspace config
    // (`main_health_gate::resolve_suppress_dispatch_during_gate` +
    // the gate loop's own master-switch read in `daemon_service.rs`).
    ("autonomous.mainHealthGate", Reloadability::RestartRequired),
    // Resolved once before the ingestion thread is spawned, then frozen for
    // that thread's lifetime (`try_init_transcript_ingest`).
    ("autonomous.transcriptIngest", Reloadability::RestartRequired),
    // `fleet.repo` / `fleet.ref` / `fleet.syncIntervalSecs` / `fleet.autoApply`
    // are captured once into `PassInputs` in `fleet_sync::start()`, before
    // the timer task is spawned — including the very config this `render`
    // itself is driven by.
    ("fleet", Reloadability::RestartRequired),
];

/// Classify one dotted config path. Unmatched paths default conservatively to
/// [`Reloadability::RestartRequired`] — see the module doc's "mechanical
/// test".
#[must_use]
pub fn classify(path: &str) -> Reloadability {
    TABLE
        .iter()
        .filter(|(prefix, _)| path == *prefix || path.starts_with(&format!("{prefix}.")))
        .max_by_key(|(prefix, _)| prefix.len())
        .map_or(Reloadability::RestartRequired, |(_, r)| *r)
}

/// Split `paths` into `(live, restart_required)`, each preserving input
/// order.
#[must_use]
pub fn partition(paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut live = Vec::new();
    let mut restart_required = Vec::new();
    for path in paths {
        match classify(path) {
            Reloadability::Live => live.push(path.clone()),
            Reloadability::RestartRequired => restart_required.push(path.clone()),
        }
    }
    (live, restart_required)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_runner_subtree_is_live() {
        assert_eq!(classify("autonomous.roleRunner"), Reloadability::Live);
        assert_eq!(classify("autonomous.roleRunner.enabled"), Reloadability::Live);
        assert_eq!(classify("autonomous.roleRunner.onIdle.judge"), Reloadability::Live);
    }

    #[test]
    fn work_finder_ceiling_and_per_repo_cap_are_live() {
        assert_eq!(classify("autonomous.workFinder.maxConcurrent"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.maxConcurrentPerRepo"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.extraSkipLabels"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.redFixEscalateAfterSecs"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.landedPrefilter"), Reloadability::Live);
    }

    #[test]
    fn work_finder_enabled_and_interval_are_restart_required() {
        assert_eq!(classify("autonomous.workFinder.enabled"), Reloadability::RestartRequired);
        assert_eq!(classify("autonomous.workFinder.intervalSecs"), Reloadability::RestartRequired);
        assert_eq!(
            classify("hyperparameters.dispatch.tickIntervalSecs"),
            Reloadability::RestartRequired
        );
    }

    /// #12090: the work-finder tuning keys hot-reload like `maxConcurrent`.
    #[test]
    fn work_finder_tuning_keys_are_live() {
        for path in [
            "autonomous.workFinder.maxAdmissionsPerTick",
            "hyperparameters.dispatch.maxAdmissionsPerTick",
            "hyperparameters.dispatch.maxConcurrent",
            "autonomous.workFinder.saturationBrake",
            "autonomous.workFinder.saturationBrake.enabled",
            "autonomous.workFinder.saturationBrake.loadPerCoreHold",
            "autonomous.workFinder.saturationBrake.cpuBusyHold",
            "autonomous.workFinder.saturationBrake.sustainTicks",
            "autonomous.workFinder.saturationBrake.starvationWarnSecs",
            "autonomous.workFinder.saturationBrake.starvationEscapeSecs",
            "autonomous.hostBreaker.enabled",
            "autonomous.hostBreaker.loadPerCoreTrip",
            "autonomous.hostBreaker.sustainTicks",
            "autonomous.hostBreaker.cooldownSecs",
        ] {
            assert_eq!(classify(path), Reloadability::Live, "{path}");
        }
    }

    #[test]
    fn stage_dwell_switch_is_live() {
        assert_eq!(classify("observability.stageDwell.enabled"), Reloadability::Live);
        assert_eq!(classify("observability.stageDwell.intervalSecs"), Reloadability::Live);
    }

    #[test]
    fn rate_limit_breaker_and_health_gate_are_restart_required() {
        assert_eq!(classify("autonomous.rateLimitBreaker.enabled"), Reloadability::RestartRequired);
        assert_eq!(classify("autonomous.mainHealthGate.enabled"), Reloadability::RestartRequired);
        assert_eq!(classify("autonomous.transcriptIngest.enabled"), Reloadability::RestartRequired);
    }

    #[test]
    fn fleet_block_is_restart_required() {
        assert_eq!(classify("fleet.syncIntervalSecs"), Reloadability::RestartRequired);
        assert_eq!(classify("fleet.autoApply"), Reloadability::RestartRequired);
    }

    #[test]
    fn unknown_path_defaults_conservatively_to_restart_required() {
        assert_eq!(classify("worktree.root"), Reloadability::RestartRequired);
        assert_eq!(classify("somethingBrandNew.knob"), Reloadability::RestartRequired);
    }

    #[test]
    fn longest_prefix_wins_over_its_parent_block() {
        // A sibling key that merely shares a string prefix with a specific
        // live knob (not a `.`-bounded ancestor) must not falsely match.
        assert_eq!(
            classify("autonomous.workFinder.maxConcurrentPerRepoLimitFoo"),
            Reloadability::RestartRequired
        );
    }

    #[test]
    fn partition_splits_and_preserves_order() {
        let paths = vec![
            "autonomous.roleRunner.enabled".to_string(),
            "autonomous.rateLimitBreaker.enabled".to_string(),
            "autonomous.workFinder.maxConcurrent".to_string(),
        ];
        let (live, restart_required) = partition(&paths);
        assert_eq!(
            live,
            vec![
                "autonomous.roleRunner.enabled".to_string(),
                "autonomous.workFinder.maxConcurrent".to_string(),
            ]
        );
        assert_eq!(restart_required, vec!["autonomous.rateLimitBreaker.enabled".to_string()]);
    }
}
