//! Which supervisor to ask about liveness, and under what name.
//!
//! The precedence is "environment wins over marker", with one deliberate
//! asymmetry the shell is explicit about: `LOOM_DAEMON_LAUNCHD` is
//! **one-directional**. A falsy value forces the pid-file path even when the
//! marker recorded `use_launchd=true`; a truthy value does nothing. The env var
//! can never force launchd *on*, because a host without launchd cannot be
//! talked into having it.

use super::consts::{DEFAULT_LAUNCHD_LABEL, DEFAULT_SYSTEMD_UNIT};
use super::env;

/// The supervisor a tick will interrogate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supervisor {
    pub use_launchd: bool,
    pub label: String,
    pub use_systemd: bool,
    pub systemd_unit: String,
}

/// Resolve for the **marker-absent** path (section 1/1b), where there is no
/// marker to defer to and every value comes from the environment or a default.
#[must_use]
pub fn resolve_without_marker(is_darwin: bool, systemctl_available: bool) -> Supervisor {
    // One-directional: only a falsy value does anything.
    let forced_off = env::var("LOOM_DAEMON_LAUNCHD").is_some_and(|v| env::is_false(&v));
    let use_launchd = !forced_off && is_darwin;

    // systemd is probed only where launchd is not in play AND the host is not
    // Darwin AND the operator opted in AND systemctl actually exists. All four,
    // because a systemd probe on a host without systemd reports "not loaded",
    // which is indistinguishable from a real outage.
    let use_systemd = !use_launchd
        && !is_darwin
        && env::var("LOOM_WATCHDOG_SYSTEMD_PROBE").is_some_and(|v| env::is_true(&v))
        && systemctl_available;

    Supervisor {
        use_launchd,
        label: env::var("LOOM_LAUNCHD_LABEL").unwrap_or_else(|| DEFAULT_LAUNCHD_LABEL.to_string()),
        use_systemd,
        systemd_unit: env::var("LOOM_SYSTEMD_UNIT")
            .unwrap_or_else(|| DEFAULT_SYSTEMD_UNIT.to_string()),
    }
}

/// Whether `systemctl` is on PATH — the shell's `command -v systemctl`.
#[must_use]
pub fn systemctl_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("systemctl").is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-dependent cases are exercised through the pure arguments; the two
    /// env knobs are read directly, so these cover the platform logic only.
    #[test]
    fn launchd_is_never_used_off_darwin() {
        let s = resolve_without_marker(false, false);
        assert!(!s.use_launchd, "a non-Darwin host has no launchd to ask");
    }

    #[test]
    fn systemd_needs_systemctl_to_actually_exist() {
        // Opting in on a host without systemctl must NOT enable the probe: a
        // "unit not loaded" answer from a host with no systemd reads exactly
        // like a real outage.
        let s = resolve_without_marker(false, false);
        assert!(!s.use_systemd);
    }

    #[test]
    fn defaults_are_the_documented_names() {
        let s = resolve_without_marker(false, false);
        assert_eq!(s.label, DEFAULT_LAUNCHD_LABEL);
        assert_eq!(s.systemd_unit, DEFAULT_SYSTEMD_UNIT);
    }
}
