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
//! container is down.
//!
//! **Two halves, so the WARN does not depend on telemetry.** [`spawn_watch`]
//! is an always-on daemon task (started with the other observers, whether or
//! not any exporter is configured) that every [`WATCH_INTERVAL`] takes one
//! bounded [`session_state::snapshot`], publishes it for other readers
//! ([`session_state::latest`]), feeds the tracker and keeps the observations.
//! [`record`], called from the telemetry collector's pass, only turns the
//! newest observations into gauge points: it never calls docker, so a wedged
//! Docker cannot stall the collector.
//!
//! **Fail open, but loudly.** When docker cannot be queried
//! ([`Snapshot::Unavailable`]) the tracker holds every account's last state
//! and the gauge emits nothing: an unqueryable docker says nothing about any
//! container, so it must never read as `missing`. It is still a host-wide
//! Codex outage for a host that has session-managed accounts (spawn refuses
//! those ticks as `SESSION_DOWN`), so it gets its own WARN on entry, the same
//! [`REMINDER_SECS`] reminder while it lasts, and a WARN when docker answers
//! again.
//!
//! Passes never overlap: a tick is skipped while the previous pass is still
//! running, and the tracker lock is taken only after the snapshot, to fold
//! it.
//!
//! Only session-managed, enabled Codex accounts are read; a pool with none
//! costs zero docker calls. [`Tracker::observe`] and [`points`] are pure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::tokens_pool::session_lifecycle::{container_name, is_session_managed};
use crate::tokens_pool::session_state::{self, SessionState, Snapshot};
use crate::tokens_pool::{account_inventory, AccountProvider};

/// How often the WARN repeats while an account's container stays not running.
pub const REMINDER_SECS: u64 = 15 * 60;

/// How often the watch takes a snapshot: the WARN lands within one interval.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(60);

/// Observations older than this are not exported (the watch has stopped or
/// docker has been unqueryable since).
const FRESH_FOR: Duration = Duration::from_secs(3 * 60);

/// Per-account last-seen state and when it was last warned about.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<String, (SessionState, u64)>,
    /// While docker is unqueryable: when that was last warned about.
    unqueryable_warned: Option<u64>,
}

impl Tracker {
    /// Docker could not be queried this pass. Every account's state is held.
    /// Returns the WARN text on entering this condition and as a reminder
    /// every [`REMINDER_SECS`] while it lasts.
    pub fn unqueryable(&mut self, reason: &str, now: u64) -> Option<String> {
        let message = match self.unqueryable_warned {
            None => format!(
                "Codex session containers cannot be observed ({reason}): Codex ticks on this \
                 host's session-managed accounts will be refused until docker answers; is the \
                 Docker daemon running?"
            ),
            Some(at) if now.saturating_sub(at) >= REMINDER_SECS => format!(
                "Codex session containers still cannot be observed ({reason}; reminder every {} \
                 min)",
                REMINDER_SECS / 60
            ),
            Some(_) => return None,
        };
        self.unqueryable_warned = Some(now);
        Some(message)
    }

    /// Docker answered this pass; the WARN text when it had not before.
    pub fn queryable(&mut self) -> Option<String> {
        self.unqueryable_warned
            .take()
            .map(|_| "Codex session containers are observable again (docker answers)".to_string())
    }

    /// Forget accounts no longer in the inventory (removed or disabled), so a
    /// re-added account is treated as first sight.
    pub fn retain(&mut self, accounts: &[&str]) {
        self.seen
            .retain(|account, _| accounts.contains(&account.as_str()));
    }

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
        SessionState::Restarting => format!(
            "Codex session container for account {account} ({container}) is restarting \
             (Docker is backing off a crash loop): Codex ticks on this account are refused; \
             see `docker logs {container}`"
        ),
        SessionState::Stopped | SessionState::Missing => format!(
            "Codex session container for account {account} ({container}) is {}: Codex ticks \
             on this account are refused until it is started \
             (`loom-daemon accounts session start {account} --mount-workspace <checkout parent>`)",
            state.as_str()
        ),
        SessionState::StaleMounts => format!(
            "Codex session container for account {account} ({container}) is running but its \
             workspace mounts no longer match the registry (stale_mounts: a registered root is \
             not mounted, or a deregistered one still is, #10364); when the session reconciler is \
             enabled it recreates the container once idle, otherwise recreate it by hand: \
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

type Observations = Vec<(String, String, SessionState)>;

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);
static LAST: Mutex<Option<(Instant, Arc<Observations>)>> = Mutex::new(None);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What one pass decided: the observations to export (`None`: export
/// nothing) and the WARN lines.
#[derive(Debug, Default, PartialEq)]
pub struct Pass {
    pub observed: Option<Observations>,
    pub warn: Vec<String>,
}

/// Fold one snapshot into the tracker for `accounts` (`(account, container)`).
/// Pure apart from the tracker.
pub fn fold(
    tracker: &mut Tracker,
    accounts: &[(String, String)],
    snapshot: &Snapshot,
    now: u64,
) -> Pass {
    let names: Vec<&str> = accounts.iter().map(|(a, _)| a.as_str()).collect();
    tracker.retain(&names);
    let mut pass = Pass::default();
    if let Snapshot::Unavailable(reason) = snapshot {
        if !accounts.is_empty() {
            pass.warn.extend(tracker.unqueryable(reason, now));
        }
        return pass;
    }
    pass.warn.extend(tracker.queryable());
    let observed: Observations = accounts
        .iter()
        .map(|(account, container)| {
            let state = snapshot
                .state_of(container)
                .unwrap_or(SessionState::Missing);
            (account.clone(), container.clone(), state)
        })
        .collect();
    for (account, container, state) in &observed {
        pass.warn
            .extend(tracker.observe(account, container, *state, now));
    }
    pass.observed = Some(observed);
    pass
}

/// The enabled, session-managed Codex accounts and their container names.
fn session_accounts(root: &Path) -> Vec<(String, String)> {
    account_inventory(root, AccountProvider::Codex)
        .map(|inventory| {
            inventory
                .into_iter()
                .filter(|a| a.enabled && is_session_managed(&a.credential_reference))
                .map(|a| {
                    let container = container_name(&a.id.name);
                    (a.id.name, container)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Set while a watch pass runs, so a slow pass is never joined by another.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Clears [`IN_FLIGHT`] when the pass ends, however it ends.
struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// One watch pass (blocking): snapshot, then fold it under the tracker lock,
/// WARN, and keep the observations.
fn watch_pass(root: &Path) {
    let accounts = session_accounts(root);
    if accounts.is_empty() {
        TRACKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(Tracker::default)
            .retain(&[]);
        *LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        return;
    }
    let registered: Vec<PathBuf> = crate::workspace_registry::WorkspaceRegistry::load_default()
        .map(|registry| registry.roots())
        .unwrap_or_default();
    let docker = std::env::var("LOOM_CODEX_SESSION_DOCKER")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "docker".to_string());
    // The docker call runs without the tracker lock held.
    let started = Instant::now();
    let snapshot =
        Arc::new(session_state::snapshot(&docker, &registered, session_state::SNAPSHOT_DEADLINE));
    session_state::publish(Arc::clone(&snapshot), started);
    let pass = {
        let mut guard = TRACKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fold(guard.get_or_insert_with(Tracker::default), &accounts, &snapshot, now_secs())
    };
    for message in &pass.warn {
        log::warn!("session: {message}");
    }
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        pass.observed.map(|observed| (started, Arc::new(observed)));
}

/// Start the always-on watch for `workspace_root`'s account pool. Runs
/// whether or not telemetry is configured; each pass runs off the async
/// runtime, its docker calls are killed at the snapshot deadline, and a tick
/// is skipped while the previous pass is still running.
pub fn spawn_watch(workspace_root: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(WATCH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if IN_FLIGHT.swap(true, Ordering::AcqRel) {
                log::info!(
                    "session: previous Codex session watch pass still running; tick skipped"
                );
                continue;
            }
            let root = workspace_root.clone();
            let pass = tokio::task::spawn_blocking(move || {
                let _in_flight = InFlight;
                watch_pass(&root);
            });
            // This only stops *waiting*: a blocking pass cannot be cancelled.
            // `IN_FLIGHT` is what keeps an overrunning pass from being joined
            // by more.
            let bound = session_state::SNAPSHOT_DEADLINE + Duration::from_secs(10);
            if tokio::time::timeout(bound, pass).await.is_err() {
                log::info!("session: Codex session watch pass overran {bound:?}; not waiting");
            }
        }
    })
}

/// The collector's part: export the gauge from the watch's newest
/// observations, when the ops sink is registered. Never calls docker.
pub fn record() {
    let observed = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(at, _)| at.elapsed() <= FRESH_FOR)
        .map(|(_, observed)| Arc::clone(observed));
    if let Some(observed) = observed.filter(|o| !o.is_empty()) {
        super::emit_metrics(points(&observed));
    }
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
        assert_eq!(pts.len(), SessionState::ALL.len());
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

    fn available(states: &[(&str, SessionState)]) -> Snapshot {
        Snapshot::Available(
            states
                .iter()
                .map(|(c, state)| {
                    (
                        (*c).to_string(),
                        session_state::Observed {
                            state: *state,
                            inspect: serde_json::Value::Null,
                        },
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn restarting_warns_as_down() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        let m = t.observe("a", C, SessionState::Restarting, 60).unwrap();
        assert!(m.contains("restarting") && m.contains(C));
    }

    #[test]
    fn an_unqueryable_docker_holds_state_and_exports_nothing() {
        let mut t = Tracker::default();
        let accounts = vec![("a".to_string(), C.to_string())];
        let up = fold(&mut t, &accounts, &available(&[(C, SessionState::Running)]), 0);
        assert_eq!(up.observed.unwrap()[0].2, SessionState::Running);
        let gone = Snapshot::Unavailable("Cannot connect to the Docker daemon".into());
        let first = fold(&mut t, &accounts, &gone, 60);
        assert_eq!(first.observed, None, "no gauge, never `missing`");
        assert_eq!(first.warn.len(), 1, "a host-wide outage WARNs on entry");
        assert!(first.warn[0].contains("cannot be observed"));
        let quiet = fold(&mut t, &accounts, &gone, 60 + REMINDER_SECS - 1);
        assert!(quiet.warn.is_empty(), "no storm inside the reminder window");
        let reminder = fold(&mut t, &accounts, &gone, 60 + REMINDER_SECS);
        assert_eq!(reminder.warn.len(), 1, "bounded reminder");
        assert!(reminder.warn[0].contains("still"));
        // Docker answers again and the container is still up: no container
        // state change, only the "observable again" line.
        let back = fold(&mut t, &accounts, &available(&[(C, SessionState::Running)]), 2_000);
        assert_eq!(back.warn.len(), 1);
        assert!(back.warn[0].contains("observable again"));
        let steady = fold(&mut t, &accounts, &available(&[(C, SessionState::Running)]), 2_060);
        assert!(steady.warn.is_empty());
    }

    #[test]
    fn an_unqueryable_docker_with_no_session_accounts_is_silent() {
        let mut t = Tracker::default();
        let gone = Snapshot::Unavailable("Cannot connect".into());
        assert!(fold(&mut t, &[], &gone, 0).warn.is_empty());
    }

    #[test]
    fn the_selection_ceiling_is_two_watch_intervals() {
        assert_eq!(session_state::LATEST_MAX_AGE, 2 * WATCH_INTERVAL);
    }

    #[test]
    fn an_account_absent_from_an_available_snapshot_is_missing() {
        let mut t = Tracker::default();
        let accounts = vec![("a".to_string(), C.to_string())];
        let pass = fold(&mut t, &accounts, &available(&[]), 0);
        assert_eq!(pass.observed.unwrap()[0].2, SessionState::Missing);
        assert_eq!(pass.warn.len(), 1);
    }

    #[test]
    fn accounts_that_leave_the_inventory_are_forgotten() {
        let mut t = Tracker::default();
        let a = vec![("a".to_string(), C.to_string())];
        fold(&mut t, &a, &available(&[(C, SessionState::Stopped)]), 0);
        fold(&mut t, &[], &available(&[]), 10);
        // Re-added and still stopped: first sight again, so it warns at once.
        let pass = fold(&mut t, &a, &available(&[(C, SessionState::Stopped)]), 20);
        assert_eq!(pass.warn.len(), 1);
    }
}
