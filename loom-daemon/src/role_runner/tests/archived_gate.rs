//! The archived-repository gate (#10562): every role the runner dispatches
//! writes to the forge, so an archived workspace gets no tick on either
//! dispatch surface (interval cadence and idle edge). The answer is seeded,
//! never read, under `cfg(test)`.

use super::*;

#[test]
#[serial]
fn an_archived_workspace_gets_no_interval_tick_and_no_idle_edge() {
    let _env = ShardEnvGuard::capture();
    let workspace = enabled_workspace();
    let root = workspace.path();
    assert!(tick_admitted(root), "precondition: a live workspace is admitted");

    roster::set_archived_for_tests(root, None, true);
    assert!(!tick_admitted(root), "an archived workspace is never dispatched");

    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let now = Instant::now();
    for busy in [true, false, true] {
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, busy, false, now).is_empty());
    }
    assert_eq!(active_run_count(&set), 0);

    // Un-archived: dispatch resumes.
    roster::set_archived_for_tests(root, None, false);
    assert!(tick_admitted(root));
}

#[test]
fn an_unseeded_root_is_not_archived_and_reads_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!roster::repo_is_archived(tmp.path(), None));
    assert!(!roster::repo_is_archived(tmp.path(), Some("acme/roster")));
    roster::set_archived_for_tests(tmp.path(), Some("acme/roster"), true);
    assert!(roster::repo_is_archived(tmp.path(), Some("acme/roster")));
    assert!(!roster::repo_is_archived(tmp.path(), None), "keyed per repo");
}
