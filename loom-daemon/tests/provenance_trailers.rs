//! #9027: a commit made by a dispatched issue sweep carries exactly the three
//! D33 trailers, and the repository's own hooks still run.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use loom_daemon::provenance::{hooks, marker::Marker};
use loom_daemon::telemetry::repo_identity;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn hook(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A repo whose origin names rjwalters/loom (repo_id stubbed), with its own
/// `commit-msg` + `pre-commit` hooks under `hooks_path` (None → `.git/hooks`).
fn repo(hooks_path: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/rjwalters/loom.git",
        ],
    );
    let hook_dir = match hooks_path {
        Some(path) => {
            git(root, &["config", "core.hooksPath", path]);
            root.join(path)
        }
        None => root.join(".git/hooks"),
    };
    hook(
        &hook_dir,
        "pre-commit",
        "touch \"$(git rev-parse --git-dir)/repo-pre-commit-ran\"",
    );
    hook(&hook_dir, "commit-msg", "printf 'Repo-Hook: ran\\n' >> \"$1\"");
    repo_identity::seed("rjwalters/loom", repo_identity::parse("1073994527 rjwalters/loom"));
    dir
}

/// `git <args>` as the dispatched sweep would run it.
fn sweep_git(root: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(args)
        .env_remove("GIT_CONFIG_COUNT");
    hooks::prepare_child_with(
        &mut cmd,
        root,
        Some(9027),
        Path::new(env!("CARGO_BIN_EXE_loom-daemon")),
    );
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn trailer(root: &Path, key: &str) -> Vec<String> {
    git(
        root,
        &[
            "log",
            "-1",
            &format!("--format=%(trailers:key={key},valueonly)"),
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect()
}

fn assert_stamped(root: &Path) {
    assert_eq!(trailer(root, "Loom-Story"), ["rjwalters/loom#9027"]);
    assert_eq!(trailer(root, "Loom-Trace-Id"), ["4e99cc89953fb6bd604a06896b83dc81"]);
    assert_eq!(trailer(root, "Loom-Build"), [loom_daemon::self_update::BUILD_STAMP]);
    let loom_keys = git(root, &["log", "-1", "--format=%(trailers:only,keyonly)"])
        .lines()
        .filter(|k| k.starts_with("Loom-"))
        .count();
    assert_eq!(loom_keys, 3, "exactly three Loom trailers");
    let build = &trailer(root, "Loom-Build")[0];
    let parts: Vec<&str> = build.split(' ').collect();
    assert_eq!(parts.len(), 3);
    assert!(parts[1] == "unknown" || parts[1].len() == 40, "{build}");
    assert!(["clean", "dirty", "unknown"].contains(&parts[2]), "{build}");
}

#[test]
fn sweep_commits_carry_exactly_three_trailers_and_repo_hooks_still_run() {
    for hooks_path in [Some(".githooks"), None] {
        let dir = repo(hooks_path);
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        git(root, &["add", "a.txt"]);
        sweep_git(
            root,
            &[
                "commit",
                "-q",
                "-m",
                "feat: a\n\nCo-Authored-By: X <x@example.invalid>",
            ],
        );
        assert_stamped(root);
        // The repository's own hooks ran first (#3638: never clobbered).
        assert!(root.join(".git/repo-pre-commit-ran").exists(), "{hooks_path:?}");
        assert_eq!(trailer(root, "Repo-Hook"), ["ran"]);
        assert_eq!(trailer(root, "Co-Authored-By"), ["X <x@example.invalid>"]);
        // Amending re-stamps in place: still exactly one of each.
        sweep_git(root, &["commit", "-q", "--amend", "--no-edit"]);
        assert_stamped(root);
        // The override lives only in the sweep's environment.
        let configured = Command::new("git")
            .current_dir(root)
            .args(["config", "--get-all", "core.hooksPath"])
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&configured.stdout).trim(), hooks_path.unwrap_or(""));
    }
}

#[test]
fn commits_outside_a_sweep_are_untouched() {
    let dir = repo(None);
    let root = dir.path();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-q", "-m", "chore: b"]);
    assert!(trailer(root, "Loom-Story").is_empty());
}

#[test]
fn trailers_subcommand_never_derives_a_trace_from_the_name() {
    // The subcommand runs in its own process, where the stubbed repo_id does
    // not reach and `gh` is off PATH: the story text survives from the origin
    // but the trace is an explicit `unknown`, never a name-derived id. The
    // resolved in-process path is covered above.
    let dir = repo(None);
    let bin = tempfile::tempdir().unwrap();
    let real_git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::os::unix::fs::symlink(real_git.trim(), bin.path().join("git")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["provenance", "trailers", "--issue", "9027", "--repo-root"])
        .arg(dir.path())
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "Loom-Story: rjwalters/loom#9027");
    assert_eq!(lines[1], "Loom-Trace-Id: unknown");
    assert_eq!(lines[2], format!("Loom-Build: {}", loom_daemon::self_update::BUILD_STAMP));
}

#[test]
fn pr_marker_subcommand_emits_one_parseable_line() {
    let dir = repo(None);
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "provenance",
            "pr-marker",
            "--sweep",
            "sweep-issue-9027-1",
            "--repo-root",
        ])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    let marker = Marker::parse(text.trim()).unwrap();
    assert_eq!(marker.sweep, "sweep-issue-9027-1");
    assert_eq!(marker.story, "unknown");
    assert_eq!(marker.base, "unknown");
    assert_eq!(marker.build, loom_daemon::self_update::BUILD_STAMP);
}
