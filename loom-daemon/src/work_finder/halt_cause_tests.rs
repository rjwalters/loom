//! Coverage for the closed hold-cause vocabulary (Issue #9017). Its own file
//! with its own minimal fakes, for the file-size-ratchet reason
//! `ready_queue_tests.rs` gives: `work_finder/tests.rs` is frozen.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::PathBuf;

use super::super::{tick_multi_with_repo_cap, WorkDispatcher, WorkItem, WorkSource};
use super::{causes_per_root, HaltCause};
use crate::main_health_gate::WorkspaceHealthStates;
use crate::types::QueueDisposition;

// ---- minimal fakes (same shape as `ready_queue_tests.rs`) -----------------

struct OneShotSource(Option<Vec<WorkItem>>);

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> anyhow::Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

fn item(n: u32) -> WorkItem {
    WorkItem::with_created_at(n, vec!["loom:issue".to_string()], None)
}

#[derive(Default)]
struct Disp {
    dispatched: Vec<u32>,
}

impl WorkDispatcher for Disp {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn occupancy(&self) -> usize {
        self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> anyhow::Result<bool> {
        self.dispatched.push(issue);
        Ok(true)
    }
}

// ---- vocabulary -----------------------------------------------------------

/// The wire tokens are pinned verbatim: a `workspace_halted` row's `detail`
/// is one of these strings, exactly, and public views rely on the closed
/// vocabulary (Issue #9017 AC).
#[test]
fn cause_tokens_are_pinned() {
    assert_eq!(HaltCause::MainRed.as_str(), "main_red");
    assert_eq!(HaltCause::GatePending.as_str(), "gate_pending");
    assert_eq!(HaltCause::TokenPool.as_str(), "token_pool");
    assert_eq!(HaltCause::PreflightAdvisory.as_str(), "preflight_advisory");
    assert_eq!(HaltCause::Drain.as_str(), "drain");
    assert_eq!(HaltCause::Breaker.as_str(), "breaker");
}

// ---- per-root fold --------------------------------------------------------

fn states_with(root: &str, halted: bool, gate: bool) -> WorkspaceHealthStates {
    let states = WorkspaceHealthStates::new();
    let root = PathBuf::from(root);
    let s = states.get_or_create(&root);
    s.set_halted(halted);
    s.set_gate_in_flight(gate);
    states
}

fn roots2() -> Vec<PathBuf> {
    vec![PathBuf::from("/tmp/repo-a"), PathBuf::from("/tmp/repo-b")]
}

/// A clean root with no holds anywhere is `None` (not held), and each cause
/// source maps to its own token (Issue #9017 AC, one value per source).
#[test]
fn each_hold_source_names_its_own_cause() {
    // main_red: root A verified-red.
    let states = states_with("/tmp/repo-a", true, false);
    let got = causes_per_root(&states, &roots2(), false, &[None, None], false, false);
    assert_eq!(got, vec![Some(HaltCause::MainRed), None]);

    // gate_pending: root A gate in flight, suppressor on.
    let states = states_with("/tmp/repo-a", false, true);
    let got = causes_per_root(&states, &roots2(), true, &[None, None], false, false);
    assert_eq!(got, vec![Some(HaltCause::GatePending), None]);

    // gate in flight but suppressor OFF ⇒ not held (byte-for-byte the bool
    // fold's pre-#4084 behaviour).
    let got = causes_per_root(&states, &roots2(), false, &[None, None], false, false);
    assert_eq!(got, vec![None, None]);

    // token_pool / preflight_advisory arrive via the preflight cause slice.
    let states = states_with("/tmp/repo-a", false, false);
    let got = causes_per_root(
        &states,
        &roots2(),
        false,
        &[
            Some(HaltCause::TokenPool),
            Some(HaltCause::PreflightAdvisory),
        ],
        false,
        false,
    );
    assert_eq!(
        got,
        vec![
            Some(HaltCause::TokenPool),
            Some(HaltCause::PreflightAdvisory)
        ]
    );

    // A preflight slice shorter than roots defaults to not held, mirroring
    // the bool fold's `unwrap_or(false)`.
    let got =
        causes_per_root(&states, &roots2(), false, &[Some(HaltCause::TokenPool)], false, false);
    assert_eq!(got, vec![Some(HaltCause::TokenPool), None]);

    // drain and breaker are daemon-global: they name every root.
    let got = causes_per_root(&states, &roots2(), false, &[None, None], true, false);
    assert_eq!(got, vec![Some(HaltCause::Drain), Some(HaltCause::Drain)]);
    let got = causes_per_root(&states, &roots2(), false, &[None, None], false, true);
    assert_eq!(got, vec![Some(HaltCause::Breaker), Some(HaltCause::Breaker)]);
}

/// Simultaneous causes resolve by the documented precedence — the row always
/// names exactly one cause, the most specific true one.
#[test]
fn simultaneous_causes_follow_the_fixed_precedence() {
    // main_red beats everything (gate + token_pool + drain + breaker).
    let states = states_with("/tmp/repo-a", true, true);
    let got =
        causes_per_root(&states, &roots2(), true, &[Some(HaltCause::TokenPool), None], true, true);
    assert_eq!(got, vec![Some(HaltCause::MainRed), Some(HaltCause::Drain)]);

    // gate_pending beats token_pool, which beats preflight_advisory.
    let states = states_with("/tmp/repo-a", false, true);
    let got = causes_per_root(
        &states,
        &roots2(),
        true,
        &[
            Some(HaltCause::TokenPool),
            Some(HaltCause::PreflightAdvisory),
        ],
        false,
        false,
    );
    assert_eq!(
        got,
        vec![
            Some(HaltCause::GatePending),
            Some(HaltCause::PreflightAdvisory)
        ]
    );

    // token_pool beats drain and breaker.
    let states = states_with("/tmp/repo-a", false, false);
    let got =
        causes_per_root(&states, &roots2(), false, &[Some(HaltCause::TokenPool), None], true, true);
    assert_eq!(got, vec![Some(HaltCause::TokenPool), Some(HaltCause::Drain)]);

    // drain beats breaker (operator intent outranks automatic suppression).
    let got = causes_per_root(&states, &roots2(), false, &[None, None], true, true);
    assert_eq!(got, vec![Some(HaltCause::Drain), Some(HaltCause::Drain)]);
}

/// `cause.is_some()` is byte-for-byte the bool fold the production loop used
/// to compute inline — the projection the loop now derives `halted` from.
#[test]
fn cause_presence_matches_the_bool_fold() {
    use super::super::dispatch_held_per_root_with_preflight;
    let states = states_with("/tmp/repo-a", true, true);
    let roots = roots2();
    let preflight = [Some(HaltCause::TokenPool), None];
    let (draining, breaker) = (false, true);
    let causes = causes_per_root(&states, &roots, true, &preflight, draining, breaker);
    let preflight_bools = preflight.map(|c| c.is_some());
    let bools = dispatch_held_per_root_with_preflight(&states, &roots, true, &preflight_bools)
        .into_iter()
        .map(|h| h || draining || breaker)
        .collect::<Vec<bool>>();
    assert_eq!(causes.iter().map(Option::is_some).collect::<Vec<bool>>(), bools);
}

// ---- the row the operator sees ---------------------------------------------

fn halted_row(halt_causes: Option<&[Option<HaltCause>]>) -> crate::types::ReadyQueueRow {
    let mut multi = vec![(OneShotSource(Some(vec![item(9)])), Disp::default())];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[],
        10.into(),
        &[true],
        halt_causes,
        usize::MAX,
        false,
        None,
        None,
        &[],
    );
    assert_eq!(report.queue.len(), 1);
    let rows = super::super::ready_queue::finish(&report.queue, &[]);
    rows.into_iter().next().unwrap()
}

/// **The AC.** A `workspace_halted` row names its cause in `detail`, one
/// token per hold source, on the closed vocabulary.
#[test]
fn workspace_halted_row_names_its_cause_in_detail() {
    for (cause, token) in [
        (HaltCause::MainRed, "main_red"),
        (HaltCause::GatePending, "gate_pending"),
        (HaltCause::TokenPool, "token_pool"),
        (HaltCause::PreflightAdvisory, "preflight_advisory"),
        (HaltCause::Drain, "drain"),
        (HaltCause::Breaker, "breaker"),
    ] {
        let row = halted_row(Some(&[Some(cause)]));
        assert_eq!(row.disposition, QueueDisposition::WorkspaceHalted, "{token}");
        assert_eq!(row.detail.as_deref(), Some(token), "{token}");
    }
}

/// Legacy callers (no cause slice) keep a cause-less row byte-for-byte.
#[test]
fn workspace_halted_row_without_causes_keeps_a_empty_detail() {
    let row = halted_row(None);
    assert_eq!(row.disposition, QueueDisposition::WorkspaceHalted);
    assert_eq!(row.detail, None);
}

/// A cause at a not-held index is inert: `halted` stays authoritative for
/// routing, so the row is dispatched, not halted.
#[test]
fn a_cause_at_a_not_held_index_never_halts_the_workspace() {
    let mut multi = vec![(OneShotSource(Some(vec![item(9)])), Disp::default())];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[],
        10.into(),
        &[false],
        Some(&[Some(HaltCause::MainRed)]),
        usize::MAX,
        false,
        None,
        None,
        &[],
    );
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![9]);
}

/// The cause is safe on public views: `exportable_detail` passes a
/// `workspace_halted` row's detail through (closed vocabulary, #9017).
#[test]
fn exportable_detail_passes_the_cause_through() {
    use crate::telemetry::queue_snapshot::exportable_detail;
    assert_eq!(
        exportable_detail(QueueDisposition::WorkspaceHalted, Some("token_pool")),
        Some("token_pool".to_string())
    );
    assert_eq!(exportable_detail(QueueDisposition::WorkspaceHalted, None), None);
    // Free-form dispositions stay redacted.
    assert_eq!(exportable_detail(QueueDisposition::DispatchError, Some("boom")), None);
}

// ---- disk admission (#11191) ------------------------------------------------

/// `heavy` (28 GB observed charge) and `light` (1 GB), `remaining_gb` left.
fn disk_budget(remaining_gb: u64) -> crate::disk_admission::DiskBudget {
    use crate::disk_admission::{ChargeSource, RepoCharge};
    let c = |repo: &str, gb| RepoCharge {
        repo: repo.into(),
        gb,
        source: ChargeSource::Observed,
    };
    crate::disk_admission::DiskBudget {
        free_gb: remaining_gb + 3,
        floor_gb: 3,
        reserved_gb: 0,
        remaining_gb,
        sweeps_in_flight: 0,
        charges: vec![c("heavy", 28), c("light", 1)],
    }
}

#[test]
fn disk_reservation_is_a_pinned_lowest_precedence_cause() {
    assert_eq!(HaltCause::DiskReservation.as_str(), "disk_reservation");
    assert_eq!(HaltCause::from_wire("disk_reservation"), Some(HaltCause::DiskReservation));
    let b = disk_budget(14);
    // Not otherwise held: the heavy root is held for disk, the light one not.
    let folded = super::with_disk_holds(vec![None, None], Some(&b));
    assert_eq!(folded, vec![Some(HaltCause::DiskReservation), None]);
    // A root another hold already names keeps that cause.
    let folded = super::with_disk_holds(vec![Some(HaltCause::MainRed), None], Some(&b));
    assert_eq!(folded[0], Some(HaltCause::MainRed));
    // No budget (admission off or unmeasured): unchanged.
    assert_eq!(super::with_disk_holds(vec![None, None], None), vec![None, None]);
}

#[test]
fn pass_two_defers_a_heavy_repo_on_disk_and_admits_a_light_one() {
    let mut multi = vec![
        (OneShotSource(Some(vec![item(1)])), Disp::default()),
        (OneShotSource(Some(vec![item(2), item(3)])), Disp::default()),
    ];
    let report = super::super::tick_multi_with_build_backoff(
        &mut multi,
        &[1, 100],
        10.into(),
        &[false, false],
        None,
        usize::MAX,
        false,
        None,
        (None, None, Some(disk_budget(14))),
        &[],
        &[],
    );
    assert!(multi[0].1.dispatched.is_empty(), "28 GB does not fit 14 GB");
    assert_eq!(multi[1].1.dispatched, vec![2, 3], "a light repo still admits");
    assert_eq!((report.dispatched, report.deferred_capacity), (2, 1));
    let rows = super::super::ready_queue::finish(&report.queue, &[]);
    let heavy = rows.iter().find(|r| r.issue == 1).unwrap();
    assert_eq!(heavy.disposition, QueueDisposition::DeferredCapacity);
    let detail = heavy.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("disk: heavy charge 28GB (observed) exceeds remaining 14GB"),
        "{detail}"
    );
}

#[test]
fn pass_two_debits_each_disk_admission() {
    // 30 GB: one heavy sweep fits (28), the second does not, and the 2 GB
    // left then admits two light sweeps but not a third.
    let mut multi = vec![
        (OneShotSource(Some(vec![item(1), item(2)])), Disp::default()),
        (OneShotSource(Some(vec![item(3), item(4), item(5)])), Disp::default()),
    ];
    let report = super::super::tick_multi_with_build_backoff(
        &mut multi,
        &[1, 100],
        10.into(),
        &[false, false],
        None,
        usize::MAX,
        false,
        None,
        (None, None, Some(disk_budget(30))),
        &[],
        &[],
    );
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert_eq!(multi[1].1.dispatched, vec![3, 4]);
    assert_eq!(report.deferred_capacity, 2);
}
