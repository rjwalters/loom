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
        }
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
    pub fn refresh(&mut self) -> usize {
        let next = ConfiguredMax::resolve(&read_work_finder_config(&self.root));
        if let Some(msg) = transition_message(self.current, next) {
            log::info!("work_finder: {msg}");
        }
        if let Some(msg) = per_repo_transition_message(self.current, next) {
            log::info!("work_finder: {msg}");
        }
        self.current = next;
        next.value
    }

    /// This tick's per-repo cap (#9090) — `None` for uncapped. Valid after
    /// [`Self::refresh`]; before the first refresh it is the startup value.
    #[must_use]
    pub fn per_repo(&self) -> Option<usize> {
        self.current.per_repo
    }
}

/// The multi-workspace loop's one-time startup line (moved here from
/// `spawn_multi_work_finder_task`, which is at its file-size ratchet).
pub fn log_loop_start(interval: Duration, initial: ConfiguredMax, max_admissions_per_tick: usize) {
    log::info!(
        "work_finder: starting multi-workspace loop (interval={}s, configured_max={} \
         (source={}, re-read every tick, #9060), max_admissions_per_tick={max_admissions_per_tick}, \
         max_concurrent_per_repo={} (re-read every tick, #9090), \
         dynamic cap = min(disk, ram, configured_max) — token axis is selection-only, \
         not a cap, since #5270; global across workspaces)",
        interval.as_secs(),
        initial.value,
        initial.source,
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
