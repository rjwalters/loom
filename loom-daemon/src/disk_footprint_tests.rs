#![allow(clippy::unwrap_used)]

use super::*;

fn live(key: &str, repo: &str, issue: Option<u32>, gb: u64) -> LiveUnit {
    LiveUnit {
        key: key.into(),
        repo: repo.into(),
        issue,
        bytes: gb * GIB,
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
    let units = assemble("loom", Path::new("/wt"), &locks, &dirs, &size_of);
    assert_eq!(units.len(), 3);
    assert_eq!(units[0], live("loom#1", "loom", Some(1), 21));
    assert_eq!(units[1], live("loom#2", "loom", Some(2), 4));
    assert_eq!(units[2], live("loom:judge-c", "loom", None, 2));
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
fn store_round_trips_and_a_corrupt_file_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d.json");
    let mut store = Store::default();
    observe(&mut store, &[live("a#1", "a", Some(1), 1)], 5);
    record_pending(&mut store, "a#2", "a", Some(2), 6);
    save(&path, &store);
    assert_eq!(load(&path), store);
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(load(&path), Store::default());
}

#[test]
fn pid_alive_sees_this_process_and_not_pid_zero() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(0));
}
