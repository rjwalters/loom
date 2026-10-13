//! `forge.credential-missing` on loom-ui (#11295), key `github`.
//!
//! The startup preflight found no usable GitHub credential: every forge call
//! this daemon makes will 401. Critical, since the host does nothing useful
//! until someone exports a token or unlocks the keychain.
//!
//! Opened (or resolved) by [`startup`] from the preflight report, and also
//! resolved by the first successful github-app refresh
//! ([`super::record_forge_credential_success`] for the primary source), which
//! puts a minted token in place without a restart. The incident is kept in a
//! [`Ledger`], so the next start that finds a credential resolves it.

use std::sync::{Mutex, OnceLock};

use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};
use crate::types::CredentialPreflightReport;

/// The rule.
pub const RULE: &str = "forge.credential-missing";
const KEY: &str = "github";

fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::load("credential_preflight")))
}

/// The open alert for `report`, or `None` when it found a usable credential.
#[must_use]
pub fn alert(report: &CredentialPreflightReport) -> Option<Alert> {
    (!report.ok).then(|| {
        Alert::new(
            RULE,
            KEY,
            "loom-daemon has no usable GitHub credential: every forge call will fail",
            Severity::Critical,
            State::Open,
        )
        .with_detail(format!("mechanism {}: {}", report.mechanism, report.message))
    })
}

/// Record the startup preflight's verdict.
pub fn startup(report: &CredentialPreflightReport) {
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reconcile(alert(report).into_iter().collect());
}

/// A refresh just put a usable credential in place.
pub fn credential_restored() {
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resolve(RULE, KEY, "A github-app refresh minted a usable credential.");
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn report(ok: bool) -> CredentialPreflightReport {
        CredentialPreflightReport::new(
            ok,
            if ok { "GH_TOKEN" } else { "none" },
            None,
            "no usable gh credential found".to_string(),
            chrono::Utc::now(),
        )
    }

    #[test]
    fn a_missing_credential_is_critical_and_a_good_one_is_nothing() {
        let a = alert(&report(false)).unwrap();
        assert_eq!(a.rule, RULE);
        assert_eq!(a.severity, Severity::Critical);
        assert!(a.detail.unwrap().contains("mechanism none"));
        assert!(alert(&report(true)).is_none());
    }

    #[test]
    fn a_later_good_start_resolves_what_a_bad_one_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        Ledger::at(Some(path.clone()), |_| {})
            .reconcile(alert(&report(false)).into_iter().collect());
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut l = Ledger::at(Some(path), move |a: Alert| sink.lock().unwrap().push(a.state));
        l.reconcile(alert(&report(true)).into_iter().collect());
        assert_eq!(*seen.lock().unwrap(), [State::Resolved]);
    }
}
