//! A work finder that is OFF on a host whose config turns it on (#11934).
//!
//! The daemon resolves the work finder env > config > default
//! ([`crate::work_finder::resolve_enabled`]). So a `LOOM_WORK_FINDER=0` in the
//! process environment switches the loop off even where the host's resolved
//! config says `autonomous.workFinder.enabled: true`, which is how a fleet
//! dispatcher declares itself one. A plain `daemon-start` used to export that
//! `0` (the FLAGS-OFF default), and two dispatchers came back from a
//! `restart --pause --then-exit` cycle dispatching nothing. The daemon was up,
//! the run state read "dispatching normally", and the only trace was a
//! `status` warning nobody was reading. The hole was the absence of a signal.
//!
//! This module is that signal, evaluated once at startup from the values the
//! daemon actually acts on:
//!
//! - an `ERROR` in the daemon log naming the env value, the config value and
//!   the fix;
//! - a critical loom-ui alert (rule [`RULE`]), opened through a persisted
//!   [`Ledger`] so a restart that fixes it resolves it, and a restart that
//!   does not posts no second `open`;
//! - the verdict on the `daemon.start` telemetry record
//!   ([`crate::observability::daemon_lifecycle::start_record`]), so SigNoz can
//!   answer "which dispatcher came up with its work finder forced off?".
//!
//! A host that turns the work finder off in config (the fleet captain) is not
//! flagged: config and env agree there. A host with no config opinion is not
//! flagged either: FLAGS-OFF is its documented default (#3911).

use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;

use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};
use crate::work_finder::{self, WorkFinderConfig, WORK_FINDER_ENABLE_ENV};

/// The loom-ui alert rule.
pub const RULE: &str = "work_finder_forced_off";
/// The alert key: one incident per host (the source already names the host).
pub const KEY: &str = "daemon";
/// The ledger scope (`<loom_dir>/loom-ui-alerts/<scope>.json`).
pub const LEDGER_SCOPE: &str = "work-finder";

/// What the work finder resolved to at startup, and from what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// Whether the loop runs in this process.
    pub enabled: bool,
    /// The resolved config's `autonomous.workFinder.enabled`.
    pub config_enabled: Option<bool>,
    /// The process's `LOOM_WORK_FINDER`, verbatim (`None`: unset).
    pub env: Option<String>,
    /// The config enables the loop and it is off anyway: see [`Self::forced_off`].
    pub forced_off: bool,
}

impl Verdict {
    /// Build a verdict. `enabled` is passed in, not recomputed, so it is
    /// always the daemon's own [`work_finder::resolve_enabled`] answer.
    #[must_use]
    pub fn new(enabled: bool, config_enabled: Option<bool>, env: Option<String>) -> Self {
        Self {
            enabled,
            config_enabled,
            env,
            forced_off: !enabled && config_enabled == Some(true),
        }
    }

    /// The log line and alert detail for a forced-off verdict.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "work_finder: OFF on a host whose config enables it (autonomous.workFinder.enabled: \
             true) because {WORK_FINDER_ENABLE_ENV}={} in this daemon's environment overrides \
             config. This host dispatches NOTHING. Likely cause: a plain `loom-daemon-start.sh` \
             (or `loom-daemon daemon-start`) relaunch exported the FLAGS-OFF default \
             (rjwalters/loom#11934). Fix: loom-daemon-stop.sh, then loom-daemon-start.sh \
             --from-config (or --work-finder); check with `loom-daemon status --json | jq \
             .work_finder.enabled`.",
            self.env.as_deref().unwrap_or("<unset>")
        )
    }

    /// The loom-ui alert for this verdict, or `None` when there is nothing
    /// to alert on.
    #[must_use]
    pub fn alert(&self) -> Option<Alert> {
        self.forced_off.then(|| {
            Alert::new(
                RULE,
                KEY,
                "Work finder forced OFF on a configured dispatcher",
                Severity::Critical,
                State::Open,
            )
            .with_detail(self.message())
        })
    }
}

static STARTUP: OnceLock<Verdict> = OnceLock::new();

/// The verdict [`read_and_check`] recorded for this process, if it ran.
#[must_use]
pub fn startup_verdict() -> Option<&'static Verdict> {
    STARTUP.get()
}

/// Log the verdict and move the loom-ui incident to match it.
pub fn report(verdict: &Verdict, ledger: &mut Ledger) {
    if verdict.forced_off {
        log::error!("{}", verdict.message());
    }
    ledger.reconcile(verdict.alert().into_iter().collect());
}

/// Read the work-finder config for `root`, evaluate and report the startup
/// verdict, and return the config for the caller to use as before.
///
/// This replaces the daemon's bare `read_work_finder_config` call so the check
/// runs on the exact config the loop is started from.
#[must_use]
pub fn read_and_check(root: &Path) -> WorkFinderConfig {
    let config = work_finder::read_work_finder_config(root);
    let verdict = Verdict::new(
        work_finder::resolve_enabled(&config),
        config.enabled,
        std::env::var(WORK_FINDER_ENABLE_ENV).ok(),
    );
    report(&verdict, &mut Ledger::load(LEDGER_SCOPE));
    let _ = STARTUP.set(verdict);
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn capture() -> (Ledger, Arc<Mutex<Vec<Alert>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let ledger = Ledger::at(None, move |a| sink.lock().expect("lock").push(a));
        (ledger, seen)
    }

    #[test]
    fn an_env_zero_over_config_enabled_is_forced_off() {
        // The #11934 state: unit/process env 0, fleet config enabled.
        let v = Verdict::new(false, Some(true), Some("0".into()));
        assert!(v.forced_off);
        let m = v.message();
        assert!(m.contains("LOOM_WORK_FINDER=0"), "{m}");
        assert!(m.contains("--from-config"), "{m}");
        assert!(m.contains(".work_finder.enabled"), "names the post-roll check: {m}");
    }

    #[test]
    fn agreeing_config_and_env_are_not_flagged() {
        // The captain: config off, env off — deliberate, not a downgrade.
        assert!(!Verdict::new(false, Some(false), Some("0".into())).forced_off);
        // No config opinion: FLAGS-OFF is the documented default (#3911).
        assert!(!Verdict::new(false, None, None).forced_off);
        // Running: nothing to say.
        assert!(!Verdict::new(true, Some(true), None).forced_off);
        assert!(!Verdict::new(true, Some(false), Some("1".into())).forced_off);
    }

    #[test]
    fn a_forced_off_start_opens_one_critical_alert() {
        let (mut ledger, seen) = capture();
        let v = Verdict::new(false, Some(true), Some("0".into()));
        report(&v, &mut ledger);
        report(&v, &mut ledger);
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 1, "a still-forced-off restart posts no second open");
        assert_eq!(seen[0].rule, RULE);
        assert_eq!(seen[0].severity, Severity::Critical);
        assert_eq!(seen[0].state, State::Open);
    }

    #[test]
    fn a_fixed_start_resolves_the_open_alert() {
        let (mut ledger, seen) = capture();
        report(&Verdict::new(false, Some(true), Some("0".into())), &mut ledger);
        report(&Verdict::new(true, Some(true), None), &mut ledger);
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].state, State::Resolved);
        assert!(!ledger.is_open(RULE, KEY));
    }

    #[test]
    fn a_healthy_start_with_nothing_open_posts_nothing() {
        let (mut ledger, seen) = capture();
        report(&Verdict::new(true, Some(true), None), &mut ledger);
        assert!(seen.lock().expect("lock").is_empty());
    }

    #[test]
    fn the_verdict_serializes_for_the_start_record() {
        let v = Verdict::new(false, Some(true), Some("0".into()));
        let j = serde_json::to_value(&v).expect("json");
        assert_eq!(j["enabled"], false);
        assert_eq!(j["config_enabled"], true);
        assert_eq!(j["env"], "0");
        assert_eq!(j["forced_off"], true);
    }
}
