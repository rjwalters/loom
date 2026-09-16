use super::*;

/// The idle-edge dispatch surface (#4364) is sharded too. Without this,
/// an idle-triggered role would duplicate across the fleet exactly as the
/// interval cadence did, and the invariant would hold on only one of the
/// two paths that spend tokens.
#[test]
#[serial]
fn the_idle_edge_is_sharded_on_the_same_key_as_the_interval_tick() {
    let _env = ShardEnvGuard::capture();
    let workspace = enabled_workspace();
    let root = workspace.path();
    let cfg = on_idle_config(Some(true), vec!["champion"]);

    let owner = (0..2)
        .find(|host| {
            ShardEnvGuard::become_host(*host, 2);
            tick_admitted(root)
        })
        .expect("exactly one of the two hosts owns this workspace");

    // The owning host fires on the busy -> idle edge...
    ShardEnvGuard::become_host(owner, 2);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let now = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    assert_eq!(
        plan_idle_runs(&mut t, &set, root, &cfg, true, false, now)
            .iter()
            .map(|(s, _)| s.name)
            .collect::<Vec<_>>(),
        vec!["champion"]
    );

    // ...and the peer, observing the same edge, does not.
    ShardEnvGuard::become_host((owner + 1) % 2, 2);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty(),
        "a non-owning host must not fire an idle-triggered role (#6374)"
    );
}

// ===================================================================
// The roster fence at the dispatch surfaces (#7691, Phase B of #6704)
//
// `role_shard`'s own tests pin the fence's arithmetic and its adversarial
// scenarios. These pin the only thing that matters operationally: that the
// two surfaces which actually spend a token — `decide_root_tick` (interval)
// and `plan_idle_runs` (the onIdle edge) — inherit it, and that the
// `LOOM_ROLE_RUNNER=0` / static-shard escape hatches still outrank it.
// ===================================================================

/// A role-runner-enabled workspace whose config also turns the roster on.
fn roster_enabled_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"shardKey":"rjwalters/loom",
           "roster":{"enabled":true,"issue":"rjwalters/loom#1234"}}}}"#,
    );
    tmp
}

/// Publish a **single-member** roster snapshot for this host, whose last
/// heartbeat was `last_beat_secs` ago. One member is deliberate: a one-host
/// ring owns every key, so "did this host tick?" isolates the fence instead
/// of also depending on which side of a multi-host ring the key hashes to.
fn publish_roster_snapshot(last_beat_secs: i64) {
    let now = chrono::Utc::now();
    let comments = vec![crate::role_shard::roster::RosterComment {
        id: 1,
        host: "host-a".to_string(),
        serves: [crate::role_shard::hash_key("rjwalters/loom")]
            .into_iter()
            .collect(),
        created_at: now - chrono::Duration::seconds(86_400),
        updated_at: now - chrono::Duration::seconds(last_beat_secs),
    }];
    crate::role_shard::roster::set_roster_snapshot(crate::role_shard::roster::RosterSnapshot {
        issue: crate::role_shard::roster::RosterIssueRef::parse("rjwalters/loom#1234").unwrap(),
        host: "host-a".to_string(),
        comments,
        ttl_secs: 900,
        settle_secs: 900,
        fetched_at: now,
    });
}

/// The self-fence, at the interval surface: a host whose own roster
/// heartbeat has gone stale spends **no** curator tick, even though the
/// static posture (unsharded) says it owns every workspace.
#[test]
#[serial]
fn a_stale_roster_heartbeat_stops_the_interval_tick_on_this_host() {
    let _env = ShardEnvGuard::capture();
    crate::role_shard::roster::clear_generation_fence_for_tests();
    let workspace = roster_enabled_workspace();
    let root = workspace.path();

    // Beating normally: the ring admits this host for this key.
    publish_roster_snapshot(60);
    let admitted_when_fresh = tick_admitted(root);

    // Heartbeat 20m stale against a 15m ttl: the fence yields.
    crate::role_shard::roster::clear_generation_fence_for_tests();
    publish_roster_snapshot(1200);
    assert!(
        !tick_admitted(root),
        "a self-fenced host must run NO roster-gated role ticks (#6704)"
    );
    assert!(
        admitted_when_fresh,
        "precondition: this host must own this key while it is beating — otherwise the assertion \
         above proves nothing"
    );

    crate::role_shard::roster::clear_roster_snapshot_for_tests();
    crate::role_shard::roster::clear_generation_fence_for_tests();
}

/// The same fence on the idle edge, which fires on every host that observes
/// it — so a fence that covered only the interval cadence would not hold.
#[test]
#[serial]
fn a_stale_roster_heartbeat_stops_the_idle_edge_on_this_host() {
    let _env = ShardEnvGuard::capture();
    crate::role_shard::roster::clear_generation_fence_for_tests();
    let workspace = roster_enabled_workspace();
    let root = workspace.path();
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let now = Instant::now();

    let fire = |fence_stale: bool| {
        crate::role_shard::roster::clear_generation_fence_for_tests();
        publish_roster_snapshot(if fence_stale { 1200 } else { 60 });
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        plan_idle_runs(&mut t, &set, root, &cfg, true, false, now)
            .iter()
            .map(|(s, _)| s.name)
            .collect::<Vec<_>>()
    };

    assert_eq!(fire(false), vec!["champion"], "precondition: the admitted host fires");
    assert!(
        fire(true).is_empty(),
        "a self-fenced host must not fire an idle-triggered role either (#6704)"
    );

    crate::role_shard::roster::clear_roster_snapshot_for_tests();
    crate::role_shard::roster::clear_generation_fence_for_tests();
}

/// Escape-hatch precedence, rung 1 over rung 3: `LOOM_ROLE_RUNNER=0` still
/// short-circuits before the roster is consulted — asserted on a host the
/// roster *admits*, so the skip cannot be the fence's doing.
#[test]
#[serial]
fn role_runner_env_zero_still_disables_a_host_the_roster_admits() {
    let _env = ShardEnvGuard::capture();
    crate::role_shard::roster::clear_generation_fence_for_tests();
    let workspace = roster_enabled_workspace();
    let root = workspace.path();
    publish_roster_snapshot(60);
    assert!(tick_admitted(root), "precondition: the roster admits this host");

    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    assert!(
        !tick_admitted(root),
        "LOOM_ROLE_RUNNER=0 must outrank an admitting roster exactly as it outranks the static \
         ring (#6704 AC3 rung 1)"
    );

    crate::role_shard::roster::clear_roster_snapshot_for_tests();
    crate::role_shard::roster::clear_generation_fence_for_tests();
}

/// Escape-hatch precedence, rung 2 over rung 3: a static
/// `LOOM_ROLE_RUNNER_SHARD_INDEX` + `shardCount` pair beats an enabled
/// roster — the documented way an operator pins a deterministic ring
/// during a roster outage or a deliberately partitioned fleet.
#[test]
#[serial]
fn a_static_shard_pair_beats_an_enabled_roster_at_the_dispatch_surface() {
    let _env = ShardEnvGuard::capture();
    crate::role_shard::roster::clear_generation_fence_for_tests();
    let workspace = roster_enabled_workspace();
    let root = workspace.path();
    // A roster that would fence this host out entirely...
    publish_roster_snapshot(1200);
    assert!(!tick_admitted(root), "precondition: the roster alone would yield");

    // ...is overridden by the static pair, whose owning host ticks.
    let owner = (0..2)
        .find(|host| {
            ShardEnvGuard::become_host(*host, 2);
            tick_admitted(root)
        })
        .expect("the static ring must decide ownership, roster or no roster");
    ShardEnvGuard::become_host(owner, 2);
    assert!(tick_admitted(root));
    ShardEnvGuard::become_host((owner + 1) % 2, 2);
    assert!(!tick_admitted(root), "the static ring still owns exactly one host per key");

    crate::role_shard::roster::clear_roster_snapshot_for_tests();
    crate::role_shard::roster::clear_generation_fence_for_tests();
}
