//! The per-repo limits `fleet.state` publishes (Issue #11606), read from a
//! real multi-workspace tick: the cap the tick resolved, each workspace's
//! occupancy when it finished, and the cap deferrals per workspace.

/// A summary with no limits omits the key, and a payload without it (an
/// older daemon) decodes empty.
#[test]
fn an_old_summary_decodes_without_limits() {
    let wire = serde_json::to_value(crate::types::WorkFinderTickSummary::default()).unwrap();
    assert!(wire.get("repo_limits").is_none());
    let summary: crate::types::WorkFinderTickSummary = serde_json::from_value(wire).unwrap();
    assert!(summary.repo_limits.is_empty());
}
