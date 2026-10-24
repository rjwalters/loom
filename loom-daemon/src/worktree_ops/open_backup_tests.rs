//! Tests for the pre-reap remote backup (#12458), against a local bare
//! `origin`: every state the operator named — clean, uncommitted, untracked,
//! unpushed, detached `HEAD` — plus the two negatives that matter most: a push
//! that fails must return `Err` (so the caller keeps the worktree), and an
//! ignored file must never reach the remote.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

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
        .env("LC_ALL", "C")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A bare `origin` and a clone `work` on branch `feature/issue-42`, pushed.
struct Fixture {
    _tmp: tempfile::TempDir,
    bare: PathBuf,
    work: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    let work = tmp.path().join("work");
    git(tmp.path(), &["init", "-q", "--bare", "origin.git"]);
    git(tmp.path(), &["init", "-q", "-b", "main", "work"]);
    fs::write(work.join("README.md"), "hello\n").unwrap();
    fs::write(work.join(".gitignore"), "secret.env\nignored-dir/\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", "feature/issue-42"]);
    fs::write(work.join("src.rs"), "fn main() {}\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    git(&work, &["push", "-q", "-u", "origin", "feature/issue-42"]);
    Fixture {
        _tmp: tmp,
        bare,
        work,
    }
}

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-20T12:34:56Z")
        .unwrap()
        .with_timezone(&Utc)
}

/// The sha `origin` holds at `ref_name`, read from the bare repo itself.
fn remote_sha(bare: &Path, ref_name: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn remote_tree(bare: &Path, sha: &str) -> Vec<String> {
    git(bare, &["ls-tree", "-r", "--name-only", sha])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn backup_ref_name_nests_item_branch_and_stamp_under_the_loom_namespace() {
    assert_eq!(
        backup_ref_name("issue-42", Some("feature/issue-42"), at()),
        "refs/loom/backup/issue-42/feature/issue-42/20261020T123456Z"
    );
    assert_eq!(
        backup_ref_name("pr-7", None, at()),
        "refs/loom/backup/pr-7/detached/20261020T123456Z"
    );
}

#[test]
fn a_clean_pushed_worktree_anchors_head_itself_on_the_remote() {
    let f = fixture();
    let head = git(&f.work, &["rev-parse", "HEAD"]);
    let rec = backup_to_remote(&f.work, "issue-42", at()).expect("backup");
    assert_eq!(rec.remote, "origin");
    assert_eq!(rec.ref_name, "refs/loom/backup/issue-42/feature/issue-42/20261020T123456Z");
    assert_eq!(rec.sha, head, "nothing beyond HEAD: the ref anchors HEAD");
    assert_eq!(rec.head, head);
    assert_eq!(rec.branch.as_deref(), Some("feature/issue-42"));
    assert!(!rec.captured_changes());
    assert_eq!(remote_sha(&f.bare, &rec.ref_name).as_deref(), Some(head.as_str()));
    assert!(rec.describe().contains(&rec.ref_name) && rec.describe().contains("git fetch origin"));
}

#[test]
fn uncommitted_tracked_untracked_and_staged_changes_are_all_in_the_snapshot_and_the_worktree_is_untouched(
) {
    let f = fixture();
    let head = git(&f.work, &["rev-parse", "HEAD"]);
    fs::write(f.work.join("src.rs"), "fn main() { changed }\n").unwrap();
    fs::write(f.work.join("notes.txt"), "untracked user work\n").unwrap();
    fs::create_dir_all(f.work.join("deep/er")).unwrap();
    fs::write(f.work.join("deep/er/file.md"), "nested untracked\n").unwrap();
    fs::write(f.work.join("staged.rs"), "staged only\n").unwrap();
    git(&f.work, &["add", "staged.rs"]);
    fs::write(f.work.join("ita.rs"), "intent to add\n").unwrap();
    git(&f.work, &["add", "-N", "ita.rs"]);
    fs::remove_file(f.work.join("README.md")).unwrap();
    let status_before = git(&f.work, &["status", "--porcelain=v1", "-z"]);
    let index_before = git(&f.work, &["write-tree"]);

    let rec = backup_to_remote(&f.work, "issue-42", at()).expect("backup");

    assert!(rec.captured_changes());
    assert_ne!(rec.sha, head);
    assert_eq!(git(&f.bare, &["rev-parse", &format!("{}^", rec.sha)]), head, "parent is HEAD");
    assert_eq!(remote_sha(&f.bare, &rec.ref_name).as_deref(), Some(rec.sha.as_str()));
    let tree = remote_tree(&f.bare, &rec.sha);
    for want in [
        "src.rs",
        "notes.txt",
        "deep/er/file.md",
        "staged.rs",
        "ita.rs",
        ".gitignore",
    ] {
        assert!(tree.iter().any(|p| p == want), "{want} missing from {tree:?}");
    }
    assert!(!tree.iter().any(|p| p == "README.md"), "the deletion is captured too");
    assert_eq!(
        git(&f.bare, &["show", &format!("{}:src.rs", rec.sha)]),
        "fn main() { changed }",
        "the modified content, not HEAD's"
    );

    // The worktree, its index, its branch and HEAD are exactly as they were.
    assert_eq!(git(&f.work, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&f.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/issue-42");
    assert_eq!(git(&f.work, &["status", "--porcelain=v1", "-z"]), status_before);
    assert_eq!(git(&f.work, &["write-tree"]), index_before, "the real index is untouched");
    assert!(
        !git_ok(&f.work, &["rev-parse", "--verify", "--quiet", "refs/stash"]),
        "no stash"
    );
    assert_eq!(fs::read_to_string(f.work.join("notes.txt")).unwrap(), "untracked user work\n");
}

#[test]
fn ignored_files_build_trees_and_loom_markers_are_never_pushed() {
    let f = fixture();
    fs::write(f.work.join("secret.env"), "TOKEN=not-really\n").unwrap();
    fs::create_dir_all(f.work.join("ignored-dir")).unwrap();
    fs::write(f.work.join("ignored-dir/blob"), "ignored\n").unwrap();
    // One marker ignored (the `loom-managed` .gitignore block), one not — a
    // pathspec-based exclusion fails on the first, a name filter on neither.
    fs::write(f.work.join(".git/info/exclude"), ".loom-managed\n").unwrap();
    fs::write(f.work.join(".loom-managed"), "").unwrap();
    fs::write(f.work.join(".loom-in-use"), "{}").unwrap();
    // A content-verified cargo build tree that is NOT gitignored (#11075).
    fs::create_dir_all(f.work.join("target/debug")).unwrap();
    fs::write(
        f.work.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n",
    )
    .unwrap();
    fs::write(
        f.work.join("target/.rustc_info.json"),
        r#"{"rustc_fingerprint":123,"outputs":{}}"#,
    )
    .unwrap();
    fs::write(f.work.join("target/debug/bin"), vec![0u8; 64]).unwrap();
    // ...and one real untracked file, so a snapshot commit is actually built.
    fs::write(f.work.join("real.txt"), "real\n").unwrap();

    let rec = backup_to_remote(&f.work, "issue-42", at()).expect("backup");
    assert!(rec.captured_changes());
    let tree = remote_tree(&f.bare, &rec.sha);
    assert!(tree.iter().any(|p| p == "real.txt"), "{tree:?}");
    for never in [
        "secret.env",
        "ignored-dir/blob",
        ".loom-managed",
        ".loom-in-use",
        "target/CACHEDIR.TAG",
        "target/debug/bin",
    ] {
        assert!(!tree.iter().any(|p| p == never), "{never} must not be pushed: {tree:?}");
    }
    // Nothing of the ignored content exists as an object on the remote either.
    let blob = git(&f.work, &["hash-object", "secret.env"]);
    assert!(!git_ok(&f.bare, &["cat-file", "-e", &blob]), "ignored blob reached the remote");
}

#[test]
fn unpushed_commits_reach_the_remote_through_the_backup_ref_without_touching_the_branch() {
    let f = fixture();
    let pushed_tip = git(&f.work, &["rev-parse", "HEAD"]);
    fs::write(f.work.join("more.rs"), "more\n").unwrap();
    git(&f.work, &["add", "more.rs"]);
    git(&f.work, &["commit", "-q", "-m", "never pushed"]);
    let local_tip = git(&f.work, &["rev-parse", "HEAD"]);
    assert!(!git_ok(&f.bare, &["cat-file", "-e", &local_tip]), "precondition: not on remote");

    let rec = backup_to_remote(&f.work, "issue-42", at()).expect("backup");
    assert_eq!(rec.sha, local_tip, "clean tree: the ref anchors the unpushed tip");
    assert_eq!(remote_sha(&f.bare, &rec.ref_name).as_deref(), Some(local_tip.as_str()));
    assert!(git_ok(&f.bare, &["cat-file", "-e", &local_tip]), "the commit is on the remote");
    assert_eq!(
        remote_sha(&f.bare, "refs/heads/feature/issue-42").as_deref(),
        Some(pushed_tip.as_str()),
        "the branch itself is not moved — no CI, no PR synchronize"
    );
}

#[test]
fn a_detached_head_is_backed_up_under_detached() {
    let f = fixture();
    let head = git(&f.work, &["rev-parse", "HEAD"]);
    git(&f.work, &["checkout", "-q", "--detach"]);
    fs::write(f.work.join("scratch.txt"), "detached work\n").unwrap();
    let rec = backup_to_remote(&f.work, "pr-7", at()).expect("backup");
    assert_eq!(rec.branch, None);
    assert_eq!(rec.ref_name, "refs/loom/backup/pr-7/detached/20261020T123456Z");
    assert_eq!(rec.head, head);
    assert!(rec.captured_changes());
    assert!(remote_tree(&f.bare, &rec.sha)
        .iter()
        .any(|p| p == "scratch.txt"));
}

#[test]
fn a_failed_push_is_an_error_and_leaves_the_worktree_and_its_files_alone() {
    let f = fixture();
    fs::write(f.work.join("precious.txt"), "do not lose\n").unwrap();
    let missing = f.bare.with_file_name("no-such-remote.git");
    git(&f.work, &["remote", "set-url", "origin", missing.to_str().unwrap()]);
    let err = backup_to_remote(&f.work, "issue-42", at()).expect_err("push must fail");
    assert!(err.contains("push of refs/loom/backup/"), "{err}");
    assert_eq!(fs::read_to_string(f.work.join("precious.txt")).unwrap(), "do not lose\n");
    assert!(!git_ok(&f.work, &["rev-parse", "--verify", "--quiet", "refs/stash"]));
    assert_eq!(git(&f.work, &["status", "--porcelain"]), "?? precious.txt");
}

#[test]
fn a_missing_origin_remote_is_an_error_before_anything_is_built() {
    let f = fixture();
    git(&f.work, &["remote", "remove", "origin"]);
    let err = backup_to_remote(&f.work, "issue-42", at()).expect_err("no origin");
    assert!(err.contains("remote 'origin' is not configured"), "{err}");
}

#[test]
fn a_rejected_push_is_an_error() {
    // The ref already exists on the remote at an UNRELATED commit (an orphan):
    // a non fast-forward update, which `git push` refuses without `--force`,
    // and this module never forces.
    let f = fixture();
    let empty_tree = git(&f.work, &["hash-object", "-t", "tree", "/dev/null"]);
    let orphan = git(&f.work, &["commit-tree", &empty_tree, "-m", "unrelated"]);
    let ref_name = backup_ref_name("issue-42", Some("feature/issue-42"), at());
    git(&f.work, &["push", "-q", "origin", &format!("{orphan}:{ref_name}")]);
    fs::write(f.work.join("x.txt"), "x\n").unwrap();
    let err = backup_to_remote(&f.work, "issue-42", at()).expect_err("rejected");
    assert!(err.contains("push of refs/loom/backup/") && err.contains("failed"), "{err}");
    assert_eq!(remote_sha(&f.bare, &ref_name).as_deref(), Some(orphan.as_str()), "untouched");
}

#[test]
fn an_untracked_nested_repository_is_refused_rather_than_recorded_as_an_empty_gitlink() {
    let f = fixture();
    let nested = f.work.join("vendor/dep");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    fs::write(nested.join("lib.rs"), "nested work\n").unwrap();
    let err = backup_to_remote(&f.work, "issue-42", at()).expect_err("nested repo");
    assert!(err.contains("nested repository 'vendor/dep/'"), "{err}");
    assert!(
        remote_sha(&f.bare, &backup_ref_name("issue-42", Some("feature/issue-42"), at())).is_none()
    );
}

#[test]
fn a_directory_that_is_not_a_worktree_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(backup_to_remote(tmp.path(), "issue-1", at()).is_err());
    assert!(backup_to_remote(&tmp.path().join("absent"), "issue-1", at()).is_err());
}

#[test]
fn the_temporary_index_is_removed_afterwards() {
    // Deterministic, not a scan of the temp dir: sibling tests in this
    // process hold their own temporary indexes concurrently.
    let index = TempIndex::new().unwrap();
    let path = index.path.clone();
    let lock = PathBuf::from(format!("{}.lock", index.path_str()));
    fs::write(&path, b"index bytes").unwrap();
    fs::write(&lock, b"").unwrap();
    assert!(path.exists() && lock.exists());
    drop(index);
    assert!(!path.exists(), "index file left behind");
    assert!(!lock.exists(), "index lock left behind");
}
