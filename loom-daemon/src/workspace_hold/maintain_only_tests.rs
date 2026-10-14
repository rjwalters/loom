//! Tests for the `maintain-only` hold (#11186).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serial_test::serial;

use super::*;
use crate::types::SweepKind;
use crate::work_finder::halt_cause::HaltCause;
use crate::workspace_hold::{
    filter_held, guard, hold_for, refuse_role_start, resync_hold_for, set_for_test, Finding,
    HeldCopy, HoldKind, Holds, Observation, WorkspaceHold, ALERT_AFTER,
};
use crate::workspace_registry::{MaintainOnlySource, WorkspaceRegistry};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}

pub(crate) fn mark(by: MaintainOnlySource) -> MaintainOnly {
    MaintainOnly { by, since: t0() }
}

fn w3() -> WorkspaceHold {
    WorkspaceHold {
        kind: HoldKind::InstallIncompatible,
        copy: HeldCopy::DefaultBranch,
        since: t0(),
        detail: "installed 0.19.800 is too old".into(),
        verdict_at: t0(),
    }
}

fn registry_with(path: &Path, roots: &[(&Path, Option<MaintainOnlySource>)]) {
    let mut reg = WorkspaceRegistry::default();
    for (root, by) in roots {
        reg.add(root, None).unwrap();
        reg.set_maintain_only(root, *by, t0());
    }
    reg.save(path).unwrap();
}

/// The registry file is the source: a mark appears, and goes, with the file.
#[test]
fn the_cache_follows_the_registry_file() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let path = dir.path().join("workspaces.json");
    let mut cache = Cache::default();

    assert_eq!(cache.lookup(&path, &a), None, "no registry: nothing held");

    registry_with(&path, &[(&a, Some(MaintainOnlySource::FleetStore)), (&b, None)]);
    let got = cache.lookup(&path, &a).unwrap();
    assert_eq!(got.by, MaintainOnlySource::FleetStore);
    assert_eq!(cache.lookup(&path, &b), None);
    // A non-canonical spelling of the root finds it too.
    assert!(cache.lookup(&path, &a.join("..").join("a")).is_some());

    registry_with(&path, &[(&a, None), (&b, Some(MaintainOnlySource::Operator))]);
    assert_eq!(cache.lookup(&path, &a), None, "released");
    assert_eq!(cache.lookup(&path, &b).unwrap().by, MaintainOnlySource::Operator);

    // An unreadable registry never lifts a hold: the last marks stand.
    std::fs::write(&path, "{ not json").unwrap();
    assert!(cache.lookup(&path, &b).is_some());

    std::fs::remove_file(&path).unwrap();
    assert_eq!(cache.lookup(&path, &b), None, "registry gone: nothing registered");
}

#[test]
fn hold_for_reports_maintain_only_and_resync_hold_for_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    set_maintain_only_for_test(root, Some(mark(MaintainOnlySource::FleetStore)));
    let hold = hold_for(root).expect("held");
    assert_eq!((hold.kind, hold.copy), (HoldKind::MaintainOnly, HeldCopy::Registry));
    assert_eq!(hold.kind.as_str(), "maintain-only");
    assert_eq!(hold.describe(), "maintain-only");
    assert!(hold.detail.contains("fleet store"), "{}", hold.detail);
    assert_eq!(resync_hold_for(root), None, "status shows only the pass's own holds there");

    let err = guard(root).expect_err("refused");
    assert_eq!(err.kind, HoldKind::MaintainOnly);
    let msg = err.to_string();
    assert!(msg.contains("maintain-only") && msg.contains("workspace release"), "{msg}");
    assert!(msg.contains("`force` does not override"), "{msg}");

    set_maintain_only_for_test(root, None);
    assert_eq!(hold_for(root), None);
    assert!(guard(root).is_ok());
}

/// Maintain-only and a W3/W4 hold coexist: each is set and lifted on its
/// own, and dispatch is refused while either stands.
#[test]
fn maintain_only_coexists_with_a_resync_hold() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    set_for_test(root, Some(w3()));
    set_maintain_only_for_test(root, Some(mark(MaintainOnlySource::Operator)));
    assert_eq!(hold_for(root).unwrap().kind, HoldKind::MaintainOnly, "the permanent one first");
    assert_eq!(resync_hold_for(root).unwrap().kind, HoldKind::InstallIncompatible);

    set_maintain_only_for_test(root, None);
    assert_eq!(hold_for(root).unwrap().kind, HoldKind::InstallIncompatible, "W3 still stands");

    set_maintain_only_for_test(root, Some(mark(MaintainOnlySource::Operator)));
    set_for_test(root, None);
    assert_eq!(hold_for(root).unwrap().kind, HoldKind::MaintainOnly, "W3 cleared, still held");
    set_maintain_only_for_test(root, None);
    assert_eq!(hold_for(root), None);
}

/// A maintain-only workspace whose files are fine is never a pass hold: no
/// roll demand, no `set`, and no 30-minute "stuck" alert, however long it
/// stands.
#[test]
fn maintain_only_raises_no_roll_demand_and_no_standing_alert() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    set_maintain_only_for_test(root, Some(mark(MaintainOnlySource::FleetStore)));
    let seen = [Observation {
        root: root.to_path_buf(),
        repo: Some("acme/admin".into()),
        default_branch: Finding::clear(),
        checkout: Finding::clear(),
    }];
    let mut holds = Holds::default();
    let running = crate::install_compat::Version::parse("0.19.980").unwrap();
    for i in 0..6 {
        let now = t0() + chrono::Duration::from_std(ALERT_AFTER).unwrap() * i;
        let transitions = holds.step(&seen, running, now, ALERT_AFTER);
        assert!(transitions.is_empty(), "pass {i}: {transitions:?}");
    }
    assert!(holds.holds().is_empty());
    assert_eq!(holds.demand(), None);
    assert!(hold_for(root).is_some(), "and dispatch is still refused");
    set_maintain_only_for_test(root, None);
}

/// The registry guard refuses issue and PR-set dispatch into a maintain-only
/// workspace, spawning nothing; a normal sibling dispatches.
#[test]
#[serial]
fn the_sweep_registry_refuses_a_maintain_only_workspace_and_not_its_sibling() {
    let held_dir = tempfile::tempdir().unwrap();
    let free_dir = tempfile::tempdir().unwrap();
    let (mut held, held_log) =
        crate::sweep_registry::test_support::fixture_registry(held_dir.path());
    let (mut free, _) = crate::sweep_registry::test_support::fixture_registry(free_dir.path());
    set_maintain_only_for_test(held_dir.path(), Some(mark(MaintainOnlySource::FleetStore)));

    for kind in [SweepKind::Issue(11186), SweepKind::PrSet(vec![1, 2])] {
        let err = held
            .dispatch(&kind, None, None, None, None)
            .expect_err("held");
        let typed = err
            .downcast_ref::<crate::workspace_hold::WorkspaceHeldDispatchError>()
            .expect("the typed refusal");
        assert_eq!(typed.kind, HoldKind::MaintainOnly);
    }
    assert!(!held_log.exists(), "nothing was spawned");
    assert!(held.list(None).is_empty(), "no registry entry");
    assert!(free
        .dispatch(&SweepKind::Issue(11187), None, None, None, None)
        .is_ok());

    // Released: the same workspace dispatches again, never re-registered.
    set_maintain_only_for_test(held_dir.path(), None);
    let out = held.dispatch(&SweepKind::Issue(11188), None, None, None, None);
    assert!(out.is_ok(), "{out:?}");
}

#[test]
fn the_role_runner_tick_and_idle_edge_skip_a_maintain_only_root() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let roots = vec![a.path().to_path_buf(), b.path().to_path_buf()];
    let mut logged = HashSet::new();
    set_maintain_only_for_test(a.path(), Some(mark(MaintainOnlySource::Operator)));
    for _ in 0..3 {
        assert_eq!(filter_held(roots.clone(), &mut logged), vec![b.path().to_path_buf()]);
    }
    assert!(refuse_role_start(a.path(), "idle edge"));
    assert!(!refuse_role_start(b.path(), "idle edge"));
    set_maintain_only_for_test(a.path(), None);
    assert_eq!(filter_held(roots.clone(), &mut logged), roots);
}

#[test]
fn the_halt_cause_token_round_trips() {
    let cause = HoldKind::MaintainOnly.halt_cause();
    assert_eq!(cause, HaltCause::MaintainOnly);
    assert_eq!(cause.as_str(), "maintain_only");
    assert_eq!(HaltCause::from_wire("maintain_only"), Some(cause));
}

/// The work finder's per-root pre-filter holds the maintain-only root with
/// its own cause and no recovery probe; the sibling is not held by it.
#[tokio::test]
async fn the_work_finder_pre_filter_names_maintain_only() {
    use crate::work_finder::pool_preflight::preflight_held_causes_per_root;
    let held_dir = tempfile::tempdir().unwrap();
    let free_dir = tempfile::tempdir().unwrap();
    let roots = vec![held_dir.path().to_path_buf(), free_dir.path().to_path_buf()];
    let pool = crate::workspace_pool::WorkspacePool::new(
        std::sync::Arc::new(crate::event_bus::EventBus::new()),
        tokio::runtime::Handle::current(),
    );
    set_maintain_only_for_test(held_dir.path(), Some(mark(MaintainOnlySource::FleetStore)));
    let (held, causes, probes) = preflight_held_causes_per_root(&pool, &roots, t0());
    assert!(held[0]);
    assert_eq!(causes[0], Some(HaltCause::MaintainOnly));
    assert!(!probes.contains(&roots[0]));
    assert_ne!(causes[1], Some(HaltCause::MaintainOnly));
    set_maintain_only_for_test(held_dir.path(), None);
}
