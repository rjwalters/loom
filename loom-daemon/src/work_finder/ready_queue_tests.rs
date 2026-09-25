//! Coverage for the per-issue ready queue (Issue #8852). Its own file with
//! its own minimal fakes, for the file-size-ratchet reason
//! `prless_retry_tests.rs` gives: `work_finder/tests.rs` is frozen.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::PathBuf;

use super::super::*;
use crate::types::{QueueDisposition, ReadyQueueRow};

struct OneShotSource(Option<Vec<WorkItem>>);

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

fn item(n: u32, labels: &[&str], created: &str) -> WorkItem {
    WorkItem::with_created_at(
        n,
        labels.iter().map(|l| (*l).to_string()).collect(),
        Some(created.to_string()),
    )
}

#[derive(Default)]
struct Disp {
    dispatched: Vec<u32>,
    in_flight: HashSet<u32>,
    backed_off: HashSet<u32>,
    peer: HashSet<u32>,
    open_pr: HashSet<u32>,
}

impl WorkDispatcher for Disp {
    fn in_flight(&self) -> HashSet<u32> {
        self.in_flight.clone()
    }
    fn backed_off(&self) -> HashSet<u32> {
        self.backed_off.clone()
    }
    fn peer_claimed(&self) -> HashSet<u32> {
        self.peer.clone()
    }
    fn occupancy(&self) -> usize {
        self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> Result<bool> {
        if self.open_pr.contains(&issue) {
            return Err(OpenPrDispatchError { issue, pr: 900 }.into());
        }
        self.dispatched.push(issue);
        Ok(true)
    }
}

fn rows(report: &TickReport, roots: &[PathBuf]) -> Vec<ReadyQueueRow> {
    ready_queue::finish(&report.queue, roots)
}

fn order(rows: &[ReadyQueueRow]) -> Vec<(u32, QueueDisposition)> {
    rows.iter().map(|r| (r.issue, r.disposition)).collect()
}

/// The queue follows the daemon's own comparator: workspace priority, then
/// `loom:urgent`, then oldest first. A `tier:*` label is carried but does not
/// move the issue.
#[test]
fn queue_is_ranked_by_dispatch_order_not_tier() {
    let mut multi = vec![
        (
            OneShotSource(Some(vec![
                item(5, &["loom:issue", "tier:goal-advancing"], "2026-09-03T00:00:00Z"),
                item(6, &["loom:issue", "loom:urgent"], "2026-09-04T00:00:00Z"),
                item(7, &["loom:issue"], "2026-09-01T00:00:00Z"),
            ])),
            Disp::default(),
        ),
        (
            OneShotSource(Some(vec![item(1, &["loom:issue"], "2026-09-09T00:00:00Z")])),
            Disp::default(),
        ),
    ];
    // Workspace 1 has the higher priority (lower number).
    let report = tick_multi(&mut multi, &[100, 0], 2, &[false, false]);
    let roots = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
    let q = rows(&report, &roots);

    assert_eq!(
        order(&q),
        vec![
            (1, QueueDisposition::Dispatched),
            (6, QueueDisposition::Dispatched),
            (7, QueueDisposition::DeferredCapacity),
            (5, QueueDisposition::DeferredCapacity),
        ]
    );
    assert_eq!(q[0].repo, "/repo/b");
    assert_eq!(q[0].rank, 1);
    assert_eq!(q[3].tier.as_deref(), Some("tier:goal-advancing"));
    assert!(q[1].urgent);
}

/// Every seen issue gets exactly one row, and the row names the same reason
/// the aggregate counter records.
#[test]
fn every_seen_issue_gets_one_row_with_its_reason() {
    let mut multi = vec![(
        OneShotSource(Some(vec![
            item(1, &["loom:issue"], "2026-09-01T00:00:00Z"),
            item(2, &["loom:issue", "loom:blocked"], "2026-09-02T00:00:00Z"),
            item(3, &["loom:issue"], "2026-09-03T00:00:00Z"),
            item(4, &["loom:issue"], "2026-09-04T00:00:00Z"),
            item(5, &["loom:issue"], "2026-09-05T00:00:00Z"),
            item(6, &["loom:issue"], "2026-09-06T00:00:00Z"),
        ])),
        Disp {
            in_flight: HashSet::from([3]),
            backed_off: HashSet::from([4]),
            peer: HashSet::from([5]),
            open_pr: HashSet::from([1]),
            ..Default::default()
        },
    )];
    let report = tick_multi(&mut multi, &[], 10, &[false]);
    let q = rows(&report, &[]);

    assert_eq!(q.len(), report.seen);
    assert_eq!(
        order(&q),
        vec![
            (1, QueueDisposition::OpenPr),
            (2, QueueDisposition::Parked),
            (3, QueueDisposition::InFlight),
            (4, QueueDisposition::DispatchBackoff),
            (5, QueueDisposition::PeerClaim),
            (6, QueueDisposition::Dispatched),
        ]
    );
    assert_eq!(q[0].detail.as_deref(), Some("open PR #900"));
    assert_eq!(q[1].detail.as_deref(), Some("loom:blocked"));
    assert_eq!(q[0].repo, "workspace #0");
}

/// A halted repo's issues still appear, marked blocked by the gate, so an
/// empty "ready" list is never mistaken for an empty backlog.
#[test]
fn a_halted_workspace_reports_its_issues_as_blocked() {
    let mut multi = vec![(
        OneShotSource(Some(vec![item(9, &["loom:issue"], "2026-09-01T00:00:00Z")])),
        Disp::default(),
    )];
    let report = tick_multi(&mut multi, &[], 10, &[true]);
    let q = rows(&report, &[]);

    assert_eq!(order(&q), vec![(9, QueueDisposition::WorkspaceHalted)]);
    assert_eq!(q[0].disposition.state(), "blocked");
}

/// The published summary carries the rows with repo names; an older wire
/// payload without `queue` still parses.
#[test]
fn summary_carries_named_rows_and_old_payloads_parse() {
    let mut multi = vec![(
        OneShotSource(Some(vec![item(3, &["loom:issue"], "2026-09-01T00:00:00Z")])),
        Disp::default(),
    )];
    let report = tick_multi(&mut multi, &[], 10, &[false]);
    let summary = tick_summary(&report, 10, chrono::Utc::now(), &[PathBuf::from("/r")]);
    assert_eq!(summary.queue.len(), 1);
    assert_eq!(summary.queue[0].repo, "/r");

    let mut json = serde_json::to_value(&summary).unwrap();
    json.as_object_mut().unwrap().remove("queue");
    let old: crate::types::WorkFinderTickSummary = serde_json::from_value(json).unwrap();
    assert!(old.queue.is_empty());
}

#[test]
fn short_detail_keeps_one_bounded_line() {
    assert_eq!(ready_queue::short_detail("a\nb"), "a");
    let long = "x".repeat(400);
    assert_eq!(ready_queue::short_detail(&long).chars().count(), 161);
}

/// Pass-2 limits resolve to their own dispositions: the ramp cap, the
/// saturation brake, and the repo-sharding slice.
#[test]
fn pass_two_limits_resolve_to_their_own_dispositions() {
    let src = |ns: &[u32]| {
        OneShotSource(Some(
            ns.iter()
                .map(|n| item(*n, &["loom:issue"], &format!("2026-09-0{n}T00:00:00Z")))
                .collect(),
        ))
    };
    let mut multi = vec![(src(&[1, 2]), Disp::default())];
    let r = tick_multi_with_sharding(&mut multi, &[], 10, &[false], 1, false, None);
    assert_eq!(
        order(&rows(&r, &[])),
        vec![
            (1, QueueDisposition::Dispatched),
            (2, QueueDisposition::DeferredRampCap)
        ]
    );

    let mut multi = vec![(src(&[3]), Disp::default())];
    let r = tick_multi_with_sharding(&mut multi, &[], 10, &[false], 10, true, None);
    assert_eq!(order(&rows(&r, &[])), vec![(3, QueueDisposition::DeferredSaturation)]);

    let mut multi = vec![(src(&[4]), Disp::default()), (src(&[5]), Disp::default())];
    let r = tick_multi_with_sharding(
        &mut multi,
        &[],
        10,
        &[false, false],
        10,
        false,
        Some(&[false, true]),
    );
    assert_eq!(
        order(&rows(&r, &[])),
        vec![
            (4, QueueDisposition::DeferredOutOfSlice),
            (5, QueueDisposition::Dispatched)
        ]
    );
}
