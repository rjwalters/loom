//! Role-run resume tests (#10832): the real role launcher behind a fake spawn
//! script and a fake `gh`.

use super::*;
use crate::roll_pause::claim_breadcrumb::ClaimBreadcrumb;
use crate::sweep_registry::test_support::{wait_for_condition, wait_for_contents};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;

const SID: &str = "4910f978-64b9-4654-942a-dae6514e859c";
const OLD: &str = "role-judge-20261007T164012Z-aaaaaaaa";

fn write_config(root: &Path, body: &str) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), body).unwrap();
}

fn enabled(root: &Path, roles: &str) {
    write_config(
        root,
        &format!(r#"{{"autonomous":{{"roleRunner":{{"enabled":true,"roles":[{roles}]}}}}}}"#),
    );
}

/// A spawn script that records its argv and resume environment, then runs
/// until `release` exists.
fn spawn_script(root: &Path, record: &Path, release: &Path, exit: i32) -> PathBuf {
    let script = root.join("fake-role-spawn.sh");
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/env bash\n{{\n  printf 'argv: %s\\n' \"$*\"\n  for v in \
             LOOM_RESUME_SESSION_ID LOOM_RESUME_PROMPT LOOM_CLAUDE_SESSION_ID LOOM_DAEMON_ITEM_ID; \
             do\n    printf '%s=%s\\n' \"$v\" \"${{!v-unset}}\"\n  done\n  printf 'done\\n'\n}} >> \
             \"{}\"\nwhile [ ! -e \"{}\" ]; do sleep 0.05; done\nexit {exit}\n",
            record.display(),
            release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Ok(f) = std::fs::File::open(&script) {
        let _ = f.sync_all();
    }
    script
}

fn launch_of(role: &str) -> RollResumeLaunch {
    RollResumeLaunch {
        runtime: "claude".to_string(),
        session_id: SID.to_string(),
        prompt: "Loom: this session was paused for a daemon roll".to_string(),
        resume_of: OLD.replace("judge", role),
        resume_count: 1,
        agent_started_at: Some("2026-10-07T16:40:12+00:00".to_string()),
        ..RollResumeLaunch::default()
    }
}

fn spec(root: &Path, role: &str, script: &Path, gh: &Path) -> RoleResumeSpec {
    RoleResumeSpec {
        root: root.to_path_buf(),
        role: role.to_string(),
        launch: launch_of(role),
        timeout_remaining: Some(Duration::from_secs(120)),
        holds_issue_creation_mutex: false,
        spawn_bin: Some(script.to_path_buf()),
        gh_bin: Some(gh.to_path_buf()),
    }
}

/// AC: a paused role run is resumed from its saved session with the
/// in-progress guard seeded, the issue-creation mutex seeded when it held it,
/// its claim breadcrumb carried, and its first start kept.
#[test]
#[serial]
fn a_paused_role_run_is_resumed_with_its_guard_mutex_and_breadcrumb_seeded() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    enabled(&root, r#""champion","judge""#);
    let ws = crate::write_scope_test_support::WritableRoot::register(&root);
    let (record, release) = (root.join("record.log"), root.join("release"));
    let script = spawn_script(&root, &record, &release, 0);
    let mut spec = spec(&root, "champion", &script, &ws.gh);
    spec.holds_issue_creation_mutex = true;
    // The paused run had taken a claim.
    let old_dir = crate::roll_pause::item_dir(
        &crate::roll_pause::default_pause_root(&root),
        &spec.launch.resume_of,
    );
    let claim = ClaimBreadcrumb {
        label: "loom:reviewing".to_string(),
        on: "pr".to_string(),
        number: 599,
        at: None,
    };
    claim_breadcrumb::write(&old_dir, &claim).unwrap();
    let in_progress = new_in_progress_guard();
    let mutex = crate::issue_creation_mutex::IssueCreationMutex::for_root(&root);

    let mut handle = launch(&spec, &in_progress, Duration::from_secs(60)).expect("it resumes");

    assert_eq!(handle.state(), RoleResumeState::Running);
    assert!(
        handle.item_id.starts_with("role-champion-") && handle.item_id != spec.launch.resume_of
    );
    assert!(handle.pid.is_some());
    // No second run of the role may start beside it.
    assert!(in_progress
        .lock()
        .unwrap()
        .contains(&(root.clone(), "champion", 0)));
    let second = launch(&spec, &in_progress, Duration::from_secs(5)).unwrap_err();
    assert_eq!(second.reason, "guard-refused:role-in-progress", "{second}");
    // Nor an issue-creating burst interleave with the one it was in.
    assert!(mutex.is_held(), "the issue-creation mutex is seeded as held");
    assert!(mutex
        .try_acquire(crate::issue_creation_mutex::ARCHITECT_PROPOSAL)
        .is_none());
    // The claim it took travels with it.
    let new_dir =
        crate::roll_pause::item_dir(&crate::roll_pause::default_pause_root(&root), &handle.item_id);
    assert_eq!(claim_breadcrumb::read(&new_dir), Some(claim));
    // It is listed for the next roll with its lineage and first start.
    let run = crate::roll_pause::live_runs::snapshot()
        .into_iter()
        .find(|r| r.item_id == handle.item_id)
        .expect("the resumed run is a live run");
    assert_eq!(run.claude_session_id.as_deref(), Some(SID));
    assert_eq!(run.started_at.to_rfc3339(), "2026-10-07T16:40:12+00:00");
    assert_eq!(
        run.timeout,
        Duration::from_secs(120),
        "the time it had left, not a fresh budget"
    );
    let lineage = run.resume.expect("its lineage");
    assert_eq!(
        (lineage.resume_of.as_str(), lineage.resume_count),
        (spec.launch.resume_of.as_str(), 1)
    );

    assert!(wait_for_contents(&record, "done", 60_000));
    let recorded = std::fs::read_to_string(&record).unwrap();
    assert!(recorded.contains("argv: -p "), "print mode, no prompt: {recorded}");
    assert!(!recorded.contains("/loom:"), "a resume passes no role prompt: {recorded}");
    assert!(recorded.contains(&format!("LOOM_RESUME_SESSION_ID={SID}")), "{recorded}");
    assert!(recorded.contains("LOOM_CLAUDE_SESSION_ID=unset"), "{recorded}");
    assert!(
        recorded.contains(&format!("LOOM_DAEMON_ITEM_ID={}", handle.item_id)),
        "{recorded}"
    );

    // When the run ends, everything it held is released.
    std::fs::write(&release, "").unwrap();
    assert!(wait_for_condition(60_000, || handle.state() != RoleResumeState::Running));
    assert_eq!(handle.state(), RoleResumeState::Succeeded);
    assert!(wait_for_condition(20_000, || in_progress.lock().unwrap().is_empty()));
    assert!(wait_for_condition(20_000, || !mutex.is_held()));
}

/// A resumed run that exits non-zero is reported as failed, for the requeue.
#[test]
#[serial]
fn a_resumed_role_run_that_fails_reports_how() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    enabled(&root, r#""judge""#);
    let ws = crate::write_scope_test_support::WritableRoot::register(&root);
    let (record, release) = (root.join("record.log"), root.join("release"));
    std::fs::write(&release, "").unwrap(); // exits at once
    let script = spawn_script(&root, &record, &release, 3);
    let in_progress = new_in_progress_guard();
    match launch(&spec(&root, "judge", &script, &ws.gh), &in_progress, Duration::from_secs(60)) {
        // Either the poll saw the live run first, or it had already ended.
        Ok(mut handle) => {
            assert!(wait_for_condition(60_000, || handle.state() != RoleResumeState::Running));
            assert!(matches!(handle.state(), RoleResumeState::Failed(_)));
        }
        Err(refusal) => assert_eq!(refusal.reason, REASON_RESUME_FAILED, "{refusal}"),
    }
    assert!(wait_for_condition(20_000, || in_progress.lock().unwrap().is_empty()));
}

/// §9 `role-disabled`: the runner is off, or the role is no longer listed.
#[test]
#[serial]
fn a_role_that_is_no_longer_enabled_is_not_resumed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    if std::env::var(ROLE_RUNNER_ENABLE_ENV).is_ok() {
        return; // the host-wide switch would decide instead of the config
    }
    write_config(&root, r#"{"autonomous":{"roleRunner":{"enabled":false}}}"#);
    let off = enabled_role(&root, "judge").unwrap_err();
    assert_eq!(off.reason, REASON_ROLE_DISABLED, "{off}");
    enabled(&root, r#""curator""#);
    let unlisted = enabled_role(&root, "judge").unwrap_err();
    assert_eq!(unlisted.reason, REASON_ROLE_DISABLED, "{unlisted}");
    assert_eq!(enabled_role(&root, "curator").unwrap(), "curator");
    assert_eq!(enabled_role(&root, "Curator").unwrap(), "curator", "the static name");

    // A disabled role is refused before anything is seeded or spawned.
    let in_progress = new_in_progress_guard();
    let script = root.join("never-run.sh");
    let refusal =
        launch(&spec(&root, "judge", &script, &script), &in_progress, Duration::from_secs(5))
            .unwrap_err();
    assert_eq!(refusal.reason, REASON_ROLE_DISABLED);
    assert!(in_progress.lock().unwrap().is_empty());
}

/// A session of one runtime is never resumed on another.
#[test]
fn a_resume_is_refused_when_the_role_now_resolves_to_another_runtime() {
    let resume = RoleRollResume {
        launch: RollResumeLaunch {
            runtime: "codex".to_string(),
            ..launch_of("judge")
        },
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    // No admission is the default Claude path.
    let why = resume.refuse(None).expect("codex session, claude launch");
    assert!(why.contains("codex") && why.contains("claude"), "{why}");
    let claude = RoleRollResume {
        launch: launch_of("judge"),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    assert_eq!(claude.refuse(None), None);
    claude.cancelled.store(true, AtomicOrdering::SeqCst);
    assert!(claude.refuse(None).unwrap().contains("cancelled"));
}

/// The requeue half: the claim label is removed and the reason is posted on
/// the PR the breadcrumb names; a root this installation may not write to is
/// refused.
#[test]
#[serial]
fn releasing_a_role_claim_removes_its_label_and_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let log = root.join("gh.log");
    let inner = root.join("inner-gh.sh");
    std::fs::write(
        &inner,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ws = crate::write_scope_test_support::WritableRoot::register_with_gh(&root, &inner);
    let claim = ClaimBreadcrumb {
        label: "loom:treating".to_string(),
        on: "pr".to_string(),
        number: 588,
        at: None,
    };
    release_claim(&root, &ws.gh, &claim, "requeued: role-disabled").unwrap();
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("pr edit 588 --remove-label loom:treating"), "{calls}");
    assert!(calls.contains("pr comment 588 --body requeued: role-disabled"), "{calls}");

    let read_only = tempfile::tempdir().unwrap();
    let ro = crate::write_scope_test_support::WritableRoot::read_only(read_only.path(), None);
    let refused = release_claim(read_only.path(), &ro.gh, &claim, "x").unwrap_err();
    assert!(refused.contains("write scope refused"), "{refused}");
}

/// A role claim is given back only while it is still the paused run's: a
/// `labeled` event later than the run's stop is someone else's claim. Clock
/// slack and an unreadable timeline leave it the run's (fail open, like the
/// sweep path's lease probe).
#[test]
fn a_role_claim_relabeled_after_the_stop_is_not_the_paused_runs() {
    let claim = ClaimBreadcrumb {
        label: "loom:reviewing".to_string(),
        on: "pr".to_string(),
        number: 77,
        at: None,
    };
    let stop = chrono::Utc::now();
    let secs = chrono::Duration::seconds;
    let why = relabeled_since(&claim, Some(stop + secs(600)), stop).unwrap_err();
    assert!(why.contains("loom:reviewing") && why.contains("#77"), "{why}");
    relabeled_since(&claim, Some(stop - secs(600)), stop).unwrap();
    relabeled_since(&claim, Some(stop + secs(RECLAIM_SLACK_SECS)), stop).unwrap();
    relabeled_since(&claim, None, stop).unwrap();
    // No stop time and no breadcrumb time: nothing to compare with.
    claim_still_ours(Path::new("/nonexistent"), Path::new("/bin/false"), &claim, None).unwrap();
}
