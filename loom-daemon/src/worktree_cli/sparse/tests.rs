//! Unit tests for the sparse-checkout family (#8195 slice 10).
//!
//! `tests/worktree_sparse_differential.rs` owns the comparison against the
//! retired shell (messages, exit codes, JSON documents, resulting git state)
//! and the byte-for-byte sentinel pin against the LIVE `write_loom_sentinel`.
//! This file owns what that comparison cannot state: the helpers' contracts
//! in isolation, and the side-effect properties asserted positively — above
//! all, that an unregistered directory is never written to and never has git
//! run against the repository that contains it.
//!
//! Every fixture root contains a space (`"sparse fixture"`): #7858 was an
//! unquoted path that turned a guard into a live `rm -rf`, and the parent
//! issue asks for that class to be covered by the port's own tests, not only
//! by the retained suite.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

#[test]
fn cone_is_user_paths_then_defaults_then_extras() {
    assert_eq!(
        cone_paths(&os(&["src/lib", "docs"]), None),
        os(&[
            "src/lib",
            "docs",
            ".claude",
            ".loom",
            ".githooks",
            "scripts"
        ])
    );
}

#[test]
fn extras_split_on_bash_default_ifs_only() {
    // Space, tab and newline separate; runs of them do not produce empty
    // fields. A carriage return is NOT in bash's default IFS, so it stays
    // inside the field.
    let extra = OsString::from("  vendor\tthird_party\n\ntools  a\rb ");
    assert_eq!(
        cone_paths(&os(&["x"]), Some(&extra)),
        os(&[
            "x",
            ".claude",
            ".loom",
            ".githooks",
            "scripts",
            "vendor",
            "third_party",
            "tools",
            "a\rb"
        ])
    );
}

#[test]
fn extras_are_not_glob_expanded() {
    // The retired unquoted `(${LOOM_WORKTREE_ALWAYS_INCLUDE})` expanded `*`
    // against the caller's cwd. The port passes it through untouched.
    let extra = OsString::from("docs/*");
    let cone = cone_paths(&os(&["x"]), Some(&extra));
    assert_eq!(cone.last(), Some(&OsString::from("docs/*")));
}

#[test]
fn duplicates_are_kept_as_the_retired_concatenation_kept_them() {
    let cone = cone_paths(&os(&["scripts"]), None);
    assert_eq!(cone.iter().filter(|p| *p == "scripts").count(), 2);
}

#[test]
fn cone_json_matches_the_retired_spacing() {
    assert_eq!(cone_json(&os(&["a", "b/c"])), r#"["a","b/c"]"#);
    assert_eq!(cone_json(&[]), "[]");
}

#[test]
fn cone_json_escapes_what_the_retired_awk_builder_did_not() {
    let json = cone_json(&os(&[r#"we"ird"#, r"back\slash", "tab\there"]));
    let parsed: Vec<String> = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(parsed, vec![r#"we"ird"#, r"back\slash", "tab\there"]);
}

#[test]
fn logical_absolute_is_bash_cd_then_pwd() {
    assert_eq!(logical_absolute(Path::new("/a/./b//c/")), PathBuf::from("/a/b/c"));
    assert_eq!(logical_absolute(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
    assert_eq!(logical_absolute(Path::new("/..")), PathBuf::from("/"));
    assert_eq!(logical_absolute(Path::new("/with space/x")), PathBuf::from("/with space/x"));
}

#[test]
fn sentinel_content_is_the_documented_format() {
    assert_eq!(
        sentinel::content("42", "feature/issue-42"),
        "# Loom-managed worktree marker\n\
         # Created by .loom/scripts/worktree.sh\n\
         # Issue: 42\n\
         # Branch: feature/issue-42\n\
         # Removing this file makes Loom treat the worktree as user-owned and refuse\n\
         # to clean it up automatically.\n"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scratch directory whose path contains a space.
fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-sparse fixture-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `git config --worktree --get <key>` in `wt`, or `None` when unset.
fn worktree_config(wt: &Path, key: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["config", "--get", key])
        .output()
        .expect("git config");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A main repo at `<dir>/repo` with files in and out of a `src/lib` cone,
/// plus a linked worktree at `<dir>/repo/.loom/worktrees/issue-<n>` for each
/// `n` in `worktrees`. Returns the main repo.
fn repo_with_worktrees(dir: &Path, worktrees: &[u32]) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for (path, body) in [
        ("src/lib/a.txt", "a"),
        ("docs/b.md", "b"),
        ("top.txt", "top"),
        ("scripts/s.sh", "s"),
    ] {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        fs::write(&file, body).expect("write");
    }
    fs::write(repo.join(".gitignore"), ".loom/\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    for n in worktrees {
        let wt = repo.join(format!(".loom/worktrees/issue-{n}"));
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("feature/issue-{n}"),
                wt.to_str().expect("utf-8"),
                "main",
            ],
        );
    }
    repo
}

fn opts(repo: &Path, wt: &Path, arm: Arm, mode: Mode) -> Options {
    Options {
        repo: repo.to_path_buf(),
        worktree: wt.to_path_buf(),
        arm,
        mode,
        issue: "7".into(),
        branch: "feature/issue-7".into(),
        json: true,
        extra_include: None,
    }
}

// ---------------------------------------------------------------------------
// Behaviour on real repositories
// ---------------------------------------------------------------------------

#[test]
fn reconfigure_sparse_applies_the_cone_and_writes_the_sentinel() {
    let dir = tmpdir("reconf-sparse");
    let repo = repo_with_worktrees(&dir, &[7]);
    let wt = repo.join(".loom/worktrees/issue-7");

    let code = run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Sparse(os(&["src/lib"]))));

    assert_eq!(code, 0);
    assert!(wt.join("src/lib/a.txt").exists(), "in-cone file materialized");
    assert!(wt.join("scripts/s.sh").exists(), "always-included path materialized");
    assert!(wt.join("top.txt").exists(), "top-level files are implicit in cone mode");
    assert!(!wt.join("docs/b.md").exists(), "out-of-cone file removed");
    assert_eq!(
        fs::read_to_string(wt.join(".loom-managed")).expect("sentinel"),
        sentinel::content("7", "feature/issue-7")
    );
    // Per-worktree config, never the shared one the main clone reads.
    assert_eq!(worktree_config(&repo, "core.sparseCheckout"), None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reconfigure_full_restores_every_file() {
    let dir = tmpdir("reconf-full");
    let repo = repo_with_worktrees(&dir, &[7]);
    let wt = repo.join(".loom/worktrees/issue-7");
    assert_eq!(run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Sparse(os(&["src/lib"])))), 0);
    assert!(!wt.join("docs/b.md").exists());

    assert_eq!(run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Full)), 0);

    assert!(wt.join("docs/b.md").exists(), "full checkout restored");
    assert_ne!(worktree_config(&wt, "core.sparseCheckout").as_deref(), Some("true"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn full_on_an_already_full_worktree_is_a_clean_no_op() {
    let dir = tmpdir("full-noop");
    let repo = repo_with_worktrees(&dir, &[7]);
    let wt = repo.join(".loom/worktrees/issue-7");
    assert_eq!(run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Full)), 0);
    assert!(wt.join("docs/b.md").exists());
    let _ = fs::remove_dir_all(&dir);
}

/// Defect 3, the false-positive side. The retired `grep -q ".../issue-4"`
/// matched the registered `issue-44`, then ran `git -C issue-4 sparse-checkout
/// disable` — which git resolves UP to the main workspace — and wrote a
/// sentinel into the unregistered directory, authorizing its deletion.
#[test]
fn an_unregistered_dir_is_refused_even_beside_a_registered_superstring() {
    let dir = tmpdir("substring");
    let repo = repo_with_worktrees(&dir, &[44]);
    let unregistered = repo.join(".loom/worktrees/issue-4");
    fs::create_dir_all(&unregistered).expect("mkdir");
    fs::write(unregistered.join("precious.txt"), "not git's").expect("write");
    // Make the main workspace sparse-visible: if anything ran
    // `sparse-checkout` against it, `core.sparseCheckout` would appear.
    assert_eq!(worktree_config(&repo, "core.sparseCheckout"), None);

    for mode in [Mode::Full, Mode::Sparse(os(&["src/lib"]))] {
        let code = run(&opts(&repo, &unregistered, Arm::Reconfigure, mode));
        assert_eq!(code, 1, "refused");
    }

    assert!(
        !unregistered.join(".loom-managed").exists(),
        "no sentinel may be written into a directory git does not know about"
    );
    assert_eq!(
        fs::read_to_string(unregistered.join("precious.txt")).expect("still there"),
        "not git's"
    );
    assert_eq!(
        worktree_config(&repo, "core.sparseCheckout"),
        None,
        "git was never run against the main workspace"
    );
    assert!(repo.join("docs/b.md").exists(), "main workspace untouched");
    let _ = fs::remove_dir_all(&dir);
}

/// Defect 3, the false-negative side: a repo reached through a symlink. `git
/// worktree list` reports symlink-resolved paths, so the retired substring
/// match missed a LIVE worktree and refused it.
#[test]
fn a_worktree_reached_through_a_symlinked_repo_path_is_registered() {
    let dir = tmpdir("symlinked");
    let repo = repo_with_worktrees(&dir, &[7]);
    let alias = dir.join("alias to repo");
    std::os::unix::fs::symlink(&repo, &alias).expect("symlink");
    let wt_via_alias = alias.join(".loom/worktrees/issue-7");

    let code = run(&opts(&alias, &wt_via_alias, Arm::Reconfigure, Mode::Sparse(os(&["src/lib"]))));

    assert_eq!(code, 0);
    assert!(wt_via_alias.join(".loom-managed").exists());
    assert!(!wt_via_alias.join("docs/b.md").exists(), "cone applied");
    let _ = fs::remove_dir_all(&dir);
}

/// Defect 1. The retired script died here with git's 128, silently, having
/// sent both of git's streams to /dev/null.
#[test]
fn a_cone_git_rejects_fails_with_1_and_writes_no_sentinel() {
    let dir = tmpdir("rejected");
    let repo = repo_with_worktrees(&dir, &[7]);
    let wt = repo.join(".loom/worktrees/issue-7");

    let code = run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Sparse(os(&["src/*"]))));

    assert_eq!(code, 1);
    assert!(
        !wt.join(".loom-managed").exists(),
        "the retired arm never reached its sentinel write on this path either"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The one failure mode the retired script could hit past the cone — `cat >`
/// dying under `set -e` — and the port keeps it fatal: success must never be
/// reported over a worktree cleanup tooling will refuse to touch. A directory
/// named `.loom-managed` makes the write fail (EISDIR) without disturbing the
/// git steps before it, the way a crashed partial cleanup could.
#[test]
fn an_unwritable_sentinel_fails_with_1_after_the_cone_applied() {
    let dir = tmpdir("sentinel-unwritable");
    let repo = repo_with_worktrees(&dir, &[7]);
    let wt = repo.join(".loom/worktrees/issue-7");
    fs::create_dir(wt.join(".loom-managed")).expect("mkdir .loom-managed");

    let code = run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Sparse(os(&["src/lib"]))));

    assert_eq!(code, 1);
    assert!(
        wt.join(".loom-managed").is_dir(),
        "the failed write must not have clobbered the obstruction"
    );
    assert!(wt.join("src/lib/a.txt").exists(), "the cone itself applied");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_arm_configures_a_no_checkout_worktree() {
    let dir = tmpdir("create");
    let repo = repo_with_worktrees(&dir, &[]);
    let wt = repo.join(".loom/worktrees/issue-7");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--no-checkout",
            "-b",
            "feature/issue-7",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    assert!(!wt.join("top.txt").exists(), "fixture: nothing checked out yet");

    let code = run(&opts(&repo, &wt, Arm::Create, Mode::Sparse(os(&["src/lib"]))));

    assert_eq!(code, 0);
    assert!(wt.join("src/lib/a.txt").exists());
    assert!(wt.join("top.txt").exists());
    assert!(!wt.join("docs/b.md").exists());
    assert!(
        !wt.join(".loom-managed").exists(),
        "the create arm leaves the sentinel to its caller, which wrote it before calling"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn malformed_invocations_exit_2_and_touch_nothing() {
    let dir = tmpdir("usage");
    let repo = repo_with_worktrees(&dir, &[7]);
    let wt = repo.join(".loom/worktrees/issue-7");

    assert_eq!(run(&opts(&repo, &wt, Arm::Create, Mode::Full)), 2);
    assert_eq!(run(&opts(&repo, &wt, Arm::Reconfigure, Mode::Sparse(vec![]))), 2);
    assert!(!wt.join(".loom-managed").exists());
    let _ = fs::remove_dir_all(&dir);
}
