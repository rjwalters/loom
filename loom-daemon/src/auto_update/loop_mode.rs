//! Whether the self-update loop runs, and in which mode (Issue #10954).
//!
//! Operator ruling, 2026-10-08: **the fleet floor applies to every fleet
//! host, not only hosts with `autonomous.autoUpdate.enabled`.** Before #10954
//! the loop was spawned only when `enabled` was true (default false), and the
//! floor check lives inside the loop, so a fleet host with autoUpdate off never
//! compared itself with the floor and could stay behind silently.
//!
//! The mode is chosen once, at spawn, from what `fleet_sync::start` found
//! (it is awaited before the loop is spawned, so the classification is known):
//!
//! | [`FloorKnowledge`] | `autoUpdate.enabled` | Loop | Mode |
//! |---|---|---|---|
//! | `Unknown` / `Set` (a fleet host) | false | spawned | [`LoopMode::Fleet`]: floor and repo-ahead rolls only |
//! | `Unknown` / `Set` (a fleet host) | true | spawned | [`LoopMode::Fleet`]: the same; `enabled` changes nothing |
//! | `NoStore` | true | spawned | [`LoopMode::ChaseLatest`]: opt-in, settle-gated |
//! | `NoStore` | false | not spawned | [`LoopMode::Off`], as before |
//!
//! A fleet host never chases the newest release (#10885), so `enabled` has
//! nothing left to govern there. A store that is named but unusable is a
//! fleet host whose floor is `Unknown`: the loop runs and does nothing for the
//! floor. There is no per-host opt-out from a floor roll: a fleet-paused
//! host below the floor still rolls (the roll replaces the pause's hold with a
//! supervised drain, and the daemon comes back still paused, #10979). Only an
//! operator stop on record (written by `restart --drain --then-exit` or
//! `fleet drain`; `restart --abort-drain` clears it) holds a roll, by skipping
//! the tick; a `stopped` host exits at boot, before this loop exists.
//!
//! There is one loop and one roll path (`trigger_pause_roll`); this module
//! adds no ticker. Each tick also re-checks the mode against the live floor
//! knowledge ([`idle_reason`]): a loop spawned only for the floor does not
//! start chasing the newest release if the host stops being a fleet host.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::fleet_sync::FloorKnowledge;

/// How the one self-update loop runs on this host (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// A fleet host: rolls for the floor and for a workspace that needs a
    /// newer daemon, never for the newest release. `auto_update_enabled` is
    /// recorded only to say, in status, that it was not what started the loop.
    Fleet {
        /// What `autonomous.autoUpdate.enabled` resolved to.
        auto_update_enabled: bool,
    },
    /// Not a fleet host, autoUpdate on: the opt-in, settle-gated chase.
    ChaseLatest,
    /// Not a fleet host, autoUpdate off: no loop.
    Off,
}

impl LoopMode {
    /// The spawn decision: a pure function of the floor knowledge at spawn
    /// and `autonomous.autoUpdate.enabled`.
    #[must_use]
    pub fn decide(knowledge: &FloorKnowledge, auto_update_enabled: bool) -> Self {
        match (knowledge, auto_update_enabled) {
            (FloorKnowledge::NoStore, true) => Self::ChaseLatest,
            (FloorKnowledge::NoStore, false) => Self::Off,
            (FloorKnowledge::Unknown(_) | FloorKnowledge::Set(_), enabled) => Self::Fleet {
                auto_update_enabled: enabled,
            },
        }
    }

    /// Whether the loop is spawned at all.
    #[must_use]
    pub fn spawns(self) -> bool {
        self != Self::Off
    }

    /// What `autonomous.autoUpdate.enabled` resolved to (#11042), reported in
    /// `status` beside `enabled`, which since #10954 means "the loop runs".
    #[must_use]
    pub fn config_enabled(self) -> bool {
        match self {
            Self::Fleet {
                auto_update_enabled,
            } => auto_update_enabled,
            Self::ChaseLatest => true,
            Self::Off => false,
        }
    }

    /// The one-time INFO line for a fleet host whose operator turned
    /// autoUpdate off explicitly (#11042): the setting does not exempt the
    /// host from the floor. `None` for every other mode, and when the setting
    /// was only defaulted.
    #[must_use]
    pub fn ignored_opt_out_note(self, explicitly_disabled: bool) -> Option<&'static str> {
        (explicitly_disabled
 && self
 == Self::Fleet {
 auto_update_enabled: false,
 })
 .then_some(
 "autoUpdate.enabled=false is set explicitly, but this is a fleet host: the setting is ignored for the fleet floor. The loop still rolls this host up to loom_min_version (and for a workspace that needs a newer daemon), never to the newest release",
 )
    }

    /// The status / log rendering: what the loop does and why it runs.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Fleet {
                auto_update_enabled: false,
            } => "fleet floor only (autoUpdate.enabled=false)",
            Self::Fleet {
                auto_update_enabled: true,
            } => "fleet floor only (autoUpdate.enabled=true has no effect on a fleet host)",
            Self::ChaseLatest => "enabled (no fleet store: chase-latest, settle-gated)",
            Self::Off => "off",
        }
    }
}

/// Why a tick does nothing at all, or `None` when it runs. Only one case: a
/// loop that runs for the fleet floor alone, on a tick where the host has no
/// fleet store. Such a host would otherwise take the chase-latest path that
/// `autoUpdate.enabled=false` keeps off.
#[must_use]
pub fn idle_reason(chase_enabled: bool, fleet_host: bool) -> Option<String> {
    (!chase_enabled && !fleet_host).then(|| {
        "not a fleet host this tick, and autoUpdate.enabled=false — the loop runs only for the \
 fleet floor, so nothing is checked"
            .to_string()
    })
}

/// Whether autoUpdate was turned off on purpose: `LOOM_AUTO_UPDATE` set to a
/// value that does not enable it, or `autonomous.autoUpdate.enabled: false`.
fn explicitly_disabled(config: &super::AutoUpdateConfig) -> bool {
    match std::env::var(super::AUTO_UPDATE_ENABLE_ENV) {
        Ok(_) => !super::resolve_enabled(config),
        Err(_) => config.enabled == Some(false),
    }
}

static MODE: OnceLock<LoopMode> = OnceLock::new();

/// The mode this process chose at spawn, or `None` before [`start`] ran.
#[must_use]
pub fn current() -> Option<LoopMode> {
    MODE.get().copied()
}

/// Decide the mode from fleet-sync's classification and the config, record
/// it for status, and spawn the one loop unless the mode is [`LoopMode::Off`].
/// Called once from `daemon_service.rs`, after `fleet_sync::start`.
pub fn start(
    workspace: &Path,
    pool: &Arc<crate::workspace_pool::WorkspacePool>,
    drain: &Arc<crate::ipc::DrainState>,
    event_bus: &Arc<crate::event_bus::EventBus>,
) -> Option<tokio::task::JoinHandle<()>> {
    let config = super::read_auto_update_config(workspace);
    super::removed_settings::warn_if_set(&config);
    let enabled = super::resolve_enabled(&config);
    let mode = LoopMode::decide(&crate::fleet_sync::floor_knowledge(), enabled);
    let _ = MODE.set(mode);
    // #11042: once, at spawn (this runs once per process).
    if let Some(note) = mode.ignored_opt_out_note(explicitly_disabled(&config)) {
        log::info!("auto_update: {note}");
    }
    if !mode.spawns() {
        log::debug!(
            "auto_update: disabled (not a fleet host; set LOOM_AUTO_UPDATE=1 or \
 autonomous.autoUpdate.enabled=true to opt in)"
        );
        return None;
    }
    let tuning = super::TickTuning::resolve(&config);
    log::info!("auto_update: {} ({})", mode.describe(), tuning.describe());
    let fallback: PathBuf = workspace.to_path_buf();
    let probe = super::ScriptAutoUpdateProbe::new(pool.clone(), fallback.clone());
    let trigger = super::IpcRollTrigger::new(
        drain.clone(),
        pool.clone(),
        fallback,
        event_bus.clone(),
        tokio::runtime::Handle::current(),
    );
    let status = Arc::new(super::AutoUpdateStatus::new(true));
    Some(super::spawn_auto_update_task(probe, trigger, status, tuning))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> FloorKnowledge {
        FloorKnowledge::Set("0.19.900".to_string())
    }

    fn unknown() -> FloorKnowledge {
        FloorKnowledge::Unknown("no pass yet".to_string())
    }

    #[test]
    fn every_fleet_host_spawns_the_loop_whatever_autoupdate_says() {
        for knowledge in [set(), unknown()] {
            for enabled in [false, true] {
                let mode = LoopMode::decide(&knowledge, enabled);
                assert_eq!(
                    mode,
                    LoopMode::Fleet {
                        auto_update_enabled: enabled
                    },
                    "{knowledge:?} enabled={enabled}"
                );
                assert!(mode.spawns(), "{knowledge:?} enabled={enabled}");
            }
        }
    }

    #[test]
    fn a_host_with_no_fleet_store_keeps_the_opt_in() {
        let on = LoopMode::decide(&FloorKnowledge::NoStore, true);
        assert_eq!(on, LoopMode::ChaseLatest);
        assert!(on.spawns());
        let off = LoopMode::decide(&FloorKnowledge::NoStore, false);
        assert_eq!(off, LoopMode::Off);
        assert!(!off.spawns());
    }

    #[test]
    fn status_says_why_the_loop_runs_with_autoupdate_off() {
        let mode = LoopMode::decide(&set(), false);
        assert_eq!(mode.describe(), "fleet floor only (autoUpdate.enabled=false)");
    }

    /// #11042: `enabled` in status means the loop runs; the setting is
    /// reported beside it, and an explicit opt-out on a fleet host is noted.
    #[test]
    fn the_setting_is_reported_and_an_explicit_fleet_opt_out_is_noted() {
        let fleet_off = LoopMode::decide(&set(), false);
        assert!(!fleet_off.config_enabled());
        assert!(LoopMode::decide(&set(), true).config_enabled());
        assert!(LoopMode::ChaseLatest.config_enabled());
        assert!(!LoopMode::Off.config_enabled());
        let note = fleet_off.ignored_opt_out_note(true).expect("noted");
        assert!(note.contains("ignored for the fleet floor"), "{note}");
        assert_eq!(fleet_off.ignored_opt_out_note(false), None, "only an explicit setting");
        assert_eq!(LoopMode::decide(&set(), true).ignored_opt_out_note(true), None);
        assert_eq!(LoopMode::Off.ignored_opt_out_note(true), None);
    }

    #[test]
    fn only_a_floor_only_loop_on_a_non_fleet_tick_idles() {
        assert!(idle_reason(false, false).is_some());
        assert_eq!(idle_reason(false, true), None);
        assert_eq!(idle_reason(true, false), None);
        assert_eq!(idle_reason(true, true), None);
    }
}
