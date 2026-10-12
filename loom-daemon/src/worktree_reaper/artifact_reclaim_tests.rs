//! Tests for the kept-worktree artifact reclaim engine (#11071). Every fixture
//! lives in its own `tempfile` dir; no test scans `/tmp`, `$TMPDIR` or
//! `~/.cache` itself. `#[serial]` because `enumerate_worktree_dirs` reads the
//! process-wide `LOOM_WORKTREE_ROOT` and some tests set the activity-window
//! env var.

use super::*;
use crate::worktree_activity::ACTIVITY_WINDOW_ENV;
use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;

const MB: usize = 1024 * 1024;

fn no_handles(_: &Path) -> Option<bool> {
    Some(false)
}

fn held_open(_: &Path) -> Option<bool> {
    Some(true)
}

fn unprobeable(_: &Path) -> Option<bool> {
    None
}

fn no_executables(_: &Path) -> Vec<LiveExecutable> {
    Vec::new()
}

/// Every fixture on one volume unless a test injects otherwise.
fn one_volume(_: &Path) -> Option<u64> {
    Some(1)
}

fn free_probes() -> ArtifactProbes<'static> {
    ArtifactProbes {
        open_handles: &no_handles,
        executing_within: &no_executables,
        volume: &one_volume,
        euid: current_euid(),
    }
}

/// The 9243 shape: an open `loom:issue`, so the classifier keeps the
/// worktree but nothing marks it in use.
fn open_issue(_: &Path, _: u32) -> WorktreeDecision {
    WorktreeDecision::SkipIssueNotClosed("OPEN".to_string())
}

fn issue_class(classify: &dyn Fn(&Path, u32) -> WorktreeDecision) -> WorktreeClass<'_> {
    WorktreeClass {
        prefix: "issue",
        parse_name: &crate::worktree_ops::naming::issue_from_worktree,
        classify,
    }
}

/// A managed worktree with a source file and a committed-looking file.
fn worktree(root: &Path, name: &str) -> PathBuf {
    let wt = root.join(".loom/worktrees").join(name);
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(wt.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    fs::write(wt.join("Cargo.lock"), "lock\n").unwrap();
    fs::write(wt.join(".loom-managed"), "").unwrap();
    wt.canonicalize().unwrap()
}

/// Put `bytes` of build output under `dir`.
fn fill(dir: &Path, bytes: usize) {
    fs::create_dir_all(dir.join("debug/deps")).unwrap();
    fs::write(dir.join("debug/deps/blob"), vec![7u8; bytes]).unwrap();
}

/// Every file outside the artifact dirs, with its content: the "source,
/// branch and uncommitted changes" a reclaim must never touch.
fn source_listing(wt: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![wt.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if dir == wt && ["target", "node_modules"].contains(&name.as_str()) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    out
}

fn run(root: &Path, classify: &dyn Fn(&Path, u32) -> WorktreeDecision) -> ReclaimReport {
    reclaim_kept(root, &issue_class(classify), false, Duration::ZERO, &free_probes(), &never_stop)
}

fn with_activity_window<T>(minutes: &str, body: impl FnOnce() -> T) -> T {
    let previous = std::env::var(ACTIVITY_WINDOW_ENV).ok();
    std::env::set_var(ACTIVITY_WINDOW_ENV, minutes);
    let out = body();
    match previous {
        Some(v) => std::env::set_var(ACTIVITY_WINDOW_ENV, v),
        None => std::env::remove_var(ACTIVITY_WINDOW_ENV),
    }
    out
}

#[test]
#[serial]
fn an_idle_kept_worktree_loses_its_target_and_keeps_its_source() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-9243");
    fill(&wt.join("target"), MB);
    fill(&wt.join("node_modules"), 1024);
    let before = source_listing(&wt);

    let report = run(tmp.path(), &open_issue);

    assert!(!wt.join("target").exists());
    assert!(!wt.join("node_modules").exists());
    assert_eq!(source_listing(&wt), before, "source and lockfile untouched");
    assert_eq!(report.scanned, 1);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.reclaimed, vec![(9243, vec!["target".into(), "node_modules".into()])]);
    assert!(report.bytes_freed() >= MB as u64);
    assert!(report
        .removed
        .iter()
        .all(|a| a.class == "issue" && a.worktree == wt));
}

#[test]
#[serial]
fn a_worktree_the_classifier_holds_in_use_is_kept_with_its_bytes_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-7");
    fill(&wt.join("target"), MB);
    let in_use = |_: &Path, _: u32| {
        WorktreeDecision::SkipInUse("active process(es) using worktree: [42]".to_string())
    };

    let report = run(tmp.path(), &in_use);

    assert!(wt.join("target").is_dir());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].1.contains("active process"));
    let (kept, why) = &report.kept[0];
    assert_eq!(kept.name, "target");
    assert!(kept.bytes >= MB as u64, "a skip reports the bytes it leaves");
    assert!(why.contains("active process"));
}

#[test]
#[serial]
fn a_live_claim_lock_keeps_the_worktree_on_the_below_floor_pass() {
    // The cross-root pass reads the local claim locks itself.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let wt = worktree(&root, "issue-5");
    fill(&wt.join("target"), MB);
    fs::create_dir_all(root.join(".loom/locks/issue-5")).unwrap();

    let report = with_activity_window("0", || {
        reclaim_idle_targets(std::slice::from_ref(&root), false, &free_probes(), &never_stop)
    });

    assert!(wt.join("target").is_dir());
    assert!(report.removed.is_empty());
    assert!(report.skipped[0].1.contains("claim-lock"), "{:?}", report.skipped);
}

#[test]
#[serial]
fn a_fresh_write_inside_the_activity_window_keeps_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-8");
    fill(&wt.join("target"), 1024);

    let report = reclaim_kept(
        tmp.path(),
        &issue_class(&open_issue),
        false,
        Duration::from_secs(30 * 60),
        &free_probes(),
        &never_stop,
    );

    assert!(wt.join("target").is_dir());
    assert!(report.skipped[0].1.contains("recent filesystem activity"));
    assert_eq!(report.kept.len(), 1);
}

#[test]
#[serial]
fn a_symlinked_target_is_kept_and_never_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-9");
    let shared = tmp.path().join("shared-cargo-target");
    fill(&shared, MB);
    std::os::unix::fs::symlink(&shared, wt.join("target")).unwrap();
    std::os::unix::fs::symlink(&shared, wt.join("node_modules")).unwrap();

    let report = run(tmp.path(), &open_issue);

    assert!(fs::symlink_metadata(wt.join("target"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(shared.join("debug/deps/blob").is_file(), "the link target is untouched");
    assert!(report.removed.is_empty());
    assert_eq!(report.kept.len(), 2);
    for (a, why) in &report.kept {
        assert!(why.contains("symlink"), "{why}");
        assert_eq!(a.bytes, 0, "a symlink is not measured through");
    }
}

#[test]
#[serial]
fn a_target_backing_a_live_executable_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-10");
    fill(&wt.join("target"), 1024);
    let exe = wt.join("target/debug/deps/blob");
    let running = move |_: &Path| {
        vec![LiveExecutable {
            pid: 4242,
            exe: exe.clone(),
        }]
    };
    let probes = ArtifactProbes {
        open_handles: &no_handles,
        executing_within: &running,
        volume: &one_volume,
        euid: current_euid(),
    };

    let report = reclaim_kept(
        tmp.path(),
        &issue_class(&open_issue),
        false,
        Duration::ZERO,
        &probes,
        &never_stop,
    );

    assert!(wt.join("target").is_dir());
    assert!(report.removed.is_empty());
    assert!(report.kept[0].1.contains("live process"), "{}", report.kept[0].1);
}

#[test]
#[serial]
fn an_open_handle_or_an_unprobeable_host_keeps_the_target() {
    for (probe, needle) in [
        (&held_open as &dyn Fn(&Path) -> Option<bool>, "holds a file open"),
        (&unprobeable as &dyn Fn(&Path) -> Option<bool>, "probe unavailable"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree(tmp.path(), "issue-11");
        fill(&wt.join("target"), 1024);
        let probes = ArtifactProbes {
            open_handles: probe,
            executing_within: &no_executables,
            volume: &one_volume,
            euid: current_euid(),
        };
        let report = reclaim_kept(
            tmp.path(),
            &issue_class(&open_issue),
            false,
            Duration::ZERO,
            &probes,
            &never_stop,
        );
        assert!(wt.join("target").is_dir());
        assert!(report.kept[0].1.contains(needle), "{}", report.kept[0].1);
    }
}

#[test]
#[serial]
fn a_target_owned_by_another_uid_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-12");
    fill(&wt.join("target"), 1024);
    let probes = ArtifactProbes {
        euid: current_euid().wrapping_add(1),
        ..free_probes()
    };
    let report = reclaim_kept(
        tmp.path(),
        &issue_class(&open_issue),
        false,
        Duration::ZERO,
        &probes,
        &never_stop,
    );
    assert!(wt.join("target").is_dir());
    assert!(report.kept[0].1.contains("owned by uid"));
}

#[test]
#[serial]
fn removal_is_largest_first_and_stops_once_above_the_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let small = worktree(tmp.path(), "issue-1");
    let big = worktree(tmp.path(), "issue-2");
    let mid = worktree(tmp.path(), "issue-3");
    fill(&small.join("target"), MB);
    fill(&big.join("target"), 3 * MB);
    fill(&mid.join("target"), 2 * MB);
    // "Free space" is back above the floor once the largest is gone.
    let big_target = big.join("target");
    let stop = |_: &ArtifactDir| {
        (!big_target.exists()).then(|| "free space back above the floor".to_string())
    };

    let report = reclaim_kept(
        tmp.path(),
        &issue_class(&open_issue),
        false,
        Duration::ZERO,
        &free_probes(),
        &stop,
    );

    assert!(!big.join("target").exists());
    assert!(mid.join("target").is_dir());
    assert!(small.join("target").is_dir());
    assert_eq!(report.reclaimed, vec![(2, vec!["target".to_string()])]);
    let deferred: Vec<u32> = report.deferred.iter().map(|(a, _)| a.num).collect();
    assert_eq!(deferred, vec![3, 1], "the rest wait, largest first");
    assert!(report
        .deferred
        .iter()
        .all(|(_, why)| why.contains("above the floor")));
    assert!(report
        .stopped
        .as_deref()
        .unwrap()
        .contains("above the floor"));
}

#[test]
#[serial]
fn the_scheduled_tier_never_stops_and_still_goes_largest_first() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, size) in [("issue-1", MB), ("issue-2", 3 * MB), ("issue-3", 2 * MB)] {
        fill(&worktree(tmp.path(), name).join("target"), size);
    }
    let report = run(tmp.path(), &open_issue);
    let order: Vec<u32> = report.removed.iter().map(|a| a.num).collect();
    assert_eq!(order, vec![2, 3, 1]);
    assert!(report.deferred.is_empty() && report.stopped.is_none());
}

#[test]
#[serial]
fn a_dry_run_lists_without_removing() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-13");
    fill(&wt.join("target"), 1024);
    let report = reclaim_kept(
        tmp.path(),
        &issue_class(&open_issue),
        true,
        Duration::ZERO,
        &free_probes(),
        &never_stop,
    );
    assert!(report.dry_run);
    assert_eq!(report.removed.len(), 1);
    assert!(wt.join("target").is_dir());
}

#[test]
#[serial]
fn a_private_workspace_issue_is_skipped_with_a_reason_not_silently() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = worktree(tmp.path(), "issue-14");
    fill(&wt.join("target"), 1024);
    fs::create_dir_all(tmp.path().join(".loom/private-jobs")).unwrap();
    fs::write(tmp.path().join(".loom/private-jobs/issue-14.json"), "{}").unwrap();
    let report = run(tmp.path(), &open_issue);
    assert!(wt.join("target").is_dir());
    assert!(report.skipped[0].1.contains("private-workspace"));
}

/// Set every mtime under `path` (post-order) `secs` into the past.
fn backdate(path: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    let meta = fs::symlink_metadata(path).unwrap();
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).unwrap().flatten() {
            backdate(&entry.path(), secs);
        }
    }
    let file = if meta.is_dir() {
        fs::File::open(path).unwrap()
    } else {
        fs::File::options().write(true).open(path).unwrap()
    };
    file.set_modified(when).unwrap();
}

fn reflog_entry(age_secs: u64) -> String {
    let at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - age_secs;
    format!(
        "{z} {z} A U Thor <a@example.com> {at} +0000\tcommit: work\n",
        z = "0".repeat(40)
    )
}

/// #11071 regression. `git gc` (`reflog expire --all`) rewrites every linked
/// worktree's `logs/HEAD` without adding an entry; on loom-worker-1 that
/// made all 84 kept worktrees read as live, so none was ever trimmed. A
/// worktree idle for hours whose reflog was just rewritten is trimmed, and
/// one whose reflog gained a real entry is not.
#[test]
#[serial]
fn a_gc_rewritten_reflog_no_longer_pins_an_idle_worktrees_target() {
    for (newest_entry_age, expect_trim) in [(10 * 3600, true), (60, false)] {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree(tmp.path(), "issue-9243");
        fill(&wt.join("target"), MB);
        let gitdir = tmp.path().join("gitdir/worktrees/issue-9243");
        fs::create_dir_all(gitdir.join("logs")).unwrap();
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature/issue-9243\n").unwrap();
        fs::write(gitdir.join("index"), "idx").unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        fs::write(gitdir.join("logs/HEAD"), reflog_entry(10 * 3600)).unwrap();
        backdate(tmp.path(), 2 * 3600);
        // The gc rewrite (or a real commit): fresh mtime on logs/HEAD.
        let mut log = reflog_entry(10 * 3600);
        if newest_entry_age < 3600 {
            log.push_str(&reflog_entry(newest_entry_age));
        }
        fs::write(gitdir.join("logs/HEAD"), log).unwrap();

        let report = reclaim_kept(
            tmp.path(),
            &issue_class(&open_issue),
            false,
            Duration::from_secs(30 * 60),
            &free_probes(),
            &never_stop,
        );

        assert_eq!(!wt.join("target").exists(), expect_trim, "{report:?}");
        if !expect_trim {
            assert!(report.skipped[0].1.contains("recent filesystem activity"));
        }
    }
}

#[test]
#[serial]
fn the_below_floor_pass_spans_every_root_largest_first() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let small = worktree(a.path(), "issue-1");
    let big = worktree(b.path(), "pr-2");
    let mid = worktree(b.path(), "issue-3");
    fill(&small.join("target"), MB);
    fill(&big.join("target"), 3 * MB);
    fill(&mid.join("target"), 2 * MB);
    let big_target = big.join("target");
    let stop = |_: &ArtifactDir| (!big_target.exists()).then(|| "above the floor".to_string());
    let roots = [a.path().to_path_buf(), b.path().to_path_buf()];

    let report =
        with_activity_window("0", || reclaim_idle_targets(&roots, false, &free_probes(), &stop));

    assert!(!big.join("target").exists(), "{report:?}");
    assert!(mid.join("target").is_dir() && small.join("target").is_dir());
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].class, "pr");
    assert_eq!(report.deferred.len(), 2);
    assert_eq!(report.scanned, 3);
}

/// #11129 review: candidates are sorted across roots, so the stop must be
/// judged per volume. Volume H (healthy, above the floor) holds the largest
/// cache; volume P (pressured, below it) holds the rest. Each volume's free
/// space is injected on its own. H's caches are never removed and never stop
/// P's reclaim; P is reclaimed until *its* free space is back above the floor.
#[test]
#[serial]
fn a_healthy_volume_neither_blocks_nor_loses_caches_to_a_pressured_one() {
    // `p_needs`: how many of P's targets must go before P is above the floor.
    for p_needs in [1usize, 2] {
        let h = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        let h_root = h.path().canonicalize().unwrap();
        let p_root = p.path().canonicalize().unwrap();
        let h_big = worktree(&h_root, "issue-1");
        let h_small = worktree(&h_root, "pr-2");
        let p_big = worktree(&p_root, "issue-3");
        let p_small = worktree(&p_root, "issue-4");
        fill(&h_big.join("target"), 5 * MB);
        fill(&h_small.join("target"), MB);
        fill(&p_big.join("target"), 3 * MB);
        fill(&p_small.join("target"), 2 * MB);

        let volume = |wt: &Path| Some(if wt.starts_with(&h_root) { 10 } else { 20 });
        let probes = ArtifactProbes {
            volume: &volume,
            ..free_probes()
        };
        let h_asked = std::cell::Cell::new(0);
        let p_targets = [p_big.join("target"), p_small.join("target")];
        let stop = |a: &ArtifactDir| {
            if a.worktree.starts_with(&h_root) {
                h_asked.set(h_asked.get() + 1);
                return Some("free space back above the floor (H: 80G >= 20G)".to_string());
            }
            let gone = p_targets.iter().filter(|t| !t.exists()).count();
            (gone >= p_needs).then(|| "free space back above the floor (P)".to_string())
        };
        let roots = [h_root.clone(), p_root.clone()];

        let report =
            with_activity_window("0", || reclaim_idle_targets(&roots, false, &probes, &stop));

        assert!(h_big.join("target").is_dir(), "healthy volume kept: {report:?}");
        assert!(h_small.join("target").is_dir(), "healthy volume kept: {report:?}");
        assert_eq!(h_asked.get(), 1, "H is measured once, then deferred as a volume");
        assert!(!p_big.join("target").exists(), "pressured volume reclaimed: {report:?}");
        assert_eq!(!p_small.join("target").exists(), p_needs == 2, "{report:?}");
        let removed: Vec<u32> = report.removed.iter().map(|a| a.num).collect();
        assert_eq!(removed, if p_needs == 1 { vec![3] } else { vec![3, 4] });
        for (a, why) in &report.deferred {
            let on_h = a.worktree.starts_with(&h_root);
            assert_eq!(why.contains("(H:"), on_h, "{a:?} deferred with its own volume's reason");
        }
        let deferred: Vec<u32> = report.deferred.iter().map(|(a, _)| a.num).collect();
        assert_eq!(
            deferred,
            if p_needs == 1 {
                vec![1, 4, 2]
            } else {
                vec![1, 2]
            }
        );
    }
}

#[test]
fn log_report_handles_every_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = |num: u32, bytes: u64| ArtifactDir {
        class: "issue",
        num,
        worktree: tmp.path().join(format!("issue-{num}")),
        name: "target".to_string(),
        bytes,
    };
    log_report(tmp.path(), &ReclaimReport::default(), "issue");
    let full = ReclaimReport {
        scanned: 4,
        reclaimed: vec![(1, vec!["target".to_string()])],
        skipped: vec![(2, "in use".to_string())],
        removed: vec![dir(1, 10)],
        kept: vec![
            (dir(2, 5), "in use".to_string()),
            (dir(4, 0), "symlink".to_string()),
        ],
        deferred: vec![(dir(3, 1), "above the floor".to_string())],
        stopped: Some("above the floor".to_string()),
        failed: vec![(dir(5, 2), "EACCES".to_string())],
        dry_run: false,
    };
    log_idle_target_report(tmp.path(), &full);
    assert_eq!(full.bytes_freed(), 10);
    assert_eq!(full.bytes_kept(), 6);
    assert!(full.detail().contains("bytes_freed=10"));
}
