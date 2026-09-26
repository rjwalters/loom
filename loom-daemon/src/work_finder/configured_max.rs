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
    pub fn refresh(&mut self) -> usize {
        let next = ConfiguredMax::resolve(&read_work_finder_config(&self.root));
        if let Some(msg) = transition_message(self.current, next) {
            log::info!("work_finder: {msg}");
        }
        self.current = next;
        next.value
    }
}

/// The multi-workspace loop's one-time startup line (moved here from
/// `spawn_multi_work_finder_task`, which is at its file-size ratchet).
pub fn log_loop_start(interval: Duration, initial: ConfiguredMax, max_admissions_per_tick: usize) {
    log::info!(
        "work_finder: starting multi-workspace loop (interval={}s, configured_max={} \
         (source={}, re-read every tick, #9060), max_admissions_per_tick={max_admissions_per_tick}, \
         dynamic cap = min(disk, ram, configured_max) — token axis is selection-only, \
         not a cap, since #5270; global across workspaces)",
        interval.as_secs(),
        initial.value,
        initial.source,
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
        }
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
