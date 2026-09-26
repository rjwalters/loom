//! Coverage for the per-repo dispatch cap and track affinity (issue #9090).
//!
//! Its own file rather than part of `work_finder::tests` for the file-size
//! ratchet reason `prless_retry_tests` documents (`tests.rs` is well over the
//! threshold and frozen), so these carry their own two-field fakes.

use std::collections::HashSet;

use super::super::*;
use crate::sweep_registry::OpenPrDispatchError;

/// A one-shot [`WorkSource`] over a fixed candidate list.
struct OneShotSource(Option<Vec<WorkItem>>);

impl OneShotSource {
    fn of(numbers: &[u32]) -> Self {
        Self(Some(
            numbers
                .iter()
                .map(|n| WorkItem::new(*n, vec!["loom:issue".to_string()]))
                .collect(),
        ))
    }
}

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

/// A [`WorkDispatcher`] whose repo can be seeded with live sweeps (the
/// per-repo occupancy the cap and affinity both read) and whose issues can be
/// made to fail the #4123 open-PR guard.
#[derive(Default)]
struct CapDispatcher {
    dispatched: Vec<u32>,
    /// Live sweeps this repo already holds at the top of the tick.
    live: usize,
    /// Issues `dispatch()` refuses with the typed open-PR error (#4123).
    pr_open: HashSet<u32>,
}

impl CapDispatcher {
    /// A repo that already holds `live` slots.
    fn holding(live: usize) -> Self {
        Self {
            live,
            ..Default::default()
        }
    }
}

impl WorkDispatcher for CapDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn occupancy(&self) -> usize {
        self.live + self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> Result<bool> {
        if self.pr_open.contains(&issue) {
            return Err(OpenPrDispatchError { issue, pr: 4242 }.into());
        }
        self.dispatched.push(issue);
        Ok(true)
    }
}

/// `tick_multi_with_repo_cap` with the machine-level gates wide open, so every
/// assertion below is about the per-repo cap alone.
fn tick_capped<const N: usize>(
    workspaces: &mut [(OneShotSource, CapDispatcher); N],
    priorities: &[u32],
    max_concurrent: usize,
    max_concurrent_per_repo: Option<usize>,
) -> TickReport {
    tick_multi_with_repo_cap(
        workspaces,
        priorities,
        max_concurrent,
        &[false; N],
        usize::MAX,
        false,
        None,
        max_concurrent_per_repo,
    )
}

/// How many rows the tick recorded under one disposition.
fn rows_with(report: &TickReport, disposition: Qd) -> usize {
    report
        .queue
        .iter()
        .filter(|r| r.disposition == Some(disposition))
        .count()
}

// ===================================================================
// Per-repo cap — admission
// ===================================================================

/// The headline AC: a deep single-repo backlog with `maxConcurrentPerRepo = 1`
/// admits exactly ONE sweep, with the global cap deliberately non-binding, and
/// the rest are visibly deferred rather than silently vanishing.
#[test]
fn per_repo_cap_bounds_admissions_while_the_global_cap_has_room() {
    let mut workspaces = [(OneShotSource::of(&[1, 2, 3, 4]), CapDispatcher::default())];
    let report = tick_capped(&mut workspaces, &[0], 10, Some(1));

    assert_eq!(report.dispatched, 1, "one repo may hold only one slot");
    assert_eq!(workspaces[0].1.dispatched, vec![1], "and it is the first in global order");
    assert_eq!(report.deferred_repo_cap, 3);
    assert_eq!(report.deferred_capacity, 0, "the machine cap was never the binding term");
    assert_eq!(
        rows_with(&report, Qd::DeferredRepoCap),
        3,
        "each deferral is visible in the ready-queue view, not dropped"
    );
}

/// The cap counts UP from the seed as the tick dispatches, so a cap of 2 admits
/// exactly two of one repo's candidates — not one, and not all of them.
#[test]
fn per_repo_cap_admits_up_to_the_cap_then_defers() {
    let mut workspaces = [(OneShotSource::of(&[1, 2, 3]), CapDispatcher::default())];
    let report = tick_capped(&mut workspaces, &[0], 10, Some(2));

    assert_eq!(workspaces[0].1.dispatched, vec![1, 2]);
    assert_eq!(report.deferred_repo_cap, 1);
}

/// The seed is each dispatcher's OWN `occupancy()`: a repo already holding its
/// cap admits nothing new this tick, while a sibling under its cap dispatches
/// normally against the same shared budget.
#[test]
fn a_repo_already_at_its_cap_admits_nothing_new() {
    let mut workspaces = [
        (OneShotSource::of(&[1, 2]), CapDispatcher::holding(2)),
        (OneShotSource::of(&[10]), CapDispatcher::default()),
    ];
    let report = tick_capped(&mut workspaces, &[0, 0], 10, Some(2));

    assert!(
        workspaces[0].1.dispatched.is_empty(),
        "the seeded repo is already at its cap of 2"
    );
    assert_eq!(report.deferred_repo_cap, 2);
    assert_eq!(workspaces[1].1.dispatched, vec![10], "its sibling is unaffected");
    assert_eq!(report.dispatched, 1);
}

/// Work conservation: a capped repo's deferral is handed to the next candidate
/// IN THE SAME TICK, so the fleet never idles while another repo has ready work
/// — even when the capped repo is the higher-priority one and affinity had
/// floated it to the front.
#[test]
fn a_capped_repo_never_idles_a_slot_a_sibling_could_use() {
    let mut workspaces = [
        // Priority 0 AND hot (1 live sweep) — first in order on both counts.
        (OneShotSource::of(&[1, 2, 3]), CapDispatcher::holding(1)),
        // Priority 100 and cold — last in order.
        (OneShotSource::of(&[10, 11]), CapDispatcher::default()),
    ];
    let report = tick_capped(&mut workspaces, &[0, 100], 10, Some(1));

    assert!(workspaces[0].1.dispatched.is_empty(), "the hot repo is at its cap");
    assert_eq!(
        workspaces[1].1.dispatched,
        vec![10],
        "the cold repo gets a slot in the SAME tick (work conservation), up to its own cap"
    );
    assert_eq!(report.dispatched, 1);
    assert_eq!(report.deferred_repo_cap, 4, "3 hot + the cold repo's second candidate");
}

/// A cap larger than the global cap is harmless: the machine-level cap still
/// binds first and the deferrals are attributed to it, not to the repo cap.
#[test]
fn a_per_repo_cap_above_the_global_cap_is_harmless() {
    let mut workspaces = [(OneShotSource::of(&[1, 2, 3]), CapDispatcher::default())];
    let report = tick_capped(&mut workspaces, &[0], 1, Some(99));

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.deferred_capacity, 2);
    assert_eq!(report.deferred_repo_cap, 0);
}

/// A `Some(0)` cap — unreachable through config/env, which both drop zero — is
/// defensively coerced to uncapped rather than deadlocking every repo.
#[test]
fn a_zero_cap_is_treated_as_absent_never_as_a_cap_of_zero() {
    let mut workspaces = [(OneShotSource::of(&[1, 2]), CapDispatcher::default())];
    let report = tick_capped(&mut workspaces, &[0], 10, Some(0));

    assert_eq!(report.dispatched, 2);
    assert_eq!(report.deferred_repo_cap, 0);
    assert!(!RepoCap::new(Some(0), vec![5]).enabled());
}

// ===================================================================
// `None` is a no-op (the upgrade path)
// ===================================================================

/// The guard on every existing fleet: with no `maxConcurrentPerRepo` the
/// admissions, their order, and the whole report are identical to a tick run
/// through the pre-#9090 seven-argument entry point.
#[test]
fn an_unset_cap_is_byte_for_byte_the_pre_9090_tick() {
    let issues_a = [1, 2, 3];
    let issues_b = [10, 11];

    let mut capped = [
        (OneShotSource::of(&issues_a), CapDispatcher::holding(1)),
        (OneShotSource::of(&issues_b), CapDispatcher::default()),
    ];
    let with_none = tick_capped(&mut capped, &[0, 100], 10, None);

    let mut legacy = [
        (OneShotSource::of(&issues_a), CapDispatcher::holding(1)),
        (OneShotSource::of(&issues_b), CapDispatcher::default()),
    ];
    let pre_9090 = tick_multi_with_sharding(
        &mut legacy,
        &[0, 100],
        10,
        &[false, false],
        usize::MAX,
        false,
        None,
    );

    assert_eq!(with_none, pre_9090, "an absent cap must change nothing at all");
    assert_eq!(with_none.deferred_repo_cap, 0);
    assert_eq!(capped[0].1.dispatched, vec![1, 2, 3]);
    assert_eq!(capped[0].1.dispatched, legacy[0].1.dispatched);
    assert_eq!(capped[1].1.dispatched, legacy[1].1.dispatched);
}

/// `tick_multi` (and therefore every pre-#9090 caller) reaches the same body
/// with no cap — one repo may still take the whole budget, which is exactly the
/// behaviour an un-opted-in fleet keeps.
#[test]
fn tick_multi_leaves_one_repo_free_to_take_the_whole_budget() {
    let mut workspaces = vec![
        (OneShotSource::of(&[1, 2, 3]), CapDispatcher::default()),
        (OneShotSource::of(&[10]), CapDispatcher::default()),
    ];
    let report = tick_multi(&mut workspaces, &[0, 100], 3, &[false, false]);

    assert_eq!(workspaces[0].1.dispatched, vec![1, 2, 3], "the whole budget, one repo");
    assert!(workspaces[1].1.dispatched.is_empty());
    assert_eq!(report.deferred_repo_cap, 0);
}

// ===================================================================
// Track affinity — ordering, subordinate to the cap
// ===================================================================

/// Affinity floats a repo that already has a live sweep ahead of a cold repo,
/// even though the cold repo's priority tier would otherwise win — and the
/// floated repo still cannot exceed its own cap, so the cold repo takes the
/// remaining slot in the same tick.
#[test]
fn affinity_orders_a_hot_repos_next_issue_ahead_of_a_cold_repos() {
    let mut workspaces = [
        // Priority 0 (higher), but COLD.
        (OneShotSource::of(&[1]), CapDispatcher::default()),
        // Priority 100 (lower), but HOT — one live sweep.
        (OneShotSource::of(&[10]), CapDispatcher::holding(1)),
    ];
    // Global budget of 2 (the hot repo's live sweep seeds occupancy 1), cap 2:
    // both candidates are admissible, so only the ORDER is under test.
    let report = tick_capped(&mut workspaces, &[0, 100], 3, Some(2));

    assert_eq!(report.dispatched, 2);
    assert_eq!(
        report
            .admissions
            .iter()
            .map(|a| a.issue)
            .collect::<Vec<_>>(),
        vec![10, 1],
        "the hot repo's issue is attempted first despite its lower priority tier"
    );
}

/// Affinity is ordering only: it must never let a hot repo exceed its cap, so
/// with a cap of 1 the hot repo (already holding 1) is deferred and the cold
/// repo's candidate is what actually dispatches.
#[test]
fn affinity_is_subordinate_to_the_per_repo_cap() {
    let mut workspaces = [
        (OneShotSource::of(&[1]), CapDispatcher::default()),
        (OneShotSource::of(&[10]), CapDispatcher::holding(1)),
    ];
    let report = tick_capped(&mut workspaces, &[0, 100], 10, Some(1));

    assert_eq!(workspaces[0].1.dispatched, vec![1], "the cold repo dispatches");
    assert!(workspaces[1].1.dispatched.is_empty(), "the hot repo is at its cap");
    assert_eq!(report.deferred_repo_cap, 1);
}

/// Affinity only reorders when the cap is configured — it is one opt-in
/// feature, not two. With `None` the hot repo gets no head start.
#[test]
fn affinity_is_off_while_the_cap_is_unset() {
    let mut workspaces = [
        (OneShotSource::of(&[1]), CapDispatcher::default()),
        (OneShotSource::of(&[10]), CapDispatcher::holding(1)),
    ];
    let report = tick_capped(&mut workspaces, &[0, 100], 10, None);

    assert_eq!(
        report
            .admissions
            .iter()
            .map(|a| a.issue)
            .collect::<Vec<_>>(),
        vec![1, 10],
        "pure priority order — no affinity float"
    );
}

/// Affinity preserves `candidate_cmp`'s order WITHIN each group (a stable
/// partition), so two hot repos and two cold repos still drain in priority
/// order inside their own group.
#[test]
fn affinity_preserves_relative_order_within_each_group() {
    let mut workspaces = [
        (OneShotSource::of(&[1]), CapDispatcher::default()), // cold, prio 0
        (OneShotSource::of(&[2]), CapDispatcher::holding(1)), // hot, prio 10
        (OneShotSource::of(&[3]), CapDispatcher::default()), // cold, prio 20
        (OneShotSource::of(&[4]), CapDispatcher::holding(1)), // hot, prio 30
    ];
    let report = tick_capped(&mut workspaces, &[0, 10, 20, 30], 99, Some(9));

    assert_eq!(
        report
            .admissions
            .iter()
            .map(|a| a.issue)
            .collect::<Vec<_>>(),
        vec![2, 4, 1, 3],
        "hot repos first in priority order, then cold repos in priority order"
    );
}

/// The comparator itself is untouched — `shape_queue` is a partition over an
/// already-sorted list, so a sorted list with affinity off comes back
/// unchanged, and with affinity on it is a permutation of the same set.
#[test]
fn shape_queue_never_adds_or_drops_a_candidate() {
    let cand = |idx: usize, number: u32| PriorityCandidate {
        workspace_idx: idx,
        workspace_priority: 0,
        urgent: false,
        created_at: None,
        number,
        complexity: None,
    };
    let candidates = vec![cand(0, 1), cand(1, 2), cand(0, 3)];
    let mut report = TickReport::default();

    let off = super::shape_queue(candidates.clone(), None, &RepoCap::disabled(), &mut report);
    assert_eq!(off, candidates, "disabled is an exact no-op");

    // Workspace 1 is hot; workspace 0 is cold.
    let cap = RepoCap::new(Some(1), vec![0, 1]);
    let on = super::shape_queue(candidates.clone(), None, &cap, &mut report);
    assert_eq!(on.len(), candidates.len());
    assert_eq!(on.first().map(|c| c.number), Some(2), "the hot repo floats to the front");
    for c in &candidates {
        assert!(on.contains(c), "#{} must survive the partition", c.number);
    }
}

// ===================================================================
// Cross-repo interleaving is already emergent (#4123), not a new mechanism
// ===================================================================

/// Requirement 3 of #9090 needs no mechanism: an issue whose linked PR is open
/// is refused by the #4123 guard (`Qd::OpenPr` / `skipped_pr_open`) and the
/// tick continues, in global order, to the next candidate — which is another
/// repo's work. This pins that behaviour so a future change cannot lose it.
#[test]
fn an_open_pr_repo_yields_the_tick_to_another_repos_work() {
    let mut workspaces = [
        (
            OneShotSource::of(&[1, 2]),
            CapDispatcher {
                pr_open: HashSet::from([1, 2]),
                ..Default::default()
            },
        ),
        (OneShotSource::of(&[10]), CapDispatcher::default()),
    ];
    // Priority puts the PR-blocked repo first; a single slot is available.
    let report = tick_capped(&mut workspaces, &[0, 100], 1, None);

    assert_eq!(report.skipped_pr_open, 2, "both of repo A's issues are guard-refused");
    assert_eq!(rows_with(&report, Qd::OpenPr), 2);
    assert_eq!(
        workspaces[1].1.dispatched,
        vec![10],
        "the next dispatch goes to the other repo, with no interleaving mechanism at all"
    );
    assert_eq!(report.dispatched, 1);
}

// ===================================================================
// Config resolution
// ===================================================================

/// The config parse mirrors `maxConcurrent`'s soft-fail contract: absent,
/// zero, negative and non-integer all resolve to "uncapped".
#[test]
fn config_parse_soft_fails_to_uncapped() {
    let wf = |body: &str| serde_json::from_str::<serde_json::Value>(body).unwrap();
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":2}"#))), Some(2));
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":0}"#))), None);
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":-1}"#))), None);
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":"2"}"#))), None);
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrent":4}"#))), None);
    assert_eq!(super::parse_config(None), None);
}

/// `read_work_finder_config` carries the key through, and an `autonomous` block
/// without it stays uncapped (the upgrade path).
#[test]
fn read_work_finder_config_reads_the_per_repo_key() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    let write = |body: &str| std::fs::write(dir.path().join(".loom/config.json"), body).unwrap();

    write(r#"{"autonomous":{"workFinder":{"maxConcurrent":6,"maxConcurrentPerRepo":3}}}"#);
    assert_eq!(read_work_finder_config(dir.path()).max_concurrent_per_repo, Some(3));

    write(r#"{"autonomous":{"workFinder":{"maxConcurrent":6}}}"#);
    assert_eq!(read_work_finder_config(dir.path()).max_concurrent_per_repo, None);
}

/// Precedence is env > config > none, and a zero/garbage env value falls
/// through to config rather than disabling the configured cap.
#[test]
#[serial_test::serial]
fn env_overrides_config_for_the_per_repo_cap() {
    let env = super::WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV;
    let config = WorkFinderConfig {
        max_concurrent_per_repo: Some(2),
        ..Default::default()
    };
    std::env::remove_var(env);
    assert_eq!(super::resolve(&config), Some(2));
    assert_eq!(super::resolve(&WorkFinderConfig::default()), None);

    std::env::set_var(env, "5");
    assert_eq!(super::resolve(&config), Some(5));
    std::env::set_var(env, "0");
    assert_eq!(super::resolve(&config), Some(2), "zero is absent, not a cap of 0");
    std::env::set_var(env, "nonsense");
    assert_eq!(super::resolve(&config), Some(2));
    std::env::remove_var(env);
}

// ===================================================================
// Edge cases
// ===================================================================

/// A short occupancy seed (fewer entries than workspaces — a caller bug, or a
/// workspace added mid-tick) must not panic and must fail OPEN: a workspace
/// with no seeded entry counts as empty, never as capped.
#[test]
fn a_missing_occupancy_entry_fails_open() {
    let mut cap = RepoCap::new(Some(1), vec![1]);
    let cand = PriorityCandidate {
        workspace_idx: 7,
        workspace_priority: 0,
        urgent: false,
        created_at: None,
        number: 1,
        complexity: None,
    };
    let mut report = TickReport::default();
    assert!(!cap.defer(&cand, &mut report), "an unseeded workspace is not at any cap");
    cap.admit(7); // must not panic
    assert_eq!(report.deferred_repo_cap, 0);
}

/// The single-workspace `tick` path is untouched by #9090 — it has no per-repo
/// cap parameter at all, so a deep one-repo backlog still fills the whole
/// budget there.
#[test]
fn the_single_workspace_tick_is_unaffected() {
    let mut source = OneShotSource::of(&[1, 2, 3]);
    let mut disp = CapDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 3);
    assert_eq!(report.deferred_repo_cap, 0);
}
