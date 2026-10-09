//! A work-finder tick over a maintain-only and a normal workspace (#11186).

use super::*;
use crate::workspace_hold::set_maintain_only_for_test;
use crate::workspace_registry::{MaintainOnly, MaintainOnlySource};

/// The production fold, end to end: the per-root pre-filter reads the
/// maintain-only mark, `causes_per_root` folds it into `halted` with cause
/// `maintain_only`, and the tick dispatches only into the normal workspace.
#[tokio::test]
async fn a_tick_dispatches_only_into_the_normal_workspace() {
    let maintained = tempfile::tempdir().unwrap();
    let normal = tempfile::tempdir().unwrap();
    let roots = vec![maintained.path().to_path_buf(), normal.path().to_path_buf()];
    let pool = crate::workspace_pool::WorkspacePool::new(
        std::sync::Arc::new(crate::event_bus::EventBus::new()),
        tokio::runtime::Handle::current(),
    );
    let mark = MaintainOnly {
        by: MaintainOnlySource::FleetStore,
        since: chrono::Utc::now(),
    };
    set_maintain_only_for_test(maintained.path(), Some(mark));

    let (_, preflight_causes, _) =
        pool_preflight::preflight_held_causes_per_root(&pool, &roots, chrono::Utc::now());
    let causes = halt_cause::causes_per_root(
        &WorkspaceHealthStates::default(),
        &roots,
        false,
        &preflight_causes,
        false,
        false,
    );
    assert_eq!(causes[0], Some(halt_cause::HaltCause::MaintainOnly));
    assert_ne!(causes[1], Some(halt_cause::HaltCause::MaintainOnly));
    // The sibling's own holds (write scope on a temp dir with no remote, for
    // one) are not under test: route it as the production loop would, minus
    // those, so the maintain-only fold is what decides.
    let halted = [causes[0].is_some(), false];

    let mut workspaces = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut workspaces, &[0, 0], 10, &halted);
    assert_eq!(report.dispatched, 1);
    assert!(
        workspaces[0].1.dispatched.is_empty(),
        "nothing into the maintain-only workspace"
    );
    assert_eq!(workspaces[1].1.dispatched, vec![2]);

    set_maintain_only_for_test(maintained.path(), None);
}
