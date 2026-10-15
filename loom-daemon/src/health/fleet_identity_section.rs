//! The `fleet_identity` health section (Issue #11740).
//!
//! A host whose `host.id` names no fleet-store host cannot render its config
//! tiers: it keeps running on whatever it last rendered and silently misses
//! every fleet config change. Before #11740 that showed only as a log WARN.
//! This section turns it into a Degraded verdict (and so a non-zero `loom-daemon
//! health` exit), reading the flag the fleet-sync config pass records
//! ([`crate::fleet_sync::ConfigPass::host_not_in_store`]).
//!
//! Conditional: no section unless the flag is set, so a host without a fleet
//! store, or one whose identity resolves, renders exactly what it did before.

use super::{HealthInputs, HealthSection, Verdict};
use crate::fleet_sync::identity::NOT_IN_STORE;

/// Assess the `fleet_identity` section: `Some` (Degraded) only when the last
/// fleet-sync pass found this host's id absent from the store.
#[must_use]
pub fn assess_fleet_identity(inputs: &HealthInputs) -> Option<HealthSection> {
    let status = inputs.fleet_sync.as_ref()?;
    if !status.config.host_not_in_store {
        return None;
    }
    Some(HealthSection::new(
        "fleet_identity",
        Verdict::Degraded,
        format!(
            "{NOT_IN_STORE}: `{}` names no host in {}; set LOOM_HOST_ID or fleet.hostId to this \
             host's roster name",
            status.host, status.repo
        ),
        serde_json::json!({
            "alert": NOT_IN_STORE,
            "host": status.host,
            "store": status.repo,
            "commit": status.config.commit,
            "at": status.at,
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fleet_sync::{ConfigPass, FleetSyncStatus, RosterPass};

    fn status(host_not_in_store: bool) -> FleetSyncStatus {
        FleetSyncStatus {
            repo: "acme/fleet".to_string(),
            reference: "main".to_string(),
            host: "loom-host-0769b806b62a427e".to_string(),
            pass: "timer".to_string(),
            at: chrono::Utc::now(),
            interval_secs: 300,
            auto_apply: true,
            config: ConfigPass {
                error: Some("not in the store".to_string()),
                host_not_in_store,
                ..ConfigPass::default()
            },
            roster: RosterPass::default(),
            state: Default::default(),
            enforced: crate::fleet_state::Enforcement::Proceed,
            floor: Default::default(),
            workspaces: Default::default(),
            checkouts: Vec::new(),
            checkouts_note: None,
            repo_config: Default::default(),
        }
    }

    fn inputs(fleet_sync: Option<FleetSyncStatus>) -> HealthInputs {
        HealthInputs {
            fleet_sync,
            ..Default::default()
        }
    }

    #[test]
    fn host_not_in_store_is_degraded_with_the_alert_text() {
        let section = assess_fleet_identity(&inputs(Some(status(true)))).unwrap();
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains(NOT_IN_STORE), "{}", section.summary);
        assert_eq!(section.detail["alert"], NOT_IN_STORE);
    }

    #[test]
    fn no_section_when_resolved_or_no_store() {
        assert!(assess_fleet_identity(&inputs(Some(status(false)))).is_none());
        assert!(assess_fleet_identity(&inputs(None)).is_none());
    }
}
