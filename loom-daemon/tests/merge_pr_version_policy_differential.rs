//! Differential test: `loom-daemon merge-pr version-policy` against the shell
//! it replaced (`merge-pr.sh`'s `_check_defaults_version_bump_collision`,
//! #7827 + #8284), on REAL repositories with the REAL canonical checker.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: build each scenario ONCE and
//! run both sides against the same bytes — here, the same on-disk origin +
//! clone pair, so the harness cannot lie about which side moved. The shell
//! side is `tests/fixtures/merge-pr-version-policy-retired.sh`, a frozen
//! byte-for-byte copy of the retired function, driven through `warning` /
//! `error` shims that emit the very `LEVEL<TAB>line` protocol the Rust prints.
//! Equality is therefore checked on the full operator-visible text and the
//! exit code, not on a summary.
//!
//! The retained `test-merge-pr-defaults-version-bump-collision.sh` suite
//! remains the behavioural specification through `merge-pr.sh`'s stub; this
//! proves the port also kept every message byte-for-byte, including the
//! scenarios whose assertions there are only `contains` checks.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git could not be executed")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const CHECKER_REL: &str = "defaults/scripts/check-defaults-version-bump.sh";

fn write_exec(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// An origin on `main` carrying the real checker, `defaults/scripts/foo.md`
/// and `VERSION=1.0.0`, plus a clone playing `$REPO_ROOT`.
struct Fixture {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    local: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let local = tmp.path().join("local");
        fs::create_dir_all(&origin).unwrap();
        git_ok(&origin, &["init", "-q"]);
        git_ok(&origin, &["checkout", "-q", "-b", "main"]);
        let checker = fs::read_to_string(repo_root().join(CHECKER_REL)).unwrap();
        write_exec(&origin.join(CHECKER_REL), &checker);
        fs::write(origin.join("defaults/scripts/foo.md"), "hello\n").unwrap();
        fs::write(origin.join("VERSION"), "1.0.0\n").unwrap();
        git_ok(&origin, &["add", "-A"]);
        git_ok(&origin, &["commit", "-q", "-m", "base"]);
        let out = Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&origin)
            .arg(&local)
            .output()
            .unwrap();
        assert!(out.status.success());
        Fixture {
            _tmp: tmp,
            origin,
            local,
        }
    }

    /// Branch `name` off origin/main, apply `edit`, commit, push, and return
    /// to `main` (the operator checkout sits on the default branch, so the
    /// on-disk checker is main's). Returns the head SHA.
    fn pr(&self, name: &str, edit: impl FnOnce(&Path)) -> String {
        git_ok(&self.local, &["fetch", "--quiet", "origin"]);
        git_ok(&self.local, &["checkout", "-q", "-B", name, "origin/main"]);
        edit(&self.local);
        git_ok(&self.local, &["add", "-A"]);
        git_ok(&self.local, &["commit", "-q", "-m", "pr change"]);
        git_ok(&self.local, &["push", "--quiet", "origin", name]);
        let sha = git_ok(&self.local, &["rev-parse", "HEAD"]);
        git_ok(&self.local, &["checkout", "-q", "main"]);
        sha
    }

    fn advance_main(&self, edit: impl FnOnce(&Path)) {
        edit(&self.origin);
        git_ok(&self.origin, &["add", "-A"]);
        git_ok(&self.origin, &["commit", "-q", "-m", "concurrent merge"]);
    }
}

fn set_version(v: &'static str) -> impl FnOnce(&Path) {
    move |d: &Path| {
        fs::write(d.join("VERSION"), format!("{v}\n")).unwrap();
        fs::write(d.join("defaults/scripts/foo.md"), "hello\nchanged\n").unwrap();
    }
}

struct Case<'a> {
    repo: &'a Path,
    default_branch: &'a str,
    branch: &'a str,
    head: &'a str,
    dry_run: bool,
}

/// `(exit code, stdout)` of the frozen shell.
fn run_shell(c: &Case) -> (i32, String) {
    let fixture = repo_root().join("loom-daemon/tests/fixtures/merge-pr-version-policy-retired.sh");
    let script = format!(
        r#"set -euo pipefail
warning() {{ printf '%s\n' "$*" | awk '{{print "WARNING\t" $0}}'; }}
error() {{ printf '%s\n' "$*" | awk '{{print "BLOCK\t" $0}}'; exit 1; }}
source "{}"
_check_defaults_version_bump_collision"#,
        fixture.display()
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .env("REPO_ROOT", c.repo)
        .env("DEFAULT_BRANCH_NAME", c.default_branch)
        .env("PR_BRANCH", c.branch)
        .env("PR_HEAD_SHA", c.head)
        .env("PR_NUMBER", "7302")
        .env("DRY_RUN", if c.dry_run { "true" } else { "false" })
        .output()
        .unwrap();
    (out.status.code().unwrap(), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `(exit code, stdout)` of the port.
fn run_rust(c: &Case) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["merge-pr", "version-policy", "--repo-root"])
        .arg(c.repo)
        .args([
            "--default-branch",
            c.default_branch,
            "--branch",
            c.branch,
            "--head-sha",
            c.head,
            "--pr",
            "7302",
        ]);
    if c.dry_run {
        cmd.arg("--dry-run");
    }
    let out = cmd.output().unwrap();
    (out.status.code().unwrap(), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run both sides, assert they agree, and return the shared result so each
/// scenario can also pin what the agreed behaviour IS.
fn agree(label: &str, c: &Case) -> (i32, String) {
    let shell = run_shell(c);
    let rust = run_rust(c);
    assert_eq!(rust, shell, "{label}: port diverged from the retired shell");
    rust
}

fn case<'a>(repo: &'a Path, branch: &'a str, head: &'a str) -> Case<'a> {
    Case {
        repo,
        default_branch: "main",
        branch,
        head,
        dry_run: false,
    }
}

#[test]
fn clean_pr_passes_silently() {
    let f = Fixture::new();
    let head = f.pr("feature/clean", |d| {
        fs::write(d.join("defaults/scripts/foo.md"), "changed\n").unwrap();
    });
    assert_eq!(agree("clean", &case(&f.local, "feature/clean", &head)), (0, String::new()));
}

#[test]
fn hand_bump_blocks_and_dry_run_only_reports() {
    let f = Fixture::new();
    let head = f.pr("feature/bump", set_version("1.0.1"));
    let (rc, out) = agree("bump", &case(&f.local, "feature/bump", &head));
    assert_eq!(rc, 1);
    assert!(out.starts_with("BLOCK\tMerge blocked: PR #7302 hand-edits"));
    let mut dry = case(&f.local, "feature/bump", &head);
    dry.dry_run = true;
    let (rc, out) = agree("bump dry-run", &dry);
    assert_eq!(rc, 0);
    assert!(out.starts_with("WARNING\t[dry-run] Would BLOCK merge of PR #7302"));
}

#[test]
fn downgrade_blocks_and_matching_main_cannot_hide_it() {
    let f = Fixture::new();
    let head = f.pr("feature/down", set_version("0.9.0"));
    assert_eq!(agree("downgrade", &case(&f.local, "feature/down", &head)).0, 1);
    let head = f.pr("feature/same", set_version("1.0.1"));
    f.advance_main(|d| fs::write(d.join("VERSION"), "1.0.1\n").unwrap());
    assert_eq!(agree("matching main", &case(&f.local, "feature/same", &head)).0, 1);
}

#[test]
fn concurrent_automated_bump_on_main_does_not_block() {
    let f = Fixture::new();
    let head = f.pr("feature/defaults", |d| {
        fs::write(d.join("defaults/scripts/foo.md"), "changed\n").unwrap();
    });
    f.advance_main(|d| fs::write(d.join("VERSION"), "1.0.1\n").unwrap());
    assert_eq!(
        agree("concurrent", &case(&f.local, "feature/defaults", &head)),
        (0, String::new())
    );
}

#[test]
fn every_skip_path_agrees() {
    let f = Fixture::new();
    let head = f.pr("feature/bump", set_version("1.0.1"));

    // Unknown default branch, empty head, empty branch: silent skips.
    let mut c = case(&f.local, "feature/bump", &head);
    c.default_branch = "";
    assert_eq!(agree("no default branch", &c), (0, String::new()));
    assert_eq!(agree("empty head", &case(&f.local, "feature/bump", "")), (0, String::new()));
    assert_eq!(agree("empty branch", &case(&f.local, "", &head)), (0, String::new()));

    // Unresolvable head.
    let dead = "000000000000000000000000000000000000dead";
    assert_eq!(agree("dead head", &case(&f.local, "feature/bump", dead)), (0, String::new()));

    // Unrelated history: the ancestry warning.
    git_ok(&f.local, &["checkout", "-q", "--orphan", "unrelated"]);
    git_ok(&f.local, &["commit", "-q", "-m", "disconnected"]);
    git_ok(&f.local, &["push", "--quiet", "origin", "unrelated"]);
    let orphan = git_ok(&f.local, &["rev-parse", "HEAD"]);
    git_ok(&f.local, &["checkout", "-q", "-f", "main"]);
    let (rc, out) = agree("ancestry", &case(&f.local, "unrelated", &orphan));
    assert_eq!(rc, 0);
    assert!(out.contains("ancestry unavailable"));

    // Failed fetch.
    git_ok(&f.local, &["remote", "set-url", "origin", "/nonexistent/origin"]);
    assert_eq!(
        agree("failed fetch", &case(&f.local, "feature/bump", &head)),
        (0, String::new())
    );
}

#[test]
fn checker_fault_is_a_warned_skip_including_multi_line_and_empty_output() {
    let f = Fixture::new();
    let head = f.pr("feature/bump", set_version("1.0.1"));
    let checker = f.local.join(CHECKER_REL);
    for body in [
        "#!/bin/sh\nexit 2\n",
        "#!/bin/sh\necho one\necho two >&2\necho\necho three\nexit 3\n",
        "#!/bin/sh\necho usage\n\n\nexit 64\n",
    ] {
        write_exec(&checker, body);
        let (rc, out) = agree(body, &case(&f.local, "feature/bump", &head));
        assert_eq!(rc, 0, "{body}");
        assert!(out.lines().all(|l| l.starts_with("WARNING\t")), "{out}");
    }
}

#[test]
fn the_pr_head_oracle_and_its_fallback_agree() {
    let f = Fixture::new();

    // #8190's shape: shrink the version-bearing set AND change VERSION.
    let head = f.pr("feature/shrink", |d| {
        let p = d.join(CHECKER_REL);
        let body: String = fs::read_to_string(&p)
            .unwrap()
            .lines()
            .filter(|l| *l != "  \"VERSION\"")
            .map(|l| format!("{l}\n"))
            .collect();
        write_exec(&p, &body);
        fs::write(d.join("VERSION"), "9.9.9\n").unwrap();
    });
    let (rc, out) = agree("shrink", &case(&f.local, "feature/shrink", &head));
    assert_eq!(rc, 0);
    assert!(out.contains(&format!("the PR head ({head})")));

    // Touching scripts/version.sh is not a bypass.
    let head = f.pr("feature/touch", |d| {
        write_exec(&d.join("scripts/version.sh"), "#!/usr/bin/env bash\n");
        fs::write(d.join("VERSION"), "1.0.1\n").unwrap();
    });
    assert_eq!(agree("touch", &case(&f.local, "feature/touch", &head)).0, 1);

    // A head without the checker falls back to main's, never a free pass.
    let head = f.pr("feature/delete", |d| {
        fs::remove_file(d.join(CHECKER_REL)).unwrap();
        fs::write(d.join("VERSION"), "1.0.1\n").unwrap();
    });
    let (rc, out) = agree("delete", &case(&f.local, "feature/delete", &head));
    assert_eq!(rc, 1);
    assert!(out.contains("'main' ("));

    // Main-side machinery drift does not flip the oracle.
    let head = f.pr("feature/plain", |d| {
        fs::write(d.join("defaults/scripts/foo.md"), "changed\n").unwrap();
    });
    f.advance_main(|d| write_exec(&d.join("scripts/version.sh"), "#!/usr/bin/env bash\n"));
    assert_eq!(agree("drift", &case(&f.local, "feature/plain", &head)), (0, String::new()));
}
