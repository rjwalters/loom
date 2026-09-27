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

/// Mirrors the #7972/#3939 cross-workspace property: on one `local-dev`
/// host (the class is host-wide, resolved once at startup), a workspace held
/// on its own heavy candidate does not starve a sibling whose per-workspace
/// `allowHeavyLocal` override is set.
#[test]
fn tick_multi_a_refused_workspace_does_not_starve_an_overridden_sibling() {
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
                policy: policy(host_class::HostClass::LocalDev, true),
                ..Default::default()
            },
        ),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);

    assert_eq!(report.skipped_host_class, 1, "workspace A's #1 is refused");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!(multi[1].1.dispatched, vec![10], "the overridden sibling still dispatches");
}

// ===================================================================
// Startup capture (Judge fix on PR #9214)
// ===================================================================

fn write_host_class(root: &std::path::Path, class: &str) {
    let loom = root.join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    let body = serde_json::json!({"autonomous": {"workFinder": {"hostClass": class}}});
    std::fs::write(loom.join("config.json"), body.to_string()).unwrap();
}

/// The class is resolved ONCE at work-finder startup and threaded into the
/// dispatchers the multi-workspace loop rebuilds EVERY tick — so a config
/// edit mid-run does not reclassify the host until a restart.
#[tokio::test]
#[serial_test::serial]
async fn host_class_is_pinned_at_startup_across_a_mid_run_config_change() {
    std::env::remove_var(host_class::HOST_CLASS_ENV);
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write_host_class(&root, "local-dev");

    // Daemon start: resolve once.
    let startup = host_class::resolve_at_startup(&root);
    assert_eq!(startup, host_class::HostClass::LocalDev);

    // Mid-run: the operator edits the config.
    write_host_class(&root, "remote-worker");
    assert_eq!(
        host_class::resolve_at_startup(&root),
        host_class::HostClass::RemoteWorker,
        "precondition: the config on disk really did change"
    );

    // The next tick's per-root rebuild still carries the startup class.
    let pool = WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current());
    let pairs = forge::dispatcher_pairs(&pool, std::slice::from_ref(&root), startup);
    assert_eq!(pairs.len(), 1);
    assert_eq!(
        pairs[0].1.heavy_local_policy().class,
        host_class::HostClass::LocalDev,
        "a mid-run config change must not reclassify the host"
    );
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}
