use super::*;
use serial_test::serial;
use std::process::{Command, Stdio};
use std::time::SystemTime;

const HOUR: i64 = 3600;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=loom-test",
            "-c",
            "user.email=loom-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// A real repository with one commit, repacked so `objects/pack` holds a
/// genuine `pack-*.pack`/`.idx` pair alongside whatever debris a test adds.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README"), b"hello").unwrap();
    git(root, &["add", "README"]);
    git(root, &["commit", "-q", "-m", "init"]);
    git(root, &["repack", "-a", "-d", "-q"]);
    tmp
}

fn objects(root: &Path) -> PathBuf {
    root.join(".git").join("objects")
}

fn write_aged(path: &Path, bytes: usize, age_secs: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![0u8; bytes]).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
        .unwrap();
}

fn real_packs(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(objects(root).join("pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("pack-"))
        })
        .collect();
    v.sort();
    v
}

fn idle(_: &[PathBuf]) -> GitLiveness {
    GitLiveness::Idle
}

fn inputs(dry_run: bool) -> PassInputs {
    PassInputs {
        enabled: true,
        grace_secs: HOUR,
        dry_run,
        now: Utc::now(),
    }
}

// ===================================================================
// Pure gates
// ===================================================================

#[test]
fn age_gate_young_old_boundary_and_skew() {
    assert!(!is_reclaimable_age(59 * 60, HOUR), "younger than grace is kept");
    assert!(is_reclaimable_age(HOUR, HOUR), "exactly at grace is reclaimable");
    assert!(is_reclaimable_age(3 * HOUR, HOUR));
    assert!(!is_reclaimable_age(-5, 0), "a future mtime is never reclaimable");
}

#[test]
fn shape_gate_accepts_only_the_two_debris_shapes() {
    let o = Path::new("/r/.git/objects");
    assert!(is_tmp_debris_path(o, &o.join("pack/tmp_pack_AbC123")));
    assert!(is_tmp_debris_path(o, &o.join("ab/tmp_obj_XyZ")));
    assert!(is_tmp_debris_path(o, &o.join("0F/tmp_obj_1")));

    for rejected in [
        "pack/pack-0123.pack",
        "pack/pack-0123.idx",
        "pack/pack-0123.keep",
        "pack/tmp_idx_abc",
        "pack/tmp_pack_",
        "pack/sub/tmp_pack_x",
        "tmp_pack_x",
        "info/tmp_pack_x",
        "info/packs",
        "zz/tmp_obj_x",
        "abc/tmp_obj_x",
        "ab/tmp_pack_x",
        "ab/0123456789abcdef",
        "pack",
    ] {
        assert!(!is_tmp_debris_path(o, &o.join(rejected)), "{rejected} must be refused");
    }
    assert!(!is_tmp_debris_path(o, Path::new("/r/.git/refs/heads/tmp_pack_x")));
    assert!(!is_tmp_debris_path(o, Path::new("/elsewhere/objects/pack/tmp_pack_x")));
    assert!(!is_tmp_debris_path(o, &o.join("pack/../pack/tmp_pack_x")));
}

#[test]
fn lsof_parser_matches_cwd_or_open_file_under_a_root_and_ignores_self() {
    let roots = vec![PathBuf::from("/w/repo")];
    let out = "p10\ncgit\nfcwd\nn/home/u\nftxt\nn/usr/bin/git\n\
               p11\ncgit\nf3\nn/w/repo/.git/objects/pack/tmp_pack_q\n";
    assert!(parse_lsof_for_roots(out, &roots, 1)
        .unwrap()
        .contains("pid 11"));
    let cwd = "p12\ncgit\nfcwd\nn/w/repo/sub\n";
    assert!(parse_lsof_for_roots(cwd, &roots, 1).is_some());
    assert!(parse_lsof_for_roots(cwd, &roots, 12).is_none(), "own pid is ignored");
    let sibling = "p13\ncgit\nfcwd\nn/w/repo-other\n";
    assert!(parse_lsof_for_roots(sibling, &roots, 1).is_none(), "prefix is component-wise");
}

#[test]
fn cooldown_gate() {
    let now = Utc::now();
    assert!(!in_cooldown(None, now, 600));
    assert!(in_cooldown(Some(now - chrono::Duration::seconds(30)), now, 600));
    assert!(!in_cooldown(Some(now - chrono::Duration::seconds(601)), now, 600));
}

// ===================================================================
// Sweep against a real repository
// ===================================================================

#[test]
fn old_debris_is_removed_young_debris_and_real_packs_are_kept() {
    let repo = fixture_repo();
    let root = repo.path();
    let o = objects(root);
    let packs_before = real_packs(root);
    assert!(!packs_before.is_empty(), "fixture must contain a real pack");

    write_aged(&o.join("pack/tmp_pack_old"), 4096, 2 * 3600);
    write_aged(&o.join("ab/tmp_obj_old"), 100, 2 * 3600);
    write_aged(&o.join("pack/tmp_pack_young"), 4096, 10 * 60);
    write_aged(&o.join("cd/tmp_obj_young"), 100, 10 * 60);

    let common = resolve_common_dir(root).unwrap();
    let totals = sweep(&common, HOUR, Utc::now(), false);
    assert_eq!(totals.files, 2);
    assert_eq!(totals.bytes, 4196);
    assert_eq!(totals.kept_young, 2);
    assert!(!o.join("pack/tmp_pack_old").exists());
    assert!(!o.join("ab/tmp_obj_old").exists());
    assert!(o.join("pack/tmp_pack_young").exists(), "inside the grace period: kept");
    assert!(o.join("cd/tmp_obj_young").exists(), "inside the grace period: kept");
    assert_eq!(real_packs(root), packs_before, "real packs untouched");
}

#[test]
fn decoys_are_never_touched_even_when_ancient() {
    let repo = fixture_repo();
    let root = repo.path();
    let o = objects(root);
    let packs_before = real_packs(root);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("precious");
    std::fs::write(&target, b"keep me").unwrap();

    let decoys = [
        o.join("pack/tmp_idx_old"),
        o.join("pack/pack-decoy.keep"),
        o.join("info/tmp_pack_old"),
        o.join("tmp_pack_at_objects_root"),
        o.join("zz/tmp_obj_old"),
        o.join("abc/tmp_obj_old"),
        root.join(".git/refs/heads/tmp_pack_x"),
    ];
    for d in &decoys {
        write_aged(d, 10, 30 * 24 * 3600);
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, o.join("pack/tmp_pack_link")).unwrap();
    std::fs::create_dir_all(o.join("pack/tmp_pack_dir")).unwrap();

    // Evaluate a day in the future so every entry, including the symlink and
    // the directory, is far past the grace period: only the shape and
    // file-type gates stand between them and removal.
    let common = resolve_common_dir(root).unwrap();
    let totals = sweep(&common, HOUR, Utc::now() + chrono::Duration::days(1), false);
    assert_eq!(totals.files, 0, "no decoy is a candidate");
    for d in &decoys {
        assert!(d.exists(), "{} must survive", d.display());
    }
    #[cfg(unix)]
    assert!(o.join("pack/tmp_pack_link").symlink_metadata().is_ok(), "symlink kept");
    assert!(target.exists(), "symlink target kept");
    assert!(o.join("pack/tmp_pack_dir").is_dir(), "directory kept");
    for p in &packs_before {
        assert!(p.exists(), "real pack {} kept", p.display());
    }
}

#[cfg(unix)]
#[test]
fn symlinked_pack_dir_is_never_traversed() {
    let repo = fixture_repo();
    let root = repo.path();
    let o = objects(root);
    // Replace `objects/pack` with a symlink to an external store holding old debris.
    let outside = tempfile::tempdir().unwrap();
    let external_pack = outside.path().join("pack");
    std::fs::create_dir_all(&external_pack).unwrap();
    let victim = external_pack.join("tmp_pack_old");
    write_aged(&victim, 10, 30 * 24 * 3600);
    std::fs::remove_dir_all(o.join("pack")).unwrap();
    std::os::unix::fs::symlink(&external_pack, o.join("pack")).unwrap();

    let common = resolve_common_dir(root).unwrap();
    let totals = sweep(&common, HOUR, Utc::now() + chrono::Duration::days(1), false);
    assert_eq!(totals.files, 0, "nothing behind a symlinked pack dir is a candidate");
    assert!(victim.exists(), "external file must survive");
}

#[test]
fn fsck_connectivity_passes_after_reclaim() {
    let repo = fixture_repo();
    let root = repo.path();
    write_aged(&objects(root).join("pack/tmp_pack_old"), 1 << 16, 2 * 3600);
    write_aged(&objects(root).join("ab/tmp_obj_old"), 64, 2 * 3600);
    let report = run_pass(root, &inputs(false), &resolve_common_dir, &idle);
    assert_eq!(report.totals.files, 2);
    git(root, &["fsck", "--connectivity-only", "--no-progress"]);
}

#[test]
fn dry_run_reports_bytes_without_deleting() {
    let repo = fixture_repo();
    let root = repo.path();
    let old = objects(root).join("pack/tmp_pack_old");
    write_aged(&old, 2048, 2 * 3600);
    let report = run_pass(root, &inputs(true), &resolve_common_dir, &idle);
    assert!(report.skipped.is_none());
    assert_eq!(report.totals.files, 1);
    assert_eq!(report.totals.bytes, 2048);
    assert!(old.exists(), "dry run deletes nothing");
    assert!(report.log_line().contains("would_remove"));
}

#[test]
fn linked_worktree_shares_the_common_dir_and_is_watched() {
    let repo = fixture_repo();
    let root = repo.path();
    let wt = root.join("wt");
    git(root, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    let common = resolve_common_dir(&wt).unwrap();
    assert_eq!(common, objects(root).parent().unwrap().canonicalize().unwrap());
    let roots = liveness_roots(&wt, &common);
    assert!(roots.contains(&wt.canonicalize().unwrap()));
    assert!(roots.contains(&root.canonicalize().unwrap()) || roots.contains(&common));

    write_aged(&objects(root).join("pack/tmp_pack_old"), 10, 2 * 3600);
    let report = run_pass(&wt, &inputs(false), &resolve_common_dir, &idle);
    assert_eq!(report.totals.files, 1, "a worktree's pass reclaims the shared store");
}

// ===================================================================
// run_pass: liveness and resolution fail closed
// ===================================================================

#[test]
fn busy_or_unknown_liveness_skips_the_whole_repo() {
    let repo = fixture_repo();
    let root = repo.path();
    let old = objects(root).join("pack/tmp_pack_old");
    write_aged(&old, 10, 2 * 3600);

    let busy = |_: &[PathBuf]| GitLiveness::Busy("pid 1 (git) has cwd /x".to_string());
    let r = run_pass(root, &inputs(false), &resolve_common_dir, &busy);
    assert!(r.skipped.as_deref().unwrap().contains("live git process"));
    assert!(old.exists());

    let unknown = |_: &[PathBuf]| GitLiveness::Unknown("lsof missing".to_string());
    let r = run_pass(root, &inputs(false), &resolve_common_dir, &unknown);
    assert!(r.skipped.as_deref().unwrap().contains("failing closed"));
    assert!(old.exists());

    let unresolved = |_: &Path| None;
    let r = run_pass(root, &inputs(false), &unresolved, &idle);
    assert!(r.skipped.is_some());
    assert!(old.exists());

    let mut off = inputs(false);
    off.enabled = false;
    let r = run_pass(root, &off, &resolve_common_dir, &idle);
    assert!(r.skipped.is_some());
    assert!(old.exists());
}

#[test]
fn a_real_git_child_with_cwd_in_the_repo_is_detected_and_blocks_reclaim() {
    let repo = fixture_repo();
    let root = repo.path();
    let old = objects(root).join("pack/tmp_pack_old");
    write_aged(&old, 10, 2 * 3600);

    // `git cat-file --batch` blocks reading stdin, so it stays alive with
    // its cwd in the repository until we close stdin.
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let r = run_pass(root, &inputs(false), &resolve_common_dir, &probe_git_liveness);
    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();

    let reason = r.skipped.expect("a live git child must skip the repo");
    assert!(
        reason.contains("live git process") || reason.contains("failing closed"),
        "unexpected skip reason: {reason}"
    );
    assert!(old.exists(), "nothing removed while git is live");

    let roots = liveness_roots(root, &resolve_common_dir(root).unwrap());
    assert!(
        !matches!(probe_git_liveness(&roots), GitLiveness::Busy(_)),
        "once the child exits the repo is no longer busy"
    );
}

// ===================================================================
// Telemetry and the cooldown-honoring production entry
// ===================================================================

#[test]
fn log_line_carries_category_and_bytes() {
    let repo = fixture_repo();
    let root = repo.path();
    write_aged(&objects(root).join("pack/tmp_pack_old"), 3000, 2 * 3600);
    let r = run_pass(root, &inputs(false), &resolve_common_dir, &idle);
    let line = r.log_line();
    assert!(line.contains("category=git_tmp_pack"), "{line}");
    assert!(line.contains("bytes_freed=3000"), "{line}");
    assert!(line.contains("files=1"), "{line}");
}

#[test]
#[serial]
fn run_for_reclaims_once_then_honors_its_cooldown() {
    reset_state_for_test();
    std::env::remove_var(GIT_TMP_RECLAIM_ENABLE_ENV);
    std::env::remove_var(GIT_TMP_RECLAIM_GRACE_MINUTES_ENV);
    std::env::remove_var(GIT_TMP_RECLAIM_MIN_INTERVAL_ENV);
    let repo = fixture_repo();
    let root = repo.path();
    write_aged(&objects(root).join("pack/tmp_pack_a"), 10, 2 * 3600);

    let first = run_for(root);
    // The real liveness probe may legitimately fail closed on a host where a
    // foreign git process is unreadable; only assert removal when it ran.
    if first.skipped.is_none() {
        assert_eq!(first.totals.files, 1);
        assert!(!objects(root).join("pack/tmp_pack_a").exists());
    }

    write_aged(&objects(root).join("pack/tmp_pack_b"), 10, 2 * 3600);
    let second = run_for(root);
    assert!(second.skipped.as_deref().unwrap().contains("cooldown"));
    assert!(objects(root).join("pack/tmp_pack_b").exists());
    reset_state_for_test();
}

#[test]
#[serial]
fn config_resolution_defaults_and_grace_floor() {
    std::env::remove_var(GIT_TMP_RECLAIM_ENABLE_ENV);
    std::env::remove_var(GIT_TMP_RECLAIM_GRACE_MINUTES_ENV);
    std::env::remove_var(GIT_TMP_RECLAIM_MIN_INTERVAL_ENV);
    let d = GitTmpReclaimConfig::default();
    assert!(resolve_enabled(&d));
    assert_eq!(resolve_grace_minutes(&d), DEFAULT_GRACE_MINUTES);
    assert_eq!(resolve_min_interval_secs(&d), DEFAULT_MIN_INTERVAL_SECS);

    let c = GitTmpReclaimConfig {
        enabled: Some(false),
        grace_minutes: Some(0),
        min_interval_secs: Some(60),
    };
    assert!(!resolve_enabled(&c));
    assert_eq!(resolve_grace_minutes(&c), MIN_GRACE_MINUTES, "grace never drops below floor");
    assert_eq!(resolve_min_interval_secs(&c), 60);

    std::env::set_var(GIT_TMP_RECLAIM_GRACE_MINUTES_ENV, "90");
    assert_eq!(resolve_grace_minutes(&c), 90);
    std::env::remove_var(GIT_TMP_RECLAIM_GRACE_MINUTES_ENV);
}

#[test]
fn clean_section_honors_dry_run_then_removes() {
    let repo = fixture_repo();
    let root = repo.path();
    let old = objects(root).join("pack/tmp_pack_old");
    write_aged(&old, 512, 2 * 3600);

    let dry = clean_section(root, true);
    assert!(old.exists(), "`loom-daemon clean --dry-run` deletes nothing");
    if dry.skipped.is_none() {
        assert_eq!((dry.totals.files, dry.totals.bytes), (1, 512));
    }

    let real = clean_section(root, false);
    // The real liveness probe may fail closed on a host with an unreadable
    // foreign git process; removal is only asserted when it answered.
    if real.skipped.is_none() {
        assert_eq!(real.totals.files, 1);
        assert!(!old.exists());
    }
}
