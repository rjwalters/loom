//! Per-tick hot-reload of the saturation brake's thresholds (#12090).
//!
//! The work-finder loop re-resolves `autonomous.workFinder.saturationBrake`
//! from the effective config (fleet store included) every tick, beside the
//! `configured_max` reload, so a fleet-gitops edit takes effect on the next
//! tick rather than the next daemon restart. Env overrides still win and,
//! being process environment, are read only at daemon start.

use std::path::Path;

use super::{AdmissionBrakeConfig, SharedAdmissionBrake};

// Allow expect_used: a poisoned config mutex is unrecoverable (see the parent).
#[allow(clippy::expect_used)]
impl SharedAdmissionBrake {
    /// Swap in a re-resolved config, returning the previous one when it
    /// changed. Runtime state (sustain streak, hold, starvation clock) carries
    /// over; the next observation is judged against the new thresholds.
    pub fn set_config(&self, next: AdmissionBrakeConfig) -> Option<AdmissionBrakeConfig> {
        let mut current = self
            .config
            .lock()
            .expect("admission brake config mutex poisoned");
        let prev = std::mem::replace(&mut *current, next);
        (prev != next).then_some(prev)
    }
}

/// Re-resolve `repo_root`'s brake config into `brake`, returning the INFO line
/// to log when it changed (`None` on the common, unchanged tick).
#[must_use]
pub fn reload(brake: &SharedAdmissionBrake, repo_root: &Path) -> Option<String> {
    let next = super::resolve_config_for(repo_root);
    let prev = brake.set_config(next)?;
    let env = [
        ("enabled", super::env_enabled().is_some()),
        ("loadPerCoreHold", super::env_load_per_core().is_some()),
        ("cpuBusyHold", super::env_cpu_busy().is_some()),
        ("sustainTicks", super::env_sustain_ticks().is_some()),
        ("starvationWarnSecs", super::env_starvation_warn_secs().is_some()),
        ("starvationEscapeSecs", super::env_starvation_escape_secs().is_some()),
    ];
    Some(format!(
        "admission_brake: saturationBrake {prev:?} -> {next:?} (source={}) — hot-applied \
         without a daemon restart (#12090)",
        crate::work_finder::configured_max::source_label(&env)
    ))
}

/// [`reload`] into the process-global brake, logging a change once at INFO.
/// A no-op when no brake is registered.
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
    fn a_config_edit_between_ticks_moves_the_hold_threshold() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{}");
        let brake = SharedAdmissionBrake::new(super::super::resolve_config_for(dir.path()));
        assert_eq!(reload(&brake, dir.path()), None, "unchanged config logs nothing");

        write(
            dir.path(),
            r#"{"autonomous":{"workFinder":{"saturationBrake":{"loadPerCoreHold":2.5,"sustainTicks":7}}}}"#,
        );
        let msg = reload(&brake, dir.path()).unwrap();
        assert!(msg.contains("hot-applied") && msg.contains("2.5"), "{msg}");
        assert_eq!(brake.config().load_per_core_threshold, 2.5);
        assert_eq!(brake.config().sustain_ticks, 7);
        assert_eq!(reload(&brake, dir.path()), None, "logged once, not every tick");

        write(
            dir.path(),
            r#"{"autonomous":{"workFinder":{"saturationBrake":{"enabled":false}}}}"#,
        );
        assert!(reload(&brake, dir.path()).is_some());
        assert!(!brake.config().enabled);
        assert!(!brake.is_holding(), "a disabled brake never holds");
    }
}
