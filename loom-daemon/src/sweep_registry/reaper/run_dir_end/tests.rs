//! #11031 at the registry seam: a sweep's run dir goes when the sweep ends,
//! and a watchdog re-dispatch does not leave the abandoned run's dir behind.
//!
//! The fixture child stands in for `loom-daemon spawn-worker`: it writes a run
//! dir under `.loom/targets` whose owner marker is its own pid (`$$`), which is
//! what `provision` records before `exec()` keeps that pid for the harness.
use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

/// Writes `<cwd>/.loom/targets/sweep-lifecycle-$$-1` with a marker naming `$$`,
/// then runs `tail`. The OAuth line matches `hung_child_registry`'s child.
fn script(tail: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         echo \"spawn-claude: using OAuth account 'faketok' (mode=random)\"\n\
         d=\"$(pwd)/.loom/targets/sweep-lifecycle-$$-1\"\n\
         mkdir -p \"$d/debug\" && echo x > \"$d/debug/lib.rlib\" && echo $$ > \"$d/.loom-run-owner\"\n\
         {tail}\n"
    )
}

fn run_dir(ws: &Path, pid: u32) -> PathBuf {
    crate::run_target_dir::targets_root(ws).join(format!("sweep-lifecycle-{pid}-1"))
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_millis(FIXTURE_CHILD_WAIT_MS);
    while std::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn marker_written(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(crate::run_target_dir::OWNER_FILE))
        .is_ok_and(|s| s.ends_with('\n'))
}

/// A sweep that ends on its own (completed or failed: the reaper's death path
/// is the same for both) has its run dir removed. A run dir of another,
/// still-running owner next to it is untouched.
#[test]
#[serial]
fn the_end_of_a_sweep_removes_its_run_dir() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = lifecycle_registry(ws, &script("exit 0"));
    // A live run's dir: owned by this test process.
    let live = crate::run_target_dir::planned_for(ws, "builder", "live");
    crate::run_target_dir::provision(&live, std::process::id()).unwrap();

    let out = reg
        .dispatch(&SweepKind::Issue(11_031), None, None, None, None)
        .unwrap();
    let dir = run_dir(ws, out.pid);
    wait_for("the fixture's run dir", || marker_written(&dir));
    wait_for("the reaper to see the sweep end", || {
        reg.reap_once();
        reg.get_status(&out.sweep_id)
            .is_some_and(|info| info.state.is_terminal())
    });
    wait_for("the sweep-end removal", || !dir.exists());
    assert!(
        live.join(crate::run_target_dir::OWNER_FILE).exists(),
        "a live owner's dir stays"
    );
}

/// The watchdog's auto-cancel ends the hung sweep before its re-dispatch, and
/// the abandoned run's dir goes with it: the re-dispatched sweep's dir is the
/// only one left.
#[test]
#[serial]
fn a_watchdog_re_dispatch_does_not_leave_the_abandoned_run_dir_behind() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = lifecycle_registry(ws, &script("exec sleep 30"));

    let first = reg
        .dispatch(&SweepKind::Issue(11_032), None, None, None, None)
        .unwrap();
    let abandoned = run_dir(ws, first.pid);
    wait_for("the hung sweep's run dir", || marker_written(&abandoned));

    backdate(&mut reg, &first.sweep_id, 600);
    assert_eq!(reg.watchdog_once(Duration::from_secs(60)), 1, "the hung sweep is re-dispatched");
    let second_id = running_issue_sweep_id(&reg, 11_032).expect("a fresh sweep");
    let second_pid = reg.get_status(&second_id).unwrap().pid;
    assert_ne!(second_pid, first.pid);
    let fresh = run_dir(ws, second_pid);

    wait_for("the abandoned run dir's removal", || !abandoned.exists());
    wait_for("the re-dispatched sweep's run dir", || marker_written(&fresh));
    let left: Vec<_> = std::fs::read_dir(crate::run_target_dir::targets_root(ws))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(left, vec![fresh.clone()], "only the fresh run's dir remains");

    let _ = reg.cancel(&second_id, Duration::from_secs(2));
    wait_for("the cancelled sweep's run dir removal", || !fresh.exists());
}
