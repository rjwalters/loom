//! The workspace hold on the idle edge (#10719).
//!
//! A hold stops new sweeps, so the held workspace's in-flight set drains and
//! it goes idle: the hold itself makes the non-idle to idle edge. Without a
//! hold check on this path every `onIdle` role would then start in the very
//! checkout the hold exists to keep work out of.

use super::*;
use crate::workspace_hold::{set_for_test, HeldCopy, HoldKind, WorkspaceHold};

fn held(kind: HoldKind) -> WorkspaceHold {
    WorkspaceHold {
        kind,
        copy: HeldCopy::Checkout,
        since: chrono::Utc::now(),
        detail: "requires daemon 0.19.950 > running 0.19.900".into(),
        verdict_at: chrono::Utc::now(),
    }
}

fn names(plan: &[(RoleSpec, RoleRunGuard)]) -> Vec<&'static str> {
    plan.iter().map(|(spec, _)| spec.name).collect()
}

/// One host, two workspaces, one idle trigger (as the work finder has): both
/// go from busy to idle on the same tick. The held one starts no `onIdle`
/// role; the unheld one still does.
#[test]
#[serial]
fn a_held_workspace_going_idle_starts_no_on_idle_role_and_its_sibling_still_does() {
    let _env = ShardEnvGuard::capture();
    let held_ws = enabled_workspace();
    let free_ws = enabled_workspace();
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let set = new_in_progress_guard();
    let mut trigger = IdleTrigger::new();
    let now = Instant::now();

    for kind in [HoldKind::DaemonTooOld, HoldKind::InstallIncompatible] {
        set_for_test(held_ws.path(), Some(held(kind)));
        for root in [held_ws.path(), free_ws.path()] {
            assert!(plan_idle_runs(&mut trigger, &set, root, &cfg, false, false, now).is_empty());
        }
        let from_held = plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, true, false, now);
        assert!(from_held.is_empty(), "{kind:?}: a held workspace starts no onIdle role");
        assert_eq!(active_run_count(&set), 0, "{kind:?}: and takes no run guard");

        // A fresh trigger for the sibling each round keeps it out of the
        // debounce window; the hold is what is under test, not the debounce.
        let mut sibling = IdleTrigger::new();
        assert!(
            plan_idle_runs(&mut sibling, &set, free_ws.path(), &cfg, false, false, now).is_empty()
        );
        let from_free = plan_idle_runs(&mut sibling, &set, free_ws.path(), &cfg, true, false, now);
        assert_eq!(names(&from_free), ["champion"], "{kind:?}: the unheld sibling still fires");
        drop(from_free);
    }

    // The edge the hold swallowed is spent: staying idle fires nothing.
    assert!(plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, true, false, now).is_empty());

    // The hold clears. The next idle edge fires as it always did.
    set_for_test(held_ws.path(), None);
    assert!(plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, false, false, now).is_empty());
    let after = plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, true, false, now);
    assert_eq!(names(&after), ["champion"], "dispatch resumes once the hold clears");
}

/// #11186: a maintain-only workspace is refused the same way, through the
/// real idle-edge planner; its sibling still fires, and releasing it lets
/// the next idle edge fire.
#[test]
#[serial]
fn a_maintain_only_workspace_going_idle_starts_no_on_idle_role() {
    use crate::workspace_hold::set_maintain_only_for_test;
    use crate::workspace_registry::{MaintainOnly, MaintainOnlySource};
    let _env = ShardEnvGuard::capture();
    let held_ws = enabled_workspace();
    let free_ws = enabled_workspace();
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let set = new_in_progress_guard();
    let now = Instant::now();
    let mark = MaintainOnly {
        by: MaintainOnlySource::FleetStore,
        since: chrono::Utc::now(),
    };
    set_maintain_only_for_test(held_ws.path(), Some(mark));

    let mut trigger = IdleTrigger::new();
    assert!(plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, false, false, now).is_empty());
    let from_held = plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, true, false, now);
    assert!(from_held.is_empty(), "a maintain-only workspace starts no onIdle role");
    assert_eq!(active_run_count(&set), 0);

    let mut sibling = IdleTrigger::new();
    assert!(plan_idle_runs(&mut sibling, &set, free_ws.path(), &cfg, false, false, now).is_empty());
    let from_free = plan_idle_runs(&mut sibling, &set, free_ws.path(), &cfg, true, false, now);
    assert_eq!(names(&from_free), ["champion"], "the normal sibling still fires");
    drop(from_free);

    set_maintain_only_for_test(held_ws.path(), None);
    let mut trigger = IdleTrigger::new();
    assert!(plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, false, false, now).is_empty());
    let after = plan_idle_runs(&mut trigger, &set, held_ws.path(), &cfg, true, false, now);
    assert_eq!(names(&after), ["champion"], "released: dispatch resumes");
}
