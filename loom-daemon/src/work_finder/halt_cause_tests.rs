//! Coverage for the closed hold-cause vocabulary (Issue #9017). Its own file
//! with its own minimal fakes, for the file-size-ratchet reason
//! `ready_queue_tests.rs` gives: `work_finder/tests.rs` is frozen.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::PathBuf;

use super::super::{tick_multi_with_build_backoff, WorkDispatcher, WorkItem, WorkSource};
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
    fn dispatch(&mut self, issue: u32) -> anyhow::Result<bool> {
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
    let got = causes_per_root(&states, &roots2(), false, &[None, None], false);
    assert_eq!(got, vec![Some(HaltCause::MainRed), None]);

    // gate_pending: root A gate in flight, suppressor on.
    let states = states_with("/tmp/repo-a", false, true);
    let got = causes_per_root(&states, &roots2(), true, &[None, None], false);
    assert_eq!(got, vec![Some(HaltCause::GatePending), None]);

    // gate in flight but suppressor OFF ⇒ not held (byte-for-byte the bool
    // fold's pre-#4084 behaviour).
    let got = causes_per_root(&states, &roots2(), false, &[None, None], false);
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
    let got = causes_per_root(&states, &roots2(), false, &[Some(HaltCause::TokenPool)], false);
    assert_eq!(got, vec![Some(HaltCause::TokenPool), None]);

    // drain is daemon-global: it names every root.
    let got = causes_per_root(&states, &roots2(), false, &[None, None], true);
    assert_eq!(got, vec![Some(HaltCause::Drain), Some(HaltCause::Drain)]);
}

// ---- the row the operator sees ---------------------------------------------

fn halted_row(halt_causes: Option<&[Option<HaltCause>]>) -> crate::types::ReadyQueueRow {
    let mut multi = vec![(OneShotSource(Some(vec![item(9)])), Disp::default())];
    let report = tick_multi_with_build_backoff(&mut multi, &[], 10, &[true], halt_causes, &[], &[]);
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
    let report = tick_multi_with_build_backoff(
        &mut multi,
        &[],
        10,
        &[false],
        Some(&[Some(HaltCause::MainRed)]),
        &[],
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

// ---- the tick's own result (#11649) ----------------------------------------

/// #11742: a held workspace keeps its own hold's cause even at weight 0 (a
/// weightless `fleet: maintain` repo reads `maintain_only`, not
/// `weight_zero`); an unheld weight-0 workspace reads `weight_zero`.
#[test]
fn a_hold_cause_wins_over_weight_zero() {
    let row = |held: bool| {
        let mut multi = vec![(OneShotSource(Some(vec![item(9)])), Disp::default())];
        let report = tick_multi_with_build_backoff(
            &mut multi,
            &[0],
            10,
            &[held],
            Some(&[held.then_some(HaltCause::MaintainOnly)]),
            &[],
            &[],
        );
        let rows = super::super::ready_queue::finish(&report.queue, &[]);
        rows.into_iter().next().unwrap()
    };
    assert_eq!(row(true).detail.as_deref(), Some("maintain_only"));
    assert_eq!(row(false).detail.as_deref(), Some("weight_zero"));
    assert_eq!(row(false).disposition, QueueDisposition::WorkspaceHalted);
}

// ---- a partial hold does not name the tick (#11857) ------------------------

/// Run one multi-workspace tick over `backlogs` (one ready list per root) with
/// `held[i]` holding root `i` as maintain-only, and return the report plus
/// its `loom.dispatch.tick` result.
fn held_tick(
    backlogs: Vec<Vec<WorkItem>>,
    held: &[bool],
) -> (super::super::TickReport, &'static str) {
    let mut multi: Vec<_> = backlogs
        .into_iter()
        .map(|ready| (OneShotSource(Some(ready)), Disp::default()))
        .collect();
    let causes: Vec<Option<HaltCause>> = held
        .iter()
        .map(|&h| h.then_some(HaltCause::MaintainOnly))
        .collect();
    let report = tick_multi_with_build_backoff(&mut multi, &[], 10, held, Some(&causes), &[], &[]);
    let result = crate::observability::ops::dispatch::tick_result_with(&report, false);
    (report, result)
}

/// **#11857 acceptance.** One maintain-only root beside healthy roots that
/// have nothing dispatchable: the hold does not cover the tick's work, so the
/// tick reads its per-candidate result, not `halted_maintain_only`. The cause
/// and the held count stay on the report for the span.
#[test]
fn one_held_root_beside_healthy_idle_roots_does_not_name_the_tick() {
    // The healthy roots' candidates are all skipped (parked), so nothing was
    // dispatchable there: that, not the hold, is why nothing started.
    let parked = |n| WorkItem::with_created_at(n, vec!["loom:blocked".to_string()], None);
    let (report, result) = held_tick(
        vec![vec![item(1), item(2)], vec![parked(3)], vec![parked(4)]],
        &[true, false, false],
    );
    assert!(report.halted, "the hold is still recorded");
    assert!(report.halt_partial);
    assert_eq!(report.halt_causes, vec![HaltCause::MaintainOnly]);
    assert_eq!(report.held_roots, 1);
    assert_eq!(result, "all_skipped");

    // Nothing ready anywhere, one root of three held: an idle tick.
    let (report, result) = held_tick(vec![vec![], vec![], vec![]], &[true, false, false]);
    assert_eq!((report.held_roots, report.halt_partial), (1, true));
    assert_eq!(result, "no_eligible_work");
    assert!(!report.halted_fully(), "a partial hold leaves tick-level reactions on");
}

/// #11857: the hold names the tick when it covers the work — every root with
/// candidates held (a healthy empty root does not dilute it), or every root
/// held when none has any.
#[test]
fn a_hold_covering_every_root_with_work_names_the_tick() {
    let (report, result) = held_tick(vec![vec![item(1)], vec![item(2)]], &[true, true]);
    assert_eq!((report.held_roots, report.halt_partial), (2, false));
    assert_eq!(result, "halted_maintain_only");
    assert!(report.halted_fully());

    // The held root is the only one with work: the hold explains the tick.
    let (report, result) = held_tick(vec![vec![item(1)], vec![]], &[true, false]);
    assert_eq!((report.held_roots, report.halt_partial), (1, false));
    assert_eq!(result, "halted_maintain_only");

    // Nothing ready anywhere and every root held.
    let (report, result) = held_tick(vec![vec![], vec![]], &[true, true]);
    assert_eq!((report.held_roots, report.halt_partial), (2, false));
    assert_eq!(result, "halted_maintain_only");

    // No hold at all: nothing counted, nothing partial.
    let (report, result) = held_tick(vec![vec![], vec![]], &[false, false]);
    assert_eq!((report.halted, report.held_roots, report.halt_partial), (false, 0, false));
    assert_eq!(result, "no_eligible_work");
}
