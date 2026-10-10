#![allow(clippy::unwrap_used)]

use super::*;

fn live(key: &str, repo: &str, issue: Option<u32>, gb: u64) -> LiveUnit {
    LiveUnit {
        key: key.into(),
        repo: repo.into(),
        issue,
        bytes: gb * GIB,
        build_measured: true,
    }
}

#[test]
fn observe_folds_the_peak_into_history_when_a_unit_ends() {
    let mut store = Store::default();
    observe(&mut store, &[live("loom#1", "loom", Some(1), 5)], 100);
    observe(&mut store, &[live("loom#1", "loom", Some(1), 26)], 160);
    // A later, smaller sample (the run dir trimmed) keeps the peak.
    observe(&mut store, &[live("loom#1", "loom", Some(1), 20)], 220);
    assert_eq!(store.inflight["loom#1"].current_bytes, 20 * GIB);
    assert_eq!(store.inflight["loom#1"].peak_bytes, 26 * GIB);
    let ended = observe(&mut store, &[], 280);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].peak_bytes, 26 * GIB);
    assert_eq!(store.repos["loom"], vec![26 * GIB]);
    assert_eq!(store.high_water_bytes("loom"), Some(26 * GIB));
    assert!(store.inflight.is_empty());
    assert_eq!(store.sampled_at, Some(280));
}

#[test]
fn history_is_a_rolling_window() {
    let mut store = Store::default();
    for i in 0..(HISTORY_LEN as u64 + 3) {
        let key = format!("r#{i}");
        observe(&mut store, &[live(&key, "r", Some(1), i + 1)], 0);
        observe(&mut store, &[], 0);
    }
    let hist = &store.repos["r"];
    assert_eq!(hist.len(), HISTORY_LEN);
    assert_eq!(hist[0], 4 * GIB, "the three oldest aged out");
}

#[test]
fn a_pending_unit_survives_its_grace_then_drops_without_history() {
    let mut store = Store::default();
    record_pending(&mut store, "loom#7", "loom", Some(7), 1_000);
    // A sample inside the grace window that does not see it keeps it.
    observe(&mut store, &[], 1_000 + PENDING_GRACE_SECS - 1);
    assert!(store.inflight.contains_key("loom#7"));
    // After the grace it is dropped, and never folded into history.
    let ended = observe(&mut store, &[], 1_000 + PENDING_GRACE_SECS);
    assert!(ended.is_empty());
    assert!(store.inflight.is_empty());
    assert!(!store.repos.contains_key("loom"));
}

#[test]
fn a_sample_adopts_a_pending_unit_of_the_same_key() {
    let mut store = Store::default();
    record_pending(&mut store, "loom#7", "loom", Some(7), 1_000);
    observe(&mut store, &[live("loom#7", "loom", Some(7), 3)], 1_010);
    let u = &store.inflight["loom#7"];
    assert_eq!(u.pending_since, None);
    assert_eq!(u.current_bytes, 3 * GIB);
    // Now sampled, it ends normally and records its peak.
    observe(&mut store, &[], 1_070);
    assert_eq!(store.repos["loom"], vec![3 * GIB]);
}

#[test]
fn stale_store_units_stop_counting_but_fresh_pending_units_do() {
    let mut store = Store::default();
    observe(&mut store, &[live("loom#1", "loom", Some(1), 5)], 0);
    record_pending(&mut store, "loom#2", "loom", Some(2), STALE_AFTER_SECS);
    let now = STALE_AFTER_SECS;
    assert!(!store.fresh(now));
    assert!(!store.inflight["loom#1"].counts(store.fresh(now), now));
    assert!(store.inflight["loom#2"].counts(store.fresh(now), now));
}

fn lock(issue: u32, pid: u32, pgid: Option<u32>) -> LockInfo {
    LockInfo {
        issue,
        owner_pid: pid,
        pgid,
    }
}

fn run_dir(name: &str, pid: u32, pgid: Option<u32>) -> RunDir {
    RunDir {
        path: PathBuf::from(format!("/r/.loom/targets/{name}")),
        owner_pid: pid,
        pgid,
    }
}

#[test]
fn assemble_attributes_run_dirs_to_their_sweep_by_pid_or_group() {
    let size_of = |p: &Path| -> u64 {
        match p.file_name().and_then(|n| n.to_str()) {
            Some("issue-1") => GIB,
            Some("sweep-a") => 20 * GIB,
            Some("sweep-b") => 4 * GIB,
            Some("judge-c") => 2 * GIB,
            _ => 0,
        }
    };
    let locks = [lock(1, 100, Some(100)), lock(2, 200, Some(200))];
    // sweep-a: owned by lock 1's pid. sweep-b: a child in lock 2's group.
    // judge-c: a role-runner tick no lock owns.
    let dirs = [
        run_dir("sweep-a", 100, Some(100)),
        run_dir("sweep-b", 250, Some(200)),
        run_dir("judge-c", 300, Some(300)),
    ];
    let units = assemble(&scan(&locks, &[], &dirs), &probes(&size_of, &no_marker));
    assert_eq!(units.len(), 3);
    assert_eq!(units[0], live("loom#1", "loom", Some(1), 21));
    assert_eq!(units[1], live("loom#2", "loom", Some(2), 4));
    assert_eq!(units[2], live("loom:judge-c", "loom", None, 2));
}

fn scan<'a>(
    locks: &'a [LockInfo],
    pr_locks: &'a [LockInfo],
    run_dirs: &'a [RunDir],
) -> RepoScan<'a> {
    RepoScan {
        repo: "loom",
        worktree_root: Path::new("/wt"),
        builds: true,
        locks,
        pr_locks,
        run_dirs,
    }
}

fn probes<'a>(
    size_of: &'a dyn Fn(&Path) -> u64,
    worktree_target: &'a dyn Fn(&Path) -> Option<PathBuf>,
) -> Probes<'a> {
    Probes {
        size_of,
        worktree_target,
    }
}

fn no_marker(_: &Path) -> Option<PathBuf> {
    None
}

/// The #11191 review's reproduction: one 26 GB loom sweep, then twenty
/// curator ticks whose run dirs hold only their 6-byte owner file, each
/// assembled exactly as the sampler assembles them. The sweep's mark must
/// survive, and the charge stay 26 GB + 10% = 29 GB.
#[test]
fn twenty_idle_role_ticks_do_not_wash_out_a_heavy_sweep() {
    let mut store = Store::default();
    let mut now = 1_000;
    let sweep_size = |p: &Path| -> u64 {
        if p.ends_with("issue-1") {
            GIB
        } else {
            25 * GIB
        }
    };
    let locks = [lock(1, 100, Some(100))];
    let dirs = [run_dir("sweep-lifecycle-100-1700000000", 100, Some(100))];
    let units = assemble(&scan(&locks, &[], &dirs), &probes(&sweep_size, &no_marker));
    observe(&mut store, &units, now);
    now += 60;
    observe(&mut store, &[], now);
    assert_eq!(store.high_water_bytes("loom"), Some(26 * GIB));

    let owner_file_only = |_: &Path| -> u64 { 6 };
    for i in 0..20u32 {
        let name = format!("curator-role-curator-20261009T1{i:05}Z-abcd{i:04}");
        let dirs = [run_dir(&name, 500 + i, Some(500 + i))];
        let units = assemble(&scan(&[], &[], &dirs), &probes(&owner_file_only, &no_marker));
        assert_eq!(units.len(), 1);
        assert_eq!(role_of_key(&units[0].key).as_deref(), Some("curator"));
        now += 60;
        observe(&mut store, &units, now);
        now += 60;
        let ended = observe(&mut store, &[], now);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].folded_into, None, "an owner-file-only dir never built");
    }
    assert_eq!(store.high_water_bytes("loom"), Some(26 * GIB));
    assert_eq!(store.repos["loom"].len(), 1);
    assert!(store.role_runs.is_empty());
    let charge = crate::disk_admission::charge_gb(store.high_water_bytes("loom"), None, 8);
    assert_eq!(charge, (29, crate::disk_admission::ChargeSource::Observed));
}

#[test]
fn a_role_run_that_built_folds_into_its_own_role_history() {
    let mut store = Store::default();
    let dirs = [run_dir(
        "judge-role-judge-20261009T100000Z-0a1b2c3d",
        9,
        Some(9),
    )];
    let seven = |_: &Path| -> u64 { 7 * GIB };
    let units = assemble(&scan(&[], &[], &dirs), &probes(&seven, &no_marker));
    observe(&mut store, &units, 0);
    let ended = observe(&mut store, &[], 60);
    assert_eq!(ended[0].folded_into.as_deref(), Some("loom:judge"));
    assert_eq!(store.role_high_water_bytes("loom", "judge"), Some(7 * GIB));
    assert_eq!(store.high_water_bytes("loom"), None, "never the repo's sweep history");
}

#[test]
fn run_dir_roles_are_read_from_the_dir_name() {
    assert_eq!(run_dir_role("curator-role-curator-20261009T100000Z-0a1b2c3d"), "curator");
    assert_eq!(run_dir_role("sweep-lifecycle-4242-1760000000"), "sweep-lifecycle");
    assert_eq!(run_dir_role("worker-4242-1760000000"), "worker");
    assert_eq!(run_dir_role("judge-c"), "judge-c");
    assert_eq!(role_of_key("loom#12"), None);
    assert_eq!(role_of_key("loom#prs-4"), None);
    assert_eq!(role_of_key("loom:sweep-lifecycle-4242-1760000000"), None);
    assert_eq!(role_of_key("loom:builder-role-builder-x"), None);
    assert_eq!(role_of_key("loom:guide-role-guide-x").as_deref(), Some("guide"));
}

#[test]
fn a_per_worktree_target_dir_is_measured_with_its_issue() {
    // #8458: the build goes to `<shared>/wt/issue-3`, recorded in the
    // worktree's marker; no run dir exists (run_target_dir precedence 1).
    let size_of = |p: &Path| -> u64 {
        if p.ends_with("wt/issue-3") && p.starts_with("/big") {
            24 * GIB
        } else {
            GIB
        }
    };
    let marker = |wt: &Path| -> Option<PathBuf> {
        wt.ends_with("issue-3")
            .then(|| PathBuf::from("/big/cargo-target/wt/issue-3"))
    };
    let locks = [lock(3, 30, Some(30))];
    let units = assemble(&scan(&locks, &[], &[]), &probes(&size_of, &marker));
    assert_eq!(units, vec![live("loom#3", "loom", Some(3), 25)]);
}

#[test]
fn a_sweep_whose_build_output_is_never_found_is_not_folded() {
    // A Cargo repo, no marker, no run dir: an operator CARGO_TARGET_DIR or a
    // containerized run. Only the source checkout is visible.
    let one = |_: &Path| -> u64 { GIB };
    let locks = [lock(4, 40, Some(40))];
    let units = assemble(&scan(&locks, &[], &[]), &probes(&one, &no_marker));
    assert!(!units[0].build_measured);
    let mut store = Store::default();
    observe(&mut store, &units, 0);
    let ended = observe(&mut store, &[], 60);
    assert!(ended[0].unmeasured);
    assert_eq!(ended[0].folded_into, None);
    assert!(store.repos.is_empty(), "the repo keeps its configured or default charge");
    // A repo with nothing to build has nothing to miss.
    let mut light = scan(&locks, &[], &[]);
    light.builds = false;
    assert!(assemble(&light, &probes(&one, &no_marker))[0].build_measured);
}

#[test]
fn a_pr_set_is_one_unit_keyed_by_its_lowest_pr() {
    let size_of = |_: &Path| -> u64 { 10 * GIB };
    let pr_locks = [lock(9, 70, Some(70)), lock(4, 70, Some(70))];
    let dirs = [run_dir("sweep-lifecycle-70-1", 70, Some(70))];
    let units = assemble(&scan(&[], &pr_locks, &dirs), &probes(&size_of, &no_marker));
    assert_eq!(units, vec![live("loom#prs-4", "loom", None, 10)]);
    // The key matches what admission records it under, so the sample adopts it.
    let (key, _, _) =
        crate::disk_admission::kind_key("loom", &crate::types::SweepKind::PrSet(vec![9, 4]));
    assert_eq!(key, "loom#prs-4");
}

#[test]
fn live_locks_and_run_dirs_skip_dead_owners() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (issue, pid) in [(5u32, 11u32), (6, 12)] {
        let dir = root.join(".loom/locks").join(format!("issue-{issue}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("owner.json"),
            format!(
                r#"{{"issue":{issue},"owner_pid":{pid},"acquired_at":"x","sweep_id":"s","pgid":{pid}}}"#
            ),
        )
        .unwrap();
    }
    let alive = |pid: u32| pid == 11 || pid == 21;
    let locks = live_locks(root, &alive);
    assert_eq!(locks, vec![lock(5, 11, Some(11))]);

    for (name, pid) in [("sweep-1", 21u32), ("judge-2", 22)] {
        let dir = crate::run_target_dir::targets_root(root).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::run_target_dir::OWNER_FILE), format!("{pid}\n")).unwrap();
    }
    let dirs = live_run_dirs(root, &alive, &|_| None);
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].owner_pid, 21);
}

#[test]
fn store_round_trips_through_a_per_process_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d.json");
    let mut store = Store::default();
    observe(&mut store, &[live("a#1", "a", Some(1), 1)], 5);
    record_pending(&mut store, "a#2", "a", Some(2), 6);
    store.role_runs.insert("a:judge".into(), vec![GIB]);
    save(&path, &store);
    assert_eq!(load(&path), store);
    let names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["d.json".to_string()], "no temp file left behind");
}

#[test]
fn a_corrupt_store_is_moved_aside_not_silently_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d.json");
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(load(&path), Store::default());
    assert!(!path.exists(), "the corrupt file is no longer at the store path");
    let aside: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains("d.json.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1);
    assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), "not json");
}

#[test]
fn a_store_without_build_measured_fields_still_folds() {
    // A store written before #11191's review round: no `build_measured`.
    let raw = r#"{"repos":{},"inflight":{"loom#1":{"repo":"loom","issue":1,
        "current_bytes":2147483648,"peak_bytes":2147483648}},"sampled_at":5}"#;
    let mut store: Store = serde_json::from_str(raw).unwrap();
    assert!(store.inflight["loom#1"].build_measured);
    observe(&mut store, &[], 65);
    assert_eq!(store.high_water_bytes("loom"), Some(2 * GIB));
}

#[test]
fn a_panicking_sample_is_caught() {
    // One sampler iteration must survive whatever a sample throws, so the
    // `disk-footprint` thread keeps running.
    let caught = std::panic::catch_unwind(|| {
        sample_guarded_with(&[], &|_| panic!("probe exploded"));
    });
    assert!(caught.is_ok(), "the panic did not escape the sampler iteration");
}

#[test]
fn pid_alive_sees_this_process_and_not_pid_zero() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(0));
}
