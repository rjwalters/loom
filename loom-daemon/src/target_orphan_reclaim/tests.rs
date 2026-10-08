//! Every test runs against temp dirs handed in as [`ScanRoot`]s. None calls
//! [`run_for`] / [`run_now`], which scan the real `/tmp` and `~/.cache`.

use super::*;
use std::time::{Duration, SystemTime};

const HOUR: i64 = 3600;

/// Set the mtime of `path` and everything under it to `hours` ago.
fn backdate(path: &Path, hours: u64) {
    let when = SystemTime::now() - Duration::from_secs(hours * 3600);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if !entry.file_type().unwrap().is_symlink() {
                backdate(&entry.path(), hours);
            }
        }
    }
    let f = std::fs::File::open(path).unwrap();
    f.set_modified(when).unwrap();
}

/// A dir with one 1 KiB file, backdated by `hours`.
fn make_dir(path: &Path, hours: u64) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("lib.rlib"), vec![0u8; 1024]).unwrap();
    backdate(path, hours);
}

struct Fixture {
    free: fn(&Path) -> Option<bool>,
    alive: fn(u32) -> bool,
    live: HashSet<u32>,
    protected: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            free: |_| Some(false),
            alive: |_| false,
            live: HashSet::new(),
            protected: Vec::new(),
        }
    }

    fn eval(&self, path: &Path) -> Result<Candidate, KeepReason> {
        let probes = Probes {
            open_handles: &self.free,
            owner_alive: &self.alive,
            live_issues: &self.live,
            protected: &self.protected,
        };
        evaluate(path, Utc::now(), 3 * HOUR, &probes)
    }
}

#[test]
fn prefix_matcher_needs_a_suffix_and_the_exact_prefix() {
    let m = NameMatcher::Prefixes(vec!["cargo-target-"]);
    assert!(m.matches("cargo-target-issue-10078"));
    assert!(!m.matches("cargo-target-"));
    assert!(!m.matches("my-cargo-target-1"));
    assert!(!m.matches("target"));
    assert!(!m.matches("cargo-targets"));
}

#[test]
fn run_dir_matcher_refuses_odd_names() {
    assert!(NameMatcher::RunDir.matches("doctor-role-doctor-20261008T000000Z-ab12"));
    assert!(!NameMatcher::RunDir.matches(""));
    assert!(!NameMatcher::RunDir.matches(".hidden"));
    assert!(!NameMatcher::RunDir.matches("a b"));
}

#[test]
fn scan_roots_cover_exactly_the_known_prefixes() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tmpdir = tempfile::tempdir().unwrap();
    let roots = scan_roots(repo.path(), Some(home.path()), Some(tmpdir.path()));
    let find = |dir: PathBuf| roots.iter().find(|r| r.dir == dir).unwrap().matcher.clone();
    assert_eq!(find(repo.path().join(".loom/targets")), NameMatcher::RunDir);
    assert_eq!(find(repo.path().join(".loom")), NameMatcher::Prefixes(vec!["target-"]));
    assert_eq!(
        find(PathBuf::from("/tmp")),
        NameMatcher::Prefixes(vec!["loom-target-", "cargo-target-"])
    );
    assert_eq!(find(tmpdir.path().to_path_buf()), NameMatcher::Prefixes(vec!["cargo-target-"]));
    assert_eq!(find(home.path().join(".cache")), NameMatcher::Prefixes(vec!["cargo-target-"]));
    assert_eq!(roots.len(), 5);
}

#[test]
fn a_tmpdir_that_is_tmp_is_merged_not_scanned_twice() {
    let repo = tempfile::tempdir().unwrap();
    let roots = scan_roots(repo.path(), None, Some(Path::new("/tmp")));
    assert_eq!(roots.iter().filter(|r| r.dir == Path::new("/tmp")).count(), 1);
    assert_eq!(roots.len(), 3);
}

#[test]
fn relative_home_and_tmpdir_are_ignored() {
    let repo = tempfile::tempdir().unwrap();
    let roots = scan_roots(repo.path(), Some(Path::new("rel")), Some(Path::new("rel2")));
    assert_eq!(roots.len(), 3);
}

#[test]
fn an_old_free_dir_is_eligible_with_its_size() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-review-9745");
    make_dir(&dir, 10);
    let got = Fixture::new().eval(&dir).unwrap();
    assert!(got.size_bytes >= 1024);
}

#[test]
fn a_younger_dir_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-1");
    make_dir(&dir, 1);
    assert!(matches!(Fixture::new().eval(&dir), Err(KeepReason::Young { .. })));
}

#[test]
fn a_fresh_write_deep_inside_an_old_dir_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-2");
    make_dir(&dir, 10);
    std::fs::create_dir_all(dir.join("debug/deps")).unwrap();
    std::fs::write(dir.join("debug/deps/new.o"), "x").unwrap();
    // Re-backdate only the top dir, so its own mtime is old.
    std::fs::File::open(&dir)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10 * 3600))
        .unwrap();
    assert!(matches!(Fixture::new().eval(&dir), Err(KeepReason::Young { .. })));
}

#[test]
fn an_open_handle_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-3");
    make_dir(&dir, 10);
    let fx = Fixture {
        free: |_| Some(true),
        ..Fixture::new()
    };
    assert_eq!(fx.eval(&dir), Err(KeepReason::OpenHandle));
}

#[test]
fn a_probe_that_cannot_run_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-4");
    make_dir(&dir, 10);
    let fx = Fixture {
        free: |_| None,
        ..Fixture::new()
    };
    assert_eq!(fx.eval(&dir), Err(KeepReason::ProbeUnavailable));
}

#[test]
fn a_live_claim_on_the_named_issue_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-issue-10078");
    make_dir(&dir, 10);
    let fx = Fixture {
        live: HashSet::from([10078]),
        ..Fixture::new()
    };
    assert_eq!(fx.eval(&dir), Err(KeepReason::LiveClaim(10078)));
}

#[test]
fn a_run_dir_whose_owner_is_running_is_kept() {
    let repo = tempfile::tempdir().unwrap();
    let dir = crate::run_target_dir::planned_for(repo.path(), "doctor", "x");
    crate::run_target_dir::provision(&dir, 4242).unwrap();
    backdate(&dir, 10);
    let fx = Fixture {
        alive: |pid| pid == 4242,
        ..Fixture::new()
    };
    assert_eq!(fx.eval(&dir), Err(KeepReason::OwnerAlive(4242)));
    assert!(Fixture::new().eval(&dir).is_ok(), "same dir, owner gone");
}

#[test]
fn a_configured_target_dir_is_never_an_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-shared");
    make_dir(&dir, 10);
    let real = crate::worktree_ops::cargo_target::realish(&dir);
    for protected in [real.clone(), real.join("wt")] {
        let fx = Fixture {
            protected: vec![protected],
            ..Fixture::new()
        };
        assert!(
            matches!(fx.eval(&dir), Err(KeepReason::ConfiguredTargetDir(_))),
            "{}",
            dir.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_wearing_the_name_is_kept_and_its_target_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    make_dir(outside.path(), 10);
    let link = tmp.path().join("cargo-target-link");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    assert_eq!(Fixture::new().eval(&link), Err(KeepReason::NotADirectory));
}

#[test]
fn a_file_wearing_the_name_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("cargo-target-file");
    std::fs::write(&file, "x").unwrap();
    backdate(&file, 10);
    assert_eq!(Fixture::new().eval(&file), Err(KeepReason::NotADirectory));
}

/// Integration: backdated candidates and decoys under temp scan roots.
fn populated() -> (tempfile::TempDir, Vec<ScanRoot>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    make_dir(outside.path(), 10);
    let repo = tmp.path().join("repo");
    let fake_tmp = tmp.path().join("tmp");
    make_dir(&repo.join(".loom/targets/doctor-1"), 10); // eligible
    make_dir(&repo.join(".loom/target-builder-10744"), 10); // eligible
    make_dir(&repo.join(".loom/worktrees"), 10); // wrong name
    make_dir(&fake_tmp.join("loom-target-10570-doctor"), 10); // eligible
    make_dir(&fake_tmp.join("cargo-target-fresh"), 0); // young
    make_dir(&fake_tmp.join("unrelated-target-1"), 10); // wrong name
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), fake_tmp.join("cargo-target-link")).unwrap();
    let roots = vec![
        ScanRoot {
            dir: repo.join(".loom/targets"),
            matcher: NameMatcher::RunDir,
        },
        ScanRoot {
            dir: repo.join(".loom"),
            matcher: NameMatcher::Prefixes(vec![LEGACY_REPO_PREFIX]),
        },
        ScanRoot {
            dir: fake_tmp,
            matcher: NameMatcher::Prefixes(TMP_PREFIXES.to_vec()),
        },
    ];
    (tmp, roots, outside)
}

fn run_fixture(
    roots: &[ScanRoot],
    dry_run: bool,
    free: fn(&Path) -> Option<bool>,
) -> TargetOrphanReport {
    let live = HashSet::new();
    let probes = Probes {
        open_handles: &free,
        owner_alive: &|_| false,
        live_issues: &live,
        protected: &[],
    };
    run_with(Path::new("/repo"), roots, Utc::now(), 3 * HOUR, dry_run, &probes)
}

#[test]
fn dry_run_reports_bytes_and_removes_nothing() {
    let (tmp, roots, _outside) = populated();
    let report = run_fixture(&roots, true, |_| Some(false));
    assert_eq!(report.eligible.len(), 3, "{report:?}");
    assert!(report.removed.is_empty());
    assert!(report.bytes_eligible() >= 3 * 1024);
    assert!(tmp.path().join("repo/.loom/targets/doctor-1").exists());
    assert!(report.log_line().contains("category=cargo_target_orphan"));
    assert!(report.log_line().contains("dry_run=true"));
}

#[test]
fn a_real_run_removes_only_eligible_dirs() {
    let (tmp, roots, outside) = populated();
    let report = run_fixture(&roots, false, |_| Some(false));
    assert_eq!(report.removed.len(), 3, "{report:?}");
    assert_eq!(report.bytes_freed(), report.bytes_eligible());
    let root = tmp.path();
    for gone in [
        "repo/.loom/targets/doctor-1",
        "repo/.loom/target-builder-10744",
        "tmp/loom-target-10570-doctor",
    ] {
        assert!(!root.join(gone).exists(), "{gone}");
    }
    for kept in [
        "repo/.loom/worktrees",
        "tmp/cargo-target-fresh",
        "tmp/unrelated-target-1",
    ] {
        assert!(root.join(kept).exists(), "{kept}");
    }
    assert!(outside.path().join("lib.rlib").exists(), "symlink target untouched");
    let line = report.log_line();
    assert!(line.contains("removed=3"), "{line}");
    assert!(line.contains(&format!("bytes_freed={}", report.bytes_freed())), "{line}");
}

#[test]
fn a_failing_liveness_probe_keeps_everything() {
    let (tmp, roots, _outside) = populated();
    let report = run_fixture(&roots, false, |_| None);
    assert!(report.removed.is_empty() && report.eligible.is_empty(), "{report:?}");
    assert!(report
        .kept
        .iter()
        .any(|(_, why)| *why == KeepReason::ProbeUnavailable));
    assert!(tmp.path().join("repo/.loom/targets/doctor-1").exists());
}

/// The production probe, against a real holder: a child whose cwd is inside
/// the dir. (The probe ignores this test's own pid, so the holder must be
/// another process.)
#[cfg(unix)]
#[test]
fn the_production_probe_sees_a_real_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-held");
    make_dir(&dir, 10);
    if production_open_handles(&dir).is_none() {
        eprintln!("no /proc and no lsof on this host; skipping");
        return;
    }
    assert_eq!(production_open_handles(&dir), Some(false));
    let mut holder = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(&dir)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let held = production_open_handles(&dir);
    let _ = holder.kill();
    let _ = holder.wait();
    assert_eq!(held, Some(true));
}

#[test]
#[serial_test::serial]
fn the_cooldown_defers_a_second_pass_for_the_same_repo_only() {
    reset_state_for_test();
    let now = Utc::now();
    let a = Path::new("/repo-a");
    assert_eq!(cooldown_reason(a, now, 1800), None);
    record_run(a, now);
    assert!(cooldown_reason(a, now + chrono::Duration::seconds(60), 1800).is_some());
    assert_eq!(cooldown_reason(a, now + chrono::Duration::seconds(1801), 1800), None);
    assert_eq!(cooldown_reason(Path::new("/repo-b"), now, 1800), None);
    reset_state_for_test();
}

#[test]
#[serial_test::serial]
fn a_disabled_pass_scans_nothing() {
    std::env::set_var(ENABLE_ENV, "0");
    let repo = tempfile::tempdir().unwrap();
    let report = run_for(repo.path());
    std::env::remove_var(ENABLE_ENV);
    assert!(!report.enabled);
    assert!(report.eligible.is_empty() && report.kept.is_empty());
}

#[test]
#[serial_test::serial]
fn config_and_env_resolution() {
    std::env::remove_var(MAX_AGE_HOURS_ENV);
    std::env::remove_var(MIN_INTERVAL_ENV);
    let none = TargetOrphanConfig::default();
    assert_eq!(resolve_max_age_hours(&none), DEFAULT_MAX_AGE_HOURS);
    assert_eq!(resolve_min_interval_secs(&none), DEFAULT_MIN_INTERVAL_SECS);
    let cfg = TargetOrphanConfig {
        enabled: Some(false),
        max_age_hours: Some(12),
        min_interval_secs: Some(60),
    };
    assert_eq!(resolve_max_age_hours(&cfg), 12);
    std::env::set_var(MAX_AGE_HOURS_ENV, "0");
    assert_eq!(resolve_max_age_hours(&cfg), 12, "a zero env value falls through");
    std::env::set_var(MAX_AGE_HOURS_ENV, "6");
    assert_eq!(resolve_max_age_hours(&cfg), 6);
    std::env::remove_var(MAX_AGE_HOURS_ENV);
}
