use super::*;

/// #6243 AC: work-conservation — when this host's slice has ZERO
/// eligible candidates, it must still drain the global (out-of-slice)
/// queue rather than starve while other repos have ready work.
#[test]
fn tick_multi_with_sharding_falls_back_to_out_of_slice_when_slice_is_empty() {
    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    // Neither workspace is in this host's preferred slice this tick.
    let preferred_slice = [false, false];
    let report = tick_multi_with_sharding(
        &mut workspaces,
        &[0, 0],
        10,
        &[false, false],
        usize::MAX,
        false,
        Some(&preferred_slice),
    );
    assert_eq!(report.dispatched, 2, "empty slice must fall back to the full global queue");
    assert_eq!(
        report.deferred_out_of_slice, 0,
        "a fallback dispatch is not a deferral — it went through"
    );
    assert_eq!(workspaces[0].1.dispatched, vec![1]);
    assert_eq!(workspaces[1].1.dispatched, vec![2]);
}

/// Issue #7691 (Phase B of #6704), dispatcher non-regression: a roster-mode
/// **yield** must not reach this loop as "owns nothing".
///
/// The role runner's fence and the dispatcher's slice read the same
/// `role_shard::decide(root)`, but with deliberately different strength —
/// there a hard filter, here a preference with a work-conserving fallback. If
/// a fence yield flipped `owned` to `false`, a self-fenced host (one whose
/// roster heartbeat is failing — i.e. one having forge trouble) would ALSO
/// stop preferring its own repos, which is dispatch starvation caused by a
/// role-rotation safety rule. This pins that it does not: `owned` keeps the
/// pre-roster verdict, only `admits_role_tick()` flips.
#[test]
#[serial]
fn a_roster_mode_yield_leaves_the_dispatcher_slice_work_conserving() {
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

    // ...and with that mask, dispatch still drains every workspace.
    let preferred_slice: Vec<bool> = vec![decision.owned, decision.owned];
    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi_with_sharding(
        &mut workspaces,
        &[0, 0],
        10,
        &[false, false],
        usize::MAX,
        false,
        Some(&preferred_slice),
    );
    assert_eq!(report.dispatched, 2, "a roster yield must never starve dispatch");
    assert_eq!(report.deferred_out_of_slice, 0);

    crate::role_shard::roster::clear_roster_snapshot_for_tests();
    crate::role_shard::roster::clear_generation_fence_for_tests();
}
