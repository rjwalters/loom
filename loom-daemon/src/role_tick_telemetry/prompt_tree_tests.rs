//! Fixture-repo tests for the remote comparison (#12509). Origins are local
//! bare repos; the comparison reads only local state, so a test makes the
//! origin a GitHub URL after the objects are in place (a local path is,
//! correctly, a foreign origin) and injects the "heard" head.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::remote::{classify_origin, fill, Expected};
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
        origin,
    }
}

/// Make `origin` a GitHub URL, as a managed checkout has. Nothing contacts it:
/// every read under test is local.
fn point_at_github(host: &Path) {
    run(
        host,
        &[
            "remote",
            "set-url",
            "origin",
            "https://user:ghp_FAKE@github.com/o/r.git",
        ],
    );
}

fn github() -> Expected {
    expected(None)
}

fn filled(root: &Path, heard: Option<(&str, &str)>) -> PromptTree {
    let mut tree = read(root).unwrap();
    fill(root, &mut tree, &github(), heard);
    tree
}

fn tip_of(f: &Fixture) -> String {
    run(&f.origin, &["rev-parse", "main"])
}

#[test]
fn behind_by_three_when_the_tip_is_fetched() {
    let f = fixture(2, 3);
    run(&f.host, &["fetch", "--quiet", "origin"]);
    point_at_github(&f.host);
    let tip = tip_of(&f);
    let tree = filled(&f.host, Some(("main", &tip)));
    assert_eq!(tree.remote_tip.as_deref(), Some(tip.as_str()));
    assert_eq!(tree.commits_behind_remote, Some(3));
    assert_eq!(tree.commits_behind, Some(3), "the local ref was fetched too");
    assert_eq!(tree.origin_ref_stale, Some(false));
    assert_eq!(tree.remote_repo.as_deref(), Some("o/r"));
    assert_eq!(tree.remote_foreign, Some(false));
}

#[test]
fn a_stale_remote_tracking_ref_under_head_is_still_behind() {
    let f = fixture(2, 3);
    // The objects are here, but `origin/main` was not moved: HEAD equals it.
    run(&f.host, &["fetch", "--quiet", "--refmap=", "origin", "refs/heads/main"]);
    point_at_github(&f.host);
    let tip = tip_of(&f);
    let tree = filled(&f.host, Some(("main", &tip)));
    assert_eq!(tree.commits_behind, Some(0), "behind its own stale ref: zero");
    assert_eq!(tree.commits_behind_remote, Some(3));
    assert_eq!(tree.origin_ref_stale, Some(true));
}

#[test]
fn diverged_counts_only_the_tips_commits() {
    let f = fixture(1, 2);
    commit(&f.host, "local only");
    run(&f.host, &["fetch", "--quiet", "origin"]);
    point_at_github(&f.host);
    let tree = filled(&f.host, Some(("main", &tip_of(&f))));
    assert_eq!(tree.commits_behind_remote, Some(2));
}

#[test]
fn up_to_date_is_zero_and_not_stale() {
    let f = fixture(2, 0);
    point_at_github(&f.host);
    let tree = filled(&f.host, Some(("main", &tip_of(&f))));
    assert_eq!(tree.commits_behind_remote, Some(0));
    assert_eq!(tree.origin_ref_stale, Some(false));
}

#[test]
fn a_tip_not_in_the_object_store_is_stale_but_never_counted_as_zero() {
    let f = fixture(2, 3);
    point_at_github(&f.host);
    let tree = filled(&f.host, Some(("main", &tip_of(&f))));
    assert_eq!(tree.origin_ref_stale, Some(true));
    assert_eq!(tree.commits_behind_remote, None);
}

#[test]
fn no_head_heard_leaves_every_remote_fact_absent() {
    let f = fixture(2, 3);
    point_at_github(&f.host);
    let tree = filled(&f.host, None);
    assert_eq!(tree.remote_tip, None);
    assert_eq!(tree.origin_ref_stale, None);
    assert_eq!(tree.commits_behind_remote, None);
    assert_eq!(tree.remote_repo.as_deref(), Some("o/r"), "origin facts are local");
}

#[test]
fn a_hostile_heard_head_never_reaches_git() {
    let f = fixture(1, 1);
    point_at_github(&f.host);
    for tip in ["--output=/tmp/x", "main", "abc", &"g".repeat(40)] {
        let tree = filled(&f.host, Some(("main", tip)));
        assert_eq!(tree.remote_tip, None, "{tip}");
    }
    let tip = tip_of(&f);
    assert_eq!(filled(&f.host, Some(("--upload-pack=x", &tip))).remote_tip, None);
}

#[test]
fn a_foreign_origin_is_flagged_and_gets_no_remote_comparison() {
    let f = fixture(1, 1);
    let tip = tip_of(&f);
    let tree = filled(&f.host, Some(("main", &tip)));
    assert_eq!(tree.remote_foreign, Some(true), "a local path origin");
    assert_eq!(tree.remote_repo, None);
    assert_eq!(tree.remote_tip, None);
    assert_eq!(tree.commits_behind_remote, None);
}

#[test]
fn an_origin_that_is_another_repo_than_the_workspaces_is_foreign() {
    let f = fixture(1, 1);
    point_at_github(&f.host);
    let mut tree = read(&f.host).unwrap();
    fill(&f.host, &mut tree, &expected(Some("o/elsewhere")), Some(("main", &tip_of(&f))));
    assert_eq!(tree.remote_foreign, Some(true));
    assert_eq!(tree.remote_tip, None);
}

#[test]
fn reading_the_freshness_writes_nothing_to_git() {
    let f = fixture(2, 3);
    run(&f.host, &["fetch", "--quiet", "--refmap=", "origin", "refs/heads/main"]);
    let _ = std::fs::remove_file(f.host.join(".git/FETCH_HEAD"));
    point_at_github(&f.host);
    let refs = run(&f.host, &["for-each-ref"]);
    let objects = run(&f.host, &["count-objects", "-v"]);
    let _ = filled(&f.host, Some(("main", &tip_of(&f))));
    assert_eq!(run(&f.host, &["for-each-ref"]), refs);
    assert_eq!(run(&f.host, &["count-objects", "-v"]), objects);
    assert!(!f.host.join(".git/FETCH_HEAD").exists());
}

/// The grep check of the acceptance criteria: the code that reads freshness
/// has no `fetch`, `ls-remote` or other network `git` call.
#[test]
fn the_freshness_code_spawns_no_network_git() {
    for (name, src) in [
        ("remote.rs", include_str!("prompt_tree/remote.rs")),
        ("freshness.rs", include_str!("../fleet_sync/checkout_ff/freshness.rs")),
    ] {
        for line in src.lines().filter(|l| !l.trim_start().starts_with("//")) {
            for banned in [
                "\"fetch\"",
                "\"ls-remote\"",
                "\"pull\"",
                "\"clone\"",
                "\"push\"",
            ] {
                assert!(!line.contains(banned), "{name}: {line}");
            }
        }
    }
    for banned in ["prompt-tree-session", "prompt-tree session-start"] {
        assert!(!include_str!("prompt_tree/remote.rs").contains(banned));
    }
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
