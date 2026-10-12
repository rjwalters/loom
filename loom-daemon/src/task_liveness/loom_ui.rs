//! `daemon.task-dead` on loom-ui (#11295), key `<task>`.
//!
//! Every [`SAMPLE`] the registry is read: a task past its staleness window,
//! or one that marked itself dead, is open; one that beats again, or is no
//! longer registered (a restarted daemon that runs it no more), is resolved.
//! The open set is a [`Ledger`], so a loop that died before a restart is
//! resolved by the new process once it is alive (or gone).
//!
//! loom-ui has no rule of its own on `loom.daemon.task_alive`, so this is the
//! one source for it.

use std::time::Duration;

use super::TaskLivenessEntry;
use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};

/// The rule.
pub const RULE: &str = "daemon.task-dead";
/// How often the registry is read.
pub const SAMPLE: Duration = Duration::from_secs(60);

/// The open alerts for `entries`.
#[must_use]
pub fn alerts(entries: &[TaskLivenessEntry]) -> Vec<Alert> {
    entries
        .iter()
        .filter(|e| !e.alive)
        .map(|e| {
            let why = e.dead_reason.as_deref().map_or_else(
                || {
                    format!(
                        "no beat for {}s (window {}s, interval {}s)",
                        e.silent_secs, e.stale_after_secs, e.interval_secs
                    )
                },
                |r| format!("it marked itself dead: {r}"),
            );
            Alert::new(
                RULE,
                &e.task,
                format!("loom-daemon loop `{}` has gone silent", e.task),
                Severity::Warning,
                State::Open,
            )
            .with_detail(format!(
                "The daemon is up but its `{}` loop is not running: {why}. Restart the daemon \
                 if it does not recover.",
                e.task
            ))
        })
        .collect()
}

/// Sample the global registry every [`SAMPLE`] for the daemon's lifetime.
pub fn spawn() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        let mut ledger = Ledger::load("task_liveness");
        loop {
            tokio::time::sleep(SAMPLE).await;
            ledger.reconcile(alerts(&super::snapshot()));
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(task: &str, alive: bool, dead_reason: Option<&str>) -> TaskLivenessEntry {
        TaskLivenessEntry {
            task: task.into(),
            alive,
            last_beat: None,
            silent_secs: 900,
            interval_secs: 300,
            stale_after_secs: 660,
            dead_reason: dead_reason.map(str::to_string),
        }
    }

    #[test]
    fn only_dead_tasks_alert_keyed_by_task() {
        let got = alerts(&[
            entry("auto_update", false, None),
            entry("role_runner.judge", true, None),
            entry("codex_session_watch", false, Some("exited")),
        ]);
        let keys: Vec<_> = got.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["auto_update", "codex_session_watch"]);
        assert!(got[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("no beat for 900s"));
        assert!(got[1].detail.as_deref().unwrap().contains("exited"));
        assert!(got.iter().all(|a| a.rule == RULE));
    }

    #[test]
    fn a_task_beating_again_resolves() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut ledger = Ledger::at(None, move |a: Alert| sink.lock().unwrap().push(a.state));
        ledger.reconcile(alerts(&[entry("auto_update", false, None)]));
        ledger.reconcile(alerts(&[entry("auto_update", true, None)]));
        assert_eq!(*seen.lock().unwrap(), [State::Open, State::Resolved]);
    }
}
