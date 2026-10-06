//! Per-account Codex session-container state (#10455; Epic #10452).
//!
//! On 2026-10-05 a host lost Codex for ~21.5 h and the only trace was a
//! stream of ~1 s role failures attributed to (repo, role): a host-wide cause
//! read as many repo-level faults, and the role runner's per-root failure
//! machine demotes repeats to DEBUG. This module is the container-level
//! signal that machine cannot give:
//!
//! * the gauge `loom.codex_session.state{account,state,container}` — every
//!   state of the closed vocabulary is emitted each pass (current at 1, the
//!   rest at 0) so a transition never leaves two states active; and
//! * a WARN on every state change (including recovery) plus a bounded
//!   reminder while an account stays not-running.
//!
//! The WARN deliberately lives here, **not** in the role runner's
//! `RootTickLogAction` machine, and never goes through `log::debug!`: it is
//! per account, not per root, and must stay visible however long the
//! container is down. It runs on every collector pass whether or not an OTLP
//! exporter is configured; only the gauge needs the ops sink.
//!
//! Only session-managed, enabled Codex accounts are read; a pool with none
//! costs zero docker calls. [`Tracker::observe`] and [`points`] are pure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::tokens_pool::session_lifecycle::{container_name, is_session_managed};
use crate::tokens_pool::session_state::{self, SessionState};
use crate::tokens_pool::{account_inventory, AccountProvider};

/// How often the WARN repeats while an account's container stays not running.
pub const REMINDER_SECS: u64 = 15 * 60;

/// Per-account last-seen state and when it was last warned about.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<String, (SessionState, u64)>,
}

impl Tracker {
    /// Feed one observation. Returns the WARN text when this observation is a
    /// state change worth reporting or a due reminder; `None` otherwise (an
    /// account first seen running, or unchanged inside the reminder window).
    pub fn observe(
        &mut self,
        account: &str,
        container: &str,
        state: SessionState,
        now: u64,
    ) -> Option<String> {
        let previous = self.seen.get(account).map(|(s, _)| *s);
        let last_warned = self.seen.get(account).map_or(0, |(_, at)| *at);
        let message = match previous {
            None if state == SessionState::Running => None,
            Some(prev) if prev == state => (state != SessionState::Running
                && now.saturating_sub(last_warned) >= REMINDER_SECS)
                .then(|| format!("{} (still, reminder every {} min)", describe(account, container, state), REMINDER_SECS / 60)),
            Some(prev) if state == SessionState::Running => Some(format!(
                "Codex session container for account {account} ({container}) recovered: {} -> running",
                prev.as_str()
            )),
            _ => Some(describe(account, container, state)),
        };
        let warned_at = if message.is_some() { now } else { last_warned };
        self.seen.insert(account.to_string(), (state, warned_at));
        message
    }
}

fn describe(account: &str, container: &str, state: SessionState) -> String {
    match state {
        SessionState::Stopped | SessionState::Missing => format!(
            "Codex session container for account {account} ({container}) is {}: Codex ticks \
             on this account are refused until it is started \
             (`loom-daemon accounts session start {account} --mount-workspace <checkout parent>`)",
            state.as_str()
        ),
        SessionState::StaleMounts => format!(
            "Codex session container for account {account} ({container}) is running but does \
             not mount every registered workspace root (stale_mounts); recreate it with \
             `loom-daemon accounts session stop {account}` then `session start`"
        ),
        SessionState::Running => {
            format!("Codex session container for account {account} ({container}) is running")
        }
    }
}

/// The gauge points for `observed` `(account, container, state)` triples.
#[must_use]
pub fn points(observed: &[(String, String, SessionState)]) -> Vec<MetricPoint> {
    observed
        .iter()
        .flat_map(|(account, container, current)| {
            SessionState::ALL.into_iter().map(move |state| {
                MetricPoint::int(MetricName::CodexSessionState, i64::from(state == *current))
                    .label("account", account.clone())
                    .label("state", state.as_str())
                    .label("container", container.clone())
            })
        })
        .collect()
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read every session-managed Codex account's container, WARN per the tracker,
/// and return the observations.
fn sample(
    root: &Path,
    registered: &[PathBuf],
    docker: &str,
) -> Vec<(String, String, SessionState)> {
    let Ok(inventory) = account_inventory(root, AccountProvider::Codex) else {
        return Vec::new();
    };
    let mut observed = Vec::new();
    for account in inventory
        .into_iter()
        .filter(|a| a.enabled && is_session_managed(&a.credential_reference))
    {
        let container = container_name(&account.id.name);
        let state = session_state::read(docker, &container, registered);
        observed.push((account.id.name, container, state));
    }
    let mut guard = TRACKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tracker = guard.get_or_insert_with(Tracker::default);
    let now = now_secs();
    for (account, container, state) in &observed {
        if let Some(message) = tracker.observe(account, container, *state, now) {
            log::warn!("session: {message}");
        }
    }
    observed
}

/// One collector pass: sample, WARN, and export the gauge when an ops sink is
/// registered.
pub async fn record(root: &Path) {
    let root = root.to_path_buf();
    let observed = tokio::task::spawn_blocking(move || {
        let registered = crate::workspace_registry::WorkspaceRegistry::load_default()
            .map(|registry| registry.roots())
            .unwrap_or_default();
        let docker = std::env::var("LOOM_CODEX_SESSION_DOCKER")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "docker".to_string());
        sample(&root, &registered, &docker)
    })
    .await
    .unwrap_or_default();
    if observed.is_empty() {
        return;
    }
    super::emit_metrics(points(&observed));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const C: &str = "loom-codex-session-a";

    #[test]
    fn first_sight_running_is_silent() {
        let mut t = Tracker::default();
        assert!(t.observe("a", C, SessionState::Running, 0).is_none());
        assert!(t.observe("a", C, SessionState::Running, 10_000).is_none());
    }

    #[test]
    fn running_to_stopped_warns_naming_account_and_container() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        let m = t.observe("a", C, SessionState::Stopped, 300).unwrap();
        assert!(m.contains("account a") && m.contains(C) && m.contains("stopped"));
    }

    #[test]
    fn repeat_inside_the_window_is_quiet_and_after_it_warns_again() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        assert!(t.observe("a", C, SessionState::Missing, 100).is_some());
        assert!(t
            .observe("a", C, SessionState::Missing, 100 + REMINDER_SECS - 1)
            .is_none());
        assert!(t
            .observe("a", C, SessionState::Missing, 100 + REMINDER_SECS)
            .is_some());
        // the reminder re-arms from the reminder, not from the first WARN
        assert!(t
            .observe("a", C, SessionState::Missing, 100 + REMINDER_SECS + 5)
            .is_none());
    }

    #[test]
    fn recovery_is_reported() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        t.observe("a", C, SessionState::Stopped, 10);
        let m = t.observe("a", C, SessionState::Running, 20).unwrap();
        assert!(m.contains("recovered") && m.contains("stopped -> running"));
        assert!(t.observe("a", C, SessionState::Running, 30).is_none());
    }

    #[test]
    fn first_sight_down_warns_and_accounts_are_independent() {
        let mut t = Tracker::default();
        assert!(t.observe("a", C, SessionState::Stopped, 5).is_some());
        assert!(t
            .observe("b", "loom-codex-session-b", SessionState::Running, 5)
            .is_none());
        assert!(t
            .observe("a", C, SessionState::StaleMounts, 6)
            .unwrap()
            .contains("stale_mounts"));
    }

    #[test]
    fn gauge_emits_every_state_with_the_current_one_at_one() {
        let pts = points(&[("a".into(), C.into(), SessionState::Stopped)]);
        assert_eq!(pts.len(), 4);
        for p in &pts {
            let state = &p.labels["state"];
            let expect = i64::from(state == "stopped");
            assert_eq!(p.value, crate::telemetry::ops::MetricValue::Int(expect), "{state}");
            assert_eq!(p.labels["account"], "a");
            assert_eq!(p.labels["container"], C);
        }
    }

    #[test]
    fn the_warn_never_goes_through_a_debug_log() {
        let source = include_str!("codex_session.rs");
        let needle = ["log::", "debug!"].concat();
        assert!(!source
            .replace(&format!("\"{needle}\""), "")
            .contains(&format!("{needle}(")));
    }
}
