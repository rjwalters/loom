//! Workspace-resync failures on loom-ui's Alerts page (#11295): rule
//! `resync.<kind>`, the same kinds as the [`super::ALERT_TOPIC`] events.
//!
//! | Rule | Key | Open while | Resolved by |
//! |---|---|---|---|
//! | `resync.failure` | the repo | [`ALERT_AFTER`] or more failures in a row | a resync, or the repo reaching W0 |
//! | `resync.repo-access` | the repo | [`ALERT_AFTER`] or more refusals in a row | the forge answering for it again |
//! | `resync.branch-protection` | the repo | a protection rule refused the push | a later resync, or W0 |
//! | `resync.network` | `workspaces` | the outage alert stands | every remote answering again |
//! | `resync.head-query` | `workspaces` | the head-query alert stands | a pass whose query works |
//! | `resync.pass-stuck` | `workspaces` | a pass has run for [`super::STUCK_AFTER_TICKS`] ticks | the next pass that finishes |
//!
//! Read from the pass memory after every finished pass (level, not edge), and
//! kept in a [`Ledger`], so a restarted daemon resolves what the previous one
//! left open once its first pass finds the repo healthy or unregistered.
//! `resync-loop` is not sent: it is a one-off refusal per version, reported on
//! the event bus, with no recovery edge to resolve it by.

use std::sync::{Mutex, OnceLock};

use super::memory::FailureKind;
use super::{Memory, WorkspacePass, ALERT_AFTER};
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

/// What stands after `pass`, read from `memory`.
#[must_use]
pub(super) fn alerts(memory: &Memory, pass: &WorkspacePass) -> Vec<Alert> {
    let mut out = Vec::new();
    for report in &pass.workspaces {
        let Some(b) = memory.backoff.get(&report.root) else {
            continue;
        };
        let kind = match b.kind {
            FailureKind::Protected => "branch-protection",
            FailureKind::Refused if b.failures >= ALERT_AFTER => "repo-access",
            FailureKind::Other if b.failures >= ALERT_AFTER => "failure",
            _ => continue,
        };
        let key = report
            .repo
            .clone()
            .unwrap_or_else(|| report.root.display().to_string());
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
    let (network, head_query) = memory.host_alerts_standing();
    if network {
        out.push(alert(
            "network",
            HOST_KEY,
            "Workspace remotes are not answering this host".to_string(),
            "The workspace resync cannot reach the repos' remotes; see `loom-daemon status`."
                .to_string(),
        ));
    }
    if head_query {
        out.push(alert(
            "head-query",
            HOST_KEY,
            "The batched default-branch head query keeps failing".to_string(),
            "Each remote is asked with git ls-remote instead.".to_string(),
        ));
    }
    out
}

/// Fold a finished pass in. A pass finished, so none is stuck.
pub(super) fn after_pass(memory: &Memory, pass: &WorkspacePass) {
    let current = alerts(memory, pass);
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reconcile(current);
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
    use super::super::{report_for, WState};
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
        alerts(memory, pass)
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
        l.reconcile(alerts(&memory, &pass));
        memory.backoff.clear();
        l.reconcile(alerts(&memory, &pass));
        assert_eq!(*seen.lock().unwrap(), [State::Open, State::Resolved]);
    }
}
