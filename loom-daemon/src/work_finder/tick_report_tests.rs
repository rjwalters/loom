//! Typed dispatch-refusal reasons and admission spans (Issue #8907).

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::{Duration, Utc};

use super::super::{tick, tick_multi, WorkDispatcher, WorkItem, WorkSource};
use super::{record_dispatch_outcome, TickReport};
use crate::observability::ops::dispatch::{admission_spans, decision_counts, tick_span};
use crate::sweep_registry::{
    ClaimLockDispatchError, CollisionDispatchError, CollisionSource, LeaseOrderDispatchError,
    TokenSelectionDispatchError,
};
use crate::telemetry::trace::{SpanName, SpanStatus};
use crate::types::QueueDisposition as Qd;

fn lease_order(issue: u32) -> anyhow::Error {
    LeaseOrderDispatchError {
        issue,
        sweep_id: "sweep-issue-1-1".into(),
        earliest_host: "peer".into(),
        earliest_sweep_id: "sweep-issue-1-0".into(),
    }
    .into()
}

fn token_selection(issue: u32) -> anyhow::Error {
    TokenSelectionDispatchError {
        issue,
        log_path: PathBuf::from("/tmp/sweep.log"),
    }
    .into()
}

fn reason(report: &TickReport, name: &str) -> usize {
    decision_counts(report)
        .iter()
        .find(|(r, _)| *r == name)
        .map(|(_, n)| *n)
        .unwrap()
}

/// Every candidate lands in exactly one reason: the subsets are netted out.
fn assert_partition(report: &TickReport, attempts: usize) {
    let total: usize = decision_counts(report).iter().map(|(_, n)| n).sum();
    assert_eq!(total, attempts, "{report:?}");
}

struct Source(Vec<WorkItem>);

impl WorkSource for Source {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.clone())
    }
}

/// Refuses every dispatch with an error built by `refuse`, except the issues
/// in `ok`, which dispatch.
struct Refusing {
    refuse: fn(u32) -> anyhow::Error,
    ok: HashSet<u32>,
}

impl WorkDispatcher for Refusing {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> Result<bool> {
        if self.ok.contains(&issue) {
            Ok(true)
        } else {
            Err((self.refuse)(issue))
        }
    }
}

fn items(numbers: &[u32]) -> Vec<WorkItem> {
    numbers
        .iter()
        .map(|n| WorkItem::new(*n, vec!["loom:issue".into()]))
        .collect()
}

#[test]
fn a_lost_lease_order_race_is_lease_order_lost_not_backoff() {
    let mut source = Source(items(&[7]));
    let mut disp = Refusing {
        refuse: lease_order,
        ok: HashSet::new(),
    };
    let report = tick(&mut source, &mut disp, 4, false).unwrap();
    // The legacy tally (log line, `loom-daemon health`) is unchanged.
    assert_eq!(report.skipped_backoff, 1);
    assert_eq!(report.refused_lease_order, 1);
    assert_eq!(reason(&report, "lease_order_lost"), 1);
    assert_eq!(reason(&report, "backoff"), 0);
    assert_partition(&report, 1);
    assert_eq!(report.admissions.len(), 1);
    assert_eq!(report.admissions[0].reason, "lease_order_lost");
    assert_eq!(report.admissions[0].result, "refused");
}

#[test]
fn a_token_selection_failure_is_token_selection_failed_not_error() {
    let mut multi = vec![(
        Source(items(&[3, 4])),
        Refusing {
            refuse: token_selection,
            ok: HashSet::from([4]),
        },
    )];
    let report = tick_multi(&mut multi, &[], 4, &[false]);
    assert_eq!(report.errors, 1, "still a real failure on the legacy tally");
    assert_eq!(reason(&report, "token_selection_failed"), 1);
    assert_eq!(reason(&report, "error"), 0);
    assert_eq!(reason(&report, "dispatched"), 1);
    assert_partition(&report, 2);
    // The ready-queue row keeps its pre-#8907 disposition.
    let row = report.queue.iter().find(|r| r.key.number == 3).unwrap();
    assert_eq!(row.disposition, Some(Qd::DispatchError));
    assert_eq!(report.occupancy, Some(1));
}

#[test]
fn collisions_and_claim_lock_contention_get_their_own_reasons() {
    let mut report = TickReport::default();
    let collision: Result<bool> = Err(CollisionDispatchError {
        issue: 1,
        source: CollisionSource::PeerClaim,
    }
    .into());
    let lock: Result<bool> = Err(ClaimLockDispatchError {
        issue: 2,
        lock: PathBuf::from("/tmp/locks/issue-2"),
    }
    .into());
    // Context wrapping (as `?` chains add) must not defeat the downcast.
    let lock = lock.map_err(|e| e.context("dispatch_inner"));
    let other: Result<bool> = Err(anyhow!("spawn failed"));
    let now = Utc::now();
    assert_eq!(record_dispatch_outcome(&mut report, 1, now, &collision).0, Qd::DispatchError);
    record_dispatch_outcome(&mut report, 2, now, &lock);
    record_dispatch_outcome(&mut report, 3, now, &other);
    assert_eq!(report.errors, 3);
    assert_eq!(reason(&report, "claim_collision"), 1);
    assert_eq!(reason(&report, "claim_lock_held"), 1);
    assert_eq!(reason(&report, "error"), 1);
    assert_partition(&report, 3);
    let results: Vec<_> = report.admissions.iter().map(|a| a.result).collect();
    assert_eq!(results, ["refused", "refused", "error"]);
}

#[test]
fn the_claim_lock_error_keeps_its_text() {
    let e = ClaimLockDispatchError {
        issue: 9,
        lock: PathBuf::from("/l/issue-9"),
    };
    assert_eq!(
        e.to_string(),
        "lock collision: issue #9 is already claimed (lock at /l/issue-9)"
    );
}

#[test]
fn each_dispatch_attempt_is_one_admission_span_under_the_tick_span() {
    let mut multi = vec![(
        Source(items(&[1, 2, 3])),
        Refusing {
            refuse: lease_order,
            ok: HashSet::from([1, 3]),
        },
    )];
    let started = Utc::now() - Duration::seconds(1);
    let report = tick_multi(&mut multi, &[], 4, &[false]);
    let tick = tick_span(&report, 4, started, Utc::now());
    let spans = admission_spans(&report, &tick);
    assert_eq!(spans.len(), 3);
    for span in &spans {
        assert_eq!(span.name, SpanName::DispatchAdmission);
        assert_eq!(span.context.trace_id, tick.context.trace_id);
        assert_eq!(span.parent_span_id.as_ref(), Some(&tick.context.span_id));
        assert_ne!(span.context.span_id, tick.context.span_id);
        assert!(span.started_at >= tick.started_at && span.ended_at <= tick.ended_at);
        assert!(span.validate().is_ok());
        assert_eq!(span.status, SpanStatus::Ok);
        // Every attribute survives the export-time allowlist.
        assert_eq!(span.clone().bounded().attributes, span.attributes);
    }
    let issues: Vec<_> = spans
        .iter()
        .map(|s| s.attributes["loom.issue"].clone())
        .collect();
    assert_eq!(issues, ["1", "2", "3"]);
    assert_eq!(spans[1].attributes["loom.dispatch.reason"], "lease_order_lost");
    assert_eq!(spans[0].attributes["loom.dispatch.admission_result"], "dispatched");
}

#[test]
fn a_tick_with_no_dispatch_attempt_has_no_admission_spans() {
    let report = TickReport::default();
    let tick = tick_span(&report, 1, Utc::now(), Utc::now());
    assert!(admission_spans(&report, &tick).is_empty());
}
