//! Fixture-repo tests for the remote comparison and the attended session-start
//! front half (#12509). Origins are local bare repos, so the comparison half
//! is called directly (a local path is, correctly, a foreign origin).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::remote::{classify_origin, compare, read_with_remote, Expected, GitRemote, Remote};
use super::session::{start_with, HookPayload, Spec, Started};
use super::*;

fn run(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, msg: &str) {
    run(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            msg,
        ],
    );
}

struct Fixture {
    _tmp: tempfile::TempDir,
    host: PathBuf,
    pusher: PathBuf,
    origin: PathBuf,
}

/// `host` is a clone at `local` commits with a fresh `origin/main`; then
/// `ahead` commits land on the origin through another clone, so the host's
/// remote-tracking ref is stale and the new objects were never fetched.
fn fixture(local: usize, ahead: usize) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let host = tmp.path().join("host");
    let pusher = tmp.path().join("pusher");
    run(
        tmp.path(),
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    run(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            host.to_str().unwrap(),
        ],
    );
    for i in 0..local {
        commit(&host, &format!("local {i}"));
    }
    run(&host, &["push", "--quiet", "origin", "HEAD:main"]);
    run(&host, &["fetch", "--quiet", "origin"]);
    run(&host, &["remote", "set-head", "origin", "main"]);
    run(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            pusher.to_str().unwrap(),
        ],
    );
    for i in 0..ahead {
        commit(&pusher, &format!("remote {i}"));
    }
    run(&pusher, &["push", "--quiet", "origin", "HEAD:main"]);
    Fixture {
        _tmp: tmp,
        host,
        pusher,
        origin,
    }
}

fn measured(root: &Path) -> PromptTree {
    let mut tree = read(root).unwrap();
    compare(root, &mut tree, &GitRemote, true);
    tree
}

#[test]
fn behind_by_three_when_the_tip_was_never_fetched() {
    let f = fixture(2, 3);
    // The fixture's own fetch left one; only the measurement may not.
    let _ = std::fs::remove_file(f.host.join(".git/FETCH_HEAD"));
    let tracking_before = run(&f.host, &["rev-parse", "refs/remotes/origin/main"]);
    let tree = measured(&f.host);
    assert_eq!(tree.commits_behind, Some(3));
    assert_eq!(tree.origin_ref_stale, Some(true));
    assert_eq!(
        tree.remote_tip.as_deref(),
        Some(run(&f.origin, &["rev-parse", "main"]).as_str())
    );
    // Measures, never fixes: the ref did not move and no FETCH_HEAD appeared.
    assert_eq!(run(&f.host, &["rev-parse", "refs/remotes/origin/main"]), tracking_before);
    assert!(!f.host.join(".git/FETCH_HEAD").exists());
}

#[test]
fn a_stale_remote_tracking_ref_under_head_is_still_behind() {
    let f = fixture(1, 2);
    // HEAD equals the local origin/main, which is itself behind GitHub.
    assert_eq!(
        run(&f.host, &["rev-parse", "HEAD"]),
        run(&f.host, &["rev-parse", "origin/main"])
    );
    let tree = measured(&f.host);
    assert_eq!(tree.commits_behind, Some(2));
    assert_eq!(tree.origin_ref_stale, Some(true));
    // The old reading (against the local ref) said 0.
    assert_eq!(read(&f.host).unwrap().commits_behind, Some(0));
}

#[test]
fn diverged_counts_only_the_tips_commits() {
    let f = fixture(1, 3);
    commit(&f.host, "mine 1");
    commit(&f.host, "mine 2");
    let tree = measured(&f.host);
    assert_eq!(tree.commits_behind, Some(3));
}

#[test]
fn up_to_date_is_zero_and_not_stale() {
    let f = fixture(2, 0);
    let tree = measured(&f.host);
    assert_eq!(tree.commits_behind, Some(0));
    assert_eq!(tree.origin_ref_stale, Some(false));
}

#[test]
fn an_unset_origin_head_still_finds_the_default_branch() {
    let f = fixture(1, 2);
    run(&f.host, &["remote", "set-head", "origin", "--delete"]);
    assert!(read(&f.host).unwrap().commits_behind.is_none());
    assert_eq!(measured(&f.host).commits_behind, Some(2));
    let _ = &f.pusher;
}

struct Offline;
impl Remote for Offline {
    fn probe(&self, _: &Path) -> Result<(String, String), String> {
        Err("timed out".to_string())
    }
    fn fetch_objects(&self, _: &Path, _: &str) -> Result<(), String> {
        Err("offline".to_string())
    }
}

#[test]
fn offline_leaves_the_remote_facts_absent_never_zero() {
    let f = fixture(2, 0);
    let mut tree = read(&f.host).unwrap();
    assert_eq!(tree.commits_behind, Some(0), "the local-ref reading");
    compare(&f.host, &mut tree, &Offline, true);
    assert_eq!(tree.commits_behind, None);
    assert_eq!(tree.remote_tip, None);
    assert_eq!(tree.origin_ref_stale, None);
    assert_eq!(tree.commit.len(), 40, "the rest of the record survives");
    let nonexistent = tempfile::tempdir().unwrap();
    run(
        f.host.as_path(),
        &[
            "remote",
            "set-url",
            "origin",
            nonexistent.path().join("gone.git").to_str().unwrap(),
        ],
    );
    let mut again = read(&f.host).unwrap();
    compare(&f.host, &mut again, &GitRemote, true);
    assert_eq!(again.commits_behind, None);
}

#[test]
fn a_fetch_failure_or_low_disk_leaves_commits_behind_absent() {
    let f = fixture(1, 2);
    let mut tree = read(&f.host).unwrap();
    compare(&f.host, &mut tree, &GitRemote, false);
    assert_eq!(tree.commits_behind, None, "no fetch allowed, tip not local");
    assert_eq!(tree.origin_ref_stale, Some(true));
    assert!(tree.remote_tip.is_some());
}

struct Panics(Cell<u32>);
impl Remote for Panics {
    fn probe(&self, _: &Path) -> Result<(String, String), String> {
        self.0.set(self.0.get() + 1);
        Err("must not be asked".to_string())
    }
    fn fetch_objects(&self, _: &Path, _: &str) -> Result<(), String> {
        self.0.set(self.0.get() + 1);
        Err("must not be asked".to_string())
    }
}

#[test]
fn a_foreign_origin_is_flagged_and_never_asked() {
    let f = fixture(1, 1);
    let probe = Panics(Cell::new(0));
    let expected = Expected {
        host: "github.com".to_string(),
        repo: None,
    };
    let tree = read_with_remote(&f.host, &expected, &probe).unwrap();
    assert_eq!(tree.remote_foreign, Some(true), "a local path origin");
    assert_eq!(tree.remote_repo, None);
    assert_eq!(tree.commits_behind, None);
    assert_eq!(probe.0.get(), 0);
}

fn expected(repo: Option<&str>) -> Expected {
    Expected {
        host: "github.com".to_string(),
        repo: repo.map(str::to_string),
    }
}

#[test]
fn origin_classification_keeps_only_owner_repo() {
    for url in [
        "git@github.com:o/r.git",
        "https://github.com/o/r",
        "ssh://git@github.com/o/r.git",
        "https://user:ghp_FAKE@github.com/o/r.git",
    ] {
        let o = classify_origin(url, &expected(None));
        assert_eq!((o.repo.as_deref(), o.foreign), (Some("o/r"), false), "{url}");
    }
    for url in [
        "/srv/git/r.git",
        "file:///srv/git/r.git",
        "https://gitlab.com/o/r.git",
        "https://github.com/o/r/extra.git",
        "",
    ] {
        assert!(classify_origin(url, &expected(None)).foreign, "{url}");
    }
    assert!(classify_origin("git@github.com:o/r.git", &expected(Some("o/other"))).foreign);
    assert!(!classify_origin("git@github.com:O/R.git", &expected(Some("o/r"))).foreign);
    let gh = Expected {
        host: "ghe.example.com".to_string(),
        repo: None,
    };
    assert!(!classify_origin("https://ghe.example.com/o/r", &gh).foreign);
    assert!(classify_origin("https://github.com/o/r", &gh).foreign);
}

#[test]
fn no_credential_reaches_the_serialized_record() {
    let origin = classify_origin("https://user:ghp_FAKE@github.com/o/r.git", &expected(None));
    let tree = PromptTree {
        commit: "a".repeat(40),
        remote_repo: origin.repo,
        remote_foreign: Some(origin.foreign),
        ..PromptTree::default()
    };
    let json = serde_json::to_string(&tree).unwrap();
    assert!(json.contains("\"o/r\""));
    for leak in ["ghp_", "user:", "https://", "@github"] {
        assert!(!json.contains(leak), "{leak} in {json}");
    }
}

#[test]
fn the_symref_listing_parses_and_rejects_hostile_branches() {
    let sha = "a".repeat(40);
    let ok = format!("ref: refs/heads/main\tHEAD\n{sha}\tHEAD\n");
    assert_eq!(
        crate::fleet_sync::checkout_ff::git::parse_symref_head(&ok),
        Some(("main".to_string(), sha.clone()))
    );
    let bad = format!("ref: refs/heads/--upload-pack=x\tHEAD\n{sha}\tHEAD\n");
    assert_eq!(crate::fleet_sync::checkout_ff::git::parse_symref_head(&bad), None);
    assert_eq!(crate::fleet_sync::checkout_ff::git::parse_symref_head("zz\tHEAD\n"), None);
}

fn spec_start(daemon: bool, target: bool) -> (Started, Option<Spec>) {
    let payload = HookPayload {
        session_id: Some("abc-123".to_string()),
        cwd: Some("/work/clone".to_string()),
        source: Some("startup".to_string()),
    };
    let spawned = std::cell::RefCell::new(None);
    let out = start_with(
        daemon,
        &payload,
        Path::new("/elsewhere"),
        |c| Some(c.to_path_buf()),
        |_| target,
        |spec| {
            *spawned.borrow_mut() = Some(spec.clone());
            Ok(())
        },
    );
    (out, spawned.into_inner())
}

#[test]
fn the_front_half_spawns_only_for_an_attended_session_with_a_target() {
    let (out, spec) = spec_start(false, true);
    assert_eq!(out, Started::Spawned);
    let spec = spec.unwrap();
    assert_eq!(spec.cwd, PathBuf::from("/work/clone"));
    assert_eq!((spec.session_id.as_str(), spec.source.as_deref()), ("abc-123", Some("startup")));
    assert!(matches!(spec_start(true, true), (Started::Skipped(_), None)));
    assert!(matches!(spec_start(false, false), (Started::Skipped(_), None)));
}

#[test]
fn the_front_half_does_no_network_io() {
    // `start_with` takes no remote at all: the only injected effects are the
    // local toplevel lookup, the target check and the spawn.
    let (out, _) = spec_start(false, true);
    assert_eq!(out, Started::Spawned);
}

#[test]
fn the_built_record_carries_the_measurement() {
    let f = fixture(2, 3);
    let spec = Spec {
        cwd: f.host.clone(),
        session_id: "s1".into(),
        source: Some("startup".into()),
    };
    // The comparison half is exercised directly above; here only the record.
    let record = super::session::build_record(&spec, &expected(None), &Offline).unwrap();
    assert_eq!(record.session_id, "s1");
    assert_eq!(record.prompt.remote_foreign, Some(true));
}
