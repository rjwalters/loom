//! The `role_tick.outcome` fact for a repo the role runner did not reach this
//! tick (#12084).
//!
//! A repo's effective cadence is the interval times the ticks a host limit
//! kept its turn from coming (champion: `600 s x ceil(repos / budget)` under a
//! 3-per-role budget). Until now the deferral was a log line only, so a
//! consumer fitting when a repo's merge lane next runs could not see it.

use std::path::Path;

use chrono::Utc;

use super::{emit, RoleTickTelemetry};
use crate::role_shard::ShardDecision;
use crate::telemetry::RoleTickResult;

/// `<index>/<count>` of the shard that owns the repo, or `None` when the host
/// is unsharded (never a guessed `0/1`).
#[must_use]
pub fn shard_owner_label(decision: &ShardDecision) -> Option<String> {
    let count = decision.posture.count()?;
    decision
        .owning_shard
        .map(|owner| format!("{owner}/{count}"))
}

/// The record for `root`'s deferred `role` tick. `deferred_by` is the limit's
/// [`crate::role_runner::concurrent_dispatch::LimitRefusal::kind`].
#[must_use]
pub fn deferred_tick(
    root: &Path,
    role: &str,
    deferred_by: &str,
    shard_owner: Option<String>,
) -> RoleTickTelemetry {
    let now = Utc::now();
    RoleTickTelemetry {
        root: root.to_path_buf(),
        role: role.to_string(),
        started_at: now,
        ended_at: now,
        result: RoleTickResult::SkippedDeferred,
        model: None,
        effort: None,
        detail: Some(format!("deferred[{deferred_by}]")),
        gated_pool: None,
        preference_tier: None,
        preference_tap: None,
        role_draw: None,
        step_sweep_id: None,
        deferred_by: Some(deferred_by.to_string()),
        shard_owner,
    }
}

/// Emit the deferred fact for `root`. **Blocking** (resolves the repo slug and
/// the shard decision); best-effort like [`emit`].
pub fn emit_deferred(root: &Path, role: &str, deferred_by: &str) {
    let owner = shard_owner_label(&crate::role_shard::decide(root));
    emit(&deferred_tick(root, role, deferred_by, owner));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_deferred_tick_names_the_limit_and_never_spawns() {
        let tick = deferred_tick(&PathBuf::from("/repo/a"), "champion", "ceiling", None);
        assert_eq!(tick.result, RoleTickResult::SkippedDeferred);
        assert!(!tick.result.spawned());
        assert_eq!(tick.deferred_by.as_deref(), Some("ceiling"));
        assert_eq!(tick.detail.as_deref(), Some("deferred[ceiling]"));
        assert_eq!(tick.shard_owner, None);
    }

    #[test]
    fn the_record_carries_deferred_by_and_shard_owner_and_omits_them_elsewhere() {
        let tick =
            deferred_tick(&PathBuf::from("/repo/a"), "champion", "role_budget", Some("2/4".into()));
        let record = super::super::build_record(
            &tick,
            "o/r".to_string(),
            crate::telemetry::RepoVisibility::Private,
            None,
        );
        let wire = serde_json::to_value(&record).unwrap();
        assert_eq!(wire["result"], "skipped_deferred");
        assert_eq!(wire["deferred_by"], "role_budget");
        assert_eq!(wire["shard_owner"], "2/4");
        let ran = super::super::build_record(
            &RoleTickTelemetry {
                result: RoleTickResult::Success,
                deferred_by: None,
                shard_owner: None,
                ..tick
            },
            "o/r".to_string(),
            crate::telemetry::RepoVisibility::Private,
            None,
        );
        let wire = serde_json::to_value(&ran).unwrap();
        assert!(wire.get("deferred_by").is_none() && wire.get("shard_owner").is_none());
    }
}
