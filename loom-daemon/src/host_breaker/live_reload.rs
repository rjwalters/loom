//! Per-tick hot-reload of the host breaker's thresholds (#12090).
//!
//! The work-finder loop re-resolves `autonomous.hostBreaker` from the
//! effective config (fleet store included) every tick, beside the
//! `configured_max` reload, so a fleet-gitops edit takes effect on the next
//! tick rather than the next daemon restart. Env overrides still win and,
//! being process environment, are read only at daemon start. The disk-full
//! guard registered by `with_disk_guard` is env-resolved and unaffected.

use std::path::Path;

use super::{HostBreakerConfig, SharedHostBreaker};

// Allow expect_used: a poisoned config mutex is unrecoverable (see the parent).
#[allow(clippy::expect_used)]
impl SharedHostBreaker {
    /// Swap in a re-resolved config, returning the previous one when it
    /// changed. Runtime state (phase, over-threshold streak, cool-down) carries
    /// over; the next observation is judged against the new thresholds.
    pub fn set_config(&self, next: HostBreakerConfig) -> Option<HostBreakerConfig> {
        let mut current = self
            .config
            .lock()
            .expect("host breaker config mutex poisoned");
        let prev = std::mem::replace(&mut *current, next);
        (prev != next).then_some(prev)
    }
}

/// Re-resolve `repo_root`'s breaker config into `breaker`, returning the INFO
/// line to log when it changed (`None` on the common, unchanged tick).
#[must_use]
pub fn reload(breaker: &SharedHostBreaker, repo_root: &Path) -> Option<String> {
    let next = super::resolve_config_for(repo_root);
    let prev = breaker.set_config(next)?;
    let env = [
        ("enabled", super::env_enabled().is_some()),
        ("loadPerCoreTrip", super::env_load_per_core().is_some()),
        ("sustainTicks", super::env_sustain_ticks().is_some()),
        ("cooldownSecs", super::env_cooldown_secs().is_some()),
    ];
    Some(format!(
        "host_breaker: hostBreaker {prev:?} -> {next:?} (source={}) — hot-applied without a \
         daemon restart (#12090)",
        crate::work_finder::configured_max::source_label(&env)
    ))
}

/// [`reload`] into the process-global breaker, logging a change once at INFO.
/// A no-op when no breaker is registered.
pub fn global_reload(repo_root: &Path) {
    if let Some(msg) = super::GLOBAL.get().and_then(|b| reload(b, repo_root)) {
        log::info!("{msg}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn write(root: &Path, body: &str) {
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        std::fs::write(root.join(".loom/config.json"), body).unwrap();
    }

    #[test]
    fn a_config_edit_between_ticks_moves_the_trip_threshold() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{}");
        let breaker = SharedHostBreaker::new(super::super::resolve_config_for(dir.path()));
        assert_eq!(reload(&breaker, dir.path()), None, "unchanged config logs nothing");
        let now = chrono::Utc::now();
        // Below a raised trip threshold, a sustained high load never trips.
        write(
            dir.path(),
            r#"{"autonomous":{"hostBreaker":{"loadPerCoreTrip":50.0,"sustainTicks":1}}}"#,
        );
        let msg = reload(&breaker, dir.path()).unwrap();
        assert!(msg.contains("hot-applied") && msg.contains("50.0"), "{msg}");
        assert_eq!(reload(&breaker, dir.path()), None, "logged once, not every tick");
        assert!(breaker.observe(Some(10.0), now).is_none());
        assert!(!breaker.is_suppressed());
        // Lower it on the next tick: the same load now trips immediately.
        write(
            dir.path(),
            r#"{"autonomous":{"hostBreaker":{"loadPerCoreTrip":5.0,"sustainTicks":1}}}"#,
        );
        assert!(reload(&breaker, dir.path()).is_some());
        assert!(breaker.observe(Some(10.0), now).is_some());
        assert!(breaker.is_suppressed());
    }
}
