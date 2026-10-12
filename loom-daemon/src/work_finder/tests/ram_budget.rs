//! The per-repo RAM charge (#11094) in pass 2 of a multi-workspace tick: each
//! candidate is charged its OWN repo's observed peak, and every admission
//! debits the tick's remaining budget, so a heavy repo's history defers only
//! that repo's work while a small sibling still dispatches.

use super::*;
use crate::ram_headroom::{RamBudget, RepoCharge};
use crate::ram_peaks::ChargeSource;

fn starred(n: u32) -> WorkItem {
    WorkItem::new(n, vec![OPERATOR_PRIORITY_LABEL.to_string()])
}

/// Workspace 0 is `heavy` (15 GB observed charge), workspace 1 is `small`
/// (2 GB default charge).
fn budget(remaining_gb: u64) -> RamBudget {
    let c = |repo: &str, gb, source| RepoCharge {
        repo: repo.into(),
        gb,
        source,
    };
    RamBudget {
        remaining_gb,
        charges: vec![
            c("heavy", 15, ChargeSource::Observed),
            c("small", 2, ChargeSource::Default),
        ],
    }
}

fn run(multi: &mut [(FakeSource, RecordingDispatcher)], max: usize, ram: RamBudget) -> TickReport {
    tick_multi_with_build_backoff(
        multi,
        // `heavy` sorts first, so the small candidate is reached only if the
        // heavy one's deferral is work-conserving.
        &[1, 100],
        max.into(),
        &[false, false],
        None,
        usize::MAX,
        false,
        None,
        (None, Some(ram)),
        &[],
        &[],
    )
}

fn ram_rows(report: &TickReport) -> Vec<(u32, String)> {
    ready_queue::finish(&report.queue, &[])
        .into_iter()
        .filter(|r| r.disposition == Qd::DeferredCapacity)
        .map(|r| (r.issue, r.detail.unwrap_or_default()))
        .collect()
}

#[test]
fn small_repo_proceeds_while_heavy_repo_is_deferred() {
    // 14 GB left: the heavy repo's 15 GB charge does not fit, the small
    // repo's 2 GB does. The shared cap is not binding (10).
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let report = run(&mut multi, 10, budget(14));
    assert!(multi[0].1.dispatched.is_empty(), "heavy candidate deferred");
    assert_eq!(multi[1].1.dispatched, vec![2], "small candidate proceeds");
    assert_eq!((report.dispatched, report.deferred_capacity), (1, 1));
    let rows = ram_rows(&report);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 1);
    assert!(rows[0].1.contains("heavy charge 15GB (observed)"), "{}", rows[0].1);
}

#[test]
fn each_admission_debits_the_remaining_budget() {
    // 20 GB: one heavy sweep (15) fits, the second does not; the 5 GB left
    // then admits two small sweeps (2 + 2) and defers the third.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
        (
            FakeSource::once(vec![issue(3), issue(4), issue(5)]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = run(&mut multi, 10, budget(20));
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert_eq!(multi[1].1.dispatched, vec![3, 4]);
    assert_eq!(report.deferred_capacity, 2);
    let deferred: Vec<u32> = ram_rows(&report).into_iter().map(|(n, _)| n).collect();
    assert_eq!(deferred, vec![2, 5]);
}

#[test]
fn a_starred_overflow_candidate_still_meets_its_ram_charge() {
    // The shared cap is full (1 running, cap 1), so a star would take the
    // overflow slot — but its heavy repo's charge does not fit.
    let busy = RecordingDispatcher {
        in_flight: [99].into_iter().collect(),
        ..RecordingDispatcher::default()
    };
    let mut multi = vec![
        (FakeSource::once(vec![starred(1)]), busy),
        (FakeSource::once(vec![]), RecordingDispatcher::default()),
    ];
    let report = run(&mut multi, 1, budget(14));
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!((report.dispatched, report.dispatched_overflow), (0, 0));
    assert_eq!(ram_rows(&report).len(), 1);
}
