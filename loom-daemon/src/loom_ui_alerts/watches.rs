//! The loom-ui alerts with no loop of their own to raise them (#11295),
//! started once by [`spawn_all`] after [`super::start`]:
//!
//! - `daemon.task-dead` ([`crate::task_liveness::loom_ui`]);
//! - `forge.ratelimit-breaker`: the rate-limit breaker has held forge polling
//!   off for over [`BREAKER_SUSTAIN`] (sampled every minute, so a release and
//!   immediate re-trip between two samples is one episode);
//! - `llm-monitor.stale`: llm-monitor's `ranking.json` is older than
//!   [`crate::tokens_pool::monitor_stale::STALE_AFTER_SECS`] (or unreadable)
//!   on two reads in a row;
//! - the start-time checks of `version.pause-failed` /
//!   `version.config-rejected` ([`crate::auto_update::pause_roll::loom_ui`]);
//! - resolving `install.*` left open by an earlier process when the install
//!   self-check is now off.

use std::path::Path;
use std::time::Duration;

use super::{Alert, Severity, State};

/// How long the breaker must hold before it is an alert.
pub const BREAKER_SUSTAIN: Duration = Duration::from_secs(15 * 60);
/// Rule for a long breaker cooldown.
pub const RULE_BREAKER: &str = "forge.ratelimit-breaker";
/// How often `ranking.json` is read.
pub const LLM_MONITOR_SAMPLE: Duration = Duration::from_secs(5 * 60);
/// Rule for a stale `ranking.json`.
pub const RULE_LLM_MONITOR_STALE: &str = "llm-monitor.stale";

/// The breaker sample: `Err(detail)` while it suppresses forge polling.
fn breaker_probe() -> Result<(), String> {
    match crate::rate_limit_breaker::global_snapshot() {
        Some(s) if s.suppressed => Err(format!(
            "Tripped by {} at {}; cooldown until {}. Every forge-polling loop skips its tick \
             (core remaining {}, graphql remaining {}).",
            s.source.as_deref().unwrap_or("unknown"),
            s.tripped_at
                .map_or_else(|| "?".to_string(), |t| t.to_rfc3339()),
            s.cooldown_until
                .map_or_else(|| "?".to_string(), |t| t.to_rfc3339()),
            s.core_remaining
                .map_or_else(|| "?".to_string(), |n| n.to_string()),
            s.graphql_remaining
                .map_or_else(|| "?".to_string(), |n| n.to_string()),
        )),
        _ => Ok(()),
    }
}

/// The breaker alert for `state`.
#[must_use]
pub fn breaker_alert(state: State, detail: Option<&String>) -> Alert {
    let title = match state {
        State::Open => {
            "The GitHub rate-limit breaker has held forge polling off for over 15 minutes"
        }
        State::Resolved => "The GitHub rate-limit breaker has released",
    };
    let alert = Alert::new(RULE_BREAKER, "github", title, Severity::Warning, state);
    match detail {
        Some(d) => alert.with_detail(d.clone()),
        None => alert,
    }
}

/// The `ranking.json` alert for `state`.
#[must_use]
pub fn llm_monitor_alert(state: State, detail: Option<&String>) -> Alert {
    let title = match state {
        State::Open => "llm-monitor's ranking.json is stale: token selection is flying blind",
        State::Resolved => "llm-monitor's ranking.json is fresh again",
    };
    let alert = Alert::new(RULE_LLM_MONITOR_STALE, "ranking.json", title, Severity::Warning, state);
    match detail {
        Some(d) => alert.with_detail(d.clone()),
        None => alert,
    }
}

/// Start every watch for the daemon at `root`.
pub fn spawn_all(root: &Path) {
    crate::task_liveness::loom_ui::spawn();
    super::sustain::spawn_watch(
        Duration::from_secs(60),
        BREAKER_SUSTAIN,
        breaker_probe,
        breaker_alert,
    );
    super::sustain::spawn_watch(
        LLM_MONITOR_SAMPLE,
        // Two bad reads in a row: one torn read mid-rewrite is not an alert.
        LLM_MONITOR_SAMPLE,
        || {
            crate::tokens_pool::monitor_stale::check(
                &crate::tokens_pool::monitor_dir::claude_monitor_dir(),
                chrono::Utc::now(),
            )
        },
        llm_monitor_alert,
    );
    crate::auto_update::pause_roll::loom_ui::startup(root);
    let self_check = crate::install_self_check::read_config(root);
    if !crate::install_self_check::resolve_enabled(&self_check) {
        crate::install_self_check::loom_ui::resolve_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_alerts_carry_their_rule_and_detail() {
        let a = breaker_alert(State::Open, Some(&"tripped by work_finder".to_string()));
        assert_eq!(a.rule, RULE_BREAKER);
        assert_eq!(a.detail.as_deref(), Some("tripped by work_finder"));
        let r = llm_monitor_alert(State::Resolved, None);
        assert_eq!(r.rule, RULE_LLM_MONITOR_STALE);
        assert_eq!(r.state, State::Resolved);
        assert!(r.detail.is_none());
    }

    #[test]
    fn no_breaker_registered_is_healthy() {
        // The unit-test binary registers no global breaker.
        assert!(breaker_probe().is_ok() || crate::rate_limit_breaker::global().is_some());
    }
}
