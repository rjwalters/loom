use super::*;

// ===================================================================
// Host sharding at the dispatch surface (#6374)
//
// `role_shard`'s own tests pin the *arithmetic* (exactly one owner per
// key, an even spread, the fail-safe fallbacks). These pin the thing
// that arithmetic alone cannot: that `decide_root_tick` and
// `plan_idle_runs` — the two surfaces that actually spend a token —
// honor it, in the right order relative to the `LOOM_ROLE_RUNNER`
// kill switch.
// ===================================================================

// The shared seams these tests drive (`ShardEnvGuard`, `enabled_workspace`,
// `enabled_workspace_with_key`, `tick_admitted`) live in the parent `tests` module,
// where the idle-edge sibling (`roster_fence`) reaches them the same way.

/// AC1 (first half), at the dispatch surface rather than in the hash:
/// across a two-host fleet, each workspace's curator tick is admitted by
/// **exactly one** host per interval — never zero (the slice would go
/// unrotated fleet-wide) and never two (the #6332 / #6352 duplication
/// this issue exists to prevent).
#[test]
#[serial]
fn two_host_fleet_admits_each_workspace_curator_tick_on_exactly_one_host() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();

    for workspace in &fleet {
        let root = workspace.path();
        let admitting: Vec<usize> = (0..2)
            .filter(|host| {
                ShardEnvGuard::become_host(*host, 2);
                tick_admitted(root)
            })
            .collect();
        assert_eq!(
            admitting.len(),
            1,
            "{} admitted by hosts {admitting:?}; exactly one host must run each workspace's \
                 role tick per interval (#6374)",
            root.display()
        );
    }
}

/// AC2, measured the way the incident measured it: the fleet-wide *count
/// of role sessions spawned per interval*. Unsharded, a 4-host fleet
/// spends 4 curator ticks per workspace; sharded, it spends 1 — the token
/// draw scales with workspaces, not workspaces x hosts.
#[test]
#[serial]
fn sharding_makes_the_fleet_wide_tick_draw_scale_with_workspaces_not_hosts() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();
    const HOSTS: usize = 4;

    // Unsharded (today's behavior, and the fail-safe fallback): every
    // host spends a tick on every workspace.
    let unsharded: usize = (0..HOSTS)
        .map(|_| fleet.iter().filter(|w| tick_admitted(w.path())).count())
        .sum();
    assert_eq!(unsharded, fleet.len() * HOSTS);

    // Sharded: the same fleet spends exactly one tick per workspace.
    let sharded: usize = (0..HOSTS)
        .map(|host| {
            ShardEnvGuard::become_host(host, HOSTS);
            fleet.iter().filter(|w| tick_admitted(w.path())).count()
        })
        .sum();
    assert_eq!(
        sharded,
        fleet.len(),
        "a {HOSTS}-host fleet drew {sharded} curator ticks for {} workspaces (#6374 AC2)",
        fleet.len()
    );
}

/// AC3: the blunt per-host kill switch keeps working, and keeps
/// short-circuiting **before** sharding is consulted — so an operator who
/// sets `LOOM_ROLE_RUNNER=0` gets zero ticks regardless of whether this
/// host owns the slice. Asserted for the owning host specifically, since
/// a non-owning host would skip for the wrong reason and prove nothing.
#[test]
#[serial]
fn role_runner_env_zero_still_disables_the_host_that_owns_the_slice() {
    let _env = ShardEnvGuard::capture();
    let workspace = enabled_workspace();
    let root = workspace.path();

    let owner = (0..2)
        .find(|host| {
            ShardEnvGuard::become_host(*host, 2);
            tick_admitted(root)
        })
        .expect("exactly one of the two hosts owns this workspace");

    ShardEnvGuard::become_host(owner, 2);
    assert!(tick_admitted(root), "precondition: the owning host ticks");

    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    assert!(
        !tick_admitted(root),
        "LOOM_ROLE_RUNNER=0 must still disable role ticks on the host that owns the slice \
             (#6374 AC3)"
    );
}

/// AC3, the other direction: sharding must not *weaken* the kill switch's
/// counterpart either — an unsharded host (no shard env at all) behaves
/// exactly as it did before #6374, owning every workspace.
#[test]
#[serial]
fn an_unsharded_host_still_ticks_every_workspace() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..6).map(|_| enabled_workspace()).collect();
    for workspace in &fleet {
        assert!(
            tick_admitted(workspace.path()),
            "an unsharded host must keep rotating every workspace (#6374 fail-safe)"
        );
    }
}

/// AC1 (second half), as far as this PR's **static** assignment goes:
/// shrinking the ring reassigns the departed host's slice to the
/// survivors, and no workspace is left unowned by the reassignment. This
/// is the operator-driven reassignment path (lower `shardCount`, or point
/// the survivor at the vacated index); automatic, roster-driven
/// reassignment on host loss is deliberately deferred to #6704 — see
/// `role_shard`'s module docs for why.
///
/// The fleet's shard keys are **pinned fixtures**, not drawn from random
/// tempdir basenames (#8683): the drawn version asserted "host 1 must have
/// owned something to orphan" — a (1/2)^12 coin flip on the *precondition*,
/// not the behavior under test — and flaked exactly there. The ring split
/// is now checked up front, so the test can fail on a broken fixture, never
/// on the draw.
#[test]
#[serial]
fn shrinking_the_ring_reassigns_the_departed_hosts_slice_to_the_survivor() {
    let _env = ShardEnvGuard::capture();
    let keys: Vec<String> = (0..12).map(|i| format!("shrinking-ring-ws-{i}")).collect();
    let on_host_1 = keys
        .iter()
        .filter(|k| crate::role_shard::owning_shard(k, 2) == Some(1))
        .count();
    assert!(
        (1..keys.len()).contains(&on_host_1),
        "fixture keys must straddle both halves of a 2-host ring (host 1 owns {on_host_1} of {})",
        keys.len()
    );
    let fleet: Vec<tempfile::TempDir> =
        keys.iter().map(|k| enabled_workspace_with_key(k)).collect();

    // Host 1 dies. Its slice is exactly what host 0 was NOT ticking.
    ShardEnvGuard::become_host(0, 2);
    let orphaned: Vec<&Path> = fleet
        .iter()
        .map(tempfile::TempDir::path)
        .filter(|root| !tick_admitted(root))
        .collect();
    assert_eq!(
        orphaned.len(),
        on_host_1,
        "host 1's slice is exactly the workspaces host 0 was not ticking (#6374)"
    );

    // The operator shrinks the ring to the one survivor; every orphaned
    // workspace is picked up, and nothing is dropped in the process.
    ShardEnvGuard::become_host(0, 1);
    for root in &fleet {
        assert!(
            tick_admitted(root.path()),
            "{} must be rotated by the surviving host after the ring shrinks (#6374)",
            root.path().display()
        );
    }
}
