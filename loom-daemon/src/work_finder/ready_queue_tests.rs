//! Coverage for the per-issue ready queue (Issue #8852). Its own file with
//! its own minimal fakes, for the file-size-ratchet reason
//! `prless_retry_tests.rs` gives: `work_finder/tests.rs` is frozen.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::PathBuf;

use super::super::*;
use crate::types::{QueueDisposition, ReadyQueueRow};

struct OneShotSource(Option<Vec<WorkItem>>);

/// A source whose forge listing may fail (`None`).
struct MaybeSource(Option<OneShotSource>);

impl WorkSource for MaybeSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        match &mut self.0 {
            Some(src) => src.list_ready_issues(),
            None => Err(anyhow::anyhow!("gh: not found")),
        }
    }
}

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
    quarantined: HashSet<u32>,
    noop: HashSet<u32>,
    declined: HashSet<u32>,
    prless: HashSet<u32>,
    fail: HashSet<u32>,
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
    fn quarantined(&self) -> HashSet<u32> {
        self.quarantined.clone()
    }
    fn noop_cooldown(&self) -> HashSet<u32> {
        self.noop.clone()
    }
    fn declined(&self) -> HashSet<u32> {
        self.declined.clone()
    }
    fn prless_retry(&self) -> HashSet<u32> {
        self.prless.clone()
    }
    fn occupancy(&self) -> usize {
        self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> Result<bool> {
        if self.fail.contains(&issue) {
            return Err(anyhow::anyhow!("spawn failed\nsecond line"));
        }
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

/// The queue follows the daemon's own comparator (#9244): starred first, then
/// workspace priority, then oldest first. A `tier:*` label is carried but does
/// not move the issue.
#[test]
fn queue_is_ranked_by_dispatch_order_not_tier() {
    let mut multi = vec![
        (
            OneShotSource(Some(vec![
                item(5, &["loom:issue", "tier:goal-advancing"], "2026-09-03T00:00:00Z"),
                item(6, &["loom:issue", "loom:operator-priority"], "2026-09-04T00:00:00Z"),
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
            (6, QueueDisposition::Dispatched),
            (1, QueueDisposition::Dispatched),
            (7, QueueDisposition::DeferredCapacity),
            (5, QueueDisposition::DeferredCapacity),
        ]
    );
    assert_eq!(q[1].repo, "/repo/b");
    assert_eq!(q[0].rank, 1);
    assert_eq!(q[3].tier.as_deref(), Some("tier:goal-advancing"));
    assert!(
        q[0].operator_priority && !q[0].urgent,
        "starred is reported, urgent is always false"
    );
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
    let summary = tick_summary(&report, 10, chrono::Utc::now(), &[PathBuf::from("/r")], None);
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

/// Brakes that live on the dispatcher, and a real dispatch failure, each get
/// their own disposition.
#[test]
fn dispatcher_brakes_and_errors_get_their_own_rows() {
    let items = (1..=5)
        .map(|n| item(n, &["loom:issue"], &format!("2026-09-0{n}T00:00:00Z")))
        .collect();
    let mut multi = vec![(
        OneShotSource(Some(items)),
        Disp {
            quarantined: HashSet::from([1]),
            noop: HashSet::from([2]),
            declined: HashSet::from([3]),
            prless: HashSet::from([4]),
            fail: HashSet::from([5]),
            ..Default::default()
        },
    )];
    let report = tick_multi(&mut multi, &[], 10, &[false]);
    let q = rows(&report, &[]);
    assert_eq!(
        order(&q),
        vec![
            (1, QueueDisposition::Quarantined),
            (2, QueueDisposition::NoopCooldown),
            (3, QueueDisposition::Declined),
            (4, QueueDisposition::PrlessRetry),
            (5, QueueDisposition::DispatchError),
        ]
    );
    assert_eq!(q[4].detail.as_deref(), Some("spawn failed"));
}

/// A repo whose listing fails is named on the summary, so its missing
/// backlog reads as incomplete rather than empty.
#[test]
fn a_failed_listing_is_named_not_shown_as_empty() {
    let ok = OneShotSource(Some(vec![item(2, &["loom:issue"], "2026-09-01T00:00:00Z")]));
    let mut multi = vec![
        (MaybeSource(None), Disp::default()),
        (MaybeSource(Some(ok)), Disp::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);
    let roots = [PathBuf::from("/repo/broken"), PathBuf::from("/repo/ok")];
    let summary = tick_summary(&report, 10, chrono::Utc::now(), &roots, None);
    assert_eq!(summary.errors, 1);
    assert_eq!(summary.listing_failed, vec!["/repo/broken".to_string()]);
    assert_eq!(order(&summary.queue), vec![(2, QueueDisposition::Dispatched)]);
}

// ===================================================================
// candidate_keys — the single dispatch-order seam (Issue #9288)
// ===================================================================

fn oldest_first(a: Option<&String>, b: Option<&String>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// The documented #3946/#9244 order, written out independently of
/// `candidate_keys`: starred first; among starred, starred-at (else
/// `createdAt`) oldest first; red-main fixes first; workspace priority asc;
/// `createdAt` oldest first; number asc. A change to dispatch order must
/// update both this and `candidate_keys` — which is the point.
fn reference_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let starred_at =
        |c: &PriorityCandidate| c.operator_priority_at.clone().or(c.created_at.clone());
    let lane = b.operator_priority.cmp(&a.operator_priority).then(
        if a.operator_priority && b.operator_priority {
            oldest_first(starred_at(a).as_ref(), starred_at(b).as_ref())
        } else {
            Ordering::Equal
        },
    );
    lane.then(b.main_red_fix.cmp(&a.main_red_fix))
        .then(a.workspace_priority.cmp(&b.workspace_priority))
        .then(oldest_first(a.created_at.as_ref(), b.created_at.as_ref()))
        .then(a.number.cmp(&b.number))
}

/// Every candidate over a small domain that exercises every key's tie and
/// both directions of every key, including the starred-at fallback.
fn candidate_domain() -> Vec<PriorityCandidate> {
    let times = [
        None,
        Some("2026-01-01T00:00:00Z"),
        Some("2026-02-01T00:00:00Z"),
    ];
    let mut out = Vec::new();
    for workspace_priority in [0, 100] {
        for operator_priority in [false, true] {
            for starred_at in times {
                for main_red_fix in [false, true] {
                    for created_at in times {
                        for number in [1, 2] {
                            out.push(PriorityCandidate {
                                workspace_priority,
                                operator_priority,
                                operator_priority_at: starred_at.map(str::to_string),
                                main_red_fix,
                                created_at: created_at.map(str::to_string),
                                number,
                                ..PriorityCandidate::default()
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

/// The property, exhaustively over every pair in the domain: the
/// lexicographic compare of `candidate_keys` IS `candidate_cmp`, and both
/// match the documented order; the first three keys are `lane_cmp`.
#[test]
fn candidate_keys_lexicographic_compare_is_candidate_cmp() {
    let domain = candidate_domain();
    for a in &domain {
        for b in &domain {
            let by_keys = ready_queue::candidate_keys(a).cmp(&ready_queue::candidate_keys(b));
            assert_eq!(by_keys, candidate_cmp(a, b), "{a:?} vs {b:?}");
            assert_eq!(by_keys, reference_cmp(a, b), "{a:?} vs {b:?}");
        }
    }
}

/// The wire projection names the keys in comparator order, carries each
/// row's own values, and distinguishes every pair the comparator does.
#[test]
fn candidate_keys_project_to_named_wire_keys() {
    assert_eq!(
        ready_queue::ordering_names(),
        vec![
            "operator_priority",
            "operator_priority_at",
            "main_red_fix",
            "workspace_priority",
            "created_at",
            "number"
        ]
    );
    let c = PriorityCandidate {
        workspace_priority: 7,
        operator_priority: true,
        created_at: Some("2026-01-01T00:00:00Z".into()),
        number: 42,
        ..PriorityCandidate::default()
    };
    let values: Vec<serde_json::Value> = ready_queue::plan_keys(&c)
        .into_iter()
        .map(|k| k.value)
        .collect();
    // A starred candidate with no starred-at orders by its createdAt.
    assert_eq!(
        values,
        vec![
            serde_json::json!(true),
            serde_json::json!("2026-01-01T00:00:00Z"),
            serde_json::json!(false),
            serde_json::json!(7),
            serde_json::json!("2026-01-01T00:00:00Z"),
            serde_json::json!(42),
        ]
    );
    let domain = candidate_domain();
    for a in &domain {
        for b in &domain {
            let equal_keys = ready_queue::plan_keys(a) == ready_queue::plan_keys(b);
            assert_eq!(equal_keys, candidate_cmp(a, b).is_eq(), "{a:?} vs {b:?}");
        }
    }
}
