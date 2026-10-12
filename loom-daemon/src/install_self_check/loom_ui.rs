//! Install self-check violations on loom-ui's Alerts page (#11295): rule
//! `install.<invariant id>` (`install.mcp-bundle-health`,
//! `install.runtimes-present`, …), key = the workspace root.
//!
//! Only while the self-check is enabled. One loop tick checks every enabled
//! root; the violations still standing after it (after repair, in repair mode)
//! are the open set, so a repaired, fixed, disabled or deregistered one is
//! resolved. Kept in a [`Ledger`]: a daemon restarted with the check turned off
//! resolves what an earlier one left open ([`resolve_all`]).

use std::path::Path;

use super::{InvariantStatus, SelfCheckReport};
use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};

/// The ledger scope.
pub const SCOPE: &str = "install_self_check";

/// The open alerts for `report` on `root`.
#[must_use]
pub fn alerts(root: &Path, report: &SelfCheckReport) -> Vec<Alert> {
    report
        .outcomes
        .iter()
        .filter_map(|o| match &o.status {
            InvariantStatus::Violation(detail) => Some(
                Alert::new(
                    format!("install.{}", o.invariant.id()),
                    root.display().to_string(),
                    format!("{}: {}", o.invariant.title(), root.display()),
                    Severity::Warning,
                    State::Open,
                )
                .with_detail(detail.clone()),
            ),
            InvariantStatus::Ok | InvariantStatus::Skipped(_) => None,
        })
        .collect()
}

/// Resolve everything a previous process left open (the check is off).
pub fn resolve_all() {
    Ledger::load(SCOPE).reconcile(Vec::new());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{Invariant, InvariantOutcome};
    use super::*;

    #[test]
    fn only_violations_alert_per_invariant_and_root() {
        let report = SelfCheckReport {
            outcomes: vec![
                InvariantOutcome {
                    invariant: Invariant::RuntimesPresent,
                    status: InvariantStatus::Violation("codex missing".into()),
                },
                InvariantOutcome {
                    invariant: Invariant::McpBundleHealth,
                    status: InvariantStatus::Ok,
                },
                InvariantOutcome {
                    invariant: Invariant::TokenRankingFresh,
                    status: InvariantStatus::Skipped("no pool".into()),
                },
            ],
        };
        let got = alerts(Path::new("/w/repo"), &report);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rule, "install.runtimes-present");
        assert_eq!(got[0].key, "/w/repo");
        assert_eq!(got[0].detail.as_deref(), Some("codex missing"));
    }
}
