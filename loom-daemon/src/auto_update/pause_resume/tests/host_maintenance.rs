//! #11812: H5 resumes a host-maintenance manifest exactly as it resumes a
//! version roll's (nothing in H5 keys on the trigger), and a stale one is
//! requeued with no claim left behind.

use super::*;

fn maintenance(id: &str, dir: &Path) -> PauseManifest {
    let mut m =
        manifest(id, Phase::Paused, vec![sweep(dir, "s-1", 1), role_run(dir, "r-1", "judge")]);
    m.roll.target_source = Some(pause_manifest::TargetSource::HostMaintenance);
    // No version is rolled to (`RollTarget::host_maintenance`).
    m.roll.to_version = "unknown".to_string();
    m.roll.from_version = Some(RUNNING.to_string());
    m
}

#[test]
fn a_host_maintenance_manifest_is_resumed_like_a_rolls() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &maintenance("rp-maint", dir.path()));
    let host = Arc::new(FakeHost::default());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!((status.items, status.resumed), (2, 2));
    assert!(status.requeued_by_reason.is_empty());
    assert_eq!(status.resumed_on.as_deref(), Some("target"), "never a rollback");
    assert_eq!(host.called("lease ").len(), 2, "leases refreshed before the relaunch");
    assert_eq!(host.called("launch ").len(), 2);
    assert_eq!(archived(&plan, "rp-maint").phase, Phase::Resumed);
    // No failed-roll back-off is recorded: nothing was rolled to.
    assert_eq!(attempt::load(plan.manifest_path.parent().unwrap()), None);
    let summary = &host.topic("daemon.roll.paused.resumed")[0];
    assert_eq!(summary["trigger"], "host-maintenance");
    assert!(host
        .topic("daemon.roll.item")
        .iter()
        .all(|p| p["trigger"] == "host-maintenance"));
}

#[test]
fn a_stale_host_maintenance_manifest_requeues_every_item_and_strands_no_claim() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut m = maintenance("rp-maint-stale", dir.path());
    // The host came back after the lease TTL (a rebuild, say).
    m.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(901);
    write(&plan, &m);
    let host = Arc::new(FakeHost::default());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.load, "stale");
    assert_eq!((status.resumed, status.requeued_by_reason[REASON_STALE]), (0, 2));
    assert!(host.called("launch ").is_empty() && host.called("lease ").is_empty());
    let done = archived(&plan, "rp-maint-stale");
    assert_eq!(done.phase, Phase::Abandoned);
    for it in &done.items {
        assert_eq!(it.status, ItemStatus::Requeued, "{}: no item without a terminal status", it.id);
    }
    // Both requeued through the forge with the stale reason: each claim is
    // handed back (label restored), none is left held for a later restart.
    let requeues = host.called("requeue ");
    assert_eq!(requeues.len(), 2, "{requeues:?}");
    assert!(requeues.iter().all(|c| c.contains(REASON_STALE)));
    assert!(host.called("recover ").is_empty(), "nothing left to restart recovery");
    assert!(!host.drain.is_draining(), "dispatch hold lifted");
}
