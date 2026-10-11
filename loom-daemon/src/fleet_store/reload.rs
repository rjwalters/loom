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
//!
//! # Why accuracy matters (#10720)
//!
//! This table used to only print a line. Since #10720 the daemon itself acts
//! on it: a restart-required change a fleet-store host applied arms the
//! unified pause-and-roll (`fleet_sync::config_restart`), which pauses every
//! agent. So every block a fleet store carries has an explicit entry below,
//! and an annotation key (any `_`-prefixed segment, which nothing reads) never
//! restarts anything. An unlisted block still defaults to restart-required,
//! so a new block is a deliberate choice ([`AUDITED_BLOCKS`]).

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
    // Re-read every multi-workspace work-finder tick since #9060 (the
    // operator ceiling) / #9090 (the per-repo cap) —
    // `ConfiguredMaxReloader::refresh()` in
    // `loom-daemon/src/work_finder/configured_max.rs`, consumed from
    // `spawn_multi_work_finder_task`'s tick body.
    ("autonomous.workFinder.maxConcurrent", Reloadability::Live),
    ("autonomous.workFinder.maxConcurrentPerRepo", Reloadability::Live),
    // Re-read fresh on every candidate-list call —
    // `WorkDispatcher::extra_skip_labels` in
    // `loom-daemon/src/work_finder/forge.rs` reads
    // `.loom/config.json` directly each time, never cached.
    ("autonomous.workFinder.extraSkipLabels", Reloadability::Live),
    // Read from config each time a red-main fix is waiting (#10118) —
    // `main_red_fix::escalate_after`, never cached.
    ("autonomous.workFinder.redFixEscalateAfterSecs", Reloadability::Live),
    // Everything else under `autonomous.workFinder` — `enabled` (gates
    // whether the loop is spawned at all), `intervalSecs` (the ticker's
    // fixed period), `maxAdmissionsPerTick`, `hostClass` /
    // `allowHeavyLocal`, `saturationBrake.*` — is resolved once at daemon
    // bring-up in `daemon_service.rs`'s work-finder startup block, before
    // the loop is spawned.
    ("autonomous.workFinder", Reloadability::RestartRequired),
    // Resolved once at startup and registered as a process-global handle
    // (`host_breaker::register_global`, `daemon_service.rs`).
    ("autonomous.hostBreaker", Reloadability::RestartRequired),
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
    // Re-read on every tick: each captain-gated singleton re-evaluates
    // `config_resolver::fleet_captain` per tick (`fleet_singletons.rs`'s
    // module contract), so a captain change needs no restart.
    ("fleet.captain", Reloadability::Live),
    // `read_auto_update_config` runs once at bring-up in `daemon_service.rs`
    // (the self-update loop's spawn block) and the result is frozen into
    // `TickTuning`.
    ("autonomous.autoUpdate", Reloadability::RestartRequired),
    // Except the pause-and-roll budgets: `PauseRollTuning::resolve` reads them
    // fresh on every roll (`auto_update/pause_roll/supervise.rs`,
    // `start_pause_roll`).
    ("autonomous.autoUpdate.pauseRoll", Reloadability::Live),
    // `worktree_reaper::read_worktree_reaper_config` runs once before the
    // reaper loop is spawned (`daemon_service.rs`).
    ("autonomous.worktreeReaper", Reloadability::RestartRequired),
    // `ci_telemetry::spawn_task_on` resolves `enabled` / the interval once,
    // before its loop is spawned (`ci_telemetry/mod.rs`, `spawn_task_with_bus`).
    ("autonomous.ciTelemetry", Reloadability::RestartRequired),
    // No daemon consumer since #11098 retired the ETA writers: only the CLI
    // `telemetry-replay` reads the legacy `fleetRefresh.signoz` endpoint
    // (`signoz_read::LEGACY_ENDPOINT_CONFIG_KEY`), fresh per invocation.
    ("autonomous.eta", Reloadability::Live),
    // `star_liveness::Settings::resolve` is called at the top of every pass of
    // the star-liveness loop (`star_liveness/task.rs`, `spawn`).
    ("autonomous.operatorPriority", Reloadability::Live),
    // `observability::read_config` runs once in `spawn_observer_tasks`
    // (`daemon_service/observer_tasks.rs`) before the exporter is spawned.
    ("observability", Reloadability::RestartRequired),
    // Read per admission / per spawn: `runtime_admission::config_runtime`,
    // `worker_spawn::profiles::lookup`, `agent_relay`'s containment check and
    // `spawn-worker.sh` each resolve the effective config at the moment a
    // worker is launched.
    ("runtimes", Reloadability::Live),
    // `credential_preflight` resolves `forge.githubApp` once at bring-up,
    // before the fleet store is even read (`fleet_sync`'s module doc).
    ("forge", Reloadability::RestartRequired),
    // `WorkspacePool::start_safehouse_narration` / `start_peer_coordination`
    // resolve `safehouse::resolve_config` once when the pool comes up.
    ("safehouse", Reloadability::RestartRequired),
    // No daemon consumer: `codesign.identity` is read by `loom-daemon health`
    // (`cli/health.rs`) and the provisioning scripts, each fresh per run.
    ("codesign", Reloadability::Live),
    // No consumer in this tree at all; nothing a restart would re-read.
    ("ratification", Reloadability::Live),
];

/// The config blocks a fleet store carries that were audited for #10720. Each
/// must have an explicit [`TABLE`] entry (pinned by a test), so adding a
/// block to the store is a deliberate classification, never a silent default.
pub const AUDITED_BLOCKS: &[&str] = &[
    "autonomous.roleRunner",
    "autonomous.workFinder",
    "autonomous.hostBreaker",
    "autonomous.rateLimitBreaker",
    "autonomous.mainHealthGate",
    "autonomous.transcriptIngest",
    "autonomous.autoUpdate",
    "autonomous.worktreeReaper",
    "autonomous.ciTelemetry",
    "autonomous.eta",
    "autonomous.operatorPriority",
    "observability",
    "fleet",
    "fleet.captain",
    "runtimes",
    "forge",
    "safehouse",
    "codesign",
    "ratification",
];

/// Whether `path` names an annotation: any segment starting with `_`
/// (`fleet._sync_note`, `autonomous.autoUpdate._settle_note`). Nothing reads
/// one, so changing it never needs a restart.
#[must_use]
pub fn is_annotation(path: &str) -> bool {
    path.split('.').any(|segment| segment.starts_with('_'))
}

/// Whether `prefix` has its own [`TABLE`] entry.
#[must_use]
pub fn has_entry(prefix: &str) -> bool {
    TABLE.iter().any(|(p, _)| *p == prefix)
}

/// Classify one dotted config path. Annotation paths ([`is_annotation`]) are
/// live; other unmatched paths default conservatively to
/// [`Reloadability::RestartRequired`] — see the module doc's "mechanical
/// test".
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
    fn work_finder_ceiling_and_per_repo_cap_are_live() {
        assert_eq!(classify("autonomous.workFinder.maxConcurrent"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.maxConcurrentPerRepo"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.extraSkipLabels"), Reloadability::Live);
        assert_eq!(classify("autonomous.workFinder.redFixEscalateAfterSecs"), Reloadability::Live);
    }

    #[test]
    fn work_finder_enabled_and_admissions_are_restart_required() {
        assert_eq!(classify("autonomous.workFinder.enabled"), Reloadability::RestartRequired);
        assert_eq!(
            classify("autonomous.workFinder.maxAdmissionsPerTick"),
            Reloadability::RestartRequired
        );
        assert_eq!(classify("autonomous.workFinder.intervalSecs"), Reloadability::RestartRequired);
        assert_eq!(
            classify("autonomous.workFinder.saturationBrake.enabled"),
            Reloadability::RestartRequired
        );
    }

    #[test]
    fn breakers_and_health_gate_are_restart_required() {
        assert_eq!(classify("autonomous.hostBreaker.enabled"), Reloadability::RestartRequired);
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

    /// #10720: an annotation segment at any depth is live, even under a
    /// restart-required block.
    #[test]
    fn annotation_paths_are_live_at_any_depth() {
        for path in [
            "_note",
            "fleet._sync_note",
            "autonomous.autoUpdate._settle_note",
            "autonomous.workFinder.saturationBrake._why",
            "somethingBrandNew._comment.nested",
        ] {
            assert!(is_annotation(path), "{path}");
            assert_eq!(classify(path), Reloadability::Live, "{path}");
        }
        // An underscore that does not start a segment is not an annotation.
        assert!(!is_annotation("fleet.sync_note"));
        assert_eq!(classify("fleet.sync_note"), Reloadability::RestartRequired);
    }

    /// #10720: every audited block has its own entry, so a block a fleet store
    /// carries is never classified by the unknown default.
    #[test]
    fn every_audited_block_has_an_explicit_entry() {
        for block in AUDITED_BLOCKS {
            assert!(has_entry(block), "{block} has no explicit reload-table entry");
        }
    }

    #[test]
    fn audited_blocks_classify_as_their_call_sites_say() {
        let cases = [
            ("autonomous.autoUpdate.enabled", Reloadability::RestartRequired),
            ("autonomous.autoUpdate.intervalSecs", Reloadability::RestartRequired),
            ("autonomous.autoUpdate.pauseRoll.pauseBudgetSecs", Reloadability::Live),
            ("autonomous.worktreeReaper.enabled", Reloadability::RestartRequired),
            ("autonomous.ciTelemetry.enabled", Reloadability::RestartRequired),
            ("autonomous.eta.enabled", Reloadability::Live),
            ("autonomous.operatorPriority.escalate", Reloadability::Live),
            ("observability.enabled", Reloadability::RestartRequired),
            ("fleet.captain", Reloadability::Live),
            ("fleet.captainArmTtlSecs", Reloadability::RestartRequired),
            ("fleet.repo", Reloadability::RestartRequired),
            ("runtimes.default", Reloadability::Live),
            ("forge.githubApp.appId", Reloadability::RestartRequired),
            ("safehouse.enabled", Reloadability::RestartRequired),
            ("codesign.identity", Reloadability::Live),
            ("ratification.anything", Reloadability::Live),
        ];
        for (path, want) in cases {
            assert_eq!(classify(path), want, "{path}");
        }
    }

    #[test]
    fn partition_splits_and_preserves_order() {
        let paths = vec![
            "autonomous.roleRunner.enabled".to_string(),
            "autonomous.hostBreaker.enabled".to_string(),
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
        assert_eq!(restart_required, vec!["autonomous.hostBreaker.enabled".to_string()]);
    }
}
