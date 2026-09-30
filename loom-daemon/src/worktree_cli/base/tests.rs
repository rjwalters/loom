//! Unit tests for the base-ref resolution (#8195 slice 13), against real repos
//! with a real bare `origin`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct Fx {
    root: PathBuf,
    repo: PathBuf,
}

fn fixture(tag: &str) -> Fx {
    let root = std::env::temp_dir().join(format!("loom-wt-base-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let origin = root.join("origin.git");
    let repo = root.join("re po"); // a space: #7858's class
    fs::create_dir_all(&repo).unwrap();
    git(&root, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&repo, &["push", "-q", "origin", "main"]);
    let root = fs::canonicalize(&root).unwrap();
    let repo = root.join("re po");
    Fx { root, repo }
}

fn data(o: &Outcome, l: Level) -> Option<String> {
    o.records
        .iter()
        .find(|(x, _)| *x == l)
        .map(|(_, t)| t.clone())
}

#[test]
fn no_base_resolves_to_origin_default() {
    let fx = fixture("nobase");
    let o = resolve(&fx.repo, "main", None, false);
    assert_eq!(o.code, 0);
    assert_eq!(data(&o, Level::BaseRef).unwrap(), "origin/main");
    assert_eq!(data(&o, Level::BaseDisplay).unwrap(), "main");
    assert!(o
        .records
        .iter()
        .any(|(l, t)| *l == Level::Success && t.contains("Fetched latest")));
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn empty_base_is_no_base() {
    let fx = fixture("emptybase");
    let o = resolve(&fx.repo, "main", Some(""), true);
    assert_eq!(o.code, 0);
    assert_eq!(data(&o, Level::BaseRef).unwrap(), "origin/main");
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn unfetchable_default_warns_and_continues() {
    let fx = fixture("nofetch");
    let o = resolve(&fx.repo, "nonexistent", None, false);
    assert_eq!(o.code, 0);
    assert!(o.records.iter().any(|(l, _)| *l == Level::Warning));
    assert_eq!(data(&o, Level::BaseRef).unwrap(), "origin/nonexistent");
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn pushed_base_prefers_origin_ref() {
    let fx = fixture("pushed");
    git(&fx.repo, &["branch", "feature/issue-1"]);
    git(&fx.repo, &["push", "-q", "origin", "feature/issue-1"]);
    let o = resolve(&fx.repo, "main", Some("feature/issue-1"), false);
    assert_eq!(o.code, 0);
    assert_eq!(data(&o, Level::BaseRef).unwrap(), "origin/feature/issue-1");
    assert_eq!(data(&o, Level::BaseDisplay).unwrap(), "origin/feature/issue-1");
    assert!(o.records.iter().any(|(l, t)| *l == Level::Info
        && t == "Stacked worktree base: origin/feature/issue-1 (from --base feature/issue-1)"));
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn local_only_base_falls_back_to_local_ref() {
    let fx = fixture("local");
    git(&fx.repo, &["branch", "feature/issue-2"]);
    let o = resolve(&fx.repo, "main", Some("feature/issue-2"), true);
    assert_eq!(o.code, 0);
    assert_eq!(data(&o, Level::BaseRef).unwrap(), "feature/issue-2");
    assert_eq!(data(&o, Level::BaseDisplay).unwrap(), "feature/issue-2");
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn missing_base_is_refused_and_never_falls_back_to_default() {
    let fx = fixture("missing");
    let o = resolve(&fx.repo, "main", Some("feature/issue-9"), false);
    assert_eq!(o.code, 1);
    assert!(data(&o, Level::BaseRef).is_none(), "must not un-stack silently");
    assert!(o
        .records
        .iter()
        .any(|(l, t)| *l == Level::Error && t.contains("not found")));
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn missing_base_in_json_mode_emits_one_valid_document() {
    let fx = fixture("missingjson");
    let o = resolve(&fx.repo, "main", Some("feature/issue-9"), true);
    assert_eq!(o.code, 1);
    let doc = data(&o, Level::Json).unwrap();
    let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(v["error"], "base-branch-not-found");
    assert_eq!(v["success"], false);
    assert!(o.records.iter().all(|(l, _)| matches!(l, Level::Json)));
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn unsafe_base_is_refused_before_git_runs_and_json_is_escaped() {
    let fx = fixture("unsafe");
    for bad in ["--upload-pack=/tmp/x", "-x", "a\"b", "a b", "../x"] {
        let o = resolve(&fx.repo, "main", Some(bad), true);
        assert_eq!(o.code, 1, "{bad}");
        let v: serde_json::Value = serde_json::from_str(&data(&o, Level::Json).unwrap())
            .unwrap_or_else(|_| panic!("invalid JSON for {bad}"));
        assert_eq!(v["error"], "unsafe-base-branch-name");
        assert_eq!(v["baseBranch"], bad);
        assert!(data(&o, Level::BaseRef).is_none());
    }
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn unsafe_default_branch_is_refused() {
    let fx = fixture("unsafedefault");
    let o = resolve(&fx.repo, "--depth=1", None, false);
    assert_eq!(o.code, 1);
    assert!(data(&o, Level::BaseRef).is_none());
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn quiet_suppresses_messages_but_not_data() {
    let fx = fixture("quiet");
    let o = resolve(&fx.repo, "main", None, true);
    assert_eq!(o.records.len(), 2);
    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn stale_origin_ref_is_still_preferred_when_fetch_of_base_fails() {
    // The remote no longer has the branch, but a remote-tracking ref remains:
    // the retired shell (best-effort `git fetch ... || true`) still used it.
    let fx = fixture("stale");
    git(&fx.repo, &["branch", "feature/issue-3"]);
    git(&fx.repo, &["push", "-q", "origin", "feature/issue-3"]);
    git(&fx.repo, &["fetch", "-q", "origin"]);
    git(&fx.root.join("origin.git"), &["branch", "-D", "feature/issue-3"]);
    let o = resolve(&fx.repo, "main", Some("feature/issue-3"), true);
    assert_eq!(o.code, 0);
    assert_eq!(data(&o, Level::BaseRef).unwrap(), "origin/feature/issue-3");
    let _ = fs::remove_dir_all(&fx.root);
}
