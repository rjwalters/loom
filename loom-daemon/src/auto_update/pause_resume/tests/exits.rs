//! Every way out of H5 hands dispatch and restart recovery back, and no
//! paused claim outlives its lease waiting on a hold (#10832, review of
//! PR #10997).

use super::fake_host::FakeHost;
use super::*;
use crate::auto_update::pause_resume::test_support::{real_registry, RegistryHost};

/// A plan for a manifest startup armed for as `armed`, with dispatch held
/// for it, as startup leaves things.
fn armed_plan(dir: &Path, armed: &str, host: &FakeHost) -> ResumePlan {
    assert!(host.drain.hold_for_roll_resume("startup".to_string()));
    ResumePlan {
        armed: Some(armed.to_string()),
        ..plan(dir)
    }
}

/// Risk 2: startup armed for a manifest that is gone by the time H5 loads it
/// (removed in between). H5 has nothing to resume, and still lifts the
/// recovery suppression and the dispatch hold it was started for.
#[test]
fn an_armed_manifest_gone_at_load_still_releases_the_hold_and_the_suppression() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::default());
    let gone = armed_plan(dir.path(), "rp-gone", &host);

    assert_eq!(run_h5(host.clone(), &gone), H5Outcome::NoManifest);

    assert_eq!(host.calls(), vec!["finish rp-gone"]);
    assert!(!host.drain.is_draining(), "dispatch is not held forever");

    // The production finish disarms the real suppression too.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let m = manifest("rp-gone-real", Phase::Paused, vec![sweep(&root, "s-gone", 41)]);
    super::super::test_support::arm_for(&root, &m);
    assert!(roll_pause::suppress::is_armed_for("rp-gone-real"));
    let host = RegistryHost::new(real_registry(&root, None, "sleep 60"));
    assert!(host.drain.hold_for_roll_resume("startup".to_string()));
    let plan = ResumePlan {
        armed: Some("rp-gone-real".to_string()),
        ..plan(&root)
    };
    assert_eq!(run_h5(host.clone(), &plan), H5Outcome::NoManifest);
    assert!(!roll_pause::suppress::is_armed_for("rp-gone-real"));
    assert!(roll_pause::suppress::held_issue(&root, 41).is_none());
    assert!(!roll_pause::hold::is_held("s-gone"), "the sweep reaper may act on it again");
    assert!(!host.drain.is_draining());
}

/// Risk 2: startup armed for a manifest that is corrupt, or of a newer
/// schema, by the time H5 loads it. It is reported and left in place, and the
/// hold and the suppression are still lifted.
#[test]
fn an_armed_manifest_unreadable_at_load_still_releases_the_hold() {
    for (body, load) in [
        ("{ not json".to_string(), "corrupt"),
        (
            r#"{"schema_version": 2, "items": "a new shape"}"#.to_string(),
            "unknown-version",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeHost::default());
        let plan = armed_plan(dir.path(), "rp-damaged", &host);
        std::fs::create_dir_all(plan.manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&plan.manifest_path, &body).unwrap();

        let H5Outcome::Unreadable(status) = run_h5(host.clone(), &plan) else {
            panic!("{load}: expected Unreadable");
        };

        assert_eq!(status.load, load);
        assert_eq!(host.calls(), vec!["finish rp-damaged"], "{load}");
        assert!(!host.drain.is_draining(), "{load}: dispatch is released");
        assert_eq!(std::fs::read_to_string(&plan.manifest_path).unwrap(), body);
    }
}

/// Risk 2: H5 finishes even when it does not return at all.
#[test]
fn a_panic_inside_h5_still_releases_the_hold() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost {
        panic_in_health: true,
        ..FakeHost::default()
    });
    let plan = armed_plan(dir.path(), "rp-panic", &host);
    write(&plan, &manifest("rp-panic", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));

    let host2 = Arc::clone(&host);
    let plan2 = plan.clone();
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || run_h5(host2, &plan2)));

    assert!(result.is_err(), "the host bug propagates");
    assert_eq!(host.called("finish "), vec!["finish rp-panic"], "finished once");
    assert!(!host.drain.is_draining());
}

/// Risk 3: a real drain replaces the hold mid-resume and the operator then
/// aborts it. H5 does not give up: it re-holds dispatch and resumes the rest.
#[test]
fn an_aborted_drain_that_replaced_the_hold_lets_h5_carry_on() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(
        &plan,
        &manifest(
            "rp-abort",
            Phase::Paused,
            vec![sweep(dir.path(), "a", 1), sweep(dir.path(), "b", 2)],
        ),
    );
    let host = Arc::new(FakeHost {
        lose_hold_after: Mutex::new(Some(1)),
        abort_drain_after: Mutex::new(Some(3)),
        ..FakeHost::default()
    });

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.resumed, 2, "both items are resumed");
    assert_eq!(host.called("launch ").len(), 2);
    assert_eq!(host.called("abort-drain").len(), 1);
    assert_eq!(host.called("finish "), vec!["finish rp-abort"]);
    assert!(!host.drain.is_draining(), "dispatch is released at the end");
    let done = archived(&plan, "rp-abort");
    assert_eq!(done.phase, Phase::Resumed);
    for event in ["hold_blocked", "hold_regained"] {
        assert!(done.events.iter().any(|e| e.event == event), "{event}");
    }
}

/// Risk 3: a real drain replaces the hold mid-resume and never lifts. The
/// item already relaunched is confirmed; once the manifest goes stale, the one
/// not yet relaunched is requeued on the forge. The manifest is archived and
/// the suppression lifted, so no paused claim waits for the next restart. The
/// operator's drain is left alone.
#[test]
fn a_drain_that_replaces_the_hold_for_good_requeues_what_was_not_relaunched() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut m = manifest(
        "rp-int",
        Phase::Paused,
        vec![sweep(dir.path(), "a", 1), sweep(dir.path(), "b", 2)],
    );
    // Fresh now, stale in about two seconds.
    m.roll.max_age_secs = 2;
    m.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(1);
    write(&plan, &m);
    let host = Arc::new(FakeHost {
        lose_hold_after: Mutex::new(Some(1)),
        ..FakeHost::default()
    });

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(host.called("launch ").len(), 1, "nothing is relaunched under the drain");
    assert_eq!((status.resumed, status.requeued_by_reason[REASON_STALE]), (1, 1));
    assert!(host
        .calls()
        .contains(&format!("requeue b {REASON_STALE} forge=true")));
    assert_eq!(host.called("finish "), vec!["finish rp-int"]);
    assert!(host.drain.is_draining(), "the operator drain still pauses dispatch");
    let done = archived(&plan, "rp-int");
    assert_eq!(item(&done, "a").status, ItemStatus::Resumed);
    assert_eq!(item(&done, "b").status, ItemStatus::Requeued);
    assert!(!plan.manifest_path.exists(), "nothing is left for the next start");
}

/// Risk 5: the manifest's max age passes during step 6. Nothing more is
/// relaunched; the items not yet reached are requeued as stale.
#[test]
fn a_manifest_that_goes_stale_mid_resume_requeues_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut m = manifest(
        "rp-late",
        Phase::Paused,
        vec![
            sweep(dir.path(), "a", 1),
            sweep(dir.path(), "b", 2),
            sweep(dir.path(), "c", 3),
        ],
    );
    // Stale about three seconds from now; the first relaunch takes longer.
    m.roll.max_age_secs = 3;
    m.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(1);
    write(&plan, &m);
    let host = Arc::new(FakeHost {
        launch_delay: Duration::from_millis(3500),
        ..FakeHost::default()
    });

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(host.called("launch ").len(), 1);
    assert_eq!((status.resumed, status.requeued_by_reason[REASON_STALE]), (1, 2));
    let done = archived(&plan, "rp-late");
    assert_eq!(item(&done, "a").status, ItemStatus::Resumed);
    for id in ["b", "c"] {
        assert_eq!(item(&done, id).reason.as_deref(), Some(REASON_STALE), "{id}");
    }
    assert!(!host.drain.is_draining());
}

/// Risk 4: the role half of a requeue re-checks that the claim is still the
/// paused run's before it removes the label, as the sweep half does.
#[test]
#[serial_test::serial]
fn a_role_requeue_leaves_a_claim_someone_took_since_alone() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (log, labeled_at) = (root.join("gh.log"), root.join("labeled-at"));
    let inner = root.join("inner-gh.sh");
    std::fs::write(
        &inner,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in *timeline*) cat '{}';; \
             esac\nexit 0\n",
            log.display(),
            labeled_at.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ws = crate::write_scope_test_support::WritableRoot::register_with_gh(&root, &inner);
    let mut it = role_run(&root, "r-judge", "judge");
    it.claim = Some(serde_json::json!({ "label": "loom:reviewing", "on": "pr", "number": 588 }));
    let stopped = it.stopped_at.unwrap();
    let notice = crate::sweep_registry::roll_requeue::RollRequeueNotice {
        reason: "role-disabled".to_string(),
        ..Default::default()
    };
    let edits = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("pr edit 588") || l.starts_with("pr comment 588"))
            .count()
    };

    // Another run claimed the PR again after this one was stopped.
    let later = stopped + chrono::Duration::seconds(1800);
    std::fs::write(&labeled_at, format!("\"{}\"\n", later.to_rfc3339())).unwrap();
    host::requeue_role(&it, &notice, true, &ws.gh).unwrap();
    assert!(std::fs::read_to_string(&log).unwrap().contains("timeline"), "it looked");
    assert_eq!(edits(), 0, "nothing is written for a claim that is not ours");

    // The label is the one this run applied before it was stopped.
    let earlier = stopped - chrono::Duration::seconds(1800);
    std::fs::write(&labeled_at, format!("\"{}\"\n", earlier.to_rfc3339())).unwrap();
    host::requeue_role(&it, &notice, true, &ws.gh).unwrap();
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("pr edit 588 --remove-label loom:reviewing"), "{calls}");
    assert_eq!(edits(), 2, "the label is released and the reason posted");
}

/// A resumed role run H5 has no handle for is not counted as resumed.
#[test]
fn an_untracked_role_resume_is_not_counted_as_resumed() {
    assert!(matches!(host::role_liveness(None), Liveness::Died { .. }));
}
