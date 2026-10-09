//! Every dispatch route is gated by disk admission (#11191): the
//! review-stall watchdog's re-dispatch (#3910), the PR-set recovery it
//! converts to (#7649) and the reaper's crash resume (#4256) all reach
//! `begin_prepared_issue_dispatch`, whose disk check refuses them before any
//! claim or spawn when the repo's charge does not fit.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tempfile::tempdir;

use crate::disk_admission::{DiskAdmissionRefused, TEST_SEAM};
use crate::disk_footprint::Store;
use crate::sweep_registry::test_support::*;
use crate::types::SweepKind;

/// Install a seam probe: `free_gb` free, 3 GB floor, an empty store (so the
/// charge is the global default, 8 GB unless overridden).
fn install(free_gb: u64) {
    TEST_SEAM.with(|c| *c.borrow_mut() = Some((free_gb, 3, Store::default())));
}

fn clear() {
    TEST_SEAM.with(|c| *c.borrow_mut() = None);
}

fn is_disk_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DiskAdmissionRefused>().is_some()
}

#[test]
fn the_reaper_resume_path_is_disk_gated() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    install(2);
    let err = registry.dispatch_resume_after_crash(35, 777).unwrap_err();
    clear();
    assert!(is_disk_refusal(&err), "{err:#}");
    assert!(
        !dir.path().join(".loom/locks/issue-35").exists(),
        "refused before the claim lock"
    );
}

#[test]
fn a_pr_set_dispatch_is_disk_gated() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());
    install(2);
    let err = registry
        .dispatch(&SweepKind::PrSet(vec![9200]), None, None, None, None)
        .unwrap_err();
    clear();
    assert!(is_disk_refusal(&err), "{err:#}");
    assert!(!record_log.exists(), "nothing was spawned");
}

#[test]
fn an_admitted_dispatch_records_its_pending_reservation() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    install(1_000);
    let ok = registry.dispatch_resume_after_crash(36, 778);
    let store = TEST_SEAM.with(|c| c.borrow().as_ref().map(|(_, _, s)| s.clone()));
    clear();
    assert!(ok.is_ok(), "{:#}", ok.unwrap_err());
    let store = store.unwrap();
    let unit = store
        .inflight
        .values()
        .find(|u| u.issue == Some(36))
        .expect("the admitted sweep is reserved for until a sample sees it");
    assert!(unit.pending_since.is_some());
}

#[test]
fn the_review_stall_watchdog_redispatch_is_disk_gated() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);
    // The original dispatch is admitted (no probe installed).
    let out = reg
        .dispatch(&SweepKind::Issue(5353), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS), "fixture child should start");
    std::fs::create_dir_all(ws.join(".loom/worktrees/issue-5353")).unwrap();

    // The disk is now short: the stalled sweep is cancelled, and its
    // re-dispatch is refused by the same rule the work finder applies.
    install(2);
    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    clear();
    assert_eq!(restarts, 0, "the re-dispatch was refused for disk");
    assert!(reg.review_stall_retried.contains(&5353), "the single attempt was spent");
    assert!(running_issue_sweep_id(&reg, 5353).is_none(), "no fresh sweep was started");
}
