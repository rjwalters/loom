//! Per-tick hot-reload of the work-finder's operator ceiling, `configured_max`
//! (issue #9060, closing the gap #6203 documented).
//!
//! The dynamic cap is `min(disk headroom, ram headroom, configured_max)`, and
//! the two headroom terms have always been re-read every tick. Until #9060 the
//! third term was resolved once at daemon bring-up and threaded into the loop
//! as a frozen `usize`, so raising a machine's ceiling to use idle capacity
//! required a daemon restart. [`ConfiguredMaxReloader::refresh`] now re-resolves
//! it every multi-workspace tick from the daemon's primary workspace, so an
//! edit to `autonomous.workFinder.maxConcurrent` (in `.loom/config.json`, or
//! host-locally in `.loom-local/local.json`) takes effect on the **next tick**.
//! In-flight sweeps are never touched: lowering the ceiling only holds back
//! new admissions until occupancy drains below it.
//!
//! # The env override stays restart-only
//!
//! Precedence is unchanged, **env > config > default**.
//! `LOOM_WORK_FINDER_MAX_CONCURRENT` is read from the daemon's own process
//! environment, and a running process never observes an edit to the systemd
//! `EnvironmentFile` / `.env` it was launched from. So while the env var is
//! set it keeps shadowing config, and [`transition_message`] logs once, on
//! each change, that a config edit is being shadowed. An operator is never
//! left wondering why an edit did nothing.
//!
//! # Soft-fail
//!
//! [`read_work_finder_config`] soft-fails a missing or malformed file to
//! "absent", so a half-written config falls back to env/default for that tick
//! (exactly the startup behavior), never to a zero ceiling. The next tick with
//! a well-formed file restores the configured value.
//!
//! # The other tuning keys (#12090)
//!
//! The same per-tick read also re-resolves `maxAdmissionsPerTick` (the #4234
//! ramp cap, read back with [`ConfiguredMaxReloader::max_admissions_per_tick`])
//! and pushes `saturationBrake.*` / `hostBreaker.*` into their process-global
//! handles ([`crate::admission_brake::global_reload`],
//! [`crate::host_breaker::global_reload`]). `intervalSecs` stays
//! restart-required: see `fleet_store::reload` for why.

use std::path::PathBuf;
use std::time::Duration;

use super::{
    read_work_finder_config, resolve_max_concurrent_with_source, ConfigSource, WorkFinderConfig,
};

/// A resolved operator ceiling plus enough provenance to explain it in a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfiguredMax {
    /// The ceiling fed into `resolve_dynamic_max_concurrent`.
    pub value: usize,
    /// Which layer supplied `value`.
    pub source: ConfigSource,
    /// The config value the env override is hiding, when one is set and
    /// differs from `value`. Always `None` unless `source` is `Env`.
    pub shadowed_config: Option<usize>,
    /// The **per-repo** cap (#9090, `autonomous.workFinder.maxConcurrentPerRepo`
    /// / `LOOM_WORK_FINDER_MAX_CONCURRENT_PER_REPO`), or `None` for uncapped —
    /// the opt-in default. Resolved on the same per-tick hot-reload path as
    /// `value` deliberately: a fleet retuning how much of one host's budget a
    /// single repo may hold must not need a daemon restart, exactly as #9060
    /// concluded for the global ceiling.
    pub per_repo: Option<usize>,
    /// The per-tick admission (ramp) cap, `maxAdmissionsPerTick` (#4234), and
    /// which layer supplied it. Re-read every tick since #12090.
    pub max_admissions_per_tick: (usize, ConfigSource),
}

impl ConfiguredMax {
    /// Resolve with precedence env > config > default.
    #[must_use]
    pub fn resolve(config: &WorkFinderConfig) -> Self {
        let (value, source) = resolve_max_concurrent_with_source(config);
        let shadowed_config = match source {
            ConfigSource::Env => config.max_concurrent.filter(|&c| c != value),
            ConfigSource::Config | ConfigSource::Default => None,
        };
        Self {
            value,
            source,
            shadowed_config,
            per_repo: super::repo_cap::resolve(config),
            max_admissions_per_tick: resolve_admissions_with_source(config),
        }
    }
}

/// `maxAdmissionsPerTick` with precedence env > config > default, naming the
/// layer — the companion of [`super::resolve_max_admissions_per_tick_with_config`].
#[must_use]
pub fn resolve_admissions_with_source(config: &WorkFinderConfig) -> (usize, ConfigSource) {
    if let Some(v) = super::env_max_admissions_per_tick() {
        (v, ConfigSource::Env)
    } else if let Some(v) = config.max_admissions_per_tick {
        (v, ConfigSource::Config)
    } else {
        (super::DEFAULT_MAX_ADMISSIONS_PER_TICK, ConfigSource::Default)
    }
}

/// The log line for a `maxAdmissionsPerTick` change (#12090), or `None` when
/// it did not change.
#[must_use]
pub fn admissions_transition_message(prev: ConfiguredMax, next: ConfiguredMax) -> Option<String> {
    let ((old, _), (new, source)) = (prev.max_admissions_per_tick, next.max_admissions_per_tick);
    (old != new).then(|| {
        format!(
            "maxAdmissionsPerTick {old} -> {new} (source={source}) — hot-applied without a \
             daemon restart (#12090); the next tick admits at most {new} new sweep(s)"
        )
    })
}

/// A `source=` label for a block of independently-resolved knobs: `config`
/// when no env override is set, else naming the env-overridden fields (which
/// a config edit cannot move until the daemon restarts without them).
#[must_use]
pub fn source_label(env_set: &[(&str, bool)]) -> String {
    let overridden: Vec<&str> = env_set
        .iter()
        .filter(|(_, set)| *set)
        .map(|(k, _)| *k)
        .collect();
    if overridden.is_empty() {
        "config".to_string()
    } else {
        format!("config, env overrides {}", overridden.join(","))
    }
}

/// The log line for a `prev → next` transition, or `None` when nothing an
/// operator would care about changed (the common, every-tick case).
#[must_use]
pub fn transition_message(prev: ConfiguredMax, next: ConfiguredMax) -> Option<String> {
    if prev.value != next.value {
        return Some(format!(
            "configured_max {} -> {} (source={}) — hot-applied without a daemon restart (#9060); \
             in-flight sweeps are unaffected, the new ceiling gates new admissions",
            prev.value, next.value, next.source
        ));
    }
    if prev.shadowed_config == next.shadowed_config {
        return None;
    }
    next.shadowed_config
        .map(|config| shadowed_message(next.value, config))
}

/// The log line for a `prev → next` **per-repo cap** transition (#9090), or
/// `None` when it did not change (the common, every-tick case).
///
/// Its own message rather than a branch of [`transition_message`]: the two
/// knobs are independent (one bounds the host, one bounds a single repo's share
/// of it) and either can change on a tick the other did not.
#[must_use]
pub fn per_repo_transition_message(prev: ConfiguredMax, next: ConfiguredMax) -> Option<String> {
    if prev.per_repo == next.per_repo {
        return None;
    }
    Some(match next.per_repo {
        Some(cap) => format!(
            "maxConcurrentPerRepo {} -> {cap} — hot-applied without a daemon restart (#9090); \
             at most {cap} of this host's {} slots may be held by any one repo, and repos with \
             a live sweep sort ahead of cold ones",
            prev.per_repo
                .map_or_else(|| "uncapped".to_string(), |c| c.to_string()),
            next.value
        ),
        None => "maxConcurrentPerRepo cleared -> uncapped (#9090) — one repo may again hold \
                 every slot of this host's budget, and track affinity is off"
            .to_string(),
    })
}

fn shadowed_message(env_value: usize, config: usize) -> String {
    format!(
        "autonomous.workFinder.maxConcurrent={config} is IGNORED — \
         {}={env_value} overrides it, and the env var is read only at daemon start. \
         Unset it and restart once so config edits hot-apply (#9060)",
        super::WORK_FINDER_MAX_CONCURRENT_ENV
    )
}

/// Holds the last-resolved ceiling across ticks so a change is logged once.
pub struct ConfiguredMaxReloader {
    root: PathBuf,
    current: ConfiguredMax,
}

impl ConfiguredMaxReloader {
    /// `initial` is the value the daemon resolved at bring-up (and already
    /// logged), so the first tick does not re-announce it.
    #[must_use]
    pub fn new(root: PathBuf, initial: ConfiguredMax) -> Self {
        Self {
            root,
            current: initial,
        }
    }

    /// Re-read config under the primary workspace, log any transition, and
    /// return this tick's ceiling.
    ///
    /// The per-repo cap (#9090) is refreshed by the same read and read back
    /// with [`Self::per_repo`] — one config read per tick serves both knobs.
    /// The same read also publishes the workspace draw's weight bonuses
    /// (`dispatch.weightBonus*`, #11742) for this tick's draw.
    pub fn refresh(&mut self) -> usize {
        let config = read_work_finder_config(&self.root);
        publish_weight_bonus(&config);
        let next = ConfiguredMax::resolve(&config);
        if let Some(msg) = transition_message(self.current, next) {
            log::info!("work_finder: {msg}");
        }
        if let Some(msg) = per_repo_transition_message(self.current, next) {
            log::info!("work_finder: {msg}");
        }
        if let Some(msg) = admissions_transition_message(self.current, next) {
            log::info!("work_finder: {msg}");
        }
        // #12090: the brake and breaker thresholds hot-apply on the same tick.
        crate::admission_brake::global_reload(&self.root);
        crate::host_breaker::global_reload(&self.root);
        self.current = next;
        next.value
    }

    /// This tick's per-tick admission cap (#12090). Valid after
    /// [`Self::refresh`]; before the first refresh it is the startup value.
    #[must_use]
    pub fn max_admissions_per_tick(&self) -> usize {
        self.current.max_admissions_per_tick.0
    }

    /// This tick's per-repo cap (#9090) — `None` for uncapped. Valid after
    /// [`Self::refresh`]; before the first refresh it is the startup value.
    #[must_use]
    pub fn per_repo(&self) -> Option<usize> {
        self.current.per_repo
    }
}

/// Publish `config`'s weight bonuses (#11742) for the next tick's
/// [`super::workspace_draw::order`], logging a change once.
pub fn publish_weight_bonus(config: &WorkFinderConfig) {
    let next = super::config::resolve_weight_bonus(config);
    let prev = super::workspace_draw::set_weight_bonus(next);
    if prev != next {
        log::info!(
            "work_finder: dispatch.weightBonus {prev:?} -> {next:?} — hot-applied without a \
             daemon restart (#11742); the next workspace draw uses the new bonuses"
        );
    }
}

/// The multi-workspace loop's one-time startup line (moved here from
/// `spawn_multi_work_finder_task`, which is at its file-size ratchet).
pub fn log_loop_start(interval: Duration, initial: ConfiguredMax) {
    log::info!(
        "work_finder: starting multi-workspace loop (interval={}s, configured_max={} \
         (source={}, re-read every tick, #9060), max_admissions_per_tick={} (source={}, \
         re-read every tick, #12090), max_concurrent_per_repo={} (re-read every tick, #9090), \
         dynamic cap = min(disk, ram, configured_max) — token axis is selection-only, \
         not a cap, since #5270; global across workspaces)",
        interval.as_secs(),
        initial.value,
        initial.source,
        initial.max_admissions_per_tick.0,
        initial.max_admissions_per_tick.1,
        initial
            .per_repo
            .map_or_else(|| "uncapped".to_string(), |c| c.to_string()),
    );
    if let Some(config) = initial.shadowed_config {
        log::warn!("work_finder: {}", shadowed_message(initial.value, config));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{DEFAULT_WORK_FINDER_MAX_CONCURRENT, WORK_FINDER_MAX_CONCURRENT_ENV};
    use super::*;
    use serial_test::serial;

    fn cm(value: usize, source: ConfigSource, shadowed_config: Option<usize>) -> ConfiguredMax {
        ConfiguredMax {
            value,
            source,
            shadowed_config,
            per_repo: None,
            max_admissions_per_tick: (
                super::super::DEFAULT_MAX_ADMISSIONS_PER_TICK,
                ConfigSource::Default,
            ),
        }
    }

    /// `cm` with a #9090 per-repo cap attached.
    fn cm_per_repo(value: usize, per_repo: Option<usize>) -> ConfiguredMax {
        ConfiguredMax {
            per_repo,
            ..cm(value, ConfigSource::Config, None)
        }
    }

    #[test]
    fn per_repo_cap_transitions_are_announced_independently() {
        let uncapped = cm_per_repo(6, None);
        let capped = cm_per_repo(6, Some(2));
        // The global ceiling did not move, so only the per-repo line fires.
        assert_eq!(transition_message(uncapped, capped), None);
        let msg = per_repo_transition_message(uncapped, capped).unwrap();
        assert!(msg.contains("maxConcurrentPerRepo uncapped -> 2"), "{msg}");
        // Stable thereafter, and clearing the key is announced too.
        assert_eq!(per_repo_transition_message(capped, capped), None);
        let cleared = per_repo_transition_message(capped, uncapped).unwrap();
        assert!(cleared.contains("uncapped"), "{cleared}");
    }

    #[test]
    #[serial]
    fn reloader_hot_applies_per_repo_cap_edits_between_ticks() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        std::env::remove_var(super::super::repo_cap::WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV);
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &max_config(6));
        let initial = ConfiguredMax::resolve(&read_work_finder_config(dir.path()));
        let mut reloader = ConfiguredMaxReloader::new(dir.path().to_path_buf(), initial);
        assert_eq!(reloader.per_repo(), None, "absent key must resolve to uncapped");

        write_config(
            dir.path(),
            r#"{"autonomous":{"workFinder":{"maxConcurrent":6,"maxConcurrentPerRepo":2}}}"#,
        );
        assert_eq!(reloader.refresh(), 6);
        assert_eq!(reloader.per_repo(), Some(2));

        // Zero is absent, never a cap of 0 (which would deadlock every repo).
        write_config(
            dir.path(),
            r#"{"autonomous":{"workFinder":{"maxConcurrent":6,"maxConcurrentPerRepo":0}}}"#,
        );
        assert_eq!(reloader.refresh(), 6);
        assert_eq!(reloader.per_repo(), None);
    }

    fn write_config(root: &std::path::Path, body: &str) {
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        std::fs::write(root.join(".loom/config.json"), body).unwrap();
    }

    fn max_config(n: usize) -> String {
        format!(r#"{{"autonomous":{{"workFinder":{{"maxConcurrent":{n}}}}}}}"#)
    }

    #[test]
    #[serial]
    fn resolve_reports_shadowed_config_only_under_env() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        let cfg = WorkFinderConfig {
            max_concurrent: Some(6),
            ..Default::default()
        };
        assert_eq!(ConfiguredMax::resolve(&cfg), cm(6, ConfigSource::Config, None));

        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
        assert_eq!(ConfiguredMax::resolve(&cfg), cm(2, ConfigSource::Env, Some(6)));
        // An env value equal to config hides nothing.
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "6");
        assert_eq!(ConfiguredMax::resolve(&cfg), cm(6, ConfigSource::Env, None));
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    }

    #[test]
    fn unchanged_ceiling_logs_nothing() {
        let same = cm(4, ConfigSource::Config, None);
        assert_eq!(transition_message(same, same), None);
        let shadowed = cm(2, ConfigSource::Env, Some(6));
        assert_eq!(transition_message(shadowed, shadowed), None);
    }

    #[test]
    fn value_change_is_announced_with_source() {
        let msg = transition_message(
            cm(2, ConfigSource::Config, None),
            cm(6, ConfigSource::Config, None),
        )
        .unwrap();
        assert!(msg.contains("configured_max 2 -> 6 (source=config)"), "{msg}");
    }

    #[test]
    fn newly_shadowed_config_edit_is_announced_once() {
        let before = cm(2, ConfigSource::Env, None);
        let after = cm(2, ConfigSource::Env, Some(6));
        let msg = transition_message(before, after).unwrap();
        assert!(msg.contains("maxConcurrent=6 is IGNORED"), "{msg}");
        assert!(msg.contains(WORK_FINDER_MAX_CONCURRENT_ENV), "{msg}");
        // Stable thereafter.
        assert_eq!(transition_message(after, after), None);
        // Removing the shadowed key is not itself worth a line.
        assert_eq!(transition_message(after, before), None);
    }

    #[test]
    #[serial]
    fn reloader_picks_up_config_edits_between_ticks() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &max_config(2));
        let initial = ConfiguredMax::resolve(&read_work_finder_config(dir.path()));
        let mut reloader = ConfiguredMaxReloader::new(dir.path().to_path_buf(), initial);
        assert_eq!(reloader.refresh(), 2);

        write_config(dir.path(), &max_config(6));
        assert_eq!(reloader.refresh(), 6);

        write_config(dir.path(), &max_config(1));
        assert_eq!(reloader.refresh(), 1);
    }

    /// #11742: the per-tick reload publishes `dispatch.weightBonus*` for the
    /// next draw, so a config edit hot-applies without a restart. (The
    /// published value is per thread under `cfg(test)`.)
    #[test]
    #[serial]
    fn reloader_publishes_the_weight_bonus() {
        use crate::priority_pick::WeightBonus;
        use crate::work_finder::workspace_draw::weight_bonus;
        let dir = tempfile::tempdir().unwrap();
        let initial = ConfiguredMax::resolve(&read_work_finder_config(dir.path()));
        let mut reloader = ConfiguredMaxReloader::new(dir.path().to_path_buf(), initial);
        reloader.refresh();
        assert_eq!(weight_bonus(), WeightBonus::default());

        write_config(
            dir.path(),
            r#"{"hyperparameters": {"dispatch": {"weightBonusRedMainFix": 7, "weightBonusImportant": 0}}}"#,
        );
        reloader.refresh();
        assert_eq!(
            weight_bonus(),
            WeightBonus {
                red_main_fix: 7,
                important: 0,
                ..WeightBonus::default()
            }
        );

        write_config(dir.path(), "{}");
        reloader.refresh();
        assert_eq!(weight_bonus(), WeightBonus::default());
    }

    #[test]
    #[serial]
    fn reloader_soft_fails_malformed_config_to_default_not_zero() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &max_config(6));
        let initial = ConfiguredMax::resolve(&read_work_finder_config(dir.path()));
        let mut reloader = ConfiguredMaxReloader::new(dir.path().to_path_buf(), initial);

        write_config(dir.path(), "{ half-written");
        assert_eq!(reloader.refresh(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
        write_config(dir.path(), &max_config(0));
        assert_eq!(reloader.refresh(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);

        write_config(dir.path(), &max_config(6));
        assert_eq!(reloader.refresh(), 6);
    }

    #[test]
    fn admissions_change_is_announced_once_with_source() {
        let base = cm(6, ConfigSource::Config, None);
        let raised = ConfiguredMax {
            max_admissions_per_tick: (8, ConfigSource::Config),
            ..base
        };
        let msg = admissions_transition_message(base, raised).unwrap();
        assert!(msg.contains("maxAdmissionsPerTick 3 -> 8 (source=config)"), "{msg}");
        assert_eq!(admissions_transition_message(raised, raised), None);
        // A ceiling-only change does not announce the admissions cap.
        assert_eq!(admissions_transition_message(base, cm(9, ConfigSource::Config, None)), None);
    }

    #[test]
    fn source_label_names_env_overridden_fields() {
        assert_eq!(source_label(&[("a", false), ("b", false)]), "config");
        assert_eq!(
            source_label(&[("a", true), ("b", false), ("c", true)]),
            "config, env overrides a,c"
        );
    }

    #[test]
    #[serial]
    fn reloader_keeps_env_override_over_config_edits() {
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &max_config(2));
        let initial = ConfiguredMax::resolve(&read_work_finder_config(dir.path()));
        let mut reloader = ConfiguredMaxReloader::new(dir.path().to_path_buf(), initial);

        write_config(dir.path(), &max_config(6));
        assert_eq!(reloader.refresh(), 2);
        assert_eq!(reloader.current, cm(2, ConfigSource::Env, Some(6)));
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    }
}
