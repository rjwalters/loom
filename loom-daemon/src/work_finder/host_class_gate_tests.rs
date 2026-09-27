//! Tick-level coverage for the host-class gate (Issue #9034): a `loom:heavy`
//! candidate must never be dispatched on a `host_class: local-dev` host
//! unless explicitly overridden — in both the single-workspace `tick` and the
//! multi-workspace `tick_multi`.
//!
//! Lives in its own file rather than in the shared `work_finder::tests`
//! module for the file-size-ratchet reason `prless_retry_tests` is split out
//! (`.loom/docs/file-size-policy.md`): `work_finder/tests.rs` is over
//! threshold and frozen. Its `RecordingDispatcher` is consequently not
//! reused — this file carries its own minimal fake, mirroring
//! `prless_retry_tests.rs`'s `BrakeDispatcher` exactly.

use super::*;
use std::collections::HashSet;

struct OneShotSource(Option<Vec<WorkItem>>);

impl OneShotSource {
    fn of(items: Vec<WorkItem>) -> Self {
        Self(Some(items))
    }
}

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

fn heavy(n: u32) -> WorkItem {
    WorkItem::new(
        n,
        vec![
            "loom:issue".to_string(),
            host_class::LOOM_HEAVY_LABEL.to_string(),
        ],
    )
}

fn plain(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".to_string()])
}

fn policy(class: host_class::HostClass, allow_heavy_local: bool) -> host_class::HeavyLocalPolicy {
    host_class::HeavyLocalPolicy {
        class,
        allow_heavy_local,
    }
}

/// A [`WorkDispatcher`] that records what it was asked to dispatch and
/// reports a configurable [`host_class::HeavyLocalPolicy`]. Every other trait
/// method keeps its default (empty / false / unclassified) implementation.
#[derive(Default)]
struct PolicyDispatcher {
    dispatched: Vec<u32>,
    policy: host_class::HeavyLocalPolicy,
}

impl WorkDispatcher for PolicyDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn heavy_local_policy(&self) -> host_class::HeavyLocalPolicy {
        self.policy
    }
    fn occupancy(&self) -> usize {
        self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> Result<bool> {
        self.dispatched.push(issue);
        Ok(true)
    }
}

// ===================================================================
// Single-workspace `tick`
// ===================================================================

/// The core acceptance criterion: a `loom:heavy` issue is never claimed on a
/// `local-dev` host — `dispatch()` is never called.
#[test]
fn tick_never_dispatches_a_heavy_issue_on_a_local_dev_host() {
    let mut source = OneShotSource::of(vec![heavy(9)]);
    let mut disp = PolicyDispatcher {
        policy: policy(host_class::HostClass::LocalDev, false),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.skipped_host_class, 1);
    assert!(disp.dispatched.is_empty(), "dispatch() must never be called");
}

/// The same issue, on a `remote-worker` host, dispatches exactly as an
/// unconstrained issue would.
#[test]
fn tick_dispatches_a_heavy_issue_on_a_remote_worker_host() {
    let mut source = OneShotSource::of(vec![heavy(9)]);
    let mut disp = PolicyDispatcher {
        policy: policy(host_class::HostClass::RemoteWorker, false),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_host_class, 0);
    assert_eq!(disp.dispatched, vec![9]);
}

/// An unclassified host (the default for every existing install) dispatches
/// a heavy issue too — zero behavior change until an operator opts in.
#[test]
fn tick_dispatches_a_heavy_issue_on_an_unclassified_host() {
    let mut source = OneShotSource::of(vec![heavy(9)]);
    let mut disp = PolicyDispatcher::default();
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_host_class, 0);
}

/// A non-heavy issue dispatches on a `local-dev` host — only `loom:heavy`
/// candidates are gated.
#[test]
fn tick_dispatches_a_non_heavy_issue_on_a_local_dev_host() {
    let mut source = OneShotSource::of(vec![plain(9)]);
    let mut disp = PolicyDispatcher {
        policy: policy(host_class::HostClass::LocalDev, false),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_host_class, 0);
}

/// The `allowHeavyLocal` override suppresses the gate entirely.
#[test]
fn tick_dispatches_a_heavy_issue_on_local_dev_with_the_override_set() {
    let mut source = OneShotSource::of(vec![heavy(9)]);
    let mut disp = PolicyDispatcher {
        policy: policy(host_class::HostClass::LocalDev, true),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_host_class, 0, "allowHeavyLocal suppresses the gate");
}

// ===================================================================
// Multi-workspace `tick_multi`
// ===================================================================

/// The same AC1-shaped assertion at the multi-workspace entry point: a
/// `local-dev` workspace never dispatches its `loom:heavy` candidate.
#[test]
fn tick_multi_never_dispatches_a_heavy_issue_on_a_local_dev_host() {
    let mut multi = vec![(
        OneShotSource::of(vec![heavy(9)]),
        PolicyDispatcher {
            policy: policy(host_class::HostClass::LocalDev, false),
            ..Default::default()
        },
    )];
    let report = tick_multi(&mut multi, &[], 10, &[false]);

    assert_eq!(report.dispatched, 0);
    assert_eq!(report.skipped_host_class, 1);
    assert!(multi[0].1.dispatched.is_empty(), "dispatch() must never be called");
}

/// Mirrors the #7972/#3939 cross-workspace property: a `local-dev` workspace
/// held on its own heavy candidate does not starve a `remote-worker`
/// sibling's identical candidate.
#[test]
fn tick_multi_a_local_dev_workspace_does_not_starve_a_remote_worker_sibling() {
    let mut multi = vec![
        (
            OneShotSource::of(vec![heavy(1)]),
            PolicyDispatcher {
                policy: policy(host_class::HostClass::LocalDev, false),
                ..Default::default()
            },
        ),
        (
            OneShotSource::of(vec![heavy(10)]),
            PolicyDispatcher {
                policy: policy(host_class::HostClass::RemoteWorker, false),
                ..Default::default()
            },
        ),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);

    assert_eq!(report.skipped_host_class, 1, "workspace A's #1 is refused");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!(multi[1].1.dispatched, vec![10], "the remote-worker sibling still dispatches");
}
