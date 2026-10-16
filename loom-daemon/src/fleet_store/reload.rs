//! Classify a changed fleet-config dotted path as live-reloadable or
//! restart-required (Issue #9597).
//!
//! This table is **code**, not `defaults/docs/fleet-config-lifecycle.md`'s
//! narrative table — the whole point of #9597 is that `render`'s per-key
//! classification cannot silently drift out of sync with what the daemon
//! actually does, the way the doc table already has (`autonomous.workFinder`.
//! `maxConcurrent` / `extraSkipLabels` both went live
//! in #9060/pre-existing work without that doc ever being updated —
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
    // Re-read every multi-workspace work-finder tick since #9060 (the cap,
    // and since #12406 the only cap) — `ConfiguredMaxReloader::refresh()` in
    // `loom-daemon/src/work_finder/configured_max.rs`, consumed from
    // `spawn_multi_work_finder_task`'s tick body. Its `hyperparameters.dispatch`
    // home is overlaid inside that same `read_work_finder_config` call.
    ("autonomous.workFinder.maxConcurrent", Reloadability::Live),
    ("hyperparameters.dispatch.maxConcurrent", Reloadability::Live),
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
    // Read from config at every work-finder tick (#12306, #12311) —
    // `discovery::Chain::for_root`, never cached. The `autonomous.workFinder`
    // key is #12306's, read as an alias of `worklist`.
    ("autonomous.workFinder.discovery", Reloadability::Live),
    ("forge.mirror.discovery", Reloadability::Live),
    // Read from config at every work-finder tick (#12298) —
    // `step_dispatch::gather_budget::spawn_concurrency`, never cached.
    ("autonomous.workFinder.spawnConcurrency", Reloadability::Live),
    // Read from the workspace's effective config on every landed pre-filter
    // call (#12205) — `work_finder::landed_prefilter::prefilter_enabled`, never cached.
    ("autonomous.workFinder.landedPrefilter", Reloadability::Live),
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
    // ...except the captain declaration (#12423): the gate is re-evaluated on
    // every ci-telemetry tick (`ci_telemetry::gate_tick` →
    // `fleet_captain::arm_singleton_job`) and on every observability export
    // (`resolve_gate_for_root` in `observability/collector.rs`).
    ("fleet.captain", Reloadability::Live),
    // Audited blocks (#12423). Each entry names the call site that decided it.
    //
    // `read_auto_update_config` runs once from `auto_update::loop_mode::start`,
    // which `daemon_service.rs` calls once per process.
    ("autonomous.autoUpdate", Reloadability::RestartRequired),
    // `daemon_service.rs` reads it once at bring-up to decide whether the
    // reaper loop is spawned at all (and its interval); the per-tick re-read
    // inside the loop cannot start a loop that was never spawned.
    ("autonomous.worktreeReaper", Reloadability::RestartRequired),
    // `observability::read_config` feeds `observer_tasks::spawn` once at
    // bring-up (exporter wiring, `observability::spawn_task`).
    ("observability", Reloadability::RestartRequired),
    // `ci_telemetry::spawn_task_on` resolves the config once (owners,
    // interval, excluded repos) before the ticker is built; only the
    // captain gate inside the tick is live (see `fleet.captain`).
    ("autonomous.ciTelemetry", Reloadability::RestartRequired),
    // No daemon consumer: `autonomous.eta` is a retired block
    // (`signoz_read::RETIRED_CONFIG_KEY`, `config_resolver`), only reported as
    // ignored. Nothing caches it, so no restart changes anything.
    ("autonomous.eta", Reloadability::Live),
    // Retired block too (`work_finder::operator_priority::warn_retired_config`
    // only warns about it); there is no value to pick up.
    ("autonomous.operatorPriority", Reloadability::Live),
    // Resolved per dispatch: `runtime_preference::resolve_for_dispatch` takes
    // the root and reads the effective config on every admission.
    ("runtimes", Reloadability::Live),
    // `forge.gateway` is installed once into the process-wide gateway handles
    // (`forge_gateway` `DAEMON`/`FRONT` `OnceLock`s), so the block defaults to
    // restart-required; its per-call readers override below.
    ("forge", Reloadability::RestartRequired),
    // Read from the effective config on each use:
    // `credential_preflight::config_github_app_mint_timeout_secs` and
    // `forge_read_pool` (`resolve_effective_config` per read).
    ("forge.githubApp", Reloadability::Live),
    ("forge.githubAppReadPool", Reloadability::Live),
    // `safehouse::resolve_config` is called once by
    // `WorkspacePool::start_safehouse_narration` / peer-claim coordination at
    // start, and the sink/coordinator it builds is held for the process.
    ("safehouse", Reloadability::RestartRequired),
    // `codesign.identity` is resolved fresh each time it is needed
    // (`health::codesign::assess_codesign_identity`, the provisioning
    // scripts); the daemon holds no copy.
    ("codesign", Reloadability::Live),
    // No `loom-daemon` process reads it: ratification keys are consumed by
    // the branch-update/sign-off scripts, fresh on each invocation.
    ("ratification", Reloadability::Live),
];

/// A path with any `_`-prefixed segment is an annotation (`fleet._sync_note`),
/// not a knob: nothing reads it, so it never needs a restart.
fn is_annotation(path: &str) -> bool {
    path.split('.').any(|segment| segment.starts_with('_'))
}

/// Classify one dotted config path. A `_`-prefixed segment at any depth marks
/// an annotation and is always [`Reloadability::Live`]. Other unmatched paths
/// default conservatively to [`Reloadability::RestartRequired`] — see the
/// module doc's "mechanical test".
#[must_use]
pub fn classify(path: &str) -> Reloadability {
    if is_annotation(path) {
        return Reloadability::Live;
    }
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
    fn work_finder_ceiling_is_live() {
        assert_eq!(classify("autonomous.workFinder.maxConcurrent"), Reloadability::Live);
        assert_eq!(classify("hyperparameters.dispatch.maxConcurrent"), Reloadability::Live);
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
    fn underscore_segment_at_any_depth_is_live() {
        for path in [
            "fleet._sync_note",
            "_note",
            "autonomous.workFinder.enabled._why",
            "somethingBrandNew._x.knob",
            "fleet._a._b",
        ] {
            assert_eq!(classify(path), Reloadability::Live, "{path}");
        }
        // An underscore that is not at a segment start is a normal key.
        assert_eq!(classify("fleet.sync_interval"), Reloadability::RestartRequired);
    }

    #[test]
    fn fleet_captain_overrides_the_fleet_prefix() {
        assert_eq!(classify("fleet.captain"), Reloadability::Live);
    }

    #[test]
    fn every_audited_block_has_an_explicit_entry() {
        use Reloadability::{Live, RestartRequired};
        for (path, want) in [
            ("autonomous.autoUpdate.enabled", RestartRequired),
            ("autonomous.worktreeReaper.enabled", RestartRequired),
            ("observability.enabled", RestartRequired),
            ("autonomous.ciTelemetry.intervalSecs", RestartRequired),
            ("autonomous.eta.enabled", Live),
            ("autonomous.operatorPriority.propagate", Live),
            ("runtimes.default", Live),
            ("forge.gateway", RestartRequired),
            ("forge.githubApp.mintTimeoutSeconds", Live),
            ("forge.githubAppReadPool", Live),
            ("safehouse.enabled", RestartRequired),
            ("codesign.identity", Live),
            ("ratification.key", Live),
        ] {
            assert_eq!(classify(path), want, "{path}");
            let block = path.split('.').next().unwrap();
            assert!(
                TABLE.iter().any(|(prefix, _)| prefix.starts_with(block)),
                "{block} has no explicit entry"
            );
        }
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
            classify("autonomous.workFinder.maxConcurrentLimitFoo"),
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
