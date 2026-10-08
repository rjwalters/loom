//! The `Fleet store:` block on `loom-daemon status` (#9596).
//!
//! A sibling module because `status_render.rs` is a frozen over-threshold
//! ledger entry, matching how [`super::operator_priority_line`] and
//! [`super::observability_line`] already live here.
//!
//! **Read client-side, not over IPC.** The snapshot this renders
//! (`<loom_dir>/fleet-sync-status.json`, written by
//! [`loom_daemon::fleet_sync`] at the end of every pass) is a fact about the
//! *host's* config tiers, not about the daemon's in-memory state — the same
//! class as the `daemon_install_state` probe `cli::status` already performs
//! locally. Reading it here also keeps it visible in exactly the case an
//! operator most needs it: when the startup sync itself is what went wrong and
//! the daemon is not answering.
//!
//! **Silent by default.** A host with no `fleet.repo` has never written the
//! snapshot, so both entry points below render nothing at all and `status`
//! output is byte-identical to a build without this feature.

use loom_daemon::fleet_sync::{self, FleetSyncStatus};

/// Print the block, or nothing when this host has no fleet-store snapshot.
pub fn print(status: Option<&FleetSyncStatus>) {
    if let Some(block) = fleet_sync::render_line(status, chrono::Utc::now()) {
        println!("{block}");
    }
}

/// The `--json` counterpart: the snapshot verbatim, or `Null` when there is
/// none. Callers assign it to a `fleet_store` key only when non-null, so the
/// payload of a host without the feature is unchanged.
#[must_use]
pub fn json(status: Option<&FleetSyncStatus>) -> serde_json::Value {
    status.map_or(serde_json::Value::Null, |s| {
        serde_json::to_value(s).unwrap_or(serde_json::Value::Null)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_snapshot_renders_nothing_in_either_surface() {
        // The byte-identical guarantee for a host with no `fleet.repo`.
        assert_eq!(json(None), serde_json::Value::Null);
        assert_eq!(fleet_sync::render_line(None, chrono::Utc::now()), None);
    }

    #[test]
    fn present_snapshot_serializes_its_fields() {
        let status = FleetSyncStatus {
            repo: "acme/fleet".to_string(),
            reference: "main".to_string(),
            host: "build-1".to_string(),
            pass: "timer".to_string(),
            at: chrono::Utc::now(),
            interval_secs: 300,
            auto_apply: false,
            config: fleet_sync::ConfigPass::default(),
            roster: fleet_sync::RosterPass::default(),
            state: loom_daemon::fleet_state::StatePass::default(),
            enforced: loom_daemon::fleet_state::Enforcement::Proceed,
            floor: fleet_sync::FloorPass::default(),
            workspaces: fleet_sync::workspace_resync::WorkspacePass::default(),
            checkouts: Vec::new(),
        };
        let value = json(Some(&status));
        assert_eq!(value["repo"], serde_json::json!("acme/fleet"));
        assert_eq!(value["autoApply"], serde_json::json!(false));
    }

    /// #9598: the desired-vs-actual pair reaches both `status` surfaces.
    #[test]
    fn run_state_reaches_the_human_and_json_surfaces() {
        let status = FleetSyncStatus {
            repo: "acme/fleet".to_string(),
            reference: "main".to_string(),
            host: "build-1".to_string(),
            pass: "timer".to_string(),
            at: chrono::Utc::now(),
            interval_secs: 300,
            auto_apply: false,
            config: fleet_sync::ConfigPass::default(),
            roster: fleet_sync::RosterPass::default(),
            state: loom_daemon::fleet_state::StatePass {
                desired: Some(loom_daemon::fleet_store::state::RunState::Paused),
                source: Some("host".to_string()),
                by: Some("operator".to_string()),
                ..Default::default()
            },
            enforced: loom_daemon::fleet_state::Enforcement::Hold,
            floor: fleet_sync::FloorPass::default(),
            workspaces: fleet_sync::workspace_resync::WorkspacePass::default(),
            checkouts: Vec::new(),
        };
        let block = fleet_sync::render_line(Some(&status), chrono::Utc::now())
            .expect("a snapshot renders a block");
        assert!(block.contains("desired paused"), "{block}");
        assert!(block.contains("HELD"), "{block}");
        assert!(block.contains("by: operator"), "{block}");

        let value = json(Some(&status));
        assert_eq!(value["state"]["desired"], serde_json::json!("paused"));
        assert_eq!(value["enforced"], serde_json::json!("hold"));
    }
}
