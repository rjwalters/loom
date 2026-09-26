use super::*;
use serial_test::serial;

// ===================================================================
// Healthy-account transition tracking (#4344)
// ===================================================================

fn snap(total: usize, available: usize) -> capacity::RankingSnapshot {
    capacity::RankingSnapshot {
        total,
        available,
        exhausted: total - available,
        ..capacity::RankingSnapshot::default()
    }
}

#[test]
fn healthy_token_transition_dedups_by_state() {
    // The tracker only advances `prev` when the healthy count changes —
    // repeated identical ticks are no-ops (the once-per-transition contract
    // the AC requires). We assert on the carried state, since the log line
    // itself is a side effect.
    let mut prev: Option<usize> = None;

    // First observation seeds silently.
    log_healthy_token_transition(&mut prev, 6, Some(&snap(7, 6)));
    assert_eq!(prev, Some(6));

    // Stable ticks: no change.
    log_healthy_token_transition(&mut prev, 6, Some(&snap(7, 6)));
    assert_eq!(prev, Some(6));

    // Drop to token-starved (0 healthy) — a transition.
    log_healthy_token_transition(&mut prev, 0, Some(&snap(7, 0)));
    assert_eq!(prev, Some(0));

    // Recovery to a new count — another transition.
    log_healthy_token_transition(&mut prev, 4, Some(&snap(7, 4)));
    assert_eq!(prev, Some(4));

    // No-ranking fallback path (raw pool size) still tracks the count.
    log_healthy_token_transition(&mut prev, 3, None);
    assert_eq!(prev, Some(3));
}

// ===================================================================
// min_available_tokens_across_roots (issue #7527)
// ===================================================================

/// A repo-local pool exhausted by its own `.ranking` must dominate the
/// minimum even though another registered root's pool is fully healthy —
/// the exact divergence the single `fallback_root` probe (`token_limit`)
/// cannot see, since it only ever probes one directory.
#[test]
#[serial]
fn min_available_tokens_across_roots_takes_the_minimum_of_each_roots_own_pool() {
    std::env::set_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV, "");

    let healthy_root = tempfile::tempdir().unwrap();
    let exhausted_root = tempfile::tempdir().unwrap();

    let healthy_pool = healthy_root.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&healthy_pool).unwrap();
    std::fs::write(healthy_pool.join("a.token"), "key-a").unwrap();
    std::fs::write(healthy_pool.join("b.token"), "key-b").unwrap();
    std::fs::write(healthy_pool.join(".ranking"), "a|available\nb|available\n").unwrap();

    let exhausted_pool = exhausted_root.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&exhausted_pool).unwrap();
    std::fs::write(exhausted_pool.join("c.token"), "key-c").unwrap();
    std::fs::write(exhausted_pool.join(".ranking"), "c|exhausted\n").unwrap();

    let roots = vec![
        healthy_root.path().to_path_buf(),
        exhausted_root.path().to_path_buf(),
    ];
    let min = min_available_tokens_across_roots(&roots);

    std::env::remove_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV);

    assert_eq!(min, Some(0), "the exhausted root's 0-healthy pool must dominate the minimum");
}

/// Two healthy roots with different account counts: the minimum is the
/// smaller of the two, not a sum or an average.
#[test]
#[serial]
fn min_available_tokens_across_roots_picks_the_smaller_healthy_count() {
    std::env::set_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV, "");

    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();

    let pool_a = root_a.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&pool_a).unwrap();
    std::fs::write(pool_a.join("a1.token"), "key").unwrap();
    std::fs::write(pool_a.join("a2.token"), "key").unwrap();
    std::fs::write(pool_a.join(".ranking"), "a1|available\na2|available\n").unwrap();

    let pool_b = root_b.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&pool_b).unwrap();
    std::fs::write(pool_b.join("b1.token"), "key").unwrap();
    std::fs::write(pool_b.join(".ranking"), "b1|available\n").unwrap();

    let roots = vec![root_a.path().to_path_buf(), root_b.path().to_path_buf()];
    let min = min_available_tokens_across_roots(&roots);

    std::env::remove_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV);

    assert_eq!(min, Some(1));
}

/// No roots to probe -> `None`, so the caller falls back to `token_limit`
/// rather than a synthetic zero that would look like total exhaustion.
#[test]
fn min_available_tokens_across_roots_is_none_for_empty_roots() {
    assert_eq!(min_available_tokens_across_roots(&[]), None);
}

// ===================================================================
// Mock source + dispatcher
// ===================================================================

/// A fake [`WorkSource`] returning a scripted sequence of results, one per
/// `tick`. Each entry is either an `Ok(items)` or a forge `Err`.
struct FakeSource {
    results: std::collections::VecDeque<Result<Vec<WorkItem>>>,
}

impl FakeSource {
    fn once(items: Vec<WorkItem>) -> Self {
        let mut results = std::collections::VecDeque::new();
        results.push_back(Ok(items));
        Self { results }
    }
}

impl WorkSource for FakeSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        self.results.pop_front().unwrap_or_else(|| Ok(Vec::new()))
    }
}

/// A recording [`WorkDispatcher`] with a configurable in-flight set.
#[derive(Default)]
struct RecordingDispatcher {
    dispatched: Vec<u32>,
    in_flight: HashSet<u32>,
    /// Issue numbers whose dispatch should report an idempotency no-op.
    noop_issues: HashSet<u32>,
    /// Issue numbers whose dispatch should error.
    fail_issues: HashSet<u32>,
    /// Issue numbers whose dispatch should be refused by the open-PR guard
    /// (#4123) — the dispatcher returns the typed [`OpenPrDispatchError`].
    pr_open_issues: HashSet<u32>,
    /// Issue numbers whose dispatch should be refused by the park-label
    /// guard (#4444) — the dispatcher returns the typed
    /// [`ParkedIssueDispatchError`], simulating a `loom:blocked` park the
    /// candidate listing had not caught yet.
    parked_issues: HashSet<u32>,
    /// Issue numbers this dispatcher reports as quarantined (Issue #3939).
    quarantined: HashSet<u32>,
    /// Issue numbers this dispatcher reports as inside a dispatch-backoff
    /// window (Issue #4485).
    backed_off: HashSet<u32>,
    /// The subset of `backed_off` this dispatcher reports as armed
    /// specifically by the open-PR guard rather than a real dispatch
    /// failure (Issue #7606).
    pr_open_backed_off: HashSet<u32>,
    /// Issue numbers this dispatcher reports as inside a no-op
    /// re-dispatch cooldown window (Issue #6670).
    noop_cooldown: HashSet<u32>,
    /// Issue numbers this dispatcher reports as inside a hard-exclusion
    /// decline cooldown window (Issue #7528).
    declined: HashSet<u32>,
    /// Issue numbers whose dispatch should be refused by the dispatch-backoff
    /// guard (#4485) — the dispatcher returns the typed
    /// [`DispatchBackoffError`], as `SweepRegistry::dispatch` step 2.8 does
    /// when a window is armed mid-tick.
    backoff_refuse_issues: HashSet<u32>,
    /// Cumulative cross-host collision count this dispatcher reports (#4085).
    collisions: u64,
    /// Issue numbers a peer host has soft-claimed over safehouse (#4028).
    peer_claimed: HashSet<u32>,
    /// Issue numbers whose dispatch should be refused by the live-claim
    /// guard (#4556) — the dispatcher returns the typed
    /// [`LiveClaimDispatchError`], as `SweepRegistry::dispatch` step 2.9 does
    /// when a sweep process for the issue is confirmed still running while
    /// `in_flight()` cannot see it (a reverted label, a released lock, or a
    /// second daemon instance on the same host).
    live_claim_issues: HashSet<u32>,
    /// Every `(issue, complexity)` pair `dispatch` was called with (#4827),
    /// so a test can assert the REAL per-issue complexity stratum reached
    /// the dispatcher rather than the pre-#4827 `None`.
    dispatched_complexity: Vec<(u32, Option<String>)>,
    /// Issue numbers whose dispatch should be refused by the
    /// claim-then-verify-order lease guard (#6287) — the dispatcher
    /// returns the typed [`LeaseOrderDispatchError`], as
    /// `SweepRegistry::dispatch` step 4d does when this host loses a
    /// lease-order tie-break to an earlier claimant (#6350).
    lease_order_issues: HashSet<u32>,
    /// Whether this dispatcher's workspace should report itself as
    /// missing `.claude/commands/loom/sweep.md` (Issue #4027 guard 2.4,
    /// quarantined at the work-finder level by #6440).
    workspace_commands_missing: bool,
    /// Additional skip-label list this dispatcher's workspace reports
    /// (Issue #6685) — the test-fake stand-in for
    /// `resolve_extra_skip_labels_with_config`.
    extra_skip_labels: Vec<String>,
    /// Capabilities this dispatcher's host reports it holds (#6893) — the
    /// test-fake stand-in for `capability::held_capabilities()`, injected
    /// rather than read from the environment so these tests never depend on
    /// (or race on) a process-global env var.
    declared_capabilities: BTreeSet<String>,
    /// This dispatcher's host identity (#7456) — the test-fake stand-in
    /// for `crate::sweep_registry::host_identity()`, injected rather than
    /// read from the environment so these tests never depend on (or race
    /// on) process-global `LOOM_HOST_ID`/`HOSTNAME` state. Defaults to
    /// `""` (via `#[derive(Default)]`), which never equals any
    /// non-empty declared host id, but is harmless for every test that
    /// never declares a host-affinity constraint in the first place
    /// (an empty `HostConstraint` matches any host id, including `""`).
    current_host_id: String,
}

impl WorkDispatcher for RecordingDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        self.in_flight.clone()
    }
    fn quarantined(&self) -> HashSet<u32> {
        self.quarantined.clone()
    }
    fn backed_off(&self) -> HashSet<u32> {
        self.backed_off.clone()
    }
    fn pr_open_backed_off(&self) -> HashSet<u32> {
        self.pr_open_backed_off.clone()
    }
    fn noop_cooldown(&self) -> HashSet<u32> {
        self.noop_cooldown.clone()
    }
    fn declined(&self) -> HashSet<u32> {
        self.declined.clone()
    }
    fn workspace_commands_missing(&self) -> bool {
        self.workspace_commands_missing
    }
    fn collisions(&self) -> u64 {
        self.collisions
    }
    fn peer_claimed(&self) -> HashSet<u32> {
        self.peer_claimed.clone()
    }
    fn extra_skip_labels(&self) -> Vec<String> {
        self.extra_skip_labels.clone()
    }
    fn declared_capabilities(&self) -> BTreeSet<String> {
        self.declared_capabilities.clone()
    }
    fn current_host_id(&self) -> String {
        self.current_host_id.clone()
    }
    fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
        self.dispatched_complexity
            .push((issue, complexity.map(str::to_owned)));
        if self.workspace_commands_missing {
            // Mirror the production `SweepRegistry::dispatch` workspace-
            // commands guard: refuse with the typed, downcast-matchable
            // error (#4027/#6440). Only reachable in these tests via a
            // deliberate mid-tick-race fixture, since the real pre-loop
            // `workspace_commands_missing()` check should already have
            // skipped the candidate before `dispatch()` is ever called.
            return Err(WorkspaceCommandsMissingDispatchError {
                workspace: std::path::PathBuf::from("/fake/workspace"),
            }
            .into());
        }
        if self.backoff_refuse_issues.contains(&issue) {
            return Err(DispatchBackoffError {
                issue,
                consecutive: 2,
                retry_after_secs: 120,
            }
            .into());
        }
        if self.pr_open_issues.contains(&issue) {
            // Mirror the production `SweepRegistry::dispatch` open-PR guard:
            // refuse with the typed, downcast-matchable error (#4123).
            return Err(OpenPrDispatchError { issue, pr: 9999 }.into());
        }
        if self.parked_issues.contains(&issue) {
            // Mirror the production `SweepRegistry::dispatch` park-label
            // guard: refuse with the typed, downcast-matchable error (#4444).
            return Err(ParkedIssueDispatchError {
                issue,
                label: "loom:blocked".to_string(),
            }
            .into());
        }
        if self.live_claim_issues.contains(&issue) {
            // Mirror the production `SweepRegistry::dispatch` live-claim
            // guard: refuse with the typed, downcast-matchable error (#4556).
            return Err(LiveClaimDispatchError {
                issue,
                evidence: crate::live_claim::LiveClaimEvidence::SweepProcess { pid: 4242 },
            }
            .into());
        }
        if self.lease_order_issues.contains(&issue) {
            // Mirror the production `SweepRegistry::dispatch` lease-order
            // guard: refuse with the typed, downcast-matchable error
            // (#6287/#6350).
            return Err(LeaseOrderDispatchError {
                issue,
                sweep_id: format!("sweep-issue-{issue}-recording"),
                earliest_host: "peer-host".to_string(),
                earliest_sweep_id: format!("sweep-issue-{issue}-peer"),
            }
            .into());
        }
        if self.fail_issues.contains(&issue) {
            anyhow::bail!("forced dispatch failure for #{issue}");
        }
        self.dispatched.push(issue);
        Ok(!self.noop_issues.contains(&issue))
    }
}

fn issue(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".to_string()])
}

// ===================================================================
// Per-issue complexity threading (#4827)
// ===================================================================

/// A ready issue whose body carries the Curator's `<!-- loom:complexity=... -->`
/// marker — the shape `GhWorkSource::list_ready_issues` now materializes
/// from the REST listing's `body` field.
fn issue_with_complexity(n: u32, tier: &str) -> WorkItem {
    issue(n).with_body(Some(format!(
        "## Context\n\nSome body text.\n\n<!-- loom:complexity={tier} -->\n"
    )))
}

/// `WorkItem::complexity()` reads the marker out of the carried body, and
/// degrades to `None` (the unchanged `routine` stratum) when the listing
/// supplied no body or the body carries no marker.
#[test]
fn work_item_extracts_complexity_from_its_body() {
    assert_eq!(issue_with_complexity(1, "complex").complexity(), Some("complex"));
    assert_eq!(issue_with_complexity(1, "mechanical").complexity(), Some("mechanical"));
    // No body at all (a synthetic item / a listing without bodies).
    assert_eq!(issue(1).complexity(), None);
    // A body with no marker (a pre-marker issue).
    assert_eq!(issue(1).with_body(Some("no marker".into())).complexity(), None);
}

/// The core #4827 acceptance criterion for the single-workspace path: the
/// issue's REAL complexity stratum reaches `dispatch()` instead of the
/// pre-#4827 hardcoded `None`.
#[test]
fn tick_threads_per_issue_complexity_into_dispatch() {
    let mut source = FakeSource::once(vec![
        issue_with_complexity(10, "complex"),
        issue_with_complexity(11, "mechanical"),
        issue(12), // no body → None → `routine`, unchanged
    ]);
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();

    assert_eq!(report.dispatched, 3);
    assert_eq!(
        dispatcher.dispatched_complexity,
        vec![
            (10, Some("complex".to_string())),
            (11, Some("mechanical".to_string())),
            (12, None),
        ]
    );
}

/// The same criterion for the multi-workspace path, where the stratum
/// travels on `PriorityCandidate` from pass 1 (listing) to pass 2
/// (dispatch) — it must survive the global priority sort.
#[test]
fn tick_multi_threads_per_issue_complexity_into_dispatch() {
    let mut workspaces = vec![
        (
            FakeSource::once(vec![issue_with_complexity(20, "complex")]),
            RecordingDispatcher::default(),
        ),
        (
            FakeSource::once(vec![issue_with_complexity(21, "routine"), issue(22)]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut workspaces, &[0, 0], 10, &[false, false]);

    assert_eq!(report.dispatched, 3);
    assert_eq!(workspaces[0].1.dispatched_complexity, vec![(20, Some("complex".to_string()))]);
    assert_eq!(
        workspaces[1].1.dispatched_complexity,
        vec![(21, Some("routine".to_string())), (22, None)]
    );
}

// ===================================================================
// Repo-sharding slice preference (Issue #6243)
// ===================================================================

/// `tick_multi_with_saturation_brake` (and therefore `tick_multi`/
/// `tick_multi_with_admission_cap`) delegates to
/// `tick_multi_with_sharding` with `preferred_slice: None` — the
/// pre-#6243 candidate list must be byte-for-byte unaffected, so the
/// existing `tick_multi_threads_per_issue_complexity_into_dispatch` test
/// above (and every other pre-existing `tick_multi*` test) keeps passing
/// unmodified. This test pins that delegation explicitly.
#[test]
fn tick_multi_with_saturation_brake_is_a_sharding_noop() {
    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi_with_saturation_brake(
        &mut workspaces,
        &[0, 0],
        10,
        &[false, false],
        usize::MAX,
        false,
    );
    assert_eq!(report.dispatched, 2);
    assert_eq!(report.deferred_out_of_slice, 0);
    assert_eq!(workspaces[0].1.dispatched, vec![1]);
    assert_eq!(workspaces[1].1.dispatched, vec![2]);
}

/// The production mask in `spawn_multi_work_finder_task` is
/// `role_shard::decide(root).owned` per root. This pins the load-bearing
/// half of that contract for #6243: a root with NO sharding configuration
/// resolves to `ShardPosture::Unsharded`, which owns every workspace — so
/// the mask is all-`true`, every candidate is in-slice, and an unsharded
/// daemon's dispatch order is byte-for-byte pre-#6243. (The sharded
/// disjointness/coverage half is `role_shard`'s own property and is
/// covered by its test module from #6374.)
#[test]
fn unconfigured_root_is_owned_so_the_production_slice_mask_is_all_true() {
    let dir = tempfile::tempdir().unwrap();
    let decision = crate::role_shard::decide(dir.path());
    assert!(
        !decision.posture.is_sharded(),
        "an unconfigured root must resolve to Unsharded, got {:?}",
        decision.posture
    );
    assert!(
        decision.owned,
        "Unsharded must own every workspace — otherwise an unconfigured \
             host would silently defer its own repos (#6243)"
    );
}

/// #6243 AC: a dispatcher prefers in-slice candidates — an out-of-slice
/// workspace's ready issue must NOT dispatch while the in-slice
/// workspace still has an eligible candidate, even though the
/// out-of-slice workspace's priority tier would otherwise win.
#[test]
fn tick_multi_with_sharding_prefers_in_slice_over_higher_priority_out_of_slice() {
    let mut workspaces = vec![
        // Workspace 0: higher priority tier (0 < 100) but OUT of slice.
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        // Workspace 1: lower priority tier but IN slice.
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let preferred_slice = [false, true];
    let report = tick_multi_with_sharding(
        &mut workspaces,
        &[0, 100],
        10,
        &[false, false],
        usize::MAX,
        false,
        Some(&preferred_slice),
    );
    assert_eq!(report.dispatched, 1, "only the in-slice candidate dispatches this tick");
    assert_eq!(report.deferred_out_of_slice, 1);
    assert!(
        workspaces[0].1.dispatched.is_empty(),
        "out-of-slice workspace must not dispatch"
    );
    assert_eq!(workspaces[1].1.dispatched, vec![2], "in-slice workspace dispatches");
}

mod roster_fence;

/// A `preferred_slice` shorter than `workspaces` (a caller bug, or a
/// workspace added between slice computation and dispatch) must not
/// panic — a missing entry defaults to "in slice" (fail open toward the
/// pre-#6243 behavior, never toward starving a real workspace).
#[test]
fn tick_multi_with_sharding_missing_slice_entry_defaults_to_in_slice() {
    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let preferred_slice = [true]; // workspace 1 has no entry
    let report = tick_multi_with_sharding(
        &mut workspaces,
        &[0, 0],
        10,
        &[false, false],
        usize::MAX,
        false,
        Some(&preferred_slice),
    );
    assert_eq!(report.dispatched, 2);
    assert_eq!(workspaces[0].1.dispatched, vec![1]);
    assert_eq!(workspaces[1].1.dispatched, vec![2]);
}

// ===================================================================
// RegistryDispatcher — production dispatch-path regression (Issue #3967)
// ===================================================================

/// Build a real `SweepRegistry` (not the `RecordingDispatcher` test
/// fake) backed by a fixture spawn binary that records its env to a
/// sibling log and exits immediately — same pattern used by the
/// `sweep_registry.rs` and `ipc.rs` fixtures.
fn setup_registry_dispatcher_in_tempdir(
) -> (RegistryDispatcher, tempfile::TempDir, std::path::PathBuf) {
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let scripts_dir = dir.path().join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    let record_log = dir.path().join("workfinder-fake-spawn.log");
    let script = format!(
        r#"#!/usr/bin/env bash
printf 'LOOM_SWEEP_CLAIM_OWNED=%s\n' "${{LOOM_SWEEP_CLAIM_OWNED:-unset}}" >> "{rec}"
printf 'argv: %s\n' "$*" >> "{rec}"
exit 0
"#,
        rec = record_log.display()
    );
    std::fs::write(&fake_bin, script).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.spawn_bin = Some(fake_bin);
    config.skip_label_flip = true;
    config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
    let registry = Arc::new(Mutex::new(SweepRegistry::new(config)));
    (RegistryDispatcher::new(registry), dir, record_log)
}

/// Issue #3967 / #4111: the autonomous work finder's real dispatch path
/// (`RegistryDispatcher`, the `WorkDispatcher` impl wired into
/// production — as opposed to the `RecordingDispatcher` test fake used
/// by every `tick`/`tick_multi` test above) must export
/// `LOOM_SWEEP_CLAIM_OWNED=<issue>` into the spawned child's env, AND
/// (#4111) append `--claim-owned <issue>` to the child's own argv, exactly
/// like the IPC `DispatchSweep` path (`ipc.rs`) and the CLI `dispatch`
/// subcommand (`main.rs`) do. `RegistryDispatcher::dispatch` forwards to
/// `SweepRegistry::dispatch` → `spawn_child`, so this closes the
/// dispatch-path-level regression coverage across all three daemon
/// dispatch entry points — for both the env-var and the argv-flag signal.
#[test]
#[serial]
fn test_registry_dispatcher_exports_claim_ownership_marker() {
    // Issue #4044: mirrors `sweep_registry::tests::FIXTURE_CHILD_WAIT_MS`
    // (that const is private to `sweep_registry`'s test module, so it
    // can't be reused here directly). A short fixed poll bound falsely
    // reddens this test under host exec-latency pressure (syspolicyd,
    // AV scanners delaying the spawned child's launch) — the bound is a
    // ceiling on a healthy-host-cheap poll, not a promptness assertion,
    // so widening it is free.
    const FIXTURE_CHILD_WAIT_MS: u128 = 120_000;

    let (mut dispatcher, _dir, record_log) = setup_registry_dispatcher_in_tempdir();

    let was_new = dispatcher
        .dispatch(3964, None)
        .expect("dispatch should succeed");
    assert!(was_new, "expected a fresh dispatch, not an idempotency no-op");

    let start = std::time::Instant::now();
    let mut recorded = String::new();
    while start.elapsed().as_millis() < FIXTURE_CHILD_WAIT_MS {
        if let Ok(s) = std::fs::read_to_string(&record_log) {
            if s.contains("LOOM_SWEEP_CLAIM_OWNED=") {
                recorded = s;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        recorded.contains("LOOM_SWEEP_CLAIM_OWNED=3964"),
        "expected the work-finder's production RegistryDispatcher to export \
             the daemon-owned-child self-claim marker; got: {recorded:?}"
    );
    // #4111: the positional argv flag must also be present on this same
    // dispatch (belt-and-suspenders — the env var alone was proven
    // insufficient for a `/loom:sweep` child to actually notice).
    assert!(
        recorded.contains("--claim-owned 3964"),
        "expected the work-finder's production RegistryDispatcher to append \
             --claim-owned 3964 to the child argv (#4111); got: {recorded:?}"
    );
}

/// Issue #7482: `RegistryDispatcher::dispatch`'s early per-attempt log
/// line — logged BEFORE `dispatch_issue_releasing_poll_lock` runs any
/// pre-spawn guard, so at this point a dispatch has only been
/// *attempted*, not confirmed — must use the "attempting" wording, never
/// the old "dispatching" wording that falsely implied a spawn had
/// already happened.
#[test]
#[serial]
fn test_registry_dispatcher_early_line_says_attempting_not_dispatching() {
    use crate::test_log_capture as capture;

    let (mut dispatcher, _dir, _record_log) = setup_registry_dispatcher_in_tempdir();

    let records = capture::capture_logs(|| {
        let was_new = dispatcher
            .dispatch(3965, None)
            .expect("dispatch should succeed");
        assert!(was_new, "expected a fresh dispatch, not an idempotency no-op");
    });

    assert!(
        records
            .iter()
            .any(|(_, msg)| msg.contains("work_finder: attempting issue #3965")),
        "expected the renamed 'attempting' line; got: {records:?}"
    );
    assert!(
        !records
            .iter()
            .any(|(_, msg)| msg.contains("dispatching issue")),
        "the misleading pre-guard 'dispatching issue' wording must never \
             be emitted (#7482); got: {records:?}"
    );
}

// ===================================================================
// Restart survivorship — journal-seeded capacity (Issue #6262)
// ===================================================================

/// Issue #6262, the headline regression: a daemon that restarts while N
/// sweeps are still running must dispatch **at most `cap - N`** on its
/// first work-finder ticks, even when the survivors' claim locks did not
/// survive the restart (so the lock-based `reconstruct()` recovered
/// nothing) and the only remaining evidence is the machine-level sweep
/// journal.
///
/// Before the journal seed, the fresh registry reported occupancy `0`, the
/// finder saw a wholly empty cap, and it refilled to `cap` **on top of**
/// the N survivors. Three restarts in an afternoon stacked that into the
/// observed 28-running-against-a-cap-of-12 overload.
///
/// Deliberately drives the **production** `RegistryDispatcher` over a real
/// `SweepRegistry` (not the `RecordingDispatcher` fake, whose in-flight set
/// is injected by the test and so could not detect this class of bug at
/// all): the whole failure lived in how the real registry seeds occupancy
/// after a restart.
#[test]
#[serial]
fn test_first_tick_after_restart_dispatches_at_most_cap_minus_journal_survivors() {
    use crate::sweep_journal::JournalEntry;

    const CAP: usize = 5;
    const SURVIVORS: [u32; 3] = [7001, 7002, 7003];

    let (mut dispatcher, dir, _record_log) = setup_registry_dispatcher_in_tempdir();
    let root = dir.path().to_path_buf();

    // A restart survivor mid-Builder has a worktree on disk — the same
    // startup-proof signal (#4003) a live sweep shows — so occupancy counts
    // it rather than discounting it as never-started.
    for issue in SURVIVORS {
        std::fs::create_dir_all(
            root.join(".loom")
                .join("worktrees")
                .join(format!("issue-{issue}")),
        )
        .unwrap();
    }

    // Simulate the restart: a brand-new registry with NO claim locks on
    // disk (they did not survive), whose only evidence of the survivors is
    // the machine journal, each pinned to a pid that is provably alive.
    let survivors: Vec<JournalEntry> = SURVIVORS
        .iter()
        .map(|&issue| JournalEntry {
            repo: root.display().to_string(),
            issue,
            pid: std::process::id(),
            started_at: chrono::Utc::now(),
        })
        .collect();
    let registry = dispatcher.registry_for_test();
    let adopted = registry
        .lock()
        .unwrap()
        .adopt_live_journal_sweeps(&survivors);
    assert_eq!(adopted, SURVIVORS.len(), "all three survivors must seed accounting");

    // A deep backlog of fresh candidates, none of which overlap the
    // survivors — so nothing is skipped as already-in-flight and every
    // admission decision is a pure capacity decision.
    let mut source = FakeSource::once((8001..=8010).map(issue).collect());
    let report = tick(&mut source, &mut dispatcher, CAP, false).unwrap();

    assert!(
        report.dispatched <= CAP - SURVIVORS.len(),
        "the first tick after a restart must dispatch at most cap - survivors \
             ({} - {} = {}); dispatched {} (#6262)",
        CAP,
        SURVIVORS.len(),
        CAP - SURVIVORS.len(),
        report.dispatched
    );
    assert!(
        report.deferred_capacity > 0,
        "the remaining backlog must be deferred on CAPACITY (the survivors hold those \
             slots), not silently admitted; report: {report:?}"
    );
    assert_eq!(report.skipped_in_flight, 0, "no candidate overlaps a survivor");
}

/// The complement of the test above, and the guard against "fix" the
/// over-dispatch by permanently over-counting: with no survivors recorded,
/// the very same setup must still fill the whole cap. A seed that inflated
/// occupancy would starve a genuinely idle host.
#[test]
#[serial]
fn test_first_tick_after_restart_with_no_survivors_still_fills_the_cap() {
    const CAP: usize = 5;

    let (mut dispatcher, _dir, _record_log) = setup_registry_dispatcher_in_tempdir();
    let registry = dispatcher.registry_for_test();
    let adopted = registry.lock().unwrap().adopt_live_journal_sweeps(&[]);
    assert_eq!(adopted, 0);
    drop(registry);

    let mut source = FakeSource::once((8001..=8010).map(issue).collect());
    let report = tick(&mut source, &mut dispatcher, CAP, false).unwrap();

    assert_eq!(
        report.dispatched, CAP,
        "an empty journal must leave the full cap available; report: {report:?}"
    );
}

// ===================================================================
// tick — dispatch scheduling
// ===================================================================

#[test]
fn test_tick_dispatches_up_to_cap() {
    // N=5 ready issues, cap K=2 → exactly 2 dispatched this tick, 3 deferred.
    let mut source = FakeSource::once((1..=5).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 2, false).unwrap();

    assert_eq!(report.seen, 5);
    assert_eq!(report.dispatched, 2);
    assert_eq!(report.deferred_capacity, 3);
    assert_eq!(report.errors, 0);
    assert_eq!(disp.dispatched, vec![1, 2]);
}

#[test]
fn test_tick_all_dispatched_when_under_cap() {
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 3);
    assert_eq!(report.deferred_capacity, 0);
    assert_eq!(disp.dispatched, vec![1, 2, 3]);
}

// ===================================================================
// tick_with_admission_cap — per-tick ramp cap (#4234, Gap 3 of #4231)
// ===================================================================

#[test]
fn test_tick_admission_cap_limits_new_dispatches_even_under_large_concurrency_cap() {
    // 6 ready candidates, plenty of concurrency room (max_concurrent=10),
    // but the ramp cap only allows 3 *new* admissions this tick — exactly
    // the #4231 6-way-fan-out scenario: a token-axis jump could make
    // max_concurrent look like it has room for all 6, but the ramp cap
    // still bounds the burst.
    let mut source = FakeSource::once((1..=6).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick_with_admission_cap(&mut source, &mut disp, 10, false, 3).unwrap();

    assert_eq!(report.seen, 6);
    assert_eq!(report.dispatched, 3, "only the ramp cap's worth admitted");
    assert_eq!(report.deferred_ramp_cap, 3, "the rest deferred to the ramp cap, not capacity");
    assert_eq!(report.deferred_capacity, 0, "concurrency cap was never the binding constraint");
    assert_eq!(disp.dispatched, vec![1, 2, 3]);
}

#[test]
fn test_tick_admission_cap_and_concurrency_cap_are_independent_and_both_apply() {
    // Concurrency cap (2) is smaller than the ramp cap (5) here, so the
    // concurrency cap is the one that actually binds — exercising that the
    // two checks compose rather than one silently overriding the other.
    let mut source = FakeSource::once((1..=6).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick_with_admission_cap(&mut source, &mut disp, 2, false, 5).unwrap();

    assert_eq!(report.dispatched, 2);
    assert_eq!(report.deferred_capacity, 4, "concurrency cap bound first");
    assert_eq!(report.deferred_ramp_cap, 0, "ramp cap never reached — occupancy hit 2 first");
}

#[test]
fn test_tick_admission_cap_unlimited_reduces_to_plain_tick() {
    // `tick()` is a thin wrapper passing `usize::MAX` — byte-for-byte the
    // pre-#4234 unlimited-admission behavior.
    let mut source_capped = FakeSource::once((1..=4).map(issue).collect());
    let mut disp_capped = RecordingDispatcher::default();
    let capped =
        tick_with_admission_cap(&mut source_capped, &mut disp_capped, 10, false, usize::MAX)
            .unwrap();

    let mut source_plain = FakeSource::once((1..=4).map(issue).collect());
    let mut disp_plain = RecordingDispatcher::default();
    let plain = tick(&mut source_plain, &mut disp_plain, 10, false).unwrap();

    assert_eq!(capped, plain);
    assert_eq!(disp_capped.dispatched, disp_plain.dispatched);
}

#[test]
fn test_tick_multi_admission_cap_shared_across_workspaces() {
    // Two workspaces, 4 candidates total, ramp cap 2 — the cap is a single
    // shared counter across both workspaces (mirrors the concurrency cap's
    // existing shared-budget contract).
    let source_a = FakeSource::once(vec![issue(1), issue(2)]);
    let disp_a = RecordingDispatcher::default();
    let source_b = FakeSource::once(vec![issue(3), issue(4)]);
    let disp_b = RecordingDispatcher::default();
    let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

    let report = tick_multi_with_admission_cap(&mut multi, &[], 10, &[false, false], 2);

    assert_eq!(report.dispatched, 2);
    assert_eq!(report.deferred_ramp_cap, 2);
    assert_eq!(report.deferred_capacity, 0);
}

#[test]
fn test_tick_admission_cap_zero_defers_everything() {
    // A ramp cap of 0 admits nothing this tick (still distinct from
    // `halted`: `seen` reflects the backlog, no main-health warning fires).
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick_with_admission_cap(&mut source, &mut disp, 10, false, 0).unwrap();

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.deferred_ramp_cap, 3);
    assert!(disp.dispatched.is_empty());
}

// ========================================================================
// Saturation admission brake (#4903)
// ========================================================================

#[test]
fn test_saturated_host_admits_no_new_sweeps() {
    // AC1: a host at/over the load-per-core hold threshold admits nothing.
    // The cap is generous (12, the value the reported worker ran with) and
    // the backlog is deep — only the brake stops it.
    let mut source = FakeSource::once((1..=5).map(issue).collect());
    let mut disp = RecordingDispatcher::default();

    let report =
        tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true).unwrap();

    assert_eq!(report.dispatched, 0, "a saturated host must admit nothing");
    assert!(disp.dispatched.is_empty());
    assert_eq!(report.deferred_saturation, 5);
    // Attributed to the HOST, not to a cap that was nowhere near binding —
    // the whole point of a separate counter.
    assert_eq!(report.deferred_capacity, 0);
    assert_eq!(report.deferred_ramp_cap, 0);
    assert!(report.saturation_held);
    // Not a main-health halt: `halted` stays false so the operator log/status
    // never blames a red main for a load hold.
    assert!(!report.halted);
    assert_eq!(report.seen, 5, "the backlog is still observed and reported");
}

#[test]
fn test_brake_never_preempts_in_flight_sweeps() {
    // AC2: in-flight sweeps are neither killed nor counted against the brake.
    // Three sweeps are already running (the reported incident's shape); the
    // brake holds new admissions and leaves the running set exactly as it was.
    let in_flight = HashSet::from([101, 102, 103]);
    let mut source = FakeSource::once(vec![issue(1), issue(2)]);
    let mut disp = RecordingDispatcher {
        in_flight: in_flight.clone(),
        ..Default::default()
    };

    let report =
        tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true).unwrap();

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.deferred_saturation, 2);
    // The running set is untouched — the brake has no path to it at all.
    assert_eq!(
        disp.in_flight, in_flight,
        "the brake must never preempt or drop a running sweep"
    );
    // And a running sweep is never mistaken for a deferred candidate.
    assert_eq!(report.skipped_in_flight, 0);
}

#[test]
fn test_brake_holds_an_in_flight_candidate_as_in_flight_not_saturation() {
    // A ready row that is ALSO already in flight (label-flip lag) must be
    // attributed to the in-flight dedup, not swept into the brake's counter —
    // otherwise "held by saturation" would over-report on a busy host.
    let mut source = FakeSource::once(vec![issue(7), issue(8)]);
    let mut disp = RecordingDispatcher {
        in_flight: HashSet::from([7]),
        ..Default::default()
    };

    let report =
        tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true).unwrap();

    assert_eq!(report.skipped_in_flight, 1);
    assert_eq!(report.deferred_saturation, 1);
}

#[test]
fn test_healthy_host_still_reaches_its_configured_cap() {
    // AC3 (regression guard for #4512): with the brake NOT engaged, an idle
    // host fills its configured cap exactly as before. If this ever fails,
    // the brake has re-introduced the over-throttling #4512 removed.
    let mut source = FakeSource::once((1..=8).map(issue).collect());
    let mut disp = RecordingDispatcher::default();

    let report =
        tick_with_saturation_brake(&mut source, &mut disp, 8, false, usize::MAX, false).unwrap();

    assert_eq!(report.dispatched, 8, "an idle 8-core host must reach its cap");
    assert_eq!(report.deferred_saturation, 0);
    assert!(!report.saturation_held);
}

#[test]
fn test_brake_disengaged_is_byte_for_byte_the_pre_brake_path() {
    // The `false` path must be indistinguishable from the pre-#4903 wrapper,
    // so the brake can never change a healthy host's schedule.
    let ready: Vec<WorkItem> = (1..=6).map(issue).collect();

    let mut source_a = FakeSource::once(ready.clone());
    let mut disp_a = RecordingDispatcher::default();
    let before = tick_with_admission_cap(&mut source_a, &mut disp_a, 4, false, 3).unwrap();

    let mut source_b = FakeSource::once(ready);
    let mut disp_b = RecordingDispatcher::default();
    let after = tick_with_saturation_brake(&mut source_b, &mut disp_b, 4, false, 3, false).unwrap();

    assert_eq!(before, after);
    assert_eq!(disp_a.dispatched, disp_b.dispatched);
}

#[test]
fn test_brake_releases_the_moment_the_host_recovers() {
    // The hold is re-evaluated every tick and nothing latches (that is the
    // host breaker's cool-down, deliberately a different mechanism): tick 1
    // saturated holds everything, tick 2 recovered dispatches everything.
    let ready: Vec<WorkItem> = (1..=3).map(issue).collect();
    let mut disp = RecordingDispatcher::default();

    let mut hot = FakeSource::once(ready.clone());
    let held =
        tick_with_saturation_brake(&mut hot, &mut disp, 10, false, usize::MAX, true).unwrap();
    assert_eq!(held.dispatched, 0);
    assert_eq!(held.deferred_saturation, 3);

    let mut cool = FakeSource::once(ready);
    let resumed =
        tick_with_saturation_brake(&mut cool, &mut disp, 10, false, usize::MAX, false).unwrap();
    assert_eq!(resumed.dispatched, 3, "admissions resume with no cool-down");
    assert_eq!(resumed.deferred_saturation, 0);
    assert!(!resumed.saturation_held);
    assert_eq!(disp.dispatched, vec![1, 2, 3]);
}

#[test]
fn test_brake_engaged_with_empty_backlog_still_reports_held() {
    // A saturated host with nothing queued must not read as idle-healthy —
    // that indistinguishability is the reporting half of #4903.
    let mut source = FakeSource::once(vec![]);
    let mut disp = RecordingDispatcher::default();

    let report =
        tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true).unwrap();

    assert_eq!(report.deferred_saturation, 0);
    assert!(report.saturation_held, "held state must survive an empty backlog");
}

#[test]
fn test_brake_and_main_health_halt_compose_halt_wins_early_return() {
    // A red main short-circuits before the candidate loop, so nothing is
    // attributed to the brake — but the brake's engagement is still recorded
    // so status does not claim the host is fine.
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();

    let report =
        tick_with_saturation_brake(&mut source, &mut disp, 12, true, usize::MAX, true).unwrap();

    assert!(report.halted);
    assert!(report.saturation_held);
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.deferred_saturation, 0);
}

#[test]
fn test_tick_multi_brake_holds_every_workspace() {
    // The brake is daemon-global: it measures the one host every workspace's
    // sweeps run on, so a hold applies across repos at once.
    let source_a = FakeSource::once(vec![issue(1), issue(2)]);
    let disp_a = RecordingDispatcher::default();
    let source_b = FakeSource::once(vec![issue(3), issue(4)]);
    let disp_b = RecordingDispatcher::default();
    let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

    let report =
        tick_multi_with_saturation_brake(&mut multi, &[], 10, &[false, false], usize::MAX, true);

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.deferred_saturation, 4);
    assert_eq!(report.deferred_capacity, 0);
    assert!(report.saturation_held);
    assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
}

#[test]
fn test_tick_multi_healthy_host_unchanged_by_the_brake() {
    // AC3 for the production (multi-workspace) path: disengaged ⇒ the
    // pre-#4903 schedule, cap and all.
    let source_a = FakeSource::once(vec![issue(1), issue(2)]);
    let disp_a = RecordingDispatcher::default();
    let source_b = FakeSource::once(vec![issue(3), issue(4)]);
    let disp_b = RecordingDispatcher::default();
    let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

    let report =
        tick_multi_with_saturation_brake(&mut multi, &[], 10, &[false, false], usize::MAX, false);

    assert_eq!(report.dispatched, 4);
    assert_eq!(report.deferred_saturation, 0);
    assert!(!report.saturation_held);
}

#[test]
fn test_tick_multi_brake_does_not_disturb_in_flight_across_workspaces() {
    // AC2 on the multi path: every workspace's running set is preserved and
    // its occupancy is never re-attributed to the brake.
    let source_a = FakeSource::once(vec![issue(1)]);
    let disp_a = RecordingDispatcher {
        in_flight: HashSet::from([900]),
        ..Default::default()
    };
    let source_b = FakeSource::once(vec![issue(2)]);
    let disp_b = RecordingDispatcher {
        in_flight: HashSet::from([901, 902]),
        ..Default::default()
    };
    let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

    let report =
        tick_multi_with_saturation_brake(&mut multi, &[], 10, &[false, false], usize::MAX, true);

    assert_eq!(report.deferred_saturation, 2);
    assert_eq!(multi[0].1.in_flight, HashSet::from([900]));
    assert_eq!(multi[1].1.in_flight, HashSet::from([901, 902]));
}

#[test]
fn test_saturation_deferrals_reach_the_published_tick_summary() {
    // AC4's plumbing: the counter must survive into the process-global
    // summary `loom-daemon health` / `status` read back, and the summary
    // line must NAME it rather than hide it among the zeros.
    let report = TickReport {
        seen: 4,
        deferred_saturation: 4,
        saturation_held: true,
        ..TickReport::default()
    };
    publish_tick_summary(&report, 12);
    let summary = last_tick_summary().expect("a tick was just published");
    assert_eq!(summary.deferred_saturation, 4);
    assert!(summary.saturation_held);
    let line = summary.reason_summary();
    assert!(line.contains("4 deferred-saturation"), "got: {line}");
    assert!(line.contains("SATURATION-HELD"), "got: {line}");
    reset_last_tick_summary();
}

#[test]
fn test_tick_existing_occupancy_counts_against_cap() {
    // 2 already in flight, cap 3 ⇒ only 1 slot free even though 4 ready.
    let mut source = FakeSource::once(vec![issue(10), issue(11), issue(12), issue(13)]);
    let mut disp = RecordingDispatcher {
        in_flight: HashSet::from([100, 101]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 3, false).unwrap();

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.deferred_capacity, 3);
    assert_eq!(disp.dispatched, vec![10]);
}

#[test]
fn test_tick_skips_issue_already_in_registry() {
    // #7 is already in flight in the registry even though the source still
    // reports it as loom:issue (label-flip lag) — it must be skipped.
    let mut source = FakeSource::once(vec![issue(7), issue(8)]);
    let mut disp = RecordingDispatcher {
        in_flight: HashSet::from([7]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_in_flight, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![8]);
}

#[test]
fn test_tick_skips_skip_labeled_issues() {
    // Each SKIP_LABELS entry disqualifies a row even in the loom:issue list.
    let mut source = FakeSource::once(vec![
        WorkItem::new(1, vec!["loom:issue".into(), "loom:building".into()]),
        WorkItem::new(2, vec!["loom:issue".into(), "loom:blocked".into()]),
        WorkItem::new(3, vec!["loom:issue".into(), "loom:operator-only".into()]),
        issue(4),
    ]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_labeled, 3);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![4]);
}

#[test]
fn test_tick_skips_quarantined_issue() {
    // Insta-crash quarantine (#3939): a quarantined issue is skipped — never
    // dispatched — and counted in `skipped_quarantined`, while its healthy
    // siblings dispatch normally.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        quarantined: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_quarantined, 1, "#2 is quarantined");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

#[test]
fn test_tick_skips_entire_batch_when_workspace_commands_missing() {
    // Issue #6440: a workspace-level structural refusal (the #4027
    // guard) must skip EVERY ready candidate in one counter bump — not
    // call `dispatch()` once per candidate only to get the same typed
    // refusal back N times, which is the 865-refusals-in-an-hour
    // incident this issue is about.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        workspace_commands_missing: true,
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_workspace_commands_missing, 3, "all 3 candidates skipped");
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.errors, 0, "a structural skip is not an error");
    assert!(
        disp.dispatched_complexity.is_empty(),
        "dispatch() must never be called once the workspace is known to be missing \
             commands; got: {:?}",
        disp.dispatched_complexity
    );
}

#[test]
fn test_tick_dispatches_normally_once_workspace_commands_present() {
    // Regression guard: the `workspace_commands_missing` default (`false`)
    // must be a pure no-op — every existing dispatcher/fixture that never
    // sets it keeps dispatching exactly as before #6440.
    let mut source = FakeSource::once(vec![issue(1), issue(2)]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_workspace_commands_missing, 0);
    assert_eq!(report.dispatched, 2);
    assert_eq!(disp.dispatched, vec![1, 2]);
}

#[test]
fn test_tick_skips_backed_off_issue() {
    // Dispatch backoff (#4485): an issue inside its backoff window is skipped
    // — never dispatched — and counted in `skipped_backoff`, while its
    // healthy siblings dispatch normally.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        backed_off: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_backoff, 1, "#2 is inside its backoff window");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

/// Issue #7606: a backed-off issue whose window was armed by the open-PR
/// guard is attributed to the more specific `skipped_pr_open_backoff`
/// counter instead of the generic `skipped_backoff` — mutually exclusive,
/// never both.
#[test]
fn test_tick_skips_pr_open_backed_off_issue_as_its_own_counter() {
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        backed_off: HashSet::from([2]),
        pr_open_backed_off: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_pr_open_backoff, 1, "#2's window was armed by the open-PR guard");
    assert_eq!(
        report.skipped_backoff, 0,
        "must not double-count under the generic backoff-skip counter"
    );
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

#[test]
fn test_tick_backed_off_does_not_consume_capacity_slot() {
    // The backoff skip happens BEFORE the capacity gate (like quarantine), so
    // a backed-off issue never reserves a slot the healthy sibling could use.
    let mut source = FakeSource::once(vec![issue(1), issue(2)]);
    let mut disp = RecordingDispatcher {
        backed_off: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 1, false).unwrap();

    assert_eq!(report.skipped_backoff, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
}

#[test]
fn test_tick_attributes_backoff_refusal_to_skipped_backoff() {
    // A backoff window armed mid-tick (by a reap between `backed_off()` and
    // the dispatch call) surfaces as the typed `DispatchBackoffError`. That is
    // a deliberate skip, NOT a dispatch failure: it must land in
    // `skipped_backoff`, never in `errors`.
    let mut source = FakeSource::once(vec![issue(7), issue(8)]);
    let mut disp = RecordingDispatcher {
        backoff_refuse_issues: HashSet::from([7]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_backoff, 1, "#7's refusal is a backoff skip");
    assert_eq!(report.errors, 0, "a backoff refusal is never a dispatch error");
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![8]);
}

// ===================================================================
// No-op re-dispatch cooldown (Issue #6670)
// ===================================================================

#[test]
fn test_tick_skips_noop_cooldown_issue() {
    // #6670: an issue whose sweep self-reported "no actionable delta this
    // pass" (and is still inside its cooldown window) is skipped — never
    // dispatched — and counted in `skipped_noop_cooldown`, while its
    // healthy siblings dispatch normally.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        noop_cooldown: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_noop_cooldown, 1, "#2 is inside its no-op cooldown window");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

#[test]
fn test_tick_noop_cooldown_does_not_consume_capacity_slot() {
    // The no-op-cooldown skip happens BEFORE the capacity gate (like
    // quarantine and backoff), so a cooling-down issue never reserves a
    // slot the healthy sibling could use.
    let mut source = FakeSource::once(vec![issue(1), issue(2)]);
    let mut disp = RecordingDispatcher {
        noop_cooldown: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 1, false).unwrap();

    assert_eq!(report.skipped_noop_cooldown, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
}

#[test]
fn test_tick_noop_cooldown_independent_of_quarantine_and_backoff() {
    // #6670 AC: the three skip-sets are independent — an issue that is
    // BOTH quarantined AND inside its no-op cooldown is still counted
    // under each reason it actually matches, and a sibling in only one
    // of the three is skipped for exactly that one reason.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3), issue(4)]);
    let mut disp = RecordingDispatcher {
        quarantined: HashSet::from([1]),
        backed_off: HashSet::from([2]),
        noop_cooldown: HashSet::from([3]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_quarantined, 1);
    assert_eq!(report.skipped_backoff, 1);
    assert_eq!(report.skipped_noop_cooldown, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![4], "only the healthy #4 dispatches");
}

// ===================================================================
// Hard-exclusion labels + decline cooldown (Issue #7528)
// ===================================================================

/// A ready issue that also carries a hard-exclusion label — the exact
/// shape of rjwalters/kicad-tools#5197 (`loom:issue` + `external`).
fn hard_excluded_issue(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".to_string(), "external".to_string()])
}

/// The primary #7528 fix: the candidate filter applies the SAME hard
/// exclusion the Curator/Builder role prompts enforce, so the issue is
/// never dispatched at all — no claim flip, no agent session, no ~90s of
/// session budget — and it is attributed to `declined-skip` rather than
/// hidden inside `labeled-skip`.
#[test]
fn test_tick_never_dispatches_a_hard_excluded_issue() {
    let mut source = FakeSource::once(vec![issue(1), hard_excluded_issue(5197), issue(3)]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_declined, 1, "#5197 carries `external`");
    assert_eq!(report.skipped_labeled, 0, "a hard exclusion is not a park");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#5197 never dispatched");
}

/// #7528 AC ("dispatched at most once per cooldown, not once per tick"),
/// candidate-filter half: the pre-fix loop re-dispatched the same
/// `loom:issue` + `external` row on EVERY tick. Repeating the same tick
/// many times must now produce zero dispatches, every time.
#[test]
fn test_repeated_ticks_never_redispatch_a_hard_excluded_issue() {
    let mut disp = RecordingDispatcher::default();
    for _ in 0..23 {
        // 23 — the observed dispatch count in the #7528 incident report.
        let mut source = FakeSource::once(vec![hard_excluded_issue(5197)]);
        let report = tick(&mut source, &mut disp, 10, false).unwrap();
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.skipped_declined, 1);
    }
    assert!(disp.dispatched.is_empty(), "23 ticks must produce 0 dispatches, not 23 (#7528)");
}

/// The exclusion is checked before the capacity gate, like every other
/// per-issue skip reason, so an excluded candidate never reserves a slot
/// its healthy sibling could have used.
#[test]
fn test_tick_hard_exclusion_does_not_consume_capacity_slot() {
    let mut source = FakeSource::once(vec![hard_excluded_issue(1), issue(2)]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 1, false).unwrap();

    assert_eq!(report.skipped_declined, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
}

/// The reaper-armed backstop half: an issue whose previous sweep declined
/// on a hard-exclusion rule is skipped for the cooldown's duration even
/// when the label itself is no longer visible on the candidate row (the
/// route the candidate filter above cannot cover — an explicit CLI/IPC
/// dispatch, a watchdog resume, a label cleared mid-flight).
#[test]
fn test_tick_skips_an_issue_inside_its_decline_cooldown() {
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        declined: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_declined, 1, "#2 is inside its decline cooldown");
    assert_eq!(report.dispatched, 2);
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

/// The decline cooldown is independent of the other three brakes: each
/// candidate is counted under exactly the reason it matches.
#[test]
fn test_tick_decline_independent_of_quarantine_backoff_and_noop() {
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3), issue(4), issue(5)]);
    let mut disp = RecordingDispatcher {
        quarantined: HashSet::from([1]),
        backed_off: HashSet::from([2]),
        noop_cooldown: HashSet::from([3]),
        declined: HashSet::from([4]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_quarantined, 1);
    assert_eq!(report.skipped_backoff, 1);
    assert_eq!(report.skipped_noop_cooldown, 1);
    assert_eq!(report.skipped_declined, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![5], "only the healthy #5 dispatches");
}

/// Regression guard: an ordinary ready issue is UNAFFECTED by #7528. A
/// candidate carrying no hard-exclusion label and with no decline on
/// record dispatches exactly as it did before, and `declined-skip` stays
/// zero (so the counter never fires spuriously on a clean backlog).
#[test]
fn test_tick_ordinary_issues_are_unaffected_by_hard_exclusion() {
    let mut source = FakeSource::once(vec![
        issue(1),
        WorkItem::new(
            2,
            // Near-miss labels that must NOT be treated as exclusions.
            vec![
                "loom:issue".to_string(),
                "loom:curated".to_string(),
                "external-dependency".to_string(),
            ],
        ),
    ]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_declined, 0);
    assert_eq!(report.dispatched, 2);
    assert_eq!(disp.dispatched, vec![1, 2]);
}

#[test]
fn test_tick_attributes_live_claim_refusal_to_skipped_in_flight() {
    // #4556: `in_flight()` is scoped to ONE daemon process and is seeded from
    // labels/locks that a false-dead verdict may already have cleared — so an
    // issue whose sweep is genuinely still running can reach the dispatch
    // call. The registry's step-2.9 guard refuses it with the typed
    // `LiveClaimDispatchError`. That is an in-flight skip (the issue really
    // IS in flight), never a dispatch error, and it must not consume the
    // healthy candidate's slot.
    let mut source = FakeSource::once(vec![issue(4275), issue(8)]);
    let mut disp = RecordingDispatcher {
        live_claim_issues: HashSet::from([4275]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_in_flight, 1, "#4275's refusal is an in-flight skip");
    assert_eq!(report.errors, 0, "a live-claim refusal is never a dispatch error");
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![8]);
}

#[test]
fn test_tick_quarantined_does_not_consume_capacity_slot() {
    // The quarantine skip happens BEFORE the capacity gate, so a quarantined
    // issue never reserves a slot: with cap 1 and #1 quarantined, #2 gets the
    // slot rather than the tick deferring #2 behind a wasted #1 dispatch.
    let mut source = FakeSource::once(vec![issue(1), issue(2)]);
    let mut disp = RecordingDispatcher {
        quarantined: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 1, false).unwrap();

    assert_eq!(report.skipped_quarantined, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
}

#[test]
fn test_tick_multi_quarantined_workspace_does_not_starve_sibling() {
    // AC #3 (#3939): workspace A's only candidate is quarantined; workspace B
    // has a healthy candidate. With a shared cap of 1, B's issue MUST be
    // dispatched — a quarantined candidate never reserves the shared slot, so
    // healthy sibling work is not starved.
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                quarantined: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(report.skipped_quarantined, 1, "workspace A's #1 is quarantined");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty(), "quarantined workspace dispatches nothing");
    assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
}

#[test]
fn test_tick_multi_backed_off_workspace_does_not_starve_sibling() {
    // #4485, mirroring the #3939 quarantine property: workspace A's only
    // candidate is inside its dispatch-backoff window; workspace B has a
    // healthy candidate. With a shared cap of 1, B's issue MUST be dispatched
    // — a backed-off candidate never reserves the shared slot.
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                backed_off: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(report.skipped_backoff, 1, "workspace A's #1 is backed off");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty(), "backed-off workspace dispatches nothing");
    assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
}

/// Issue #7606: the multi-workspace path attributes an open-PR-guard-armed
/// backoff window the same way as the single-workspace `tick` — its own
/// `skipped_pr_open_backoff` counter, mutually exclusive with the generic
/// `skipped_backoff`.
#[test]
fn test_tick_multi_pr_open_backed_off_counted_separately() {
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                backed_off: HashSet::from([1]),
                pr_open_backed_off: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);

    assert_eq!(report.skipped_pr_open_backoff, 1, "workspace A's #1 is open-PR backed off");
    assert_eq!(report.skipped_backoff, 0, "must not also count under the generic counter");
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[1].1.dispatched, vec![10]);
}

#[test]
fn test_tick_multi_noop_cooldown_workspace_does_not_starve_sibling() {
    // #6670, mirroring the #3939 quarantine / #4485 backoff properties:
    // workspace A's only candidate is inside its no-op re-dispatch
    // cooldown window; workspace B has a healthy candidate. With a shared
    // cap of 1, B's issue MUST be dispatched — a cooling-down candidate
    // never reserves the shared slot.
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                noop_cooldown: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(report.skipped_noop_cooldown, 1, "workspace A's #1 is cooling down");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty(), "cooling-down workspace dispatches nothing");
    assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
}

#[test]
fn test_tick_multi_hard_exclusion_does_not_starve_sibling() {
    // #7528, mirroring the #3939 quarantine / #4485 backoff / #6670
    // cooldown properties: workspace A's only candidate carries a
    // hard-exclusion label; workspace B has a healthy candidate. With a
    // shared cap of 1, B's issue MUST be dispatched — and A's must never
    // be, on this or any later tick.
    let mut multi = vec![
        (
            FakeSource::once(vec![hard_excluded_issue(5197)]),
            RecordingDispatcher::default(),
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(report.skipped_declined, 1, "workspace A's #5197 carries `external`");
    assert_eq!(report.skipped_labeled, 0, "a hard exclusion is not a park");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty(), "hard-excluded workspace dispatches nothing");
    assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
}

#[test]
fn test_tick_multi_decline_cooldown_does_not_starve_sibling() {
    // The reaper-armed backstop half of #7528, same starvation property.
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                declined: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(report.skipped_declined, 1, "workspace A's #1 is inside its decline cooldown");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
}

#[test]
fn test_tick_multi_workspace_commands_missing_does_not_starve_sibling() {
    // Issue #6440, mirroring the #3939 quarantine / #4485 backoff
    // properties: workspace A is structurally broken (missing
    // .claude/commands/loom/sweep.md) with THREE ready candidates, none
    // of which may ever reserve the shared slot; workspace B has one
    // healthy candidate. With a shared cap of 1, B's issue MUST be
    // dispatched, and A's entire batch is skipped in ONE counter bump
    // (not three individual `dispatch()` calls).
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1), issue(2), issue(3)]),
            RecordingDispatcher {
                workspace_commands_missing: true,
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(
        report.skipped_workspace_commands_missing, 3,
        "all 3 of workspace A's candidates skipped in one batch"
    );
    assert_eq!(report.dispatched, 1);
    assert!(
        multi[0].1.dispatched_complexity.is_empty(),
        "the broken workspace's dispatch() must never be called"
    );
    assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
}

#[test]
fn test_tick_multi_backoff_refusal_counts_as_backoff_skip() {
    // A mid-tick backoff refusal in `tick_multi` is attributed to
    // `skipped_backoff`, not `errors` — same typed-downcast rule as `tick`.
    let mut multi = vec![(
        FakeSource::once(vec![issue(5), issue(6)]),
        RecordingDispatcher {
            backoff_refuse_issues: HashSet::from([5]),
            ..Default::default()
        },
    )];
    let report = tick_multi(&mut multi, &[], 10, &[false]);

    assert_eq!(report.skipped_backoff, 1);
    assert_eq!(report.errors, 0);
    assert_eq!(multi[0].1.dispatched, vec![6]);
}

#[test]
fn test_tick_peer_claim_skipped_under_distinct_counter() {
    // A peer host's live soft claim (#4028) skips the issue under its OWN
    // distinct counter — never folded into labeled/in-flight/quarantine — and
    // does not consume a capacity slot (checked before the cap gate), so the
    // healthy sibling issue takes the slot.
    let mut source = FakeSource::once(vec![issue(1), issue(2)]);
    let mut disp = RecordingDispatcher {
        peer_claimed: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 1, false).unwrap();

    assert_eq!(report.skipped_peer_claim, 1, "#1 is peer-claimed");
    assert_eq!(report.skipped_labeled, 0, "peer-claim is NOT a label skip");
    assert_eq!(report.skipped_in_flight, 0, "peer-claim is NOT an in-flight skip");
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "the slot goes to the un-claimed #2");
}

#[test]
fn test_tick_stops_skipping_once_peer_claim_clears() {
    // Once the peer claim lapses (empty peer_claimed set, mirroring a TTL
    // expiry / retraction), the previously-skipped issue dispatches normally.
    let mut source = FakeSource::once(vec![issue(1)]);
    let mut disp = RecordingDispatcher::default(); // no peer claims now
    let report = tick(&mut source, &mut disp, 5, false).unwrap();

    assert_eq!(report.skipped_peer_claim, 0);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![1]);
}

#[test]
fn test_tick_idempotency_noop_not_counted_as_dispatch() {
    // dispatch() returns Ok(false) (a sweep with the same key was already
    // running) — it must not count as a new dispatch nor consume a slot.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        noop_issues: HashSet::from([1]),
        ..Default::default()
    };
    // Cap of 2: #1 is a no-op (frees its slot), so #2 AND #3 still dispatch.
    let report = tick(&mut source, &mut disp, 2, false).unwrap();

    assert_eq!(report.dispatched, 2, "only #2 and #3 are new dispatches");
    assert_eq!(report.skipped_in_flight, 1, "#1 was an idempotency no-op");
    assert_eq!(report.deferred_capacity, 0);
    assert_eq!(disp.dispatched, vec![1, 2, 3]);
}

#[test]
fn test_tick_surfaces_collision_total() {
    // The dispatcher reports a cumulative cross-host collision count (#4085);
    // the tick surfaces it on the report so the per-tick summary line can log
    // the running baseline.
    let mut source = FakeSource::once(vec![issue(1)]);
    let mut disp = RecordingDispatcher {
        collisions: 3,
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 5, false).unwrap();
    assert_eq!(report.collisions, 3, "collision total surfaced from dispatcher");
    assert_eq!(report.dispatched, 1);
}

#[test]
fn test_tick_collision_total_surfaced_when_halted() {
    // Even on a halted tick (no dispatch), the running collision baseline is
    // carried forward onto the report.
    let mut source = FakeSource::once(vec![issue(1)]);
    let mut disp = RecordingDispatcher {
        collisions: 2,
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 5, true).unwrap();
    assert!(report.halted);
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.collisions, 2);
}

#[test]
fn test_tick_multi_sums_collision_totals() {
    // tick_multi sums the collision totals across every workspace's
    // dispatcher (#4085).
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                collisions: 2,
                ..Default::default()
            },
        ),
        (
            FakeSource::once(vec![issue(2)]),
            RecordingDispatcher {
                collisions: 5,
                ..Default::default()
            },
        ),
    ];
    let report = tick_multi(&mut multi, &[0, 0], 10, &[false, false]);
    assert_eq!(report.collisions, 7, "collision totals summed across workspaces");
}

#[test]
fn test_tick_dispatch_error_is_non_fatal() {
    // #2 errors; the tick still dispatches #1 and #3 and reports 1 error.
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        fail_issues: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 2);
    assert_eq!(report.errors, 1);
    assert_eq!(disp.dispatched, vec![1, 3]);
}

#[test]
fn test_tick_open_pr_refusal_counts_as_pr_open_skip_not_error() {
    // #2 has an open linked PR: `dispatch()` refuses with the typed
    // OpenPrDispatchError, which the finder attributes to `skipped_pr_open`
    // — NOT `errors` — while its siblings dispatch normally (#4123).
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        pr_open_issues: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_pr_open, 1, "#2's open-PR refusal is a pr-open-skip");
    assert_eq!(report.errors, 0, "an open-PR skip is never a dispatch error");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

/// Issue #7482: the past-tense `work_finder: dispatched issue #N` line
/// (added at this call site, logged only on a confirmed `Ok(true)` new
/// spawn) must NOT be emitted for an issue the open-PR guard (#4123)
/// subsequently refuses — that refusal never reaches the `Ok(true)` arm.
/// Reuses the same `RecordingDispatcher` open-PR-guard mock as
/// `test_tick_open_pr_refusal_counts_as_pr_open_skip_not_error` above.
#[test]
fn test_tick_open_pr_refusal_does_not_log_dispatched_line() {
    use crate::test_log_capture as capture;

    let mut source = FakeSource::once(vec![issue(2)]);
    let mut disp = RecordingDispatcher {
        pr_open_issues: HashSet::from([2]),
        ..Default::default()
    };

    let records = capture::capture_logs(|| {
        let report = tick(&mut source, &mut disp, 10, false).unwrap();
        assert_eq!(report.skipped_pr_open, 1);
        assert_eq!(report.dispatched, 0);
    });

    assert!(
        !records
            .iter()
            .any(|(_, msg)| msg.contains("work_finder: dispatched issue #2")),
        "a guard-refused issue must never log the past-tense 'dispatched' \
             line; got: {records:?}"
    );
}

/// Issue #7482: the idempotent no-op case (`Ok(false)` — a sweep with the
/// same key was already running) is neither a guard refusal nor a real
/// dispatch, and must likewise never log the past-tense `dispatched`
/// line — it belongs exclusively to the confirmed-new-spawn `Ok(true)`
/// arm. Reuses the same `noop_issues` mock as
/// `test_tick_idempotency_noop_not_counted_as_dispatch` above.
#[test]
fn test_tick_idempotency_noop_does_not_log_dispatched_line() {
    use crate::test_log_capture as capture;

    let mut source = FakeSource::once(vec![issue(1)]);
    let mut disp = RecordingDispatcher {
        noop_issues: HashSet::from([1]),
        ..Default::default()
    };

    let records = capture::capture_logs(|| {
        let report = tick(&mut source, &mut disp, 10, false).unwrap();
        assert_eq!(report.skipped_in_flight, 1);
        assert_eq!(report.dispatched, 0);
    });

    assert!(
        !records
            .iter()
            .any(|(_, msg)| msg.contains("work_finder: dispatched issue #1")),
        "an idempotency no-op must never log the past-tense 'dispatched' \
             line; got: {records:?}"
    );
}

/// Issue #7482: the mirror-positive case — a genuinely successful
/// dispatch (`Ok(true)`) MUST log the past-tense `work_finder: dispatched
/// issue #N` line, so the corrected wording still clearly signals a real
/// spawn happened (not just the absence of the misleading pre-guard
/// line).
#[test]
fn test_tick_successful_dispatch_logs_dispatched_line() {
    use crate::test_log_capture as capture;

    let mut source = FakeSource::once(vec![issue(1)]);
    let mut disp = RecordingDispatcher::default();

    let records = capture::capture_logs(|| {
        let report = tick(&mut source, &mut disp, 10, false).unwrap();
        assert_eq!(report.dispatched, 1);
    });

    assert!(
        records.iter().any(|(level, msg)| *level == log::Level::Info
            && msg.contains("work_finder: dispatched issue #1")),
        "a confirmed new dispatch must log the past-tense 'dispatched' \
             line at INFO; got: {records:?}"
    );
}

/// Issue #6350 (Ask 2): a lease-order tie-break loss (#6287) is a
/// deliberate skip, not a failure — the finder must attribute it to
/// `skipped_backoff` (the counter `dispatch()`'s own #6350 backoff-arm
/// now governs for this outcome), never `errors`, while siblings dispatch
/// normally.
#[test]
fn test_tick_lease_order_refusal_counts_as_backoff_skip_not_error() {
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        lease_order_issues: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_backoff, 1, "#2's lease-order loss is a backoff skip");
    assert_eq!(report.errors, 0, "a lease-order-loss skip is never a dispatch error");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

#[test]
fn test_park_labels_are_the_non_building_subset_of_skip_labels() {
    // #4444: the dispatch-time guard keys on PARK_LABELS, so the two
    // constants must stay in lockstep — SKIP_LABELS is exactly
    // BUILDING_LABEL + PARK_LABELS + OPERATOR_HOLD_LABEL, and PARK_LABELS
    // must never contain `loom:building` (a guard that refused it would break
    // the watchdogs' and the reaper's re-dispatch of the daemon's OWN claim).
    assert!(
        !PARK_LABELS.contains(&BUILDING_LABEL),
        "PARK_LABELS must exclude {BUILDING_LABEL}: it is legitimately present on a \
             watchdog / checkpoint-resume re-dispatch of the daemon's own claim"
    );
    for park in PARK_LABELS {
        assert!(
            SKIP_LABELS.contains(park),
            "{park} is a park label, so the work-finder query must skip it too"
        );
    }
    let mut expected: Vec<&str> = vec![BUILDING_LABEL];
    expected.extend_from_slice(PARK_LABELS);
    expected.push(OPERATOR_HOLD_LABEL);
    assert_eq!(
        SKIP_LABELS, expected,
        "SKIP_LABELS is composed as BUILDING_LABEL + PARK_LABELS + OPERATOR_HOLD_LABEL"
    );
}

// The vibesql#6664 operator-hold skip-not-park test lives in its own
// sibling file rather than being appended here: this module is over the
// 1000-line ratchet threshold and frozen at its current size
// (`.loom/docs/file-size-policy.md`), the same pattern `roster_fence`
// already uses in this file.
mod operator_hold;

#[test]
fn test_hard_exclusion_labels_are_disjoint_from_skip_labels() {
    // #7528: the hard-exclusion list is deliberately NOT folded into
    // SKIP_LABELS. Two reasons, both load-bearing:
    //
    //  1. Accounting — a SKIP_LABELS hit lands in `labeled-skip`, and
    //     conflating "a human parked this" with "this issue is not Loom's
    //     to work on yet" hides an intake backlog inside the park tally.
    //  2. `is_skipped_with_capabilities`'s #6893 exemption reasons about
    //     SKIP_LABELS membership to decide what the mechanical lane may
    //     relax; a hard exclusion must never be relaxable by anything.
    //
    // So assert the two sets stay disjoint. If a future label genuinely
    // belongs in both, that is a deliberate decision that should have to
    // edit this test.
    for excluded in crate::hard_exclusion::HARD_EXCLUSION_LABELS {
        assert!(
            !SKIP_LABELS.contains(excluded),
            "{excluded} is a hard exclusion (#7528), counted as `declined-skip`; it must \
                 not also be a SKIP_LABELS entry (`labeled-skip`)"
        );
        // And the work item's own park predicate must stay blind to it,
        // so the two filters cannot double-count one candidate.
        let item = WorkItem::new(1, vec!["loom:issue".into(), (*excluded).into()]);
        assert!(
            !item.is_skipped(),
            "the park predicate must not claim {excluded}: the dedicated #7528 filter owns it"
        );
        assert_eq!(crate::hard_exclusion::declining_label(&item.labels), Some(*excluded));
    }
}

#[test]
fn test_tick_park_label_refusal_counts_as_labeled_skip_not_error() {
    // #2 carries `loom:blocked` on the forge but the candidate listing was
    // stale: `dispatch()` refuses with the typed ParkedIssueDispatchError,
    // which the finder attributes to `skipped_labeled` — NOT `errors` — while
    // its siblings dispatch normally (#4444).
    let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
    let mut disp = RecordingDispatcher {
        parked_issues: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_labeled, 1, "#2's park refusal is a labeled-skip");
    assert_eq!(report.errors, 0, "a park-label skip is never a dispatch error");
    assert_eq!(report.skipped_pr_open, 0, "and it is not an open-PR skip either");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
}

#[test]
fn test_tick_multi_park_label_refusal_counts_as_labeled_skip() {
    // Same attribution in the multi-workspace tick (#4444).
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                parked_issues: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[0, 0], 10, &[false, false]);

    assert_eq!(report.skipped_labeled, 1);
    assert_eq!(report.errors, 0);
    assert_eq!(report.dispatched, 1);
}

#[test]
fn test_tick_skip_only_pr_open_tick_is_reported() {
    // A tick whose ONLY outcome is a pr-open-skip must still surface a
    // non-empty report (dispatched == 0, errors == 0) — the counter carries
    // the visibility, and the tick-log gate includes `skipped_pr_open` so
    // such a tick is no longer silent (#4123).
    let mut source = FakeSource::once(vec![issue(5)]);
    let mut disp = RecordingDispatcher {
        pr_open_issues: HashSet::from([5]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.errors, 0);
    assert_eq!(report.skipped_pr_open, 1);
    assert!(disp.dispatched.is_empty(), "nothing dispatched on a skip-only tick");
}

#[test]
fn test_tick_multi_open_pr_refusal_counts_as_pr_open_skip() {
    // The multi-workspace path attributes the open-PR refusal the same way
    // as the single-workspace `tick` (#4123): the epic supervisor and
    // watchdogs route through the same `dispatch()` seam, so this coverage
    // matches the guard's placement.
    let src_a = FakeSource::once(vec![issue(1), issue(2)]);
    let disp_a = RecordingDispatcher {
        pr_open_issues: HashSet::from([2]),
        ..Default::default()
    };
    let mut pairs: Vec<(FakeSource, RecordingDispatcher)> = vec![(src_a, disp_a)];
    let report = tick_multi(&mut pairs, &[], 10, &[false]);

    assert_eq!(report.skipped_pr_open, 1);
    assert_eq!(report.errors, 0);
    assert_eq!(report.dispatched, 1);
    assert_eq!(pairs[0].1.dispatched, vec![1]);
}

#[test]
fn test_tick_source_error_propagates_then_next_tick_succeeds() {
    // First tick's source errors; tick() returns Err (the loop logs it,
    // non-fatal). The second tick succeeds and dispatches normally.
    let mut results = std::collections::VecDeque::new();
    results.push_back(Err(anyhow::anyhow!("gh unavailable")));
    results.push_back(Ok(vec![issue(1), issue(2)]));
    let mut source = FakeSource { results };
    let mut disp = RecordingDispatcher::default();

    let first = tick(&mut source, &mut disp, 10, false);
    assert!(first.is_err(), "source error propagates out of the tick");
    assert_eq!(disp.dispatched.len(), 0, "no dispatch on the erroring tick");

    let second = tick(&mut source, &mut disp, 10, false).unwrap();
    assert_eq!(second.dispatched, 2, "the next tick proceeds normally");
    assert_eq!(disp.dispatched, vec![1, 2]);
}

#[test]
fn test_tick_empty_ready_is_noop() {
    let mut source = FakeSource::once(vec![]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();
    assert_eq!(report.without_occupancy(), TickReport::default());
    assert!(disp.dispatched.is_empty());
}

// ===================================================================
// tick — reactive main-health halt (Phase C, #3812)
// ===================================================================

#[test]
fn test_tick_halted_dispatches_zero_with_backlog() {
    // A red `main` (halted=true) dispatches nothing even with ample capacity
    // and a full backlog; existing in-flight sweeps are untouched.
    let mut source = FakeSource::once((1..=5).map(issue).collect());
    let mut disp = RecordingDispatcher {
        in_flight: HashSet::from([100, 101]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, true).unwrap();

    assert!(report.halted, "report must flag the halt");
    assert_eq!(report.seen, 5, "backlog is still observed");
    assert_eq!(report.dispatched, 0, "zero dispatch while halted");
    assert_eq!(report.deferred_capacity, 0);
    assert!(disp.dispatched.is_empty(), "no sweeps started while halted");
}

#[test]
fn test_tripped_host_breaker_suppresses_tick_without_aborting_running_work() {
    // The host-distress circuit breaker (#4235) is consulted at the tick
    // choke point by folding its `is_suppressed()` into the same `halted`
    // bool the main-health gate uses. This proves the two load-bearing
    // properties end-to-end: a *tripped* breaker dispatches ZERO new sweeps,
    // and the running (in-flight) sweeps are left untouched — drain, don't
    // abort.
    use crate::host_breaker::{BreakerPhase, HostBreakerConfig, SharedHostBreaker};
    let now = chrono::Utc::now();
    let breaker = SharedHostBreaker::new(HostBreakerConfig {
        enabled: true,
        load_per_core_threshold: 2.5,
        sustain_ticks: 3,
        cooldown_secs: 300,
    });
    // Not yet tripped: the breaker does not suppress, so a tick dispatches.
    assert!(!breaker.is_suppressed());

    // Three sustained over-threshold samples trip it to Open.
    breaker.observe(Some(4.0), now);
    breaker.observe(Some(4.0), now);
    breaker.observe(Some(4.0), now);
    assert_eq!(breaker.snapshot().phase, BreakerPhase::Open);
    assert!(breaker.is_suppressed(), "tripped breaker suppresses dispatch");

    // Feed the breaker's suppression into the tick's `halted` input, exactly
    // as the work-finder loop does.
    let mut source = FakeSource::once((1..=5).map(issue).collect());
    let mut disp = RecordingDispatcher {
        in_flight: HashSet::from([100, 101]),
        ..Default::default()
    };
    let halted = breaker.is_suppressed();
    let report = tick(&mut source, &mut disp, 10, halted).unwrap();

    assert!(report.halted, "a tripped breaker halts the tick");
    assert_eq!(report.seen, 5, "backlog is still observed");
    assert_eq!(report.dispatched, 0, "zero new dispatch while the breaker is open");
    assert!(disp.dispatched.is_empty(), "no new sweeps started");
    // Drain, don't abort: the two running sweeps are untouched.
    assert_eq!(disp.in_flight, HashSet::from([100, 101]));
}

#[test]
fn test_host_breaker_cooldown_release_resumes_dispatch() {
    // After the breaker cools down and releases, the tick resumes dispatch —
    // the "cool-down release" half of the Test Plan, driven through the same
    // `halted`-composition path the loop uses.
    use crate::host_breaker::{BreakerPhase, HostBreakerConfig, SharedHostBreaker};
    let t0 = chrono::Utc::now();
    let breaker = SharedHostBreaker::new(HostBreakerConfig {
        enabled: true,
        load_per_core_threshold: 2.5,
        sustain_ticks: 3,
        cooldown_secs: 300,
    });
    // Trip → Open.
    for _ in 0..3 {
        breaker.observe(Some(4.0), t0);
    }
    assert!(breaker.is_suppressed());
    // Load drops → CoolDown (still suppressed).
    breaker.observe(Some(0.1), t0 + chrono::Duration::seconds(10));
    assert_eq!(breaker.snapshot().phase, BreakerPhase::CoolDown);
    assert!(breaker.is_suppressed(), "cool-down still suppresses");
    // Cool-down elapses with acceptable load → Closed, dispatch resumes.
    breaker.observe(Some(0.1), t0 + chrono::Duration::seconds(400));
    assert_eq!(breaker.snapshot().phase, BreakerPhase::Closed);
    assert!(!breaker.is_suppressed());

    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, breaker.is_suppressed()).unwrap();
    assert!(!report.halted);
    assert_eq!(report.dispatched, 3, "dispatch resumes once the breaker releases");
    assert_eq!(disp.dispatched, vec![1, 2, 3]);
}

#[test]
fn test_tick_resumes_dispatch_once_halt_cleared() {
    // Same source shape: halted ⇒ zero, then not halted ⇒ dispatches.
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let halted = tick(&mut source, &mut disp, 10, true).unwrap();
    assert!(halted.halted);
    assert_eq!(halted.dispatched, 0);
    assert!(disp.dispatched.is_empty());

    // Next tick with the halt cleared dispatches normally.
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let resumed = tick(&mut source, &mut disp, 10, false).unwrap();
    assert!(!resumed.halted);
    assert_eq!(resumed.dispatched, 3);
    assert_eq!(disp.dispatched, vec![1, 2, 3]);
}

// ===================================================================
// tick_multi — multi-workspace fan-out (#3928)
// ===================================================================

fn err_source(msg: &'static str) -> FakeSource {
    let mut results = std::collections::VecDeque::new();
    results.push_back(Err(anyhow::anyhow!(msg)));
    FakeSource { results }
}

#[test]
fn test_tick_multi_single_workspace_matches_single_tick() {
    // Empty-registry equivalence: one workspace behaves exactly like tick().
    let mut multi =
        vec![(FakeSource::once((1..=3).map(issue).collect()), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[], 10, &[false]);
    assert_eq!(report.seen, 3);
    assert_eq!(report.dispatched, 3);
    assert_eq!(report.deferred_capacity, 0);
    assert_eq!(multi[0].1.dispatched, vec![1, 2, 3]);
}

#[test]
fn test_tick_multi_routes_to_correct_workspace() {
    // Two workspaces, each with its own ready set — dispatch must route to
    // the matching dispatcher, not aggregate onto one.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);
    assert_eq!(report.seen, 4);
    assert_eq!(report.dispatched, 4);
    assert_eq!(multi[0].1.dispatched, vec![1, 2], "workspace 0 dispatches its own issues");
    assert_eq!(multi[1].1.dispatched, vec![10, 11], "workspace 1 dispatches its own issues");
}

#[test]
fn test_tick_multi_shared_global_cap_across_workspaces() {
    // Cap 3 shared across TWO workspaces each holding 5 ready issues: the
    // COMBINED dispatch count is exactly 3, never 3-per-workspace (the token
    // pool / scratch volume are machine-level, so the budget is global).
    let mut multi = vec![
        (FakeSource::once((1..=5).map(issue).collect()), RecordingDispatcher::default()),
        (FakeSource::once((10..=14).map(issue).collect()), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 3, &[false, false]);
    let total: usize = multi.iter().map(|(_, d)| d.dispatched.len()).sum();
    assert_eq!(report.dispatched, 3, "combined dispatch never exceeds the global cap");
    assert_eq!(total, 3, "sum of per-workspace dispatches equals the global cap");
    assert_eq!(report.deferred_capacity, 7, "the remaining 7 are deferred");
}

#[test]
fn test_tick_multi_existing_occupancy_is_summed_globally() {
    // Workspace 0 already has 2 in-flight, workspace 1 has 1 in-flight;
    // global occupancy is 3, cap is 4 ⇒ only 1 free slot across both.
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1), issue(2)]),
            RecordingDispatcher {
                in_flight: HashSet::from([100, 101]),
                ..Default::default()
            },
        ),
        (
            FakeSource::once(vec![issue(10), issue(11)]),
            RecordingDispatcher {
                in_flight: HashSet::from([200]),
                ..Default::default()
            },
        ),
    ];
    let report = tick_multi(&mut multi, &[], 4, &[false, false]);
    assert_eq!(report.dispatched, 1, "3 in-flight + cap 4 ⇒ 1 free slot globally");
    assert_eq!(report.deferred_capacity, 3);
    // The single free slot goes to the first workspace's first ready issue.
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert!(multi[1].1.dispatched.is_empty());
}

#[test]
fn test_tick_multi_one_workspace_errors_others_proceed() {
    // Workspace 1's forge query fails; workspaces 0 and 2 must still be
    // polled and dispatched in the same tick (per-repo error isolation).
    let mut multi = vec![
        (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
        (err_source("bad auth for repo B"), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(30)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false, false]);
    assert_eq!(report.errors, 1, "the failing workspace is counted, not fatal");
    assert_eq!(report.dispatched, 3, "the two healthy workspaces still dispatch");
    assert_eq!(multi[0].1.dispatched, vec![1, 2]);
    assert!(multi[1].1.dispatched.is_empty(), "the erroring workspace dispatched nothing");
    assert_eq!(multi[2].1.dispatched, vec![30]);
}

#[test]
fn test_tick_multi_halted_dispatches_zero_across_workspaces() {
    let mut multi = vec![
        (FakeSource::once((1..=3).map(issue).collect()), RecordingDispatcher::default()),
        (FakeSource::once((10..=12).map(issue).collect()), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[true, true]);
    assert!(report.halted);
    assert_eq!(report.seen, 6, "backlog across both workspaces is still observed");
    assert_eq!(report.dispatched, 0, "zero dispatch while halted");
    assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
}

#[test]
fn test_tick_multi_empty_workspace_set_is_noop() {
    let mut multi: Vec<(FakeSource, RecordingDispatcher)> = vec![];
    let report = tick_multi(&mut multi, &[], 10, &[]);
    assert_eq!(report.without_occupancy(), TickReport::default());
}

#[test]
fn test_tick_multi_per_repo_gate_red_repo_halts_only_itself() {
    // Per-repo main-health gate (#3930): repo A (index 0) is red, repo B
    // (index 1) is green. A dispatches NOTHING; B still dispatches its full
    // backlog. A's backlog is still counted in `seen` for logging.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[true, false]);
    assert!(report.halted, "report.halted is set when any repo was gated");
    assert_eq!(report.seen, 4, "both repos' backlogs are observed");
    assert_eq!(report.dispatched, 2, "only repo B dispatched");
    assert!(multi[0].1.dispatched.is_empty(), "red repo A dispatched nothing");
    assert_eq!(multi[1].1.dispatched, vec![10, 11], "green repo B dispatched its backlog");
}

#[test]
fn test_tick_multi_per_repo_gate_other_repo_red_does_not_halt_us() {
    // Mirror image: repo A (index 0) is green, repo B (index 1) is red. A
    // dispatches; B does not. A red repo never halts a sibling.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, true]);
    assert!(report.halted, "a gated sibling still sets report.halted");
    assert_eq!(report.dispatched, 2, "only repo A dispatched");
    assert_eq!(multi[0].1.dispatched, vec![1, 2], "green repo A dispatched its backlog");
    assert!(multi[1].1.dispatched.is_empty(), "red repo B dispatched nothing");
}

#[test]
fn test_tick_multi_red_repo_in_flight_still_seeds_global_occupancy() {
    // A red repo's in-flight sweeps are never touched and still count toward
    // the shared global budget: repo A (red) has 3 in-flight, cap is 3, so the
    // green repo B gets ZERO free slots this tick even though A itself skips.
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1)]),
            RecordingDispatcher {
                in_flight: HashSet::from([100, 101, 102]),
                ..Default::default()
            },
        ),
        (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 3, &[true, false]);
    assert!(report.halted);
    assert_eq!(report.dispatched, 0, "occupancy already at cap from red repo's in-flight");
    assert_eq!(report.deferred_capacity, 2, "repo B's 2 ready issues deferred by the budget");
    assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
}

#[test]
fn test_tick_multi_none_halted_is_not_reported_halted() {
    // No repo gated ⇒ report.halted is false (reduces to pre-#3930 semantics).
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);
    assert!(!report.halted);
    assert_eq!(report.dispatched, 2);
}

#[test]
fn test_tick_multi_halt_survives_a_failing_forge_query() {
    // #3974 AC3: `report.halted` must be derived from the shared halt flags
    // the gate writes, never accumulated as a side effect of the
    // candidate-gathering loop. In the incident `gh` was dead in the
    // daemon's process tree, so `list_ready_issues` errored *before* the
    // loop reached its halt check — the finder then logged "main-health gate
    // cleared — resuming dispatch" in the same window the gate was logging
    // "still RED".
    let mut multi =
        vec![(err_source("gh: No user exists for uid 501"), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[], 10, &[true]);
    assert_eq!(report.errors, 1, "the forge query failed");
    assert!(
        report.halted,
        "a halted repo must still report halted when its forge query fails — \
             otherwise work_finder and main_health_gate disagree"
    );
    assert!(multi[0].1.dispatched.is_empty());
}

#[test]
fn test_tick_multi_halt_reported_even_when_repo_has_no_backlog() {
    // Same derivation property with an empty (rather than failing) source:
    // "halted" is a property of the gate state, not of what the tick saw.
    let mut multi = vec![(FakeSource::once(vec![]), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[], 10, &[true]);
    assert_eq!(report.seen, 0);
    assert!(report.halted, "halt is derived from the shared flag, not from the backlog");
}

#[test]
fn test_tick_multi_extra_halt_entries_are_ignored() {
    // A `halted` slice longer than the workspace list (a stale snapshot)
    // must not manufacture a halt for workspaces that do not exist.
    let mut multi = vec![(FakeSource::once(vec![issue(1)]), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[], 10, &[false, true, true]);
    assert!(!report.halted, "only the present workspaces' flags count");
    assert_eq!(report.dispatched, 1);
}

#[test]
fn test_tick_multi_missing_halt_entry_defaults_not_halted() {
    // A short `halted` slice (fewer entries than workspaces) treats the
    // unspecified workspaces as not halted rather than panicking.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[true]);
    assert!(report.halted);
    assert_eq!(report.dispatched, 1, "workspace 1 (no halt entry) still dispatches");
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!(multi[1].1.dispatched, vec![10]);
}

// ===================================================================
// tick_multi — cross-repo priority ordering (#3946)
// ===================================================================

fn issue_at(n: u32, created_at: &str) -> WorkItem {
    WorkItem::with_created_at(n, vec!["loom:issue".into()], Some(created_at.to_string()))
}

fn urgent_issue(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), URGENT_LABEL.into()])
}

#[test]
fn test_tick_multi_higher_priority_repo_dispatches_first_under_cap() {
    // ACCEPTANCE (#3946): the LOWER-priority repo (index 0, priority 100) has
    // OLDER and MORE candidates than the HIGHER-priority repo (index 1,
    // priority 0). Under a global cap of 2, the higher-priority repo's
    // candidates MUST dispatch first anyway — a deep/old product backlog
    // never starves a small high-priority tool repo.
    let mut multi = vec![
        (
            // Low-priority repo: 4 candidates, all OLDER (2023 timestamps).
            FakeSource::once(vec![
                issue_at(1, "2023-01-01T00:00:00Z"),
                issue_at(2, "2023-01-02T00:00:00Z"),
                issue_at(3, "2023-01-03T00:00:00Z"),
                issue_at(4, "2023-01-04T00:00:00Z"),
            ]),
            RecordingDispatcher::default(),
        ),
        (
            // High-priority repo: 2 candidates, both NEWER (2025 timestamps).
            FakeSource::once(vec![
                issue_at(50, "2025-01-01T00:00:00Z"),
                issue_at(51, "2025-01-02T00:00:00Z"),
            ]),
            RecordingDispatcher::default(),
        ),
    ];
    // priorities parallel to workspaces: repo 0 = 100 (low), repo 1 = 0 (high).
    let report = tick_multi(&mut multi, &[100, 0], 2, &[false, false]);

    assert_eq!(report.dispatched, 2, "the global cap of 2 is filled");
    assert_eq!(report.deferred_capacity, 4, "the low-priority repo's 4 are deferred");
    assert!(
        multi[0].1.dispatched.is_empty(),
        "the low-priority repo dispatches NOTHING despite older/more candidates"
    );
    assert_eq!(
        multi[1].1.dispatched,
        vec![50, 51],
        "the high-priority repo's candidates dispatch first"
    );
}

#[test]
fn test_tick_multi_urgent_beats_older_within_same_tier() {
    // Within one workspace-priority tier, `loom:urgent` sorts ahead of an
    // older / lower-numbered non-urgent sibling. Cap 1 ⇒ only the urgent one.
    let mut multi = vec![(
        FakeSource::once(vec![
            issue_at(1, "2023-01-01T00:00:00Z"), // older, non-urgent
            urgent_issue(9),                     // newer, urgent
        ]),
        RecordingDispatcher::default(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(
        multi[0].1.dispatched,
        vec![9],
        "the urgent issue outranks the older non-urgent one in the same tier"
    );
}

#[test]
fn test_tick_multi_oldest_first_within_same_tier_and_urgency() {
    // Same tier, neither urgent: oldest-first by createdAt. Cap 1 ⇒ the
    // oldest (#7, 2022) dispatches before the newer (#2, 2024).
    let mut multi = vec![(
        FakeSource::once(vec![
            issue_at(2, "2024-06-01T00:00:00Z"),
            issue_at(7, "2022-06-01T00:00:00Z"),
        ]),
        RecordingDispatcher::default(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![7], "the older issue dispatches first");
}

#[test]
fn test_tick_multi_missing_priority_entry_defaults() {
    // A short `priorities` slice (fewer entries than workspaces) treats the
    // unspecified workspaces as the default tier rather than panicking. With
    // both at the default, ordering reduces to age/number.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);
    assert_eq!(report.dispatched, 2);
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert_eq!(multi[1].1.dispatched, vec![10]);
}

#[test]
fn test_candidate_cmp_ordering() {
    use std::cmp::Ordering;
    let mk = |idx, prio, urgent, created: Option<&str>, num| PriorityCandidate {
        workspace_idx: idx,
        workspace_priority: prio,
        urgent,
        complexity: None,
        created_at: created.map(str::to_string),
        number: num,
    };

    // Priority dominates everything else: prio 0 (urgent=false, newer) still
    // beats prio 100 (urgent=true, older).
    let high = mk(0, 0, false, Some("2025-01-01T00:00:00Z"), 999);
    let low = mk(1, 100, true, Some("2000-01-01T00:00:00Z"), 1);
    assert_eq!(candidate_cmp(&high, &low), Ordering::Less);

    // Same tier: urgent before non-urgent.
    let u = mk(0, 100, true, Some("2025-01-01T00:00:00Z"), 50);
    let n = mk(0, 100, false, Some("2000-01-01T00:00:00Z"), 1);
    assert_eq!(candidate_cmp(&u, &n), Ordering::Less);

    // Same tier + same urgency: oldest-first.
    let old = mk(0, 100, false, Some("2020-01-01T00:00:00Z"), 80);
    let new = mk(0, 100, false, Some("2024-01-01T00:00:00Z"), 2);
    assert_eq!(candidate_cmp(&old, &new), Ordering::Less);

    // A dated issue sorts before an undated one (Some < None).
    let dated = mk(0, 100, false, Some("2024-01-01T00:00:00Z"), 5);
    let undated = mk(0, 100, false, None, 4);
    assert_eq!(candidate_cmp(&dated, &undated), Ordering::Less);

    // Fully-tied keys fall through to the number tiebreak (lower first).
    let a = mk(0, 100, false, None, 3);
    let b = mk(0, 100, false, None, 8);
    assert_eq!(candidate_cmp(&a, &b), Ordering::Less);
}

// ===================================================================
// WorkItem
// ===================================================================

#[test]
fn test_work_item_is_urgent() {
    assert!(!issue(1).is_urgent());
    assert!(urgent_issue(1).is_urgent());
    assert!(WorkItem::new(1, vec!["loom:issue".into(), "loom:urgent".into()]).is_urgent());
}

#[test]
fn test_work_item_is_skipped() {
    assert!(!issue(1).is_skipped());
    assert!(WorkItem::new(1, vec!["loom:building".into()]).is_skipped());
    assert!(WorkItem::new(1, vec!["loom:blocked".into()]).is_skipped());
    assert!(WorkItem::new(1, vec!["loom:operator-only".into()]).is_skipped());
    assert!(!WorkItem::new(1, vec!["loom:curated".into()]).is_skipped());
}

// ===================================================================
// Capability-aware `loom:operator-mechanical` lane (#6893)
//
// `is_skipped()` / `is_skipped_with_extra()` are deliberately untouched
// by this lane — every case below asserts the pre-#6893 behavior still
// holds through those two, and only `is_skipped_with_capabilities()`
// with a NON-EMPTY held set ever differs.
// ===================================================================

/// The capability set a host declares it holds.
fn caps(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).to_string()).collect()
}

/// A ready `loom:operator-only` + `loom:operator-mechanical` item whose
/// body carries the given capability markers.
fn mechanical_item(number: u32, markers: &[&str]) -> WorkItem {
    let body = markers
        .iter()
        .map(|m| format!("<!-- loom:capability={m} -->"))
        .collect::<Vec<_>>()
        .join("\n");
    WorkItem::new(
        number,
        vec![
            "loom:issue".into(),
            "loom:operator-only".into(),
            "loom:operator-mechanical".into(),
        ],
    )
    .with_body(Some(format!("## Task\n\nDo the thing.\n\n{body}\n")))
}

#[test]
fn test_mechanical_item_with_declared_capability_held_is_dispatchable() {
    // AC1: the whole point — a mechanical item whose declared capability
    // this worker holds is no longer unconditionally parked.
    let item = mechanical_item(1, &["host-sudo"]);
    assert!(item.is_skipped(), "the base label still parks it for every other caller");
    assert!(item.is_skipped_with_extra(&[]), "the #6685 path is unchanged");
    assert!(
        !item.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])),
        "a capability-declaring mechanical item is dispatchable to a worker that holds it"
    );
    assert_eq!(
        item.mechanical_routing(&caps(&["host-sudo"])),
        crate::capability::MechanicalRouting::ProposeDispatch,
        "AC4: the only lane it may enter is propose/dry-run mode"
    );
}

#[test]
fn test_mechanical_item_stays_parked_when_the_capability_is_not_held() {
    let item = mechanical_item(1, &["host-sudo"]);
    // No declaration at all (the default on every host).
    assert!(item.is_skipped_with_capabilities(&[], &BTreeSet::new()));
    // A different capability.
    assert!(item.is_skipped_with_capabilities(&[], &caps(&["tailnet-access"])));
    assert_eq!(
        item.mechanical_routing(&caps(&["tailnet-access"])),
        crate::capability::MechanicalRouting::MissingCapabilities {
            missing: vec!["host-sudo".to_string()]
        },
        "the gap is named, so the caller can turn the park into a capability request"
    );
}

#[test]
fn test_mechanical_capabilities_are_anded_not_ored() {
    let item = mechanical_item(1, &["host-sudo", "cloud-profile:prod-aws"]);
    assert!(
        item.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])),
        "holding one of two declared capabilities is not enough"
    );
    assert!(
        !item.is_skipped_with_capabilities(&[], &caps(&["host-sudo", "cloud-profile:prod-aws"]))
    );
}

#[test]
fn test_mechanical_item_without_a_marker_stays_parked_exactly_as_today() {
    // Fail-closed: "no capability declared" is not "no capability needed".
    let bare = WorkItem::new(
        1,
        vec![
            "loom:issue".into(),
            "loom:operator-only".into(),
            "loom:operator-mechanical".into(),
        ],
    )
    .with_body(Some("## Task\n\nRotate the deploy key.\n".to_string()));
    assert!(bare.is_skipped_with_capabilities(&[], &caps(&["host-sudo", "tailnet-access"])));
    // ...and so does one whose body was never fetched.
    let no_body = WorkItem::new(
        2,
        vec![
            "loom:operator-only".into(),
            "loom:operator-mechanical".into(),
        ],
    );
    assert!(no_body.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])));
}

#[test]
fn test_mechanical_item_with_an_unknown_capability_fails_closed() {
    // An out-of-vocabulary value is exactly as undispatchable as none, even
    // when the worker "holds" the same typo'd string.
    let item = mechanical_item(1, &["host-sudo", "root"]);
    assert!(item.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])));
    assert!(item.is_skipped_with_capabilities(&[], &caps(&["host-sudo", "root"])));
}

#[test]
fn test_other_operator_only_sub_kinds_remain_hard_skipped() {
    // #6885 non-goal, asserted directly: the other three sub-kinds are
    // unaffected — a marker in their body is ignored entirely, even for a
    // maximally-capable worker.
    let all_caps = caps(&["host-sudo", "forge-admin-token", "tailnet-access"]);
    for sub_kind in [
        "loom:operator-decision",
        "loom:operator-blocked",
        "loom:operator-objective",
    ] {
        let item = WorkItem::new(
            1,
            vec![
                "loom:issue".into(),
                "loom:operator-only".into(),
                sub_kind.into(),
            ],
        )
        .with_body(Some("<!-- loom:capability=host-sudo -->".to_string()));
        assert!(
            item.is_skipped_with_capabilities(&[], &all_caps),
            "{sub_kind} must stay hard-skipped"
        );
        assert_eq!(
            item.mechanical_routing(&all_caps),
            crate::capability::MechanicalRouting::NotApplicable,
            "{sub_kind} is not a capability-lane item at all"
        );
    }
}

#[test]
fn test_capability_exemption_never_overrides_another_skip_reason() {
    let markers = "<!-- loom:capability=host-sudo -->";
    let held = caps(&["host-sudo"]);
    // loom:blocked is an independent park with its own release condition.
    let blocked = WorkItem::new(
        1,
        vec![
            "loom:operator-only".into(),
            "loom:operator-mechanical".into(),
            "loom:blocked".into(),
        ],
    )
    .with_body(Some(markers.to_string()));
    assert!(blocked.is_skipped_with_capabilities(&[], &held));
    // loom:needs-capability is explicitly out of scope (#5817 non-goal).
    let needs_capability = WorkItem::new(
        2,
        vec![
            "loom:operator-only".into(),
            "loom:operator-mechanical".into(),
            "loom:needs-capability".into(),
        ],
    )
    .with_body(Some(markers.to_string()));
    assert!(needs_capability.is_skipped_with_capabilities(&[], &held));
    // loom:building — the daemon's own in-flight claim.
    let building = WorkItem::new(
        3,
        vec![
            "loom:operator-only".into(),
            "loom:operator-mechanical".into(),
            "loom:building".into(),
        ],
    )
    .with_body(Some(markers.to_string()));
    assert!(building.is_skipped_with_capabilities(&[], &held));
    // A repo-configured extra skip label (#6685) still wins too.
    let extra = mechanical_item(4, &["host-sudo"]);
    let extra = WorkItem::new(
        extra.number,
        [extra.labels.clone(), vec!["blocked-upstream".into()]].concat(),
    )
    .with_body(extra.body.clone());
    assert!(extra.is_skipped_with_capabilities(&["blocked-upstream".to_string()], &held));
    assert!(
        !extra.is_skipped_with_capabilities(&[], &held),
        "...and without that configured label it is dispatchable again"
    );
}

#[test]
fn test_empty_held_set_is_byte_for_byte_is_skipped_with_extra() {
    // The default on every host: the lane is completely inert.
    let extra = ["blocked-upstream".to_string()];
    for item in [
        issue(1),
        WorkItem::new(2, vec!["loom:blocked".into()]),
        WorkItem::new(3, vec!["loom:operator-only".into()]),
        WorkItem::new(4, vec!["loom:issue".into(), "blocked-upstream".into()]),
        mechanical_item(5, &["host-sudo"]),
        mechanical_item(6, &[]),
    ] {
        assert_eq!(
            item.is_skipped_with_capabilities(&[], &BTreeSet::new()),
            item.is_skipped_with_extra(&[]),
            "#{} with no extra labels",
            item.number
        );
        assert_eq!(
            item.is_skipped_with_capabilities(&extra, &BTreeSet::new()),
            item.is_skipped_with_extra(&extra),
            "#{} with an extra skip label",
            item.number
        );
    }
}

#[test]
fn test_capability_lane_does_not_touch_ordinary_items() {
    // A plain `loom:issue` row is dispatchable with or without the lane, and
    // a plain `loom:operator-only` row is parked with or without it.
    let held = caps(&["host-sudo", "forge-admin-token", "tailnet-access"]);
    assert!(!issue(1).is_skipped_with_capabilities(&[], &held));
    let operator_only = WorkItem::new(2, vec!["loom:issue".into(), "loom:operator-only".into()])
        .with_body(Some("<!-- loom:capability=host-sudo -->".to_string()));
    assert!(
        operator_only.is_skipped_with_capabilities(&[], &held),
        "a marker WITHOUT the mechanical sub-kind label changes nothing"
    );
}

// ===================================================================
// Host-affinity constraint (#7456)
// ===================================================================

/// A ready `loom:issue` item pinned to `host_id` via the `loom:host:<id>`
/// label convention.
fn host_pinned_item(number: u32, host_id: &str) -> WorkItem {
    WorkItem::new(number, vec!["loom:issue".into(), format!("loom:host:{host_id}")])
}

/// A ready `loom:issue` item pinned to `host_id` via the body-marker
/// convention.
fn host_pinned_item_via_marker(number: u32, host_id: &str) -> WorkItem {
    issue(number).with_body(Some(format!("## Task\n\n<!-- loom:requires-host={host_id} -->\n")))
}

#[test]
fn work_item_host_constraint_reads_the_label_convention() {
    let item = host_pinned_item(1, "loom-worker-2");
    assert_eq!(item.host_constraint().required, hosts_set(&["loom-worker-2"]));
}

#[test]
fn work_item_host_constraint_reads_the_marker_convention() {
    let item = host_pinned_item_via_marker(1, "loom-worker-2");
    assert_eq!(item.host_constraint().required, hosts_set(&["loom-worker-2"]));
}

#[test]
fn work_item_with_no_declaration_has_an_empty_host_constraint() {
    assert!(issue(1).host_constraint().is_empty());
}

fn hosts_set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).to_string()).collect()
}

/// AC1 (#7456): a host-pinned issue is never claimed by a non-matching
/// host — `dispatch()` is never called (`dispatched` stays empty), the
/// tick attributes the skip to its own counter, and no other skip
/// counter fires for it.
#[test]
fn tick_never_dispatches_a_host_pinned_issue_on_a_non_matching_host() {
    let mut source = FakeSource::once(vec![host_pinned_item(9, "loom-worker-2")]);
    let mut dispatcher = RecordingDispatcher {
        current_host_id: "mac-studio".to_string(),
        ..RecordingDispatcher::default()
    };
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.skipped_host_constraint, 1);
    assert_eq!(report.skipped_labeled, 0, "not the ordinary park-label path");
    assert!(dispatcher.dispatched_complexity.is_empty(), "dispatch() must never be called");
}

/// AC2: the exact same issue, on the matching host, dispatches exactly as
/// an unconstrained issue would.
#[test]
fn tick_dispatches_a_host_pinned_issue_on_the_matching_host() {
    let mut source = FakeSource::once(vec![host_pinned_item(9, "loom-worker-2")]);
    let mut dispatcher = RecordingDispatcher {
        current_host_id: "loom-worker-2".to_string(),
        ..RecordingDispatcher::default()
    };
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_host_constraint, 0);
    assert_eq!(dispatcher.dispatched_complexity, vec![(9, None)]);
}

/// The body-marker convention behaves identically to the label
/// convention on both sides of the match.
#[test]
fn tick_host_constraint_via_body_marker_skips_and_matches_identically() {
    let mut source = FakeSource::once(vec![host_pinned_item_via_marker(9, "loom-worker-2")]);
    let mut dispatcher = RecordingDispatcher {
        current_host_id: "mac-studio".to_string(),
        ..RecordingDispatcher::default()
    };
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.skipped_host_constraint, 1);

    let mut source2 = FakeSource::once(vec![host_pinned_item_via_marker(9, "loom-worker-2")]);
    let mut dispatcher2 = RecordingDispatcher {
        current_host_id: "loom-worker-2".to_string(),
        ..RecordingDispatcher::default()
    };
    let report2 = tick(&mut source2, &mut dispatcher2, 10, false).unwrap();
    assert_eq!(report2.dispatched, 1);
}

/// An issue declaring no host-affinity constraint at all dispatches on
/// every host — the default, zero-behavior-change case (#7456).
#[test]
fn tick_unconstrained_issue_dispatches_regardless_of_host_id() {
    for host_id in ["loom-worker-1", "loom-worker-2", "mac-studio", ""] {
        let mut source = FakeSource::once(vec![issue(9)]);
        let mut dispatcher = RecordingDispatcher {
            current_host_id: host_id.to_string(),
            ..RecordingDispatcher::default()
        };
        let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
        assert_eq!(report.dispatched, 1, "host_id={host_id:?}");
        assert_eq!(report.skipped_host_constraint, 0, "host_id={host_id:?}");
    }
}

/// The any-of label form (multiple `loom:host:<id>` labels) matches
/// whichever declared host is running.
#[test]
fn tick_any_of_host_labels_matches_either_declared_host() {
    let item = WorkItem::new(
        9,
        vec![
            "loom:issue".into(),
            "loom:host:loom-worker-1".into(),
            "loom:host:loom-worker-2".into(),
        ],
    );
    for matching_host in ["loom-worker-1", "loom-worker-2"] {
        let mut source = FakeSource::once(vec![item.clone()]);
        let mut dispatcher = RecordingDispatcher {
            current_host_id: matching_host.to_string(),
            ..RecordingDispatcher::default()
        };
        let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
        assert_eq!(report.dispatched, 1, "matching_host={matching_host}");
    }

    let mut source = FakeSource::once(vec![item]);
    let mut dispatcher = RecordingDispatcher {
        current_host_id: "mac-studio".to_string(),
        ..RecordingDispatcher::default()
    };
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.skipped_host_constraint, 1);
}

/// AC1's "live daemon test with two registered host ids", approximated at
/// the `tick_multi_with_sharding` integration level (two independent
/// `(source, dispatcher)` pairs, each with its own declared host id,
/// ticking over the SAME two-issue backlog): the host-pinned issue is
/// claimed by its matching workspace/host only, and the unconstrained
/// issue is claimed by whichever workspace's tick sees it first — never
/// both, and never the pinned one by the wrong host.
#[test]
fn tick_multi_with_two_hosts_only_the_matching_one_claims_the_pinned_issue() {
    let mut workspaces = vec![
        (
            FakeSource::once(vec![host_pinned_item(100, "loom-worker-2")]),
            RecordingDispatcher {
                current_host_id: "loom-worker-1".to_string(),
                ..RecordingDispatcher::default()
            },
        ),
        (
            FakeSource::once(vec![host_pinned_item(100, "loom-worker-2")]),
            RecordingDispatcher {
                current_host_id: "loom-worker-2".to_string(),
                ..RecordingDispatcher::default()
            },
        ),
    ];
    let report = tick_multi(&mut workspaces, &[0, 0], 10, &[false, false]);

    assert_eq!(report.dispatched, 1, "exactly one of the two hosts claims it");
    assert_eq!(report.skipped_host_constraint, 1, "the other host skips it");
    assert!(
        workspaces[0].1.dispatched_complexity.is_empty(),
        "loom-worker-1 (non-matching) must never call dispatch()"
    );
    assert_eq!(
        workspaces[1].1.dispatched_complexity,
        vec![(100, None)],
        "loom-worker-2 (matching) claims it"
    );
}

// ===================================================================
// Additional configurable skip-label list (Issue #6685)
// ===================================================================

#[test]
fn test_is_skipped_with_extra_matches_configured_label() {
    let item = WorkItem::new(1, vec!["loom:issue".into(), "blocked-upstream".into()]);
    assert!(!item.is_skipped(), "a repo-local label alone is not a SKIP_LABELS entry");
    assert!(
        item.is_skipped_with_extra(&["blocked-upstream".to_string()]),
        "the same label IS a skip once configured as an extra skip label"
    );
    assert!(
        !item.is_skipped_with_extra(&["some-other-label".to_string()]),
        "an unrelated extra label must not false-positive"
    );
}

#[test]
fn test_is_skipped_with_extra_empty_list_is_byte_for_byte_is_skipped() {
    assert!(!issue(1).is_skipped_with_extra(&[]));
    assert!(WorkItem::new(1, vec!["loom:blocked".into()]).is_skipped_with_extra(&[]));
}

#[test]
fn test_is_skipped_with_extra_never_needs_building_label_to_skip() {
    // Regression guard (#6685 AC3): SKIP_LABELS already includes
    // BUILDING_LABEL — is_skipped_with_extra must still report a
    // loom:building item as skipped (it's the pre-existing, intentional
    // in-flight-claim skip), and an EXTRA list containing it changes
    // nothing (still skipped, for the same original reason).
    let building = WorkItem::new(1, vec!["loom:building".into()]);
    assert!(building.is_skipped_with_extra(&[]));
    assert!(building.is_skipped_with_extra(&["loom:building".to_string()]));
}

#[test]
fn test_resolve_extra_skip_labels_precedence_and_parsing() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);

    // Default: no config, no env ⇒ empty.
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(resolve_extra_skip_labels_with_config(&cfg), Vec::<String>::new());

    // Config supplies the list.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"extraSkipLabels": ["blocked-upstream", " needs-vendor-fix ", ""]}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec![
            "blocked-upstream".to_string(),
            "needs-vendor-fix".to_string()
        ],
        "whitespace is trimmed and empty entries dropped"
    );

    // Env overrides config entirely (comma-separated).
    std::env::set_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV, "env-label-a, env-label-b ,,");
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec!["env-label-a".to_string(), "env-label-b".to_string()]
    );
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);
}

#[test]
fn test_resolve_extra_skip_labels_never_includes_building_label() {
    // Regression guard (#6685 AC3): even a misconfigured operator
    // naming `loom:building` explicitly must never come back out of
    // resolution — SKIP_LABELS' own "loom:building is never a park"
    // invariant must survive a bad config/env value.
    let cfg = WorkFinderConfig {
        extra_skip_labels: Some(vec!["loom:building".to_string(), "blocked-upstream".to_string()]),
        ..Default::default()
    };
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec!["blocked-upstream".to_string()],
        "loom:building must be filtered out of the resolved extra list"
    );

    std::env::set_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV, "loom:building,blocked-upstream");
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec!["blocked-upstream".to_string()],
        "the same guard applies to the env-var source"
    );
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);
}

#[test]
fn test_tick_skips_issue_with_configured_extra_skip_label() {
    // #6685 AC1: a workspace configured with an additional skip-label
    // (mirroring the rjwalters/vibesql#6399 `blocked-upstream` repro)
    // excludes a matching issue from dispatch candidates.
    let mut source = FakeSource::once(vec![
        WorkItem::new(1, vec!["loom:issue".into(), "blocked-upstream".into()]),
        issue(2),
    ]);
    let mut disp = RecordingDispatcher {
        extra_skip_labels: vec!["blocked-upstream".to_string()],
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_labeled, 1, "#1 carries the configured extra skip label");
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "#1 never dispatched");
}

#[test]
fn test_tick_dispatches_capability_matched_mechanical_item() {
    // #6893 AC1, end-to-end through `tick`: #1 declares `host-sudo` and
    // this host holds it (dispatched into the propose lane); #2 declares a
    // capability this host does not hold; #3 is a `loom:operator-decision`
    // item with the same marker in its body (hard-skipped, unchanged).
    let mut source = FakeSource::once(vec![
        mechanical_item(1, &["host-sudo"]),
        mechanical_item(2, &["tailnet-access"]),
        WorkItem::new(
            3,
            vec![
                "loom:issue".into(),
                "loom:operator-only".into(),
                "loom:operator-decision".into(),
            ],
        )
        .with_body(Some("<!-- loom:capability=host-sudo -->".to_string())),
    ]);
    let mut disp = RecordingDispatcher {
        declared_capabilities: caps(&["host-sudo"]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(disp.dispatched, vec![1], "only the capability-matched item dispatches");
    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_labeled, 2, "#2 and #3 stay parked");
}

#[test]
fn test_tick_parks_every_mechanical_item_when_the_host_declares_nothing() {
    // The default on every host: `declared_capabilities()` is empty, so the
    // tick behaves exactly as it did before #6893.
    let mut source = FakeSource::once(vec![mechanical_item(1, &["host-sudo"]), issue(2)]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(disp.dispatched, vec![2]);
    assert_eq!(report.skipped_labeled, 1);
}

#[test]
fn test_tick_multi_capability_declaration_is_per_workspace() {
    // Two workspaces with the identical mechanical item: only the one whose
    // host declares the capability dispatches it.
    let mut workspaces = vec![
        (
            FakeSource::once(vec![mechanical_item(1, &["host-sudo"])]),
            RecordingDispatcher {
                declared_capabilities: caps(&["host-sudo"]),
                ..Default::default()
            },
        ),
        (
            FakeSource::once(vec![mechanical_item(1, &["host-sudo"])]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut workspaces, &[0, 0], 10, &[false, false]);

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_labeled, 1);
    assert_eq!(workspaces[0].1.dispatched, vec![1]);
    assert!(workspaces[1].1.dispatched.is_empty());
}

#[test]
fn test_tick_multi_skips_issue_with_per_workspace_extra_skip_label() {
    // The per-workspace/per-repo shape of AC1: workspace A configures
    // `blocked-upstream` as an extra skip label; workspace B does not,
    // so its own issue #1 (carrying the same label name) is unaffected.
    let mut workspaces = vec![
        (
            FakeSource::once(vec![WorkItem::new(
                1,
                vec!["loom:issue".into(), "blocked-upstream".into()],
            )]),
            RecordingDispatcher {
                extra_skip_labels: vec!["blocked-upstream".to_string()],
                ..Default::default()
            },
        ),
        (
            FakeSource::once(vec![WorkItem::new(
                1,
                vec!["loom:issue".into(), "blocked-upstream".into()],
            )]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut workspaces, &[0, 0], 10, &[false, false]);

    assert_eq!(report.skipped_labeled, 1, "only workspace A's #1 is skipped");
    assert_eq!(report.dispatched, 1, "workspace B's #1 still dispatches");
    assert_eq!(workspaces[0].1.dispatched, Vec::<u32>::new());
    assert_eq!(workspaces[1].1.dispatched, vec![1]);
}

// ===================================================================
// Self-declared re-check interval marker (Issue #6685)
// ===================================================================

/// A ready issue whose body carries the `<!-- loom:recheck-interval=... -->`
/// marker and a synthetic `updatedAt`, mirroring `issue_with_complexity`'s
/// shape for the sibling marker.
fn issue_with_recheck_interval(n: u32, value: &str, updated_at: &str) -> WorkItem {
    issue(n)
        .with_body(Some(format!(
            "## Context\n\nSome body text.\n\n<!-- loom:recheck-interval={value} -->\n"
        )))
        .with_updated_at(Some(updated_at.to_string()))
}

#[test]
fn test_recheck_interval_marker_parsing() {
    assert_eq!(
        issue_with_recheck_interval(1, "6h", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(6 * 3600))
    );
    assert_eq!(
        issue_with_recheck_interval(1, "45m", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(45 * 60))
    );
    assert_eq!(
        issue_with_recheck_interval(1, "2d", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(2 * 86_400))
    );
    assert_eq!(
        issue_with_recheck_interval(1, "3600", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(3600)),
        "a bare integer defaults to seconds"
    );
    // No marker at all.
    assert_eq!(issue(1).recheck_interval(), None);
    // Malformed / zero / unrecognized unit — all degrade to None, never
    // an error.
    assert_eq!(
        issue_with_recheck_interval(1, "0h", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
    assert_eq!(
        issue_with_recheck_interval(1, "abc", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
    assert_eq!(
        issue_with_recheck_interval(1, "5w", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
    assert_eq!(
        issue_with_recheck_interval(1, "", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
}

#[test]
fn test_is_within_recheck_interval() {
    let now = chrono::Utc::now();
    let five_minutes_ago = (now - chrono::Duration::minutes(5)).to_rfc3339();
    let two_hours_ago = (now - chrono::Duration::hours(2)).to_rfc3339();

    let fresh = issue_with_recheck_interval(1, "1h", &five_minutes_ago);
    assert!(
        fresh.is_within_recheck_interval(now),
        "updated 5m ago, 1h interval ⇒ still within the window"
    );

    let stale = issue_with_recheck_interval(1, "1h", &two_hours_ago);
    assert!(
        !stale.is_within_recheck_interval(now),
        "updated 2h ago, 1h interval ⇒ elapsed, due for a recheck"
    );

    // No marker ⇒ never within the (nonexistent) interval.
    assert!(!issue(1).is_within_recheck_interval(now));

    // Marker present but no updated_at ⇒ never within the interval
    // (degrades to "always due", never silently suppresses dispatch).
    let no_timestamp = issue(1).with_body(Some("<!-- loom:recheck-interval=6h -->".to_string()));
    assert!(!no_timestamp.is_within_recheck_interval(now));
}

#[test]
fn test_tick_skips_issue_within_its_recheck_interval() {
    // #6685 AC2: a tracker issue carrying a self-declared re-check-
    // interval marker is skipped until that interval elapses.
    let now = chrono::Utc::now();
    let five_minutes_ago = (now - chrono::Duration::minutes(5)).to_rfc3339();
    let mut source = FakeSource::once(vec![
        issue_with_recheck_interval(1, "1h", &five_minutes_ago),
        issue(2),
    ]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_recheck_interval, 1, "#1 is still within its declared window");
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "#1 never dispatched");
}

#[test]
fn test_tick_dispatches_issue_once_recheck_interval_elapses() {
    let now = chrono::Utc::now();
    let two_hours_ago = (now - chrono::Duration::hours(2)).to_rfc3339();
    let mut source = FakeSource::once(vec![issue_with_recheck_interval(1, "1h", &two_hours_ago)]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_recheck_interval, 0);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![1]);
}

#[test]
fn test_recheck_interval_independent_of_noop_cooldown() {
    // #6685 AC2: the recheck-interval check must never read or be gated
    // by `noop_cooldown` state — an issue with NO recheck-interval
    // marker but an ACTIVE noop_cooldown is still skipped (for the
    // noop_cooldown reason, not recheck_interval), and an issue WITH a
    // fresh recheck-interval marker but NO noop_cooldown record is
    // skipped for the recheck_interval reason alone.
    let now = chrono::Utc::now();
    let five_minutes_ago = (now - chrono::Duration::minutes(5)).to_rfc3339();
    let mut source = FakeSource::once(vec![
        issue(1), // plain issue, no marker, in noop_cooldown
        issue_with_recheck_interval(2, "1h", &five_minutes_ago), // fresh marker, no noop_cooldown record
    ]);
    let mut disp = RecordingDispatcher {
        noop_cooldown: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_noop_cooldown, 1, "#1 skipped via noop_cooldown alone");
    assert_eq!(report.skipped_recheck_interval, 1, "#2 skipped via recheck_interval alone");
    assert_eq!(report.dispatched, 0);
    assert!(disp.dispatched.is_empty());
}

// ===================================================================
// Env-var configuration
// ===================================================================

#[test]
#[serial]
fn test_enabled_off_by_default() {
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    assert!(!enabled(), "unset ⇒ disabled (zero behavior change)");
}

#[test]
#[serial]
fn test_enabled_truthy_values() {
    for v in ["1", "true", "yes", "on", "TRUE", "On", " Yes "] {
        std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
        assert!(enabled(), "{v:?} should enable");
    }
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_enabled_falsy_values() {
    for v in ["0", "false", "no", "off", "", "maybe"] {
        std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
        assert!(!enabled(), "{v:?} should not enable");
    }
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_interval_default_and_override() {
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));

    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "120");
    assert_eq!(resolve_interval(), Duration::from_secs(120));

    // Zero and unparseable fall back to the default.
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "garbage");
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
}

#[test]
#[serial]
fn test_resolve_max_concurrent_default_and_override() {
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);

    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "7");
    assert_eq!(resolve_max_concurrent(), 7);

    // Zero and unparseable fall back to the default.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
    assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
}

// ===================================================================
// resolve_dynamic_max_concurrent — Phase B work-driven policy (#3811),
// per-token concurrency factor (#3947), CPU term REMOVED (#4512)
// ===================================================================

#[test]
fn test_dynamic_cap_is_min_of_three_inputs() {
    // Never exceeds any bound. `usize::MAX` ram = the term doesn't bind.
    assert_eq!(resolve_dynamic_max_concurrent(10, usize::MAX, 10), 10);
    assert_eq!(resolve_dynamic_max_concurrent(3, usize::MAX, 9), 3, "disk binds");
    assert_eq!(resolve_dynamic_max_concurrent(9, usize::MAX, 4), 4, "maxConcurrent binds");
}

#[test]
fn test_dynamic_cap_has_no_cpu_term_at_all() {
    // #4512 AC1: the arity itself is the guard (a >3-arg call no longer
    // compiles), and the value must be independent of host CPU state: this
    // same input yields the same cap on a 95%-idle 8-core worker and on a
    // saturated one, which is the whole point — an idle host must not be
    // throttled to 2 by an estimate (`estCoresPerSweep`) that priced every
    // sweep as a build.
    assert_eq!(resolve_dynamic_max_concurrent(36, usize::MAX, 10), 10);
}

#[test]
fn test_dynamic_cap_has_no_token_axis_term_either() {
    // #5270 AC1: a starved token pool (few/no healthy accounts) must NOT
    // cap the dynamic concurrency — only disk headroom, RAM headroom, and
    // the configured ceiling do. The arity itself is the guard (a >3-arg
    // call no longer compiles); this asserts the *value* is independent of
    // any token-pool state, unlike the pre-#5270 formula which would have
    // pinned this to (near-)zero when accounts were exhausted.
    assert_eq!(
        resolve_dynamic_max_concurrent(36, usize::MAX, 10),
        10,
        "disk (36) and ceiling (10) alone determine the cap"
    );
}

#[test]
fn test_dynamic_cap_disk_headroom_bound() {
    // A nearly-full scratch volume (disk headroom 1) caps concurrency at 1
    // even with a high ceiling.
    assert_eq!(resolve_dynamic_max_concurrent(1, usize::MAX, 8), 1);
    // A full volume (0 headroom) drops the cap to 0 — dispatch nothing.
    // Disk meters an exhaustible resource, so this hard floor stays.
    assert_eq!(resolve_dynamic_max_concurrent(0, usize::MAX, 8), 0);
}

#[test]
fn test_dynamic_cap_ram_headroom_bound() {
    // #5270 AC3: critically-low available RAM caps concurrency exactly the
    // way disk headroom already does — same posture, same hard floor.
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 1, 8), 1, "ram binds");
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 0, 8), 0, "ram exhausted");
    // Whichever of disk/ram is smaller binds, regardless of position.
    assert_eq!(resolve_dynamic_max_concurrent(2, 5, 100), 2, "disk (2) < ram (5)");
    assert_eq!(resolve_dynamic_max_concurrent(5, 2, 100), 2, "ram (2) < disk (5)");
}

#[test]
fn test_dynamic_cap_zero_disk_dispatches_nothing() {
    // No disk headroom ⇒ cap 0 ⇒ a subsequent tick dispatches nothing.
    let cap = resolve_dynamic_max_concurrent(0, usize::MAX, 10);
    assert_eq!(cap, 0);
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, cap, false).unwrap();
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.deferred_capacity, 3);
    assert!(disp.dispatched.is_empty());
}

#[test]
fn test_dynamic_cap_zero_ram_dispatches_nothing() {
    // No available RAM ⇒ cap 0 ⇒ a subsequent tick dispatches nothing —
    // mirrors test_dynamic_cap_zero_disk_dispatches_nothing exactly.
    let cap = resolve_dynamic_max_concurrent(usize::MAX, 0, 10);
    assert_eq!(cap, 0);
    let mut source = FakeSource::once((1..=3).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, cap, false).unwrap();
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.deferred_capacity, 3);
    assert!(disp.dispatched.is_empty());
}

// ===================================================================
// Eager reclaim trigger (#7512) — the coupling between this loop's
// 60s disk read and the worktree reaper's 15-minute reclaim cadence.
// These mirror the dynamic-cap tests directly above: same inputs, but
// asserting *whether reclaim is attempted* rather than the cap value.
// ===================================================================

#[test]
fn test_eager_reclaim_fires_when_disk_is_about_to_zero_the_cap() {
    // The #7512 headline case: disk 0, RAM and ceiling both healthy. The
    // cap is about to be finalized as 0, so the eager pass must be
    // attempted first.
    assert_eq!(resolve_dynamic_max_concurrent(0, 8, 10), 0);
    assert!(crate::eager_reclaim::should_trigger(false, 0, 8, 10));
}

#[test]
fn test_eager_reclaim_fires_when_disk_merely_binds_the_cap_down() {
    // Disk has not reached 0 yet but is already costing dispatch slots
    // (cap 1 instead of 8). Reclaiming here is what keeps it off 0.
    assert_eq!(resolve_dynamic_max_concurrent(1, 8, 10), 1);
    assert!(crate::eager_reclaim::should_trigger(false, 1, 8, 10));
}

#[test]
fn test_eager_reclaim_does_not_fire_on_an_ordinary_tick() {
    // Healthy disk: the ceiling binds, not disk. No reclaim — this is the
    // overwhelmingly common tick and it must stay side-effect-free.
    assert_eq!(resolve_dynamic_max_concurrent(36, usize::MAX, 10), 10);
    assert!(!crate::eager_reclaim::should_trigger(false, 36, usize::MAX, 10));
}

#[test]
fn test_eager_reclaim_does_not_fire_when_ram_is_the_binding_axis() {
    // Cap is 1, but disk is not why — freeing disk would not buy a single
    // slot, so the forge/docker round-trips would be pure waste.
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 1, 8), 1);
    assert!(!crate::eager_reclaim::should_trigger(false, usize::MAX, 1, 8));
}

#[test]
fn test_eager_reclaim_does_not_fire_on_an_unmeasurable_disk_probe() {
    // `disk_headroom_limit` returns usize::MAX when `df` is unmeasurable
    // (#4164, unknown != zero) and the cap is left unclamped by disk — so
    // the eager pass must not fire either.
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, usize::MAX, 10), 10);
    assert!(!crate::eager_reclaim::should_trigger(false, usize::MAX, usize::MAX, 10));
}

#[test]
fn test_eager_reclaim_fires_once_per_crossing_not_once_per_tick() {
    // The exact property that keeps the 60s dispatch loop from becoming a
    // forge-polling loop while a disk stays full for a reason the daemon
    // cannot reclaim (a dataset, another tenant): ten consecutive
    // zero-disk ticks trigger exactly ONE eager pass.
    let mut was_binding = false;
    let mut fired = 0usize;
    for _ in 0..10 {
        if crate::eager_reclaim::should_trigger(was_binding, 0, 8, 10) {
            fired += 1;
        }
        // Post-reclaim re-probe still reads 0 — nothing was reclaimable.
        was_binding = crate::eager_reclaim::disk_axis_binds_cap_down(0, 8, 10);
        assert_eq!(resolve_dynamic_max_concurrent(0, 8, 10), 0, "cap stays 0 either way");
    }
    assert_eq!(fired, 1, "edge-triggered, not level-triggered");
}

#[test]
fn test_eager_reclaim_rearms_after_a_successful_reclaim() {
    // A pass that actually frees space takes the disk term back above the
    // binding threshold; a LATER drop is a genuine new crossing and must
    // fire again rather than being suppressed forever.
    let mut was_binding = false;
    assert!(crate::eager_reclaim::should_trigger(was_binding, 0, 8, 10));
    // Post-reclaim re-probe: 12 GB-worth of headroom recovered.
    was_binding = crate::eager_reclaim::disk_axis_binds_cap_down(12, 8, 10);
    assert!(!was_binding);
    assert_eq!(resolve_dynamic_max_concurrent(12, 8, 10), 8, "cap recovered to the ram term");
    // Hours later it fills up again.
    assert!(crate::eager_reclaim::should_trigger(was_binding, 0, 8, 10));
}

#[test]
fn test_dynamic_cap_unbounded_by_max_concurrent_default() {
    // A machine whose operator has NOT tuned the knob rides the shipped
    // default rather than an estimate of its cores or its token pool.
    assert_eq!(
        resolve_dynamic_max_concurrent(100, usize::MAX, DEFAULT_WORK_FINDER_MAX_CONCURRENT),
        DEFAULT_WORK_FINDER_MAX_CONCURRENT
    );
}

// ===================================================================
// Dynamic cap composed with tick — scale-up / scale-to-zero (#3811)
// ===================================================================

#[test]
fn test_scale_up_with_growing_backlog_bounded_by_dynamic_cap() {
    // Fixed resources: disk=4, ceiling=10 ⇒ dynamic cap 4. As the backlog
    // grows tick-over-tick, effective concurrency scales up but is bounded
    // by the cap (min(cap, backlog)).
    let cap = resolve_dynamic_max_concurrent(4, usize::MAX, 10);
    assert_eq!(cap, 4);

    // Backlog 2 (< cap): all 2 dispatch, nothing deferred.
    let mut source = FakeSource::once((1..=2).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, cap, false).unwrap();
    assert_eq!(report.dispatched, 2, "backlog 2 < cap 4 ⇒ 2 dispatched");
    assert_eq!(report.deferred_capacity, 0);

    // Backlog 6 (> cap): scales up to the cap (4), defers the surplus (2).
    let mut source = FakeSource::once((10..=15).map(issue).collect());
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, cap, false).unwrap();
    assert_eq!(report.dispatched, 4, "backlog 6 > cap 4 ⇒ scaled up to cap");
    assert_eq!(report.deferred_capacity, 2);
}

#[test]
fn test_scale_to_zero_on_empty_backlog() {
    // Even with ample resources (cap 5), an empty backlog dispatches nothing
    // — no capacity is pre-reserved and no idle workers are spawned.
    let cap = resolve_dynamic_max_concurrent(5, usize::MAX, 5);
    assert_eq!(cap, 5);
    let mut source = FakeSource::once(vec![]);
    let mut disp = RecordingDispatcher::default();
    let report = tick(&mut source, &mut disp, cap, false).unwrap();
    assert_eq!(report.without_occupancy(), TickReport::default(), "no activity");
    assert!(disp.dispatched.is_empty());
}

// ===================================================================
// Config-file surface — read_work_finder_config soft-fail (#3813)
// ===================================================================

fn write_config(dir: &Path, body: &str) {
    let loom_dir = dir.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    std::fs::write(loom_dir.join("config.json"), body).unwrap();
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_file_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_malformed_json_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "{not valid json");
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_autonomous_block_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"terminals": []}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_work_finder_block_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
fn test_config_full_block_is_parsed() {
    let tmp = tempfile::tempdir().unwrap();
    // `perTokenConcurrency` is deliberately included here even though it is
    // retired (#5743) and no longer has a corresponding `WorkFinderConfig`
    // field — this is exactly the "a fleet host still has the old key in its
    // committed config" scenario, and parsing must silently ignore it (a
    // `serde_json::Value` walk never errors on an unread key) rather than
    // failing to start.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"perTokenConcurrency": 4, "cpuUtilizationTarget": 0.6, "estCoresPerSweep": 3.5, "workFinder": {"enabled": true, "intervalSecs": 90, "maxConcurrent": 5, "maxAdmissionsPerTick": 4}}}"#,
    );
    assert_eq!(
        read_work_finder_config(tmp.path()),
        WorkFinderConfig {
            enabled: Some(true),
            interval_secs: Some(90),
            max_concurrent: Some(5),
            max_admissions_per_tick: Some(4),
            extra_skip_labels: None,
            // Retired keys are recorded (accepted-but-ignored), not parsed.
            deprecated_cpu_keys: vec!["cpuUtilizationTarget", "estCoresPerSweep"],
        }
    );
}

// ===================================================================
// Retired cpuUtilizationTarget / estCoresPerSweep knobs: accepted but
// IGNORED, never a config error (#4512, replacing the #4032 parsing tests)
// ===================================================================

#[test]
fn test_deprecated_cpu_knobs_are_accepted_and_recorded_not_parsed() {
    // A fleet upgrades the daemon binary before it edits every repo's
    // committed config, so a stale key must parse fine and simply do nothing
    // — recorded only so the deprecation warning can name it.
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"cpuUtilizationTarget": 0.5, "estCoresPerSweep": 1.5,
                "workFinder": {"enabled": true, "maxConcurrent": 10}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.deprecated_cpu_keys, vec!["cpuUtilizationTarget", "estCoresPerSweep"]);
    // The live knobs in the same block still parse normally: a deprecated
    // sibling must never poison the rest of the config.
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.max_concurrent, Some(10));
}

#[test]
#[serial(loom_config_env)]
fn test_deprecated_cpu_knobs_accepted_at_any_value_including_nonsense() {
    // Pre-#4512 these were range-filtered/type-checked because a value was
    // consumed. Nothing consumes them now, so out-of-range, wrong-type, and
    // even absurd values are all equally inert — and equally non-fatal.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    for body in [
        r#"{"autonomous": {"cpuUtilizationTarget": 0}}"#,
        r#"{"autonomous": {"cpuUtilizationTarget": 1.5}}"#,
        r#"{"autonomous": {"estCoresPerSweep": -2}}"#,
        r#"{"autonomous": {"estCoresPerSweep": "many"}}"#,
        r#"{"autonomous": {"estCoresPerSweep": true}}"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), body);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.deprecated_cpu_keys.len(), 1, "must be accepted-but-noted: {body}");
        // And it must not disturb any live knob.
        assert_eq!(cfg.max_concurrent, None);
        assert_eq!(cfg.enabled, None);
    }
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

#[test]
fn test_deprecated_cpu_knobs_absent_or_null_are_not_reported() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 4}}}"#);
    assert!(read_work_finder_config(tmp.path())
        .deprecated_cpu_keys
        .is_empty());

    // An explicit `null` is "not set" — warning about it would be noise.
    let tmp2 = tempfile::tempdir().unwrap();
    write_config(tmp2.path(), r#"{"autonomous": {"estCoresPerSweep": null}}"#);
    assert!(read_work_finder_config(tmp2.path())
        .deprecated_cpu_keys
        .is_empty());
}

#[test]
fn test_warn_deprecated_cpu_knobs_is_a_noop_without_any_retired_setting() {
    // No config keys, no env vars — nothing to warn about, and (crucially)
    // no panic and no config error. The one-shot `Once` inside means this
    // test cannot assert the log line itself; the observable contract is
    // that it is safe and side-effect-free on the clean path.
    warn_deprecated_cpu_knobs(&WorkFinderConfig::default());
}

#[test]
#[serial]
fn test_deprecated_cpu_knob_notice_is_none_when_nothing_is_set() {
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
    assert!(deprecated_cpu_knob_notice(&WorkFinderConfig::default()).is_none());
}

#[test]
#[serial]
fn test_deprecated_cpu_knob_notice_names_the_config_keys_that_are_set() {
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
    let cfg = WorkFinderConfig {
        deprecated_cpu_keys: vec!["estCoresPerSweep"],
        ..Default::default()
    };
    let notice = deprecated_cpu_knob_notice(&cfg).expect("a set key must produce a notice");
    assert!(notice.contains("estCoresPerSweep"), "must name the key: {notice}");
    assert!(notice.contains("IGNORED"), "must say it is ignored: {notice}");
    // Actionable: it must point at the knob that replaced it.
    assert!(
        notice.contains("autonomous.workFinder.maxConcurrent"),
        "must name the replacement knob: {notice}"
    );
    // A config-only notice must not fabricate an env source.
    assert!(!notice.contains("env LOOM_"), "no env source was set: {notice}");
}

#[test]
#[serial]
fn test_deprecated_cpu_knob_notice_names_env_vars_and_combines_both_sources() {
    std::env::set_var(DEPRECATED_CPU_ENV_VARS[0], "0.85");
    let env_only = deprecated_cpu_knob_notice(&WorkFinderConfig::default())
        .expect("a set env var must produce a notice");
    assert!(env_only.contains(DEPRECATED_CPU_ENV_VARS[0]), "{env_only}");

    let both = deprecated_cpu_knob_notice(&WorkFinderConfig {
        deprecated_cpu_keys: vec!["estCoresPerSweep"],
        ..Default::default()
    })
    .expect("notice");
    assert!(both.contains("estCoresPerSweep"), "{both}");
    assert!(both.contains(DEPRECATED_CPU_ENV_VARS[0]), "{both}");
    assert!(both.contains(" and "), "both sources must be joined: {both}");

    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
}

#[test]
fn test_deprecated_knob_name_tables_stay_in_sync() {
    // The config keys and env vars are two halves of one deprecation; a
    // future edit that adds one must add the other (the warning names both).
    assert_eq!(DEPRECATED_CPU_CONFIG_KEYS.len(), DEPRECATED_CPU_ENV_VARS.len());
    assert!(DEPRECATED_CPU_ENV_VARS
        .iter()
        .all(|v| v.starts_with("LOOM_")));
}

#[test]
#[serial(loom_config_env)]
fn test_config_retired_per_token_concurrency_key_is_ignored() {
    // #5743: `perTokenConcurrency` fed a disclaimed, causally-irrelevant
    // status number and has been fully retired — `WorkFinderConfig` no
    // longer has a field for it. A host whose committed
    // `.loom/config.json` still sets the key (this repo's own included, at
    // the time this issue was filed) must start cleanly and simply ignore
    // it, not fail to parse.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"perTokenConcurrency": 3}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_enabled_false_is_disabled_flag() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": false}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.enabled, Some(false));
    assert_eq!(cfg.interval_secs, None);
    assert_eq!(cfg.max_concurrent, None);
}

#[test]
fn test_config_zero_interval_and_max_drop_to_none() {
    // A zero interval/max in config is treated as absent so it falls through
    // to the built-in default rather than a useless value.
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"enabled": true, "intervalSecs": 0, "maxConcurrent": 0}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.interval_secs, None);
    assert_eq!(cfg.max_concurrent, None);
}

// ===================================================================
// config_resolver migration (#4058) — tier precedence
// ===================================================================

fn write_project_config(dir: &Path, body: &str) {
    let full = dir.join(crate::config_resolver::PROJECT_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

fn write_local_config(dir: &Path, body: &str) {
    let full = dir.join(crate::config_resolver::LOCAL_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_only_is_honored_like_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 5}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.max_concurrent, Some(5));
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 5, "intervalSecs": 60}}}"#,
    );
    write_project_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 9}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    // Overlapping `maxConcurrent` -> project tier wins.
    assert_eq!(cfg.max_concurrent, Some(9));
    // Non-overlapping keys still supplied by the legacy tier.
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.interval_secs, Some(60));
}

#[test]
#[serial(loom_config_env)]
fn test_config_local_tier_overrides_legacy_and_project() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 5}}}"#);
    write_project_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 9}}}"#);
    write_local_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 2}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.max_concurrent, Some(2));
}

// ===================================================================
// Config-file surface — resolve_* precedence env > config > default (#3813)
// ===================================================================

#[test]
#[serial]
fn test_resolve_enabled_precedence() {
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);

    // Absent config + unset env ⇒ default off (zero behavior change).
    assert!(!resolve_enabled(&WorkFinderConfig::default()));

    // Config alone enables when env is unset.
    let on = WorkFinderConfig {
        enabled: Some(true),
        ..Default::default()
    };
    assert!(resolve_enabled(&on));
    let off = WorkFinderConfig {
        enabled: Some(false),
        ..Default::default()
    };
    assert!(!resolve_enabled(&off));

    // Env overrides config in both directions.
    std::env::set_var(WORK_FINDER_ENABLE_ENV, "1");
    assert!(resolve_enabled(&off), "env truthy overrides config=false");
    std::env::set_var(WORK_FINDER_ENABLE_ENV, "0");
    assert!(!resolve_enabled(&on), "env falsy overrides config=true");
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_interval_with_config_precedence() {
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_interval_with_config(&WorkFinderConfig::default()),
        Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS)
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        interval_secs: Some(120),
        ..Default::default()
    };
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));

    // Env overrides config.
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "45");
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(45));

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "nope");
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
}

#[test]
#[serial]
fn test_resolve_max_concurrent_with_config_precedence() {
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_max_concurrent_with_config(&WorkFinderConfig::default()),
        DEFAULT_WORK_FINDER_MAX_CONCURRENT
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        max_concurrent: Some(8),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);

    // Env overrides config.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 2);

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
}

/// #6203: [`resolve_max_concurrent_with_source`] must report the same
/// numeric precedence as [`resolve_max_concurrent_with_config`] while
/// additionally naming which layer supplied the value, so the startup log
/// line can tell an operator whether a config edit was actually picked
/// up.
#[test]
#[serial]
fn test_resolve_max_concurrent_with_source_precedence() {
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_max_concurrent_with_source(&WorkFinderConfig::default()),
        (DEFAULT_WORK_FINDER_MAX_CONCURRENT, ConfigSource::Default)
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        max_concurrent: Some(8),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (8, ConfigSource::Config));

    // Env overrides config.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (2, ConfigSource::Env));

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (8, ConfigSource::Config));
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (8, ConfigSource::Config));
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

    // `.0` of the source-aware resolver must always match the plain
    // resolver — they share the same underlying precedence.
    assert_eq!(
        resolve_max_concurrent_with_source(&cfg).0,
        resolve_max_concurrent_with_config(&cfg)
    );
}

// ===================================================================
// resolve_max_admissions_per_tick_with_config — env > config > default
// (#4234)
// ===================================================================

#[test]
#[serial]
fn test_resolve_max_admissions_per_tick_with_config_precedence() {
    std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_max_admissions_per_tick_with_config(&WorkFinderConfig::default()),
        DEFAULT_MAX_ADMISSIONS_PER_TICK
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        max_admissions_per_tick: Some(7),
        ..Default::default()
    };
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);

    // Env overrides config.
    std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "1");
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 1);

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "0");
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);
    std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "nope");
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);
    std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);
}

#[test]
fn test_read_work_finder_config_parses_max_admissions_per_tick() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxAdmissionsPerTick": 5}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.max_admissions_per_tick, Some(5));
}

#[test]
fn test_read_work_finder_config_drops_zero_max_admissions_per_tick() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxAdmissionsPerTick": 0}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.max_admissions_per_tick, None, "a zero cap is treated as absent");
}

#[test]
#[serial]
fn test_retired_per_token_concurrency_env_var_is_silently_ignored() {
    // #5743: `LOOM_PER_TOKEN_CONCURRENCY` no longer resolves to anything —
    // no function reads it any more. Setting it must not be a startup
    // error and must not affect the dynamic cap, which has had no token
    // term since #5270.
    //
    // #7455: a live `loom-daemon` on the host running this test can leak
    // `LOOM_WORK_FINDER_MAX_CONCURRENT` into the ambient env, which would
    // silently override the `Some(10)` set below (env > config >
    // default) and produce a spurious mismatch unrelated to the retired
    // var under test — clear it first so the test stays hermetic.
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    std::env::set_var("LOOM_PER_TOKEN_CONCURRENCY", "7");
    let cfg = WorkFinderConfig {
        max_concurrent: Some(10),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 10);
    assert_eq!(
        resolve_dynamic_max_concurrent(36, usize::MAX, 10),
        10,
        "a retired env var must not clamp the cap"
    );
    std::env::remove_var("LOOM_PER_TOKEN_CONCURRENCY");
}

#[test]
#[serial]
fn test_retired_cpu_env_vars_are_ignored_not_honored() {
    // #4512: `LOOM_CPU_UTILIZATION_TARGET` / `LOOM_EST_CORES_PER_SWEEP` no
    // longer resolve to anything (the functions that read them are gone).
    // The observable contract is that setting them changes NO cap input and
    // is not an error — only the deprecation warning notices them.
    //
    // #7455: clear `LOOM_WORK_FINDER_MAX_CONCURRENT` first — a live
    // `loom-daemon` on the host running this test can leak it into the
    // ambient env, which would silently override the `Some(10)` set
    // below (env > config > default) and produce a spurious mismatch
    // unrelated to the retired CPU vars under test.
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::set_var(var, "0.01");
    }
    let cfg = WorkFinderConfig {
        max_concurrent: Some(10),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 10);
    assert_eq!(
        resolve_dynamic_max_concurrent(36, usize::MAX, 10),
        10,
        "a retired env knob must not clamp the cap"
    );
    // Safe to call with only env-side deprecation present (no config keys).
    warn_deprecated_cpu_knobs(&cfg);
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
}

// ===================================================================
// Token-capacity advisory transitions (#3902)
// ===================================================================

fn pressured_assessment() -> capacity::PressureAssessment {
    // token_limit 0 (zero healthy accounts); 12 deferred ⇒ genuinely
    // token-starved + pressured.
    let snap = capacity::RankingSnapshot {
        total: 7,
        available: 0,
        exhausted: 7,
        ..capacity::RankingSnapshot::default()
    };
    capacity::assess_pressure(Some(&snap), 7, 0, 12, capacity::DEFAULT_ADVISORY_MIN_QUEUED)
}

fn calm_assessment() -> capacity::PressureAssessment {
    // Nothing deferred ⇒ not pressured (healthy pool).
    let snap = capacity::RankingSnapshot {
        total: 7,
        available: 7,
        ..capacity::RankingSnapshot::default()
    };
    capacity::assess_pressure(Some(&snap), 7, 7, 0, capacity::DEFAULT_ADVISORY_MIN_QUEUED)
}

#[test]
fn transition_enters_pressure_and_publishes_advisory() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let a = pressured_assessment();
    assert!(a.pressured);

    // Not previously pressured ⇒ transition fires, returns true.
    let now = emit_capacity_transition(&bus, false, &a);
    assert!(now, "entered pressured state");

    match sub.try_recv().expect("an advisory event was published") {
        Event::CapacityAdvisory {
            pressured,
            queued,
            healthy_accounts,
            message,
            ..
        } => {
            assert!(pressured);
            assert_eq!(queued, 12);
            assert_eq!(healthy_accounts, 0);
            assert!(message.contains("loom-daemon tokens bootstrap"));
        }
        other => panic!("expected CapacityAdvisory, got {other:?}"),
    }
}

#[test]
fn transition_is_deduplicated_while_pressure_persists() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let a = pressured_assessment();

    // Already pressured ⇒ no new event, state stays true.
    let now = emit_capacity_transition(&bus, true, &a);
    assert!(now);
    assert!(
        matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
        "no duplicate advisory while pressure persists"
    );
}

#[test]
fn transition_recovers_and_publishes_symmetric_event() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let calm = calm_assessment();

    // Was pressured, now calm ⇒ recovery event, state returns to false.
    let now = emit_capacity_transition(&bus, true, &calm);
    assert!(!now, "left pressured state");

    match sub.try_recv().expect("a recovery event was published") {
        Event::CapacityAdvisory {
            pressured, message, ..
        } => {
            assert!(!pressured);
            assert!(message.contains("restored"));
        }
        other => panic!("expected CapacityAdvisory recovery, got {other:?}"),
    }
}

#[test]
fn transition_stays_calm_when_never_pressured() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let calm = calm_assessment();

    let now = emit_capacity_transition(&bus, false, &calm);
    assert!(!now);
    assert!(
        matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
        "no event when staying calm"
    );
}

#[test]
fn capacity_advisory_event_topic() {
    let ev = Event::CapacityAdvisory {
        pressured: true,
        queued: 3,
        healthy_accounts: 1,
        exhausted_accounts: 6,
        total_accounts: 7,
        estimated_drain_minutes: Some(90),
        message: "x".to_string(),
    };
    assert_eq!(ev.topic(), "daemon.capacity.advisory");
}

// ===================================================================
// Gate-in-flight dispatch suppressor (#4084)
// ===================================================================

#[test]
fn test_dispatch_held_per_root_gate_in_flight_holds_only_its_own_root() {
    // A root whose gate run is in flight is held; a sibling with no gate in
    // flight is NOT — the #3930 per-repo isolation contract must survive.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let held = dispatch_held_per_root(&states, &[root_a, root_b], true);
    assert_eq!(held, vec![true, false], "only the in-flight root is held");
}

#[test]
fn test_dispatch_held_per_root_suppressor_disabled_is_is_halted_only() {
    // With the suppressor off, the in-flight term drops out entirely — the
    // result is byte-for-byte the pre-#4084 `is_halted`-only vector, even
    // for a root with a gate run in flight.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    states.get_or_create(&root_b).set_halted(true);
    let held = dispatch_held_per_root(&states, &[root_a.clone(), root_b.clone()], false);
    assert_eq!(
        held,
        vec![false, true],
        "suppressor off ⇒ gate-in-flight is ignored; only verified-red holds"
    );
    // Sanity: with the suppressor on, root_a is additionally held.
    let held_on = dispatch_held_per_root(&states, &[root_a, root_b], true);
    assert_eq!(held_on, vec![true, true]);
}

#[test]
fn test_dispatch_held_per_root_with_drain_holds_every_root() {
    // A daemon-global scheduled drain (#4090) holds EVERY root at once,
    // regardless of each root's gate state — the merge with #4084 must not
    // let the per-root gate term shadow the global drain term.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    // Neither root is verified-red nor has a gate in flight.
    let held = dispatch_held_per_root_with_drain(
        &states,
        &[root_a, root_b],
        true, // suppressor on
        true, // draining
    );
    assert_eq!(
        held,
        vec![true, true],
        "a scheduled drain holds every root regardless of gate state"
    );
}

#[test]
fn test_dispatch_held_per_root_with_drain_gate_still_per_root_when_not_draining() {
    // With no drain in progress the gate-in-flight term stays strictly
    // per-root: only the root whose gate run is in flight is held, its
    // sibling keeps dispatching (#3930 isolation contract survives #4090).
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let held = dispatch_held_per_root_with_drain(
        &states,
        &[root_a, root_b],
        true,  // suppressor on
        false, // not draining
    );
    assert_eq!(
        held,
        vec![true, false],
        "gate-in-flight holds only its own root when not draining"
    );
}

#[test]
fn test_dispatch_held_per_root_with_drain_terms_are_independent() {
    // Both terms compose additively: a drain holds a healthy root, and a
    // gate in flight holds its own root — with the drain on, every root is
    // held whether or not its gate is in flight.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a"); // gate in flight
    let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let held = dispatch_held_per_root_with_drain(
        &states,
        &[root_a, root_b],
        true, // suppressor on
        true, // draining
    );
    assert_eq!(held, vec![true, true], "drain OR gate-in-flight: both roots held");
}

#[test]
fn test_dispatch_held_per_root_with_drain_no_drain_matches_per_root() {
    // With `draining = false` the result is byte-for-byte the plain
    // per-root vector — the drain fold is a pure superset, never a
    // regression of the #4084 / #3930 semantics.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    states.get_or_create(&root_b).set_halted(true);
    let roots = [root_a, root_b];
    let plain = dispatch_held_per_root(&states, &roots, true);
    let with_drain = dispatch_held_per_root_with_drain(&states, &roots, true, false);
    assert_eq!(plain, with_drain, "draining=false ⇒ identical to dispatch_held_per_root");
}

/// Issue #6007 — the livelock, from the *admission* side. The work finder
/// reads exactly one bit (`DrainState::flag`, surfaced here as `draining`), so
/// what matters is what a **refused** drain deadline does to that bit. Before
/// #6007 the first refusal cleared it, dispatch resumed, more sweeps were
/// admitted, and the next drain was strictly harder to satisfy — a busy host
/// could never roll. Now the refusal *retains* the roll, so admission stays
/// held and the in-flight set can actually reach zero; only once the roll is
/// abandoned (its paused-dispatch budget spent) does admission resume, so real
/// work is never blocked indefinitely.
#[test]
fn test_admission_stays_held_across_a_roll_refusal_then_resumes_when_abandoned() {
    use crate::ipc::{DrainState, RollRefusal};

    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    let roots = [root_a, root_b];

    let drain = DrainState::new();
    let _ = drain.begin(std::time::Duration::from_secs(1800), false, false);
    assert_eq!(
        dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
        vec![true, true],
        "a scheduled drain holds every root"
    );

    // The deadline passes with sweeps still in flight: refused, roll retained.
    let started = drain.snapshot().started_at.expect("started_at");
    assert!(matches!(
        drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
        RollRefusal::Deferred { .. }
    ));
    assert_eq!(
        dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
        vec![true, true],
        "#6007: admission must STAY held across the refusal — resuming here is the livelock"
    );

    // Budget spent: the roll is abandoned and admission resumes, so a wedged
    // sweep cannot starve the host of work forever.
    assert!(matches!(
        drain.refuse_roll_deadline(started + chrono::Duration::seconds(7200)),
        RollRefusal::Abandoned { .. }
    ));
    assert_eq!(
        dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
        vec![false, false],
        "an abandoned roll returns the admission window to the work finder"
    );
}

#[test]
fn test_gate_in_flight_root_dispatches_zero_new_sweeps() {
    // End-to-end through `tick_multi`: a root marked held (as
    // `dispatch_held_per_root` would for a gate in flight) dispatches
    // nothing, while its healthy sibling gets the shared slot.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let halted = dispatch_held_per_root(&states, &[root_a, root_b], true);

    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &halted);

    assert!(report.halted, "the gated root marks the tick as halted");
    assert!(
        multi[0].1.dispatched.is_empty(),
        "root with a gate run in flight dispatches zero new sweeps"
    );
    assert_eq!(
        multi[1].1.dispatched,
        vec![10],
        "sibling root with no gate in flight is unaffected"
    );
}

// ===================================================================
// Pre-flight-advisory dispatch hold (#5030)
// ===================================================================

#[test]
fn test_dispatch_held_per_root_with_preflight_holds_only_the_tripped_root() {
    // A workspace whose pre-flight breaker is holding (broken .mcp.json)
    // holds only its own root; a healthy sibling keeps dispatching — the
    // #3930 per-repo isolation contract must survive the #5030 fold.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a"); // pre-flight held
    let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
    let preflight_held = [true, false];
    let held =
        dispatch_held_per_root_with_preflight(&states, &[root_a, root_b], true, &preflight_held);
    assert_eq!(
        held,
        vec![true, false],
        "only the tripped root is held; its healthy sibling keeps dispatching"
    );
}

#[test]
fn test_dispatch_held_per_root_with_preflight_empty_slice_matches_per_root() {
    // An all-false / missing pre-flight slice is byte-for-byte the plain
    // per-root vector — the fold is a pure superset, never a regression of
    // the #4084 / #3930 semantics.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    states.get_or_create(&root_b).set_halted(true);
    let roots = [root_a, root_b];
    let plain = dispatch_held_per_root(&states, &roots, true);
    let folded = dispatch_held_per_root_with_preflight(&states, &roots, true, &[]);
    assert_eq!(plain, folded, "empty pre-flight slice ⇒ identical to dispatch_held_per_root");
}

#[test]
fn test_dispatch_held_per_root_with_preflight_composes_with_verified_red() {
    // The pre-flight hold and the #3930 verified-red hold compose
    // additively per root: root_a is held by pre-flight, root_b by a red
    // main, root_c is healthy on both axes.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    let root_c = std::path::PathBuf::from("/tmp/repo-c");
    states.get_or_create(&root_b).set_halted(true);
    let held = dispatch_held_per_root_with_preflight(
        &states,
        &[root_a, root_b, root_c],
        true,
        &[true, false, false],
    );
    assert_eq!(held, vec![true, true, false]);
}

#[test]
fn test_preflight_held_root_dispatches_zero_new_sweeps() {
    // Regression (#5030): end-to-end through `tick_multi`, a workspace whose
    // pre-flight advisory has tripped (its breaker is holding) dispatches
    // ZERO new sweeps even with a full backlog, while its healthy sibling
    // takes the shared slot — the burn-every-slot incident cannot recur.
    // Mirrors `test_gate_in_flight_root_dispatches_zero_new_sweeps`.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a"); // pre-flight held
    let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
    let halted =
        dispatch_held_per_root_with_preflight(&states, &[root_a, root_b], true, &[true, false]);

    let mut multi = vec![
        (FakeSource::once((1..=5).map(issue).collect()), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &halted);

    assert!(report.halted, "the pre-flight-held root marks the tick as halted");
    assert_eq!(report.seen, 6, "both backlogs are still observed");
    assert!(
        multi[0].1.dispatched.is_empty(),
        "a pre-flight-broken workspace dispatches zero new sweeps (no slot burn)"
    );
    assert_eq!(
        multi[1].1.dispatched,
        vec![10],
        "the healthy sibling keeps dispatching against the shared budget"
    );
}

#[test]
fn test_preflight_probe_tick_resumes_dispatch_to_recovering_root() {
    // Under the half-open design, a probe tick reports the root as NOT held
    // (`preflight_held=false` for that root), so `tick_multi` lets one
    // dispatch through to test recovery — proving the breaker never blocks
    // the very dispatch needed to prove the workspace is fixed.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a"); // probing this tick
    let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
                                                          // A probe tick maps `PreflightDispatchGate::Probe` → not held.
    let halted =
        dispatch_held_per_root_with_preflight(&states, &[root_a, root_b], true, &[false, false]);

    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &halted);
    assert!(!report.halted);
    assert_eq!(
        multi[0].1.dispatched,
        vec![1],
        "a probe tick lets one dispatch through to the recovering root"
    );
    assert_eq!(multi[1].1.dispatched, vec![10]);
}

// ===================================================================
// Last-tick publication (#4761)
// ===================================================================

#[test]
#[serial(work_finder_last_tick)]
fn last_tick_summary_is_none_before_any_tick() {
    reset_last_tick_summary();
    assert!(last_tick_summary().is_none());
}

#[test]
#[serial(work_finder_last_tick)]
fn publishing_a_tick_makes_its_counters_readable_cross_process() {
    reset_last_tick_summary();
    let at = chrono::Utc::now();
    let report = TickReport {
        seen: 12,
        dispatched: 2,
        skipped_in_flight: 9,
        skipped_pr_open: 1,
        errors: 0,
        halted: false,
        ..TickReport::default()
    };
    publish_tick_summary_at(&report, 7, at);

    let summary = last_tick_summary().expect("a published tick must be readable");
    assert_eq!(summary.at, at);
    assert_eq!(summary.max_concurrent, 7);
    assert_eq!(summary.seen, 12);
    assert_eq!(summary.dispatched, 2);
    assert_eq!(summary.skipped_in_flight, 9);
    assert_eq!(summary.skipped_pr_open, 1);
    assert!(!summary.halted);
}

#[test]
#[serial(work_finder_last_tick)]
fn publishing_replaces_the_previous_tick() {
    reset_last_tick_summary();
    publish_tick_summary(
        &TickReport {
            seen: 1,
            ..TickReport::default()
        },
        3,
    );
    publish_tick_summary(
        &TickReport {
            seen: 99,
            ..TickReport::default()
        },
        4,
    );
    let summary = last_tick_summary().unwrap();
    assert_eq!(summary.seen, 99);
    assert_eq!(summary.max_concurrent, 4);
}

/// The rendered summary must show only the *non-zero* skip reasons, so an
/// operator reading one line is not scanning a wall of zeros.
#[test]
fn reason_summary_omits_zero_terms() {
    let summary = crate::types::WorkFinderTickSummary {
        seen: 12,
        dispatched: 2,
        skipped_in_flight: 10,
        ..Default::default()
    };
    assert_eq!(summary.reason_summary(), "12 seen, 2 dispatched, 10 in-flight-skip");
}

#[test]
fn reason_summary_flags_a_halted_tick() {
    let summary = crate::types::WorkFinderTickSummary {
        seen: 5,
        halted: true,
        ..Default::default()
    };
    assert!(summary.reason_summary().ends_with("HALTED"));
}

/// Issue #5302: `TickReport::collisions` was already logged on the
/// per-tick `work_finder: tick — …` line (#4085) but never reached the
/// wire-carried [`crate::types::WorkFinderTickSummary`], so
/// `loom-daemon status` / `GetDaemonStatus` could not see a cross-host
/// collision without scraping the daemon log. Assert the count now
/// survives publication.
#[test]
fn publish_tick_summary_carries_collisions_through() {
    reset_last_tick_summary();
    publish_tick_summary(
        &TickReport {
            seen: 4,
            dispatched: 1,
            collisions: 3,
            ..TickReport::default()
        },
        2,
    );
    let summary = last_tick_summary().unwrap();
    assert_eq!(summary.collisions, 3, "collision total must survive publication");
    assert!(
        summary
            .reason_summary()
            .contains("3 cross-host-collision(s)"),
        "reason_summary must surface a non-zero collision count: {}",
        summary.reason_summary()
    );
}

/// A clean tick (no collisions) must not mention collisions at all — the
/// same "only non-zero terms" discipline every other counter follows.
#[test]
fn reason_summary_omits_zero_collisions() {
    let summary = crate::types::WorkFinderTickSummary {
        seen: 1,
        dispatched: 1,
        ..Default::default()
    };
    assert!(!summary.reason_summary().contains("collision"));
}
