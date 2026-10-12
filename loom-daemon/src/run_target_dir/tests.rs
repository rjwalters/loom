use super::*;

fn cargo_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    dir
}

fn inputs<'a>() -> SpawnInputs<'a> {
    SpawnInputs {
        run_id: "123-abc",
        role: Some("builder"),
        ..SpawnInputs::default()
    }
}

#[test]
fn planned_path_lives_under_the_targets_root_and_is_recognized() {
    let repo = cargo_repo();
    let dir = planned_for(repo.path(), "doctor", "role-doctor-20261008T000000Z-abcd1234");
    assert_eq!(dir.parent().unwrap(), targets_root(repo.path()));
    assert!(is_run_target_dir(&dir), "{}", dir.display());
}

#[test]
fn hostile_segments_cannot_escape_the_targets_root() {
    let repo = cargo_repo();
    let dir = planned_for(repo.path(), "../../etc", "a/b/../c");
    assert_eq!(dir.parent().unwrap(), targets_root(repo.path()));
    assert!(is_run_target_dir(&dir));
    assert_eq!(planned_for(repo.path(), "", "").file_name().unwrap(), "run-run");
}

#[test]
fn run_dir_shape_refuses_everything_else() {
    for bad in [
        "relative/.loom/targets/x",
        "/tmp/cargo-target-1",
        "/repo/.loom/target-doctor-1",
        "/repo/.loom/targets",
        "/repo/targets/x",
        "/repo/.loom/targets/x/y",
        "/repo/.loom/targets/has space",
        "/repo/.loom/targets/..",
    ] {
        assert!(!is_run_target_dir(Path::new(bad)), "{bad}");
    }
    assert!(is_run_target_dir(Path::new("/repo/.loom/targets/judge-1")));
}

#[test]
fn a_plain_cargo_spawn_gets_a_derived_run_dir() {
    let repo = cargo_repo();
    let dir = decide(repo.path(), &inputs()).unwrap();
    assert_eq!(dir, planned_for(repo.path(), "builder", "123-abc"));
}

#[test]
fn the_planned_path_from_the_launcher_wins_over_a_derived_one() {
    let repo = cargo_repo();
    let planned = planned_for(repo.path(), "judge", "role-judge-x");
    let planned_s = planned.display().to_string();
    let got = decide(
        repo.path(),
        &SpawnInputs {
            planned: Some(&planned_s),
            ..inputs()
        },
    );
    assert_eq!(got, Some(planned));
}

#[test]
fn a_planned_path_outside_the_shape_is_ignored_not_trusted() {
    let repo = cargo_repo();
    let got = decide(
        repo.path(),
        &SpawnInputs {
            planned: Some("/home/me"),
            ..inputs()
        },
    )
    .unwrap();
    assert_eq!(got, planned_for(repo.path(), "builder", "123-abc"));
}

#[test]
fn each_precedence_input_leaves_the_environment_alone() {
    let repo = cargo_repo();
    let per = repo.path().join("wt/issue-1");
    let cases: [(&str, SpawnInputs<'_>); 3] = [
        (
            "per-worktree #8458 dir",
            SpawnInputs {
                per_worktree: Some(&per),
                ..inputs()
            },
        ),
        (
            "operator CARGO_TARGET_DIR",
            SpawnInputs {
                ambient: Some("/data/shared-target"),
                ..inputs()
            },
        ),
        (
            "containerized re-entry",
            SpawnInputs {
                containerized: true,
                ..inputs()
            },
        ),
    ];
    for (label, case) in cases {
        assert_eq!(decide(repo.path(), &case), None, "{label}");
    }
}

#[test]
fn a_blank_ambient_value_does_not_count_as_operator_set() {
    let repo = cargo_repo();
    let got = decide(
        repo.path(),
        &SpawnInputs {
            ambient: Some("  "),
            ..inputs()
        },
    );
    assert!(got.is_some());
}

#[test]
fn a_non_cargo_repo_gets_nothing() {
    let repo = tempfile::tempdir().unwrap();
    assert_eq!(decide(repo.path(), &inputs()), None);
}

#[test]
fn provision_creates_the_dir_and_records_the_owner() {
    let repo = cargo_repo();
    let dir = planned_for(repo.path(), "builder", "1");
    assert_eq!(provision(&dir, 4242), Some(dir.clone()));
    assert!(dir.is_dir());
    assert_eq!(owner_pid(&dir), Some(4242));
}

#[test]
fn provision_refuses_a_path_outside_the_shape() {
    let repo = cargo_repo();
    assert_eq!(provision(&repo.path().join("elsewhere"), 1), None);
    assert!(!repo.path().join("elsewhere").exists());
}

#[test]
fn provision_exports_nothing_when_create_fails() {
    let repo = cargo_repo();
    // `.loom` is a FILE, so `.loom/targets/x` cannot be created.
    std::fs::write(repo.path().join(".loom"), "").unwrap();
    assert_eq!(provision(&planned_for(repo.path(), "judge", "1"), 1), None);
}

#[test]
fn run_end_removes_the_dir_on_every_outcome() {
    let repo = cargo_repo();
    let guard = RunDirGuard::plan(repo.path(), "doctor", "t1");
    provision(guard.dir(), 999_999).unwrap();
    std::fs::write(guard.dir().join("big.rlib"), vec![0u8; 1024]).unwrap();
    let got = guard.finish_with(&|_| false, &|_| false, &|p| std::fs::remove_dir_all(p));
    assert_eq!(got, Removal::Removed);
    assert!(!guard.dir().exists());
}

#[test]
fn run_end_with_nothing_created_is_absent_not_an_error() {
    let repo = cargo_repo();
    let guard = RunDirGuard::plan(repo.path(), "judge", "t2");
    assert_eq!(guard.finish_with(&|_| false, &|_| false, &|_| Ok(())), Removal::Absent);
}

#[test]
fn run_end_keeps_a_dir_whose_owner_is_still_running() {
    let repo = cargo_repo();
    let guard = RunDirGuard::plan(repo.path(), "builder", "t3");
    provision(guard.dir(), 77).unwrap();
    let got = guard.finish_with(&|pid| pid == 77, &|_| false, &|_| panic!("must not remove"));
    assert!(matches!(got, Removal::Kept(_)), "{got:?}");
    assert!(guard.dir().is_dir());
}

#[test]
fn a_failing_removal_is_reported_not_raised() {
    let repo = cargo_repo();
    let guard = RunDirGuard::plan(repo.path(), "builder", "t4");
    provision(guard.dir(), 1).unwrap();
    let got = guard.finish_with(&|_| false, &|_| false, &|_| Err(std::io::Error::other("EBUSY")));
    assert!(matches!(got, Removal::Failed(ref m) if m.contains("EBUSY")), "{got:?}");
}

#[cfg(unix)]
#[test]
fn run_end_never_follows_a_symlink_out_of_the_targets_root() {
    let repo = cargo_repo();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), "x").unwrap();
    let guard = RunDirGuard::plan(repo.path(), "builder", "t5");
    std::fs::create_dir_all(targets_root(repo.path())).unwrap();
    std::os::unix::fs::symlink(outside.path(), guard.dir()).unwrap();
    let got = guard.finish_with(&|_| false, &|_| false, &|_| panic!("must not remove"));
    assert!(matches!(got, Removal::Kept(_)), "{got:?}");
    assert!(outside.path().join("keep").exists());
    std::fs::remove_file(guard.dir()).unwrap();
}

#[test]
fn the_guard_passes_the_planned_path_to_the_child() {
    let repo = cargo_repo();
    let guard = RunDirGuard::plan(repo.path(), "judge", "t6");
    let mut cmd = std::process::Command::new("true");
    guard.apply(&mut cmd);
    let value = cmd
        .get_envs()
        .find(|(k, _)| *k == RUN_TARGET_DIR_ENV)
        .and_then(|(_, v)| v)
        .unwrap();
    assert_eq!(Path::new(value), guard.dir());
}

#[test]
fn run_end_keeps_the_dir_while_the_ticks_process_group_has_a_live_member() {
    let repo = cargo_repo();
    let mut guard = RunDirGuard::plan(repo.path(), "builder", "t7");
    // The recorded owner (the harness) is gone; a descendant is not.
    provision(guard.dir(), 999_999).unwrap();
    guard.watch_process_group(4242);
    let got = guard.finish_with(&|_| false, &|pgid| pgid == 4242, &|_| panic!("must not remove"));
    assert!(matches!(got, Removal::Kept(ref why) if why.contains("4242")), "{got:?}");
    assert!(guard.dir().is_dir());
    // Once the group is empty the same guard removes it.
    let got = guard.finish_with(&|_| false, &|_| false, &|p| std::fs::remove_dir_all(p));
    assert_eq!(got, Removal::Removed);
}

#[test]
fn an_unwatched_guard_never_asks_about_a_process_group() {
    let repo = cargo_repo();
    let guard = RunDirGuard::plan(repo.path(), "builder", "t8");
    provision(guard.dir(), 999_999).unwrap();
    let got =
        guard.finish_with(&|_| false, &|_| panic!("no group"), &|p| std::fs::remove_dir_all(p));
    assert_eq!(got, Removal::Removed);
}

#[test]
fn group_ids_that_do_not_name_one_group_read_as_alive() {
    // kill(0, 0) is this process's own group and kill(-1, 0) is everything.
    assert!(process_group_alive(0));
    assert!(process_group_alive(1));
    assert!(process_group_alive(u32::MAX));
}

/// The real probe against a real group: alive while a member runs, gone once
/// the member is reaped.
#[cfg(unix)]
#[test]
fn the_process_group_probe_tracks_a_real_group() {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = child.id();
    assert!(process_group_alive(pgid));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!process_group_alive(pgid));
}

// ============================================================================
// #11031: owner identity, marker-last removal, sweep-end reclaim.
// ============================================================================

fn provisioned(repo: &Path, name: &str, pid: u32) -> PathBuf {
    let dir = planned_for(repo, "sweep-lifecycle", name);
    provision(&dir, pid).unwrap();
    std::fs::create_dir_all(dir.join("debug/deps")).unwrap();
    std::fs::write(dir.join("debug/deps/lib.rlib"), b"x").unwrap();
    dir
}

/// `provision` records the owner's start identity next to the pid, and the
/// production probe reads the same identity back for the same live process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn provision_records_the_owners_start_identity() {
    let repo = cargo_repo();
    let me = std::process::id();
    let dir = provisioned(repo.path(), "1", me);
    let recorded = owner::recorded_start_token(&dir).expect("an identity is recorded");
    assert_eq!(Some(recorded), owner::process_start_token(me));
    assert_eq!(owner::running_owner(&dir), Some(me), "a live owner is running");
}

/// PID reuse: the pid in the marker is alive, but it is not the process that
/// wrote the marker. The live process here is this test; the recorded
/// identity is somebody else's.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_reused_pid_is_not_the_owner() {
    let repo = cargo_repo();
    let me = std::process::id();
    let dir = provisioned(repo.path(), "1", me);
    std::fs::write(dir.join(owner::OWNER_START_FILE), "linux:other-boot:1\n").unwrap();
    assert_eq!(owner::running_owner(&dir), None, "a different process wears the pid");
}

#[test]
fn identity_decides_only_on_positive_evidence() {
    let repo = cargo_repo();
    let dir = provisioned(repo.path(), "1", 4242);
    std::fs::write(dir.join(owner::OWNER_START_FILE), "t1\n").unwrap();
    let alive = |p: u32| p == 4242;
    let same = |_: u32| Some("t1".to_string());
    let other = |_: u32| Some("t2".to_string());
    let unknown = |_: u32| None;
    assert_eq!(owner::running_owner_with(&dir, &alive, &same), Some(4242));
    assert_eq!(owner::running_owner_with(&dir, &alive, &other), None, "pid reused");
    assert_eq!(owner::running_owner_with(&dir, &alive, &unknown), Some(4242), "unknown keeps");
    assert_eq!(owner::running_owner_with(&dir, &|_| false, &same), None, "owner gone");
    // A dir provisioned before #11031 has no identity: bare liveness decides.
    std::fs::remove_file(dir.join(owner::OWNER_START_FILE)).unwrap();
    assert_eq!(owner::running_owner_with(&dir, &alive, &other), Some(4242));
}

#[test]
fn marker_last_removal_removes_everything() {
    let repo = cargo_repo();
    let dir = provisioned(repo.path(), "1", 4242);
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join("link")).unwrap();
        owner::remove_marker_last(&dir).unwrap();
        assert!(outside.path().join("keep").exists(), "a symlink's target is untouched");
    }
    #[cfg(not(unix))]
    owner::remove_marker_last(&dir).unwrap();
    assert!(!dir.exists());
}

#[test]
fn a_sweeps_dirs_are_found_by_the_pid_in_their_marker() {
    let repo = cargo_repo();
    let mine = provisioned(repo.path(), "777-1", 777);
    let _other = provisioned(repo.path(), "778-1", 778);
    std::fs::create_dir_all(targets_root(repo.path()).join("sweep-lifecycle-777-2")).unwrap();
    assert_eq!(sweep_end::dirs_owned_by(repo.path(), 777), vec![mine]);
    assert!(sweep_end::dirs_owned_by(repo.path(), 0).is_empty());
    assert!(sweep_end::dirs_owned_by(repo.path(), 779).is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlinked_targets_root_yields_no_sweep_dirs() {
    let repo = cargo_repo();
    let elsewhere = tempfile::tempdir().unwrap();
    let dir = elsewhere.path().join("sweep-lifecycle-777-1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(OWNER_FILE), "777\n").unwrap();
    std::fs::create_dir_all(repo.path().join(".loom")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), targets_root(repo.path())).unwrap();
    assert!(sweep_end::dirs_owned_by(repo.path(), 777).is_empty());
}

fn sweep_probes<'a>(
    group_alive: &'a dyn Fn(u32) -> bool,
    owner_alive: &'a dyn Fn(u32) -> bool,
    open_handles: &'a dyn Fn(&Path) -> Option<bool>,
) -> sweep_end::SweepEndProbes<'a> {
    sweep_end::SweepEndProbes {
        group_alive,
        owner_alive,
        identity: &|_| None,
        open_handles,
        remove: &owner::remove_marker_last,
    }
}

/// Every gate keeps the dir on its own; with all of them clear it goes.
#[test]
fn the_sweep_end_gates_each_keep_the_dir() {
    let repo = cargo_repo();
    let dir = provisioned(repo.path(), "777-1", 777);
    let free = |_: &Path| Some(false);
    let kept = |r: Removal| matches!(r, Removal::Kept(_));
    let probes = sweep_probes(&|_| true, &|_| false, &free);
    assert!(kept(sweep_end::reclaim_with(&dir, Some(777), &probes)), "group alive");
    let probes = sweep_probes(&|_| false, &|p| p == 777, &free);
    assert!(kept(sweep_end::reclaim_with(&dir, Some(777), &probes)), "owner alive");
    let probes = sweep_probes(&|_| false, &|_| false, &|_| Some(true));
    assert!(kept(sweep_end::reclaim_with(&dir, Some(777), &probes)), "held open");
    let probes = sweep_probes(&|_| false, &|_| false, &|_| None);
    assert!(kept(sweep_end::reclaim_with(&dir, Some(777), &probes)), "probe unavailable");
    assert!(dir.join("debug/deps/lib.rlib").exists());
    let probes = sweep_probes(&|_| false, &|_| false, &free);
    assert_eq!(sweep_end::reclaim_with(&dir, Some(777), &probes), Removal::Removed);
    assert!(!dir.exists());
}

/// The production entry point, end to end: a dead owner's dir goes on the
/// detached thread; a live owner's dir (this process) is never touched.
#[test]
fn the_sweep_end_reclaim_removes_a_dead_owners_dir_only() {
    let repo = cargo_repo();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let dir = provisioned(repo.path(), "dead", dead);
    // Should the number be reused meanwhile, the identity still says "gone".
    std::fs::write(dir.join(owner::OWNER_START_FILE), "exited\n").unwrap();
    sweep_end::reclaim_at_sweep_end(repo.path(), "sweep-issue-1-1", dead, None)
        .expect("a dir to reclaim")
        .join()
        .unwrap();
    assert!(!dir.exists());

    let me = std::process::id();
    let live = provisioned(repo.path(), "live", me);
    sweep_end::reclaim_at_sweep_end(repo.path(), "sweep-issue-2-1", me, None)
        .expect("a dir to consider")
        .join()
        .unwrap();
    assert!(live.join("debug/deps/lib.rlib").exists(), "a live owner's dir is never touched");
}
