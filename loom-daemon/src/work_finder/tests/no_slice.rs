//! Repo sharding is retired from dispatch (Issue #11682, operator ruling
//! 2026-10-13). The #6243 slice deferred every out-of-slice candidate as soon
//! as this host's slice was non-empty at the top of the tick, and pass 2 could
//! then reject every in-slice candidate (open linked PR, per-repo cap, build
//! back-off) without dispatching any of them: the host sat idle with free
//! slots while other repos' work waited. These tests pin the fix: with a
//! 3-host ring configured, a host whose own repos cannot use a slot dispatches
//! other repos' work in the SAME tick. The shard verdict still feeds the
//! dispatch plan's `owning_shard` (the fleet-merge primary rule) and the role
//! runner (#6374), neither of which these tests change.

use super::*;
use crate::role_shard::{self, ShardDecision, ShardPosture, ValueSource};

const RING: usize = 3;

/// This host's posture in a configured 3-host ring.
fn ring_member(index: usize) -> ShardPosture {
    ShardPosture::Sharded {
        index,
        count: RING,
        index_source: ValueSource::Env,
        count_source: ValueSource::Config,
    }
}

/// Three repo keys: the first is owned by shard 0 (this host), the other two
/// by other hosts. Found by search, not hard-coded, so a change to the ring's
/// hash cannot silently turn this into a test with no in-slice repo.
fn three_repos_one_owned_here() -> [String; 3] {
    let mut owned = None;
    let mut others = Vec::new();
    for n in 0..1000 {
        let key = format!("acme/repo-{n}");
        match role_shard::owning_shard(&key, RING) {
            Some(0) if owned.is_none() => owned = Some(key),
            Some(s) if s != 0 && others.len() < 2 => others.push(key),
            _ => {}
        }
        if owned.is_some() && others.len() == 2 {
            break;
        }
    }
    let owned = owned.expect("some key hashes to shard 0");
    let [b, c]: [String; 2] = others.try_into().expect("two keys hash elsewhere");
    [owned, b, c]
}

/// The per-root shard verdicts the daemon loop computes, for the ring above.
fn decisions(keys: &[String; 3]) -> Vec<ShardDecision> {
    keys.iter()
        .map(|k| {
            let root = std::path::PathBuf::from(format!("/repos/{k}"));
            role_shard::decide_with(ring_member(0), &root, Some(k))
        })
        .collect()
}

fn ready_elsewhere() -> [(FakeSource, RecordingDispatcher); 2] {
    [
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(20)]), RecordingDispatcher::default()),
    ]
}

/// The planned rows carry `owning_shard` from the ring and no slice field.
fn assert_plan_has_no_slice(report: &TickReport, shards: &[ShardDecision]) {
    let inputs = dispatch_plan::PlanInputs::new(10, Duration::from_secs(60), shards);
    let summary = tick_summary(report, 10, chrono::Utc::now(), &[], Some(&inputs));
    assert!(summary.plan.expect("multi-workspace plan").shard.configured);
    for row in &summary.queue {
        let wire = serde_json::to_value(row).unwrap();
        assert!(wire.get("in_slice").is_none(), "{wire}");
        assert_ne!(wire["gate"], "out_of_slice", "{wire}");
        assert_ne!(wire["disposition"], "deferred_out_of_slice", "{wire}");
        assert!(row.plan.owning_shard.is_some(), "owning_shard kept: {wire}");
    }
}

/// Acceptance 1, open-PR form: every in-slice issue already has an open
/// linked PR. Before #11682 this host deferred both out-of-slice issues and
/// then dispatched nothing; now they dispatch in the same tick.
#[test]
fn in_slice_repo_with_only_open_prs_yields_the_tick_to_other_repos() {
    let keys = three_repos_one_owned_here();
    let shards = decisions(&keys);
    assert_eq!(
        shards.iter().map(|d| d.owned).collect::<Vec<_>>(),
        vec![true, false, false],
        "precondition: only the first repo is in this host's ring slice"
    );

    let [b, c] = ready_elsewhere();
    let mut workspaces = vec![
        (
            FakeSource::once(vec![issue(1), issue(2)]),
            RecordingDispatcher {
                pr_open_issues: [1, 2].into_iter().collect(),
                ..RecordingDispatcher::default()
            },
        ),
        b,
        c,
    ];
    let report = tick_multi_with_sharding(
        &mut workspaces,
        &[0, 100, 100],
        10,
        &[false; 3],
        usize::MAX,
        false,
    );

    assert_eq!(report.skipped_pr_open, 2, "the in-slice issues hit the open-PR guard");
    assert_eq!(report.dispatched, 2, "free slots go to other repos this tick");
    assert_eq!(workspaces[1].1.dispatched, vec![10]);
    assert_eq!(workspaces[2].1.dispatched, vec![20]);
    assert_plan_has_no_slice(&report, &shards);
}

/// Acceptance 1, repo-cap form: the in-slice repo is at
/// `maxConcurrentPerRepo`, so its candidates defer and the other repos'
/// candidates take the slots in the same tick.
#[test]
fn in_slice_repo_at_its_cap_yields_the_tick_to_other_repos() {
    let keys = three_repos_one_owned_here();
    let shards = decisions(&keys);
    assert!(shards[0].owned && !shards[1].owned && !shards[2].owned);

    let [b, c] = ready_elsewhere();
    let mut workspaces = vec![
        (
            FakeSource::once(vec![issue(1), issue(2)]),
            // One live sweep: at a per-repo cap of 1.
            RecordingDispatcher {
                in_flight: [1000].into_iter().collect(),
                ..RecordingDispatcher::default()
            },
        ),
        b,
        c,
    ];
    let report = tick_multi_with_repo_cap(
        &mut workspaces,
        &[0, 100, 100],
        10.into(),
        &[false; 3],
        None,
        usize::MAX,
        false,
        Some(1),
        &[],
    );

    assert_eq!(report.deferred_repo_cap, 2, "the in-slice repo is at its cap");
    assert_eq!(report.dispatched, 2, "free slots go to other repos this tick");
    assert!(workspaces[0].1.dispatched.is_empty());
    assert_eq!(workspaces[1].1.dispatched, vec![10]);
    assert_eq!(workspaces[2].1.dispatched, vec![20]);
    assert_plan_has_no_slice(&report, &shards);
}

/// The ring no longer orders dispatch: an out-of-ring repo's ready issue
/// dispatches alongside an in-ring one, with no deferral of either.
#[test]
fn the_ring_neither_defers_nor_reorders_dispatch() {
    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(3)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi_with_sharding(
        &mut workspaces,
        &[100, 100, 100],
        10,
        &[false; 3],
        usize::MAX,
        false,
    );
    assert_eq!(report.dispatched, 3);
    let rows = ready_queue::finish(&report.queue, &[]);
    assert!(rows.iter().all(|r| r.disposition == Qd::Dispatched));
}
