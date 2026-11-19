use super::*;

/// Issue #7691 (Phase B of #6704), dispatcher non-regression: a roster-mode
/// **yield** gates role ticks only and never reaches dispatch.
///
/// A fence yield must not flip `owned` to `false`: `owned` keeps the
/// pre-roster verdict and only `admits_role_tick()` flips. Since #11682 the
/// dispatcher reads no shard verdict at all (its repo slice is retired), so a
/// self-fenced host — one whose roster heartbeat is failing — still
/// dispatches every workspace.
#[test]
#[serial]
fn a_roster_mode_yield_leaves_dispatch_work_conserving() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(crate::config_resolver::LEGACY_CONFIG_REL),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"shardKey":"rjwalters/loom",
           "roster":{"enabled":true,"issue":"rjwalters/loom#1234"}}}}"#,
    )
    .unwrap();

    // A roster this host joined and then lost: its own record is 20m stale
    // against a 15m ttl, so the fence yields (condition 1).
    let now = chrono::Utc::now();
    let comments: Vec<crate::role_shard::roster::RosterComment> = ["host-a", "host-b"]
        .iter()
        .enumerate()
        .map(|(i, host)| crate::role_shard::roster::RosterComment {
            id: u64::try_from(i).unwrap() + 1,
            host: (*host).to_string(),
            serves: [crate::role_shard::hash_key("rjwalters/loom")]
                .into_iter()
                .collect(),
            created_at: now - chrono::Duration::seconds(86_400),
            updated_at: if i == 0 {
                now - chrono::Duration::seconds(1200)
            } else {
                now - chrono::Duration::seconds(60)
            },
        })
        .collect();
    crate::role_shard::roster::set_roster_snapshot(crate::role_shard::roster::RosterSnapshot {
        issue: crate::role_shard::roster::RosterIssueRef::parse("rjwalters/loom#1234").unwrap(),
        host: "host-a".to_string(),
        comments,
        ttl_secs: 900,
        settle_secs: 900,
        fetched_at: now,
    });

    let decision = crate::role_shard::decide(root);
    assert!(
        decision.roster.is_yield(),
        "precondition: the fence must be yielding, got {:?}",
        decision.roster
    );
    assert!(!decision.admits_role_tick(), "role ticks are what a yield gates");
    assert!(
        decision.owned,
        "a fence yield must NOT reach work_finder as `owns nothing` (#6704 blast radius)"
    );

    // ...and dispatch still drains every workspace.
    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut workspaces, &[1, 1], 10, &[false, false]);
    assert_eq!(report.dispatched, 2, "a roster yield must never starve dispatch");

    crate::role_shard::roster::clear_roster_snapshot_for_tests();
    crate::role_shard::roster::clear_generation_fence_for_tests();
}
