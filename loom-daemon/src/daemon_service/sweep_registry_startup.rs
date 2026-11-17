//! Sweep-registry knob configuration + shared-handle + reaper wiring, extracted
//! verbatim from `run_daemon` (Issue #13642, slice of the `run_daemon`
//! decomposition from #13017; same shape as `credential_startup`).
//!
//! Pure move — statement order, comments and log strings are unchanged. The
//! caller invokes [`configure_and_share`] after `reconstruct()` and
//! `claim_reconciliation::spawn_periodic_reconciliation_task`, and before
//! `workspace_pool.seed(...)`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use loom_daemon::sweep_registry::{self, StartupRaceConfig, SweepRegistry};

/// Values later startup phases need from the registry wiring.
pub(super) struct SweepRegistryStartup {
    pub sweep_registry: Arc<Mutex<SweepRegistry>>,
    /// Still read by the watchdog block in `run_daemon`.
    pub startup_race_config: StartupRaceConfig,
    /// Reaper task handle (bound as `_reaper_handle` before the move).
    pub reaper_handle: tokio::task::JoinHandle<()>,
}

pub(super) fn configure_and_share(
    mut sweep: SweepRegistry,
    sweep_workspace: &Path,
) -> SweepRegistryStartup {
    // Startup-race mitigation (Issue #3887): resolve the dispatch stagger + the
    // watchdog knobs from `.loom/config.json → autonomous` with env override
    // (precedence env > config > default). The stagger serializes back-to-back
    // child startups so a burst dispatch does not trip the 0-HTTPS MCP-init
    // race; the watchdog is the self-healing backstop for any hang that slips
    // past it.
    let startup_race_config = sweep_registry::read_startup_race_config(sweep_workspace);
    let dispatch_stagger = sweep_registry::resolve_dispatch_stagger(&startup_race_config);
    sweep.set_dispatch_stagger(dispatch_stagger);
    log::info!("sweep_registry: dispatch stagger = {}ms (#3887)", dispatch_stagger.as_millis());

    // Startup-proof occupancy grace (Issue #4003): a freshly-dispatched sweep
    // counts toward the work-finder's admission budget regardless of progress
    // for this long; past it, a sweep with zero observed startup-proof signal
    // (no worktree/checkpoint/log-past-header) stops consuming a slot, well
    // before the (unchanged) 300s startup watchdog would cancel/re-dispatch it.
    let startup_proof_grace = sweep_registry::resolve_startup_proof_grace(&startup_race_config);
    sweep.set_startup_proof_grace(startup_proof_grace);
    log::info!(
        "sweep_registry: startup-proof occupancy grace = {}s (#4003)",
        startup_proof_grace.as_secs()
    );

    // Insta-crash quarantine (#3939): resolve env > config > default for the
    // default workspace so the reaper quarantines a repeatedly-insta-crashing
    // issue instead of letting the work finder re-dispatch it every tick.
    let quarantine_config = sweep_registry::resolve_quarantine_config(sweep_workspace);
    sweep.set_quarantine_config(quarantine_config);
    log::info!(
        "sweep_registry: insta-crash quarantine {} (threshold={}, ttl={}s, insta-crash<{}s) (#3939)",
        if quarantine_config.enabled { "enabled" } else { "disabled" },
        quarantine_config.threshold,
        quarantine_config.ttl.as_secs(),
        quarantine_config.insta_crash_secs
    );

    // Per-issue dispatch backoff (#4485): resolve env > config > default for the
    // default workspace so a failing issue's re-dispatch cadence is bounded even
    // when its deaths land in a quarantine carve-out (#4122 exhaustion, #4386
    // pre-flight) and never reach the 3-strike quarantine threshold.
    let dispatch_backoff_config = sweep_registry::resolve_dispatch_backoff_config(sweep_workspace);
    sweep.set_dispatch_backoff_config(dispatch_backoff_config);
    log::info!(
        "sweep_registry: per-issue dispatch backoff {} (base={}s, max={}s) (#4485)",
        if dispatch_backoff_config.enabled {
            "enabled"
        } else {
            "disabled"
        },
        dispatch_backoff_config.base.as_secs(),
        dispatch_backoff_config.max.as_secs()
    );

    // No-op re-dispatch cooldown (#6670): resolve env > config > default for
    // the default workspace so a sweep's self-reported "no actionable delta
    // this pass" holds its issue out of dispatch for a cooldown window,
    // distinct from and independent of the insta-crash quarantine and
    // dispatch backoff above.
    let noop_cooldown_config = sweep_registry::resolve_noop_cooldown_config(sweep_workspace);
    sweep.set_noop_cooldown_config(noop_cooldown_config);
    // #11481: re-arm windows a restart would otherwise drop.
    sweep.replay_noop_cooldown_from_journal(chrono::Utc::now());
    log::info!(
        "sweep_registry: no-op re-dispatch cooldown {} (cooldown={}s) (#6670)",
        if noop_cooldown_config.enabled {
            "enabled"
        } else {
            "disabled"
        },
        noop_cooldown_config.cooldown.as_secs()
    );

    // Hard-exclusion decline cooldown (#7528): resolve env > config > default
    // for the default workspace so a sweep that declines because the issue
    // carries a `crate::hard_exclusion` label (`external` today) is held out
    // of dispatch for a cooldown window instead of being re-offered on the
    // very next tick. Independent of all three brakes above.
    let decline_cooldown_config = sweep_registry::resolve_decline_cooldown_config(sweep_workspace);
    sweep.set_decline_cooldown_config(decline_cooldown_config);
    log::info!(
        "sweep_registry: hard-exclusion decline cooldown {} (cooldown={}s, warn_threshold={}, \
         labels={:?}) (#7528)",
        if decline_cooldown_config.enabled {
            "enabled"
        } else {
            "disabled"
        },
        decline_cooldown_config.cooldown.as_secs(),
        decline_cooldown_config.warn_threshold,
        loom_daemon::hard_exclusion::HARD_EXCLUSION_LABELS.as_slice()
    );

    // PR-less retry bound (#7972): resolve env > config > default for the
    // default workspace so an issue whose dispatches keep ending without a
    // pull request is spaced out and — at the threshold — held with
    // `loom:blocked`, instead of being re-claimed forever. The only brake of
    // the four keyed on "did the dispatch produce a PR" rather than on how the
    // child died, which is why it catches the shape the others structurally
    // cannot (a sweep that advances its checkpoint and still produces nothing).
    // Resolve + set + log live in the module (`configure_prless_retry`), not
    // inline here: `daemon_service.rs` is over the file-size ratchet's
    // threshold and frozen at its current size (.loom/docs/file-size-policy.md).
    sweep_registry::configure_prless_retry(&mut sweep, sweep_workspace);

    // Claude-wrapper pre-flight-death workspace tripwire (#4386): resolve
    // env > config > default for the default workspace so a fleet-wide,
    // cross-issue spawn failure (e.g. a stale `.mcp.json`) trips a visible
    // advisory instead of reading as an idle-healthy daemon.
    let preflight_tripwire_config =
        sweep_registry::resolve_preflight_tripwire_config(sweep_workspace);
    sweep.set_preflight_tripwire_config(preflight_tripwire_config);
    log::info!(
        "sweep_registry: pre-flight-death tripwire threshold={} (#4386)",
        preflight_tripwire_config.threshold
    );

    // Cross-host collision detection + enforcement (#4085, upgraded from
    // detection-only by #5789): resolve env > config > default(off) for the
    // default workspace. When enabled, a confirmed pre-flip collision now
    // backs off the dispatch instead of only recording it.
    let detect_collisions = sweep_registry::resolve_collision_detection(sweep_workspace);
    sweep.set_collision_detection(detect_collisions);
    log::info!(
        "sweep_registry: cross-host collision detection {} (#4085)",
        if detect_collisions {
            "enabled"
        } else {
            "disabled"
        }
    );

    let sweep_registry = Arc::new(Mutex::new(sweep));
    let reaper_handle = sweep_registry::spawn_reaper_task(sweep_registry.clone());

    SweepRegistryStartup {
        sweep_registry,
        startup_race_config,
        reaper_handle,
    }
}
