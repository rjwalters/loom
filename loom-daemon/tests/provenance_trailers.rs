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
    sweep_git_with(root, args, &[]);
}

/// [`sweep_git`] in an environment that already carries env-scoped config.
fn sweep_git_with(root: &Path, args: &[&str], ambient: &[(&str, &str)]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(args)
        .env_remove("GIT_CONFIG_COUNT");
    if !ambient.is_empty() {
        for (i, (key, value)) in ambient.iter().enumerate() {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), key)
                .env(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
        cmd.env("GIT_CONFIG_COUNT", ambient.len().to_string());
    }
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

/// `loom-daemon provenance pr-marker` outside Actions, with `gh` off PATH.
fn pr_marker(root: &Path, extra: &[&str], body: Option<&str>) -> Marker {
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["provenance", "pr-marker", "--repo-root"])
        .arg(root)
        .args(extra)
        .env("PATH", bin.path())
        .env_remove("LOOM_SWEEP_ID")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    for key in [
        "GITHUB_ACTIONS",
        "GITHUB_REPOSITORY",
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
    ] {
        cmd.env_remove(key);
    }
    if body.is_some() {
        cmd.args(["--body-file", "-"]);
    }
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(body.unwrap_or("").as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    Marker::parse(text.trim()).unwrap()
}

#[test]
fn pr_marker_subcommand_emits_one_parseable_line() {
    let dir = repo(None);
    let marker = pr_marker(dir.path(), &["--sweep", "sweep-issue-9027-1"], None);
    assert_eq!(marker.sweep, "sweep-issue-9027-1");
    assert_eq!(marker.story, "none", "no closing issue → no story");
    assert_eq!(marker.base, "unknown");
    assert_eq!(marker.run, "none", "not an Actions run");
    assert_eq!(marker.build, loom_daemon::self_update::BUILD_STAMP);
    assert_eq!(marker.prompts, "unknown unknown");
}

#[test]
fn pr_marker_story_follows_d32_from_the_body() {
    let dir = repo(None);
    let root = dir.path();
    // Outside a sweep: sweep=none, not unknown.
    assert_eq!(pr_marker(root, &[], Some("Docs only.")).sweep, "none");
    // Exactly one closing issue joins that issue's story (repo_id unresolvable
    // out of process, so the trace is an explicit unknown).
    let one = pr_marker(root, &[], Some("Closes #9027"));
    assert_eq!((one.story.as_str(), one.trace.as_str()), ("rjwalters/loom#9027", "unknown"));
    // Several: the PR is its own story (not yet numbered), never the first issue's.
    let two = pr_marker(root, &[], Some("Closes #9068\nCloses #9027"));
    assert_eq!((two.story.as_str(), two.trace.as_str()), ("unknown", "unknown"));
    let none = pr_marker(root, &[], Some("Relates to #9027"));
    assert_eq!(none.story, "none");
}

#[test]
fn env_scoped_config_passes_through_to_the_repo_hook_lookup() {
    // A container-style ambient `safe.directory`, plus an ambient
    // `core.hooksPath` the repo's effective hooks come from: the shim must drop
    // only loom's own entry, so it chains to the ambient dir, and the hook it
    // runs still sees `safe.directory`.
    let dir = repo(None);
    let root = dir.path();
    let ambient_hooks = root.join("ambient-hooks");
    hook(
        &ambient_hooks,
        "pre-commit",
        "git config --get safe.directory > \"$(git rev-parse --git-dir)/ambient-safe-directory\"",
    );
    let ambient_hooks = ambient_hooks.to_str().unwrap().to_string();
    std::fs::write(root.join("c.txt"), "c").unwrap();
    git(root, &["add", "c.txt"]);
    sweep_git_with(
        root,
        &["commit", "-q", "-m", "chore: c"],
        &[("safe.directory", "*"), ("core.hooksPath", &ambient_hooks)],
    );
    let seen = std::fs::read_to_string(root.join(".git/ambient-safe-directory")).unwrap();
    assert_eq!(seen.trim(), "*");
    assert!(
        !root.join(".git/repo-pre-commit-ran").exists(),
        "the ambient hooksPath, not .git/hooks, is the repo's effective one"
    );
    assert_stamped(root);
}

#[test]
fn a_non_github_origin_stamps_story_none() {
    let dir = repo(None);
    let root = dir.path();
    git(
        root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://gitlab.example.invalid/a/b.git",
        ],
    );
    std::fs::write(root.join("d.txt"), "d").unwrap();
    git(root, &["add", "d.txt"]);
    sweep_git(root, &["commit", "-q", "-m", "chore: d"]);
    assert_eq!(trailer(root, "Loom-Story"), ["none"]);
    assert_eq!(trailer(root, "Loom-Trace-Id"), ["unknown"]);
}
