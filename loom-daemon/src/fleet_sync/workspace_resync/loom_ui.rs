//! Workspace-resync failures on loom-ui's Alerts page (#11295): rule
//! `resync.<kind>`, the same kinds as the [`super::ALERT_TOPIC`] events.
//!
//! | Rule | Key | Open while | Resolved by |
//! |---|---|---|---|
//! | `resync.failure` | the repo | [`ALERT_AFTER`] or more failures in a row | a resync, or the repo reaching W0 |
//! | `resync.repo-access` | the repo | [`ALERT_AFTER`] or more refusals in a row | the repo read again (and needing no resync), or a resync |
//! | `resync.branch-protection` | the repo | a protection rule refused the push | a later resync, or W0 |
//! | `resync.network` | `workspaces` | the outage alert stands | every remote answering again |
//! | `resync.head-query` | `workspaces` | the head-query alert stands | a pass whose query works |
//! | `resync.pass-stuck` | `workspaces` | a pass has run for [`super::STUCK_AFTER_TICKS`] ticks | the next pass that finishes |
//!
//! Read from the pass memory after every finished pass (level, not edge), and
//! kept in a [`Ledger`]. The failure counts are process memory, so a restarted
//! daemon starts each one again from zero: an incident the ledger holds stays
//! open while its condition is still counting (any failure of that kind on
//! the repo, a failing pass of the host-level kind) or the repo could not be
//! classified this pass, and resolves only on a pass that shows it healthy, or
//! once the repo is unregistered. So a roll never resolves and re-opens a
//! standing incident.
//! `resync-loop` is not sent: it is a one-off refusal per version, reported on
//! the event bus, with no recovery edge to resolve it by.

use std::sync::{Mutex, OnceLock};

use super::memory::FailureKind;
use super::{Memory, WState, WorkspacePass, ALERT_AFTER};
use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};

/// Key of the host-level kinds.
pub const HOST_KEY: &str = "workspaces";

fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::load("workspace_resync")))
}

fn alert(kind: &str, key: &str, title: String, detail: String) -> Alert {
    Alert::new(format!("resync.{kind}"), key, title, Severity::Warning, State::Open)
        .with_detail(detail)
}

/// What stands after `pass`, read from `memory`. `held` says whether the
/// ledger has `(rule, key)` open already: such an incident needs only a
/// failure still counting, not the alert threshold.
#[must_use]
pub(super) fn alerts(
    memory: &Memory,
    pass: &WorkspacePass,
    held: &dyn Fn(&str, &str) -> bool,
) -> Vec<Alert> {
    let mut out = Vec::new();
    for report in &pass.workspaces {
        let key = report
            .repo
            .clone()
            .unwrap_or_else(|| report.root.display().to_string());
        let Some(b) = memory.backoff.get(&report.root) else {
            if report.state == WState::Unknown {
                // Not classified this pass: no evidence either way.
                for kind in ["failure", "repo-access", "branch-protection"] {
                    if held(&format!("resync.{kind}"), &key) {
                        out.push(alert(
                            kind,
                            &key,
                            format!("Loom resync of {key} is failing ({kind})"),
                            "Not checked again yet.".to_string(),
                        ));
                    }
                }
            }
            continue;
        };
        let kind = match b.kind {
            FailureKind::Protected => "branch-protection",
            FailureKind::Refused => "repo-access",
            FailureKind::Other => "failure",
            FailureKind::Unreachable => continue,
        };
        let protected = b.kind == FailureKind::Protected;
        if !(protected || b.failures >= ALERT_AFTER || held(&format!("resync.{kind}"), &key)) {
            continue;
        }
        out.push(alert(
            kind,
            &key,
            format!("Loom resync of {key} is failing ({kind})"),
            format!(
                "{} failure(s) in a row; next attempt {}: {}",
                b.failures,
                b.next_attempt.to_rfc3339(),
                b.last_error
            ),
        ));
    }
    let ((network, outage), (head_query, query_failing)) = memory.host_conditions();
    if network || (outage && held("resync.network", HOST_KEY)) {
        out.push(alert(
            "network",
            HOST_KEY,
            "Workspace remotes are not answering this host".to_string(),
            "The workspace resync cannot reach the repos' remotes; see `loom-daemon status`."
                .to_string(),
        ));
    }
    if head_query || (query_failing && held("resync.head-query", HOST_KEY)) {
        out.push(alert(
            "head-query",
            HOST_KEY,
            "The batched default-branch head query keeps failing".to_string(),
            "Each remote is asked with git ls-remote instead.".to_string(),
        ));
    }
    out
}

/// Fold `pass` into `ledger`.
pub(super) fn fold(ledger: &mut Ledger, memory: &Memory, pass: &WorkspacePass) {
    let current = alerts(memory, pass, &|rule, key| ledger.is_open(rule, key));
    ledger.reconcile(current);
}

/// Fold a finished pass in. A pass finished, so none is stuck.
pub(super) fn after_pass(memory: &Memory, pass: &WorkspacePass) {
    fold(
        &mut ledger()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        memory,
        pass,
    );
}

/// A pass has not ended for `ticks` ticks.
pub(super) fn stuck(ticks: u32, detail: &str) {
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .open(alert(
            "pass-stuck",
            HOST_KEY,
            format!("A workspace resync pass has not ended in {ticks} ticks"),
            detail.to_string(),
        ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::memory::Backoff;
    use super::super::report_for;
    use super::*;
    use std::path::PathBuf;

    fn pass_with(root: &str, repo: Option<&str>) -> WorkspacePass {
        let mut report = report_for(&PathBuf::from(root));
        report.repo = repo.map(str::to_string);
        WorkspacePass {
            workspaces: vec![report],
            ..WorkspacePass::default()
        }
    }

    fn backoff(kind: FailureKind, failures: u32) -> Backoff {
        Backoff {
            failures,
            next_attempt: chrono::Utc::now(),
            state: WState::W1,
            last_error: "push refused".into(),
            installed: None,
            requires_daemon: None,
            refusal: None,
            kind,
        }
    }

    fn rules(memory: &Memory, pass: &WorkspacePass) -> Vec<(String, String)> {
        alerts(memory, pass, &|_, _| false)
            .into_iter()
            .map(|a| (a.rule, a.key))
            .collect()
    }

    #[test]
    fn a_repo_alerts_by_kind_from_the_third_failure() {
        let pass = pass_with("/w/a", Some("o/a"));
        let mut memory = Memory::default();
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Other, ALERT_AFTER - 1));
        assert!(rules(&memory, &pass).is_empty());
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Other, ALERT_AFTER));
        assert_eq!(rules(&memory, &pass), [("resync.failure".into(), "o/a".into())]);
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Refused, ALERT_AFTER));
        assert_eq!(rules(&memory, &pass), [("resync.repo-access".into(), "o/a".into())]);
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Protected, 1));
        assert_eq!(rules(&memory, &pass), [("resync.branch-protection".into(), "o/a".into())]);
        // An unreachable remote is the host's outage, never the repo's alert.
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Unreachable, 9));
        assert!(rules(&memory, &pass).is_empty());
    }

    #[test]
    fn a_repo_without_a_github_name_is_keyed_by_its_root() {
        let pass = pass_with("/w/b", None);
        let mut memory = Memory::default();
        memory
            .backoff
            .insert("/w/b".into(), backoff(FailureKind::Other, ALERT_AFTER));
        assert_eq!(rules(&memory, &pass), [("resync.failure".into(), "/w/b".into())]);
    }

    #[test]
    fn a_cleared_backoff_resolves_through_the_ledger() {
        let pass = pass_with("/w/a", Some("o/a"));
        let mut memory = Memory::default();
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Other, ALERT_AFTER));
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut l = Ledger::at(None, move |a: Alert| sink.lock().unwrap().push(a.state));
        fold(&mut l, &memory, &pass);
        // Unclassified this pass (state unknown): no evidence, still open.
        memory.backoff.clear();
        fold(&mut l, &memory, &pass);
        assert_eq!(*seen.lock().unwrap(), [State::Open]);
        // Classified healthy: resolved.
        let mut healthy = pass.clone();
        healthy.workspaces[0].state = WState::W0;
        fold(&mut l, &memory, &healthy);
        assert_eq!(*seen.lock().unwrap(), [State::Open, State::Resolved]);
    }

    /// The counts are process memory: after a restart a repo still refused,
    /// and an outage still going, count from one again. What the previous
    /// process opened must stay open through that, not resolve and re-open.
    #[test]
    fn incidents_inherited_from_a_restart_stay_open_while_still_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace_resync.json");
        let pass = pass_with("/w/a", Some("o/a"));
        let now = chrono::Utc::now();
        {
            let mut memory = Memory::default();
            memory
                .backoff
                .insert("/w/a".into(), backoff(FailureKind::Refused, ALERT_AFTER));
            for _ in 0..ALERT_AFTER {
                memory.note_network(1, 1, Some("timeout"), std::time::Duration::from_secs(60), now);
            }
            let mut l = Ledger::at(Some(path.clone()), |_| {});
            fold(&mut l, &memory, &pass);
            assert_eq!(l.open_incidents().len(), 2, "{l:?}");
        }
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut l = Ledger::at(Some(path), move |a: Alert| {
            sink.lock().unwrap().push((a.rule, a.state));
        });
        // The new process: one refusal, one failing pass so far.
        let mut memory = Memory::default();
        memory
            .backoff
            .insert("/w/a".into(), backoff(FailureKind::Refused, 1));
        memory.note_network(1, 1, Some("timeout"), std::time::Duration::from_secs(60), now);
        fold(&mut l, &memory, &pass);
        assert!(seen.lock().unwrap().is_empty(), "nothing flaps");
        // Recovery: the repo reads again and every remote answers.
        memory.backoff.clear();
        memory.down.clear();
        memory.note_network(2, 0, None, std::time::Duration::from_secs(60), now);
        let mut healthy = pass.clone();
        healthy.workspaces[0].state = WState::W0;
        fold(&mut l, &memory, &healthy);
        let mut got = seen.lock().unwrap().clone();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            [
                ("resync.network".to_string(), State::Resolved),
                ("resync.repo-access".to_string(), State::Resolved)
            ]
        );
    }
}
