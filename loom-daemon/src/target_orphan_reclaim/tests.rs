//! Every test runs against temp dirs handed in as [`ScanRoot`]s. None calls
//! [`run_for`] / [`run_now`], which scan the real `/tmp` and `~/.cache`.

use super::*;
use std::time::{Duration, SystemTime};

const HOUR: i64 = 3600;
/// The production defaults: 3 h for legacy dirs, 10 min for a dead owner's.
const AGES: AgeGates = AgeGates {
    max_age_secs: 3 * HOUR,
    dead_owner_grace_secs: 600,
};

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

/// A run dir as `provision` leaves it (owner marker included), backdated.
fn make_run_dir(path: &Path, hours: u64) {
    make_dir(path, hours);
    std::fs::write(path.join(crate::run_target_dir::OWNER_FILE), "999999\n").unwrap();
    backdate(path, hours);
}

struct Fixture {
    free: fn(&Path) -> Option<bool>,
    alive: fn(u32) -> bool,
    identity: fn(u32) -> Option<String>,
    live: HashSet<u32>,
    protected: Vec<PathBuf>,
    gates: RootGates,
    euid: u32,
}

impl Fixture {
    fn new() -> Self {
        Self {
            free: |_| Some(false),
            alive: |_| false,
            identity: |_| None,
            live: HashSet::new(),
            protected: Vec::new(),
            gates: RootGates::default(),
            euid: current_euid(),
        }
    }

    fn eval(&self, path: &Path) -> Result<Candidate, KeepReason> {
        let probes = Probes {
            open_handles: &self.free,
            owner_alive: &self.alive,
            owner_identity: &self.identity,
            live_issues: &self.live,
            protected: &self.protected,
            euid: self.euid,
        };
        evaluate(path, self.gates, Utc::now(), AGES, &probes)
    }
}

const SHARED: RootGates = RootGates {
    require_owner_marker: false,
    require_own_uid: true,
};
const RUN_DIRS: RootGates = RootGates {
    require_owner_marker: true,
    require_own_uid: false,
};

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
fn run_dir_matcher_needs_the_role_dash_run_id_shape() {
    let m = NameMatcher::RunDir;
    assert!(m.matches("doctor-role-doctor-20261008T000000Z-ab12"));
    assert!(m.matches("worker-83920-1791490000"));
    assert!(m.matches("doctor-1"));
    for odd in [
        "",
        ".hidden",
        "a b",
        "Documents",
        "Desktop",
        "src",
        "-x",
        "x-",
        "_a-b",
        "a--",
    ] {
        assert!(!m.matches(odd), "{odd:?}");
    }
    // What the runner and the spawn actually produce always matches.
    for (role, id) in [
        ("doctor", "role-doctor-20261008T000000Z-ab12cdef"),
        ("my role", "1-2"),
    ] {
        let dir = crate::run_target_dir::planned_for(Path::new("/r"), role, id);
        assert!(m.matches(dir.file_name().unwrap().to_str().unwrap()), "{}", dir.display());
    }
}

/// Judge finding 3 (#11013): on a shared root a bare prefix is not enough. A
/// human's own `CARGO_TARGET_DIR=~/.cache/cargo-target-shared` is invisible
/// to a launchd/systemd daemon and must never be a candidate.
#[test]
fn the_shared_root_matcher_takes_agent_shaped_names_only() {
    let m = NameMatcher::AgentShaped(TMP_PREFIXES.to_vec());
    for agent in [
        "cargo-target-issue-10078",
        "cargo-target-review-9745",
        "cargo-target-pr-11013",
        "cargo-target-builder-10744",
        "cargo-target-doctor-8370",
        "cargo-target-judge-1",
        "loom-target-10570-doctor",
        "loom-target-doctor-10570",
        "loom-target-issue-8370-judge",
    ] {
        assert!(m.matches(agent), "{agent}");
    }
    for human in [
        "cargo-target-x",
        "cargo-target-shared",
        "cargo-target-",
        "cargo-target-1",
        "cargo-target-issue",
        "cargo-target-issue-",
        "cargo-target-issue-x",
        "cargo-target-issue-10078-mine",
        "cargo-target-my-issue-3",
        "cargo-target-release",
        "cargo-target-doctor-99999999999",
        "loom-target-x",
        "loom-target-2026",
        "my-cargo-target-issue-1",
        "cargo-targets",
    ] {
        assert!(!m.matches(human), "{human}");
    }
    // `$TMPDIR` and `~/.cache` know only the `cargo-target-` prefix.
    let cache = NameMatcher::AgentShaped(vec![CARGO_TARGET_PREFIX]);
    assert!(cache.matches("cargo-target-issue-1"));
    assert!(!cache.matches("loom-target-issue-1"));
    assert!(!cache.matches("cargo-target-x"));
}

#[test]
fn scan_roots_cover_exactly_the_known_prefixes() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tmpdir = tempfile::tempdir().unwrap();
    let roots = scan_roots(repo.path(), Some(home.path()), Some(tmpdir.path()));
    let find = |dir: PathBuf| {
        let root = roots.iter().find(|r| r.dir == dir).unwrap();
        (root.matcher.clone(), root.scope)
    };
    let agent = |prefixes: &[&'static str]| NameMatcher::AgentShaped(prefixes.to_vec());
    assert_eq!(find(repo.path().join(".loom/targets")), (NameMatcher::RunDir, RootScope::Repo));
    assert_eq!(
        find(repo.path().join(".loom")),
        (NameMatcher::Prefixes(vec!["target-"]), RootScope::Repo)
    );
    assert_eq!(
        find(PathBuf::from("/tmp")),
        (agent(&["loom-target-", "cargo-target-"]), RootScope::Shared)
    );
    assert_eq!(
        find(tmpdir.path().to_path_buf()),
        (agent(&["cargo-target-"]), RootScope::Shared)
    );
    assert_eq!(find(home.path().join(".cache")), (agent(&["cargo-target-"]), RootScope::Shared));
    assert_eq!(roots.len(), 5);
    // Every root outside the repo is shared, so it gets the ownership gate
    // and never the bare-prefix matcher.
    for root in &roots {
        let inside = root.dir.starts_with(repo.path());
        assert_eq!(root.scope == RootScope::Repo, inside, "{root:?}");
        assert_eq!(root.gates().require_own_uid, !inside, "{root:?}");
        assert_eq!(matches!(root.matcher, NameMatcher::AgentShaped(_)), !inside, "{root:?}");
    }
    assert!(roots[0].gates().require_owner_marker);
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

/// The three production root shapes, rebased onto a temp repo and a temp
/// stand-in for `/tmp`. Never the real `/tmp`, `$TMPDIR` or `~/.cache`.
fn roots_for(repo: &Path, fake_tmp: &Path) -> Vec<ScanRoot> {
    vec![
        ScanRoot {
            dir: repo.join(".loom/targets"),
            matcher: NameMatcher::RunDir,
            scope: RootScope::Repo,
        },
        ScanRoot {
            dir: repo.join(".loom"),
            matcher: NameMatcher::Prefixes(vec![LEGACY_REPO_PREFIX]),
            scope: RootScope::Repo,
        },
        ScanRoot {
            dir: fake_tmp.to_path_buf(),
            matcher: NameMatcher::AgentShaped(TMP_PREFIXES.to_vec()),
            scope: RootScope::Shared,
        },
    ]
}

/// Integration: backdated candidates and decoys under temp scan roots.
fn populated() -> (tempfile::TempDir, Vec<ScanRoot>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    make_dir(outside.path(), 10);
    let repo = tmp.path().join("repo");
    let fake_tmp = tmp.path().join("tmp");
    make_run_dir(&repo.join(".loom/targets/doctor-1"), 10); // eligible
    make_dir(&repo.join(".loom/targets/doctor-2"), 10); // no owner marker
    make_dir(&repo.join(".loom/target-builder-10744"), 10); // eligible
    make_dir(&repo.join(".loom/worktrees"), 10); // wrong name
    make_dir(&fake_tmp.join("loom-target-10570-doctor"), 10); // eligible
    make_dir(&fake_tmp.join("cargo-target-issue-7"), 0); // young
    make_dir(&fake_tmp.join("cargo-target-shared"), 10); // a human's name
    make_dir(&fake_tmp.join("unrelated-target-1"), 10); // wrong name
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), fake_tmp.join("cargo-target-issue-8")).unwrap();
    let roots = roots_for(&repo, &fake_tmp);
    (tmp, roots, outside)
}

fn run_fixture_as(
    repo: &Path,
    roots: &[ScanRoot],
    dry_run: bool,
    free: fn(&Path) -> Option<bool>,
    euid: u32,
) -> TargetOrphanReport {
    let live = HashSet::new();
    let probes = Probes {
        open_handles: &free,
        owner_alive: &|_| false,
        owner_identity: &|_| None,
        live_issues: &live,
        protected: &[],
        euid,
    };
    run_with(repo, roots, Utc::now(), AGES, dry_run, &probes)
}

/// [`populated`]'s roots: the repo is the parent of the first root's `.loom`.
fn run_fixture(
    roots: &[ScanRoot],
    dry_run: bool,
    free: fn(&Path) -> Option<bool>,
) -> TargetOrphanReport {
    let repo = roots[1].dir.parent().unwrap().to_path_buf();
    run_fixture_as(&repo, roots, dry_run, free, current_euid())
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
        "repo/.loom/targets/doctor-2",
        "repo/.loom/worktrees",
        "tmp/cargo-target-issue-7",
        "tmp/cargo-target-shared",
        "tmp/unrelated-target-1",
    ] {
        assert!(root.join(kept).exists(), "{kept}");
    }
    assert!(report.refused_roots.is_empty(), "{report:?}");
    assert!(report
        .kept
        .contains(&(root.join("repo/.loom/targets/doctor-2"), KeepReason::NoOwnerMarker)));
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
        dead_owner_grace_minutes: None,
    };
    assert_eq!(resolve_max_age_hours(&cfg), 12);
    std::env::set_var(MAX_AGE_HOURS_ENV, "0");
    assert_eq!(resolve_max_age_hours(&cfg), 12, "a zero env value falls through");
    std::env::set_var(MAX_AGE_HOURS_ENV, "6");
    assert_eq!(resolve_max_age_hours(&cfg), 6);
    std::env::remove_var(MAX_AGE_HOURS_ENV);
}

// ============================================================================
// Judge finding 1 (#11013): a symlinked scan root must not reach outside the
// repo, and `.loom/targets` must not accept an arbitrary name.
// ============================================================================

/// A stand-in for `$HOME`: innocent, old, unopened dirs with plain names, one
/// of which is even run-dir shaped and carries an owner marker.
fn innocent_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    for name in ["Documents", "Desktop", "my-project", "target-archive"] {
        make_dir(&home.path().join(name), 100);
    }
    make_run_dir(&home.path().join("doctor-1"), 100);
    home
}

fn assert_untouched(home: &Path) {
    for name in [
        "Documents",
        "Desktop",
        "my-project",
        "target-archive",
        "doctor-1",
    ] {
        assert!(home.join(name).join("lib.rlib").is_file(), "{name} must survive");
    }
}

#[cfg(unix)]
#[test]
fn a_symlinked_targets_root_is_refused_and_nothing_behind_it_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = innocent_home();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    std::os::unix::fs::symlink(home.path(), repo.join(".loom/targets")).unwrap();
    let roots = roots_for(&repo, &tmp.path().join("tmp"));
    let report = run_fixture_as(&repo, &roots, false, |_| Some(false), current_euid());
    assert!(report.eligible.is_empty() && report.removed.is_empty(), "{report:?}");
    assert!(report.kept.is_empty(), "a refused root is not even listed: {report:?}");
    assert_eq!(report.refused_roots.len(), 1, "{report:?}");
    assert_eq!(report.refused_roots[0].0, repo.join(".loom/targets"));
    assert!(report.refused_roots[0].1.contains("is a symlink"), "{report:?}");
    assert_untouched(home.path());
    assert!(repo.join(".loom/targets").is_symlink(), "the link itself stays");
}

/// The symlink is one level up: `.loom/targets` is a real dir, but only on
/// the far side of a symlinked `.loom`. Both repo roots are refused.
#[cfg(unix)]
#[test]
fn a_symlink_in_any_component_below_the_repo_root_refuses_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    make_run_dir(&elsewhere.path().join("targets/doctor-1"), 100);
    make_dir(&elsewhere.path().join("target-builder-1"), 100);
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), repo.join(".loom")).unwrap();
    let roots = roots_for(&repo, &tmp.path().join("tmp"));
    for root in &roots[..2] {
        let why = vet_root(&repo, root).unwrap_err();
        assert!(why.contains(".loom is a symlink"), "{}: {why}", root.dir.display());
    }
    let report = run_fixture_as(&repo, &roots, false, |_| Some(false), current_euid());
    assert!(report.removed.is_empty() && report.eligible.is_empty(), "{report:?}");
    assert_eq!(report.refused_roots.len(), 2, "{report:?}");
    assert!(elsewhere.path().join("targets/doctor-1/lib.rlib").is_file());
    assert!(elsewhere.path().join("target-builder-1/lib.rlib").is_file());
}

#[test]
fn vet_root_accepts_real_dirs_and_refuses_everything_else() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".loom/targets")).unwrap();
    let roots = roots_for(&repo, tmp.path());
    assert_eq!(vet_root(&repo, &roots[0]), Ok(()));
    assert_eq!(vet_root(&repo, &roots[1]), Ok(()));
    assert_eq!(vet_root(&repo, &roots[2]), Ok(()), "a shared root is vetted per child");
    // A repo-scoped root that is not under the repo root at all.
    let stray = ScanRoot {
        dir: tmp.path().to_path_buf(),
        matcher: NameMatcher::RunDir,
        scope: RootScope::Repo,
    };
    assert!(vet_root(&repo, &stray)
        .unwrap_err()
        .contains("not under the repo root"));
    // `..` cannot be used to climb back out.
    let climbing = ScanRoot {
        dir: repo.join(".loom/../.."),
        ..stray.clone()
    };
    assert!(vet_root(&repo, &climbing).is_err());
    // The repo root itself is never a scan root.
    let whole_repo = ScanRoot {
        dir: repo.clone(),
        ..stray
    };
    assert!(vet_root(&repo, &whole_repo).is_err());
    // A missing root is refused quietly (no `refused_roots` entry).
    let empty = tmp.path().join("empty-repo");
    std::fs::create_dir_all(&empty).unwrap();
    let roots = roots_for(&empty, tmp.path());
    let report = run_fixture_as(&empty, &roots[..2], false, |_| Some(false), current_euid());
    assert!(report.refused_roots.is_empty(), "{report:?}");
}

/// Even with a sound root, `.loom/targets` holds only what `provision` made.
#[test]
fn a_dir_without_the_owner_marker_is_never_a_run_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("doctor-1");
    make_dir(&plain, 100);
    let fx = Fixture {
        gates: RUN_DIRS,
        ..Fixture::new()
    };
    assert_eq!(fx.eval(&plain), Err(KeepReason::NoOwnerMarker));
    // A marker that is not a pid, or is a symlink, is not a marker.
    let marker = plain.join(crate::run_target_dir::OWNER_FILE);
    std::fs::write(&marker, "not-a-pid").unwrap();
    backdate(&plain, 100);
    assert_eq!(fx.eval(&plain), Err(KeepReason::NoOwnerMarker));
    #[cfg(unix)]
    {
        std::fs::remove_file(&marker).unwrap();
        let real = tmp.path().join("pidfile");
        std::fs::write(&real, "4242").unwrap();
        std::os::unix::fs::symlink(&real, &marker).unwrap();
        assert_eq!(fx.eval(&plain), Err(KeepReason::NoOwnerMarker));
        std::fs::remove_file(&marker).unwrap();
    }
    std::fs::write(&marker, "4242\n").unwrap();
    backdate(&plain, 100);
    assert!(fx.eval(&plain).is_ok(), "the real marker makes it a run dir");
}

// ============================================================================
// Judge finding 2 (#11013): shared roots need an ownership gate.
// ============================================================================

#[test]
fn only_our_own_uid_owns_a_shared_root_candidate() {
    assert!(owned_by_us(501, 501));
    assert!(!owned_by_us(502, 501));
    assert!(!owned_by_us(0, 501), "root's dir is not ours");
    assert!(!owned_by_us(501, 0), "and running as root makes nobody's dir ours");
}

/// A real foreign-uid dir needs root to create, so the gate is driven from
/// the other side: the same dir, evaluated as if the daemon were another
/// user. Every other gate would pass (old, free, no claim).
#[cfg(unix)]
#[test]
fn another_users_dir_under_a_shared_root_is_kept() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cargo-target-issue-10078");
    make_dir(&dir, 100);
    let owner = std::fs::metadata(&dir).unwrap().uid();
    for daemon_euid in [owner.wrapping_add(1), 0]
        .into_iter()
        .filter(|&u| u != owner)
    {
        let fx = Fixture {
            gates: SHARED,
            euid: daemon_euid,
            ..Fixture::new()
        };
        assert_eq!(fx.eval(&dir), Err(KeepReason::ForeignOwner { uid: owner }));
    }
    let ours = Fixture {
        gates: SHARED,
        euid: owner,
        ..Fixture::new()
    };
    assert!(ours.eval(&dir).is_ok(), "our own dir still goes");
    // Inside the repo the gate does not apply: a root daemon still collects
    // the repo owner's dirs there.
    let in_repo = Fixture {
        euid: owner.wrapping_add(1),
        ..Fixture::new()
    };
    assert!(in_repo.eval(&dir).is_ok());
}

#[cfg(unix)]
#[test]
fn a_pass_running_as_another_user_removes_nothing_from_a_shared_root() {
    let (tmp, roots, _outside) = populated();
    let repo = tmp.path().join("repo");
    let report =
        run_fixture_as(&repo, &roots, false, |_| Some(false), current_euid().wrapping_add(1));
    let shared = tmp.path().join("tmp/loom-target-10570-doctor");
    assert!(shared.join("lib.rlib").is_file(), "{report:?}");
    assert!(report
        .kept
        .iter()
        .any(|(p, why)| *p == shared && matches!(why, KeepReason::ForeignOwner { .. })));
    assert_eq!(report.removed.len(), 2, "only the two repo-internal dirs: {report:?}");
    assert!(report.removed.iter().all(|c| c.path.starts_with(&repo)), "{report:?}");
}

// ============================================================================
// #11031: a dead owner's run dir goes after a grace of minutes, not hours.
// ============================================================================

/// Set the mtime of `path` and everything under it to `secs` ago.
fn backdate_secs(path: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if !entry.file_type().unwrap().is_symlink() {
                backdate_secs(&entry.path(), secs);
            }
        }
    }
    std::fs::File::open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

fn run_dir_fx() -> Fixture {
    Fixture {
        gates: RUN_DIRS,
        ..Fixture::new()
    }
}

#[test]
fn a_dead_owners_run_dir_goes_after_the_grace_not_the_max_age() {
    let repo = tempfile::tempdir().unwrap();
    let dir = crate::run_target_dir::planned_for(repo.path(), "sweep-lifecycle", "723062-1");
    crate::run_target_dir::provision(&dir, 4242).unwrap();
    std::fs::write(dir.join("lib.rlib"), vec![0u8; 1024]).unwrap();
    backdate_secs(&dir, 5 * 60);
    assert!(
        matches!(run_dir_fx().eval(&dir), Err(KeepReason::Young { .. })),
        "inside the grace a straggling write may still land"
    );
    backdate_secs(&dir, 11 * 60);
    assert!(run_dir_fx().eval(&dir).is_ok(), "11 min after the last write, owner gone");

    // An unmarked legacy location of the same age still waits the 3 h.
    let legacy = repo.path().join(".loom/target-builder-10744");
    make_dir(&legacy, 0);
    backdate_secs(&legacy, 11 * 60);
    assert!(matches!(Fixture::new().eval(&legacy), Err(KeepReason::Young { .. })));
}

#[test]
fn a_live_owners_run_dir_is_never_touched_however_old() {
    let repo = tempfile::tempdir().unwrap();
    let dir = crate::run_target_dir::planned_for(repo.path(), "sweep-lifecycle", "4242-1");
    crate::run_target_dir::provision(&dir, 4242).unwrap();
    std::fs::write(dir.join(crate::run_target_dir::owner::OWNER_START_FILE), "t1\n").unwrap();
    backdate(&dir, 100);
    let alive = Fixture {
        alive: |pid| pid == 4242,
        identity: |_| Some("t1".to_string()),
        ..run_dir_fx()
    };
    assert_eq!(alive.eval(&dir), Err(KeepReason::OwnerAlive(4242)));
    let unknown_identity = Fixture {
        identity: |_| None,
        ..alive
    };
    assert_eq!(
        unknown_identity.eval(&dir),
        Err(KeepReason::OwnerAlive(4242)),
        "an identity that cannot be read is a keep"
    );
}

#[test]
fn a_reused_owner_pid_does_not_keep_the_dir() {
    let repo = tempfile::tempdir().unwrap();
    let dir = crate::run_target_dir::planned_for(repo.path(), "sweep-lifecycle", "4242-1");
    crate::run_target_dir::provision(&dir, 4242).unwrap();
    std::fs::write(dir.join(crate::run_target_dir::owner::OWNER_START_FILE), "t1\n").unwrap();
    backdate_secs(&dir, 11 * 60);
    let reused = Fixture {
        alive: |pid| pid == 4242,
        identity: |_| Some("t2".to_string()),
        ..run_dir_fx()
    };
    assert!(reused.eval(&dir).is_ok(), "pid 4242 now belongs to another process");
}

/// The production probes against a real live process (this test) wearing the
/// marker's pid: kept with its own identity, collected with a foreign one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn production_identity_sees_through_pid_reuse() {
    let repo = tempfile::tempdir().unwrap();
    let me = std::process::id();
    let dir = crate::run_target_dir::planned_for(repo.path(), "sweep-lifecycle", "me-1");
    crate::run_target_dir::provision(&dir, me).unwrap();
    backdate_secs(&dir, 11 * 60);
    let fx = Fixture {
        alive: crate::live_claim::pid_is_live_process,
        identity: crate::run_target_dir::owner::process_start_token,
        ..run_dir_fx()
    };
    assert_eq!(fx.eval(&dir), Err(KeepReason::OwnerAlive(me)));
    std::fs::write(dir.join(crate::run_target_dir::owner::OWNER_START_FILE), "other\n").unwrap();
    backdate_secs(&dir, 11 * 60);
    assert!(fx.eval(&dir).is_ok());
}

#[test]
fn a_pass_removes_a_dead_owners_run_dir_minutes_after_its_last_write() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let dead = repo.join(".loom/targets/sweep-lifecycle-723062-1791560640");
    let fresh = repo.join(".loom/targets/sweep-lifecycle-3487011-1791557602");
    for dir in [&dead, &fresh] {
        std::fs::create_dir_all(dir.join("debug")).unwrap();
        std::fs::write(dir.join("debug/lib.rlib"), vec![0u8; 1024]).unwrap();
        std::fs::write(dir.join(crate::run_target_dir::OWNER_FILE), "999999\n").unwrap();
    }
    backdate_secs(&dead, 15 * 60);
    backdate_secs(&fresh, 2 * 60);
    let roots = roots_for(&repo, &tmp.path().join("tmp"));
    let report = run_fixture_as(&repo, &roots[..1], false, |_| Some(false), current_euid());
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert!(!dead.exists());
    assert!(fresh.join("debug/lib.rlib").exists(), "still inside its grace");
}

#[test]
#[serial_test::serial]
fn dead_owner_grace_resolution() {
    std::env::remove_var(DEAD_OWNER_GRACE_ENV);
    std::env::remove_var(MAX_AGE_HOURS_ENV);
    let none = TargetOrphanConfig::default();
    assert_eq!(resolve_dead_owner_grace_minutes(&none), DEFAULT_DEAD_OWNER_GRACE_MINUTES);
    assert_eq!(
        resolve_ages(&none),
        AgeGates {
            max_age_secs: 3 * HOUR,
            dead_owner_grace_secs: 600
        }
    );
    let cfg = TargetOrphanConfig {
        dead_owner_grace_minutes: Some(30),
        ..TargetOrphanConfig::default()
    };
    assert_eq!(resolve_dead_owner_grace_minutes(&cfg), 30);
    std::env::set_var(DEAD_OWNER_GRACE_ENV, "0");
    assert_eq!(resolve_dead_owner_grace_minutes(&cfg), 30, "a zero env value falls through");
    std::env::set_var(DEAD_OWNER_GRACE_ENV, "5");
    assert_eq!(resolve_dead_owner_grace_minutes(&cfg), 5);
    std::env::remove_var(DEAD_OWNER_GRACE_ENV);
    assert_eq!(
        AgeGates::from_units(1, 600).dead_owner_grace_secs,
        HOUR,
        "the grace never exceeds the max age"
    );
}

#[test]
fn the_config_block_reads_the_grace() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".loom")).unwrap();
    std::fs::write(
        repo.path().join(".loom/config.json"),
        r#"{"autonomous":{"worktreeReaper":{"targetOrphanReclaim":{"deadOwnerGraceMinutes":20}}}}"#,
    )
    .unwrap();
    assert_eq!(read_config(repo.path()).dead_owner_grace_minutes, Some(20));
}
