//! Unit tests for the upstream-tracking correction and drift report (#8195
//! slice 9).
//!
//! # What is pinned here and what is pinned elsewhere
//!
//! `tests/worktree_upstream_differential.rs` owns the **message** comparison —
//! it runs the real binary and a frozen copy of the retired shell over 86
//! scenarios and compares stdout byte for byte. Nothing here duplicates that;
//! [`super::Reporter`] writes to the process's real stdout, so an in-process
//! test could not read it back without changing the port's shape to suit the
//! test.
//!
//! What this file owns instead is everything the differential *cannot* see:
//! the helpers' contracts in isolation (where a "simplification" would pass
//! every scenario in the corpus and still be wrong), and the side-effect
//! properties stated positively rather than as agreement with another
//! implementation — agreement is worth exactly as much as the thing agreed
//! with, and the retired shell is not a specification of what *should*
//! happen, only of what *did*.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scratch directory. `tag` may contain a space — several cases need one
/// (#7858's class).
fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-upstream-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
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
    out
}

/// A repo at `<dir>/work` with a bare `origin`, a `main` commit and a feature
/// branch. `dir` and `branch` may contain characters a shell would mangle.
///
/// `push_branch` controls whether `origin/<branch>` exists at all — the
/// difference between "reuse an in-flight PR branch" and "a local branch that
/// was never pushed".
fn repo(dir: &Path, branch: &str, push_branch: bool) -> PathBuf {
    let origin = dir.join("origin.git");
    let work = dir.join("work");
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main", "--bare"])
        .arg(&origin)
        .status()
        .expect("git init --bare")
        .success());
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&work)
        .status()
        .expect("git init")
        .success());
    git(&work, &["config", "user.name", "Loom Test"]);
    git(&work, &["config", "user.email", "t@example.invalid"]);
    git(&work, &["remote", "add", "origin", origin.to_str().expect("utf-8")]);
    fs::write(work.join("base.txt"), "base\n").expect("write");
    git(&work, &["add", "base.txt"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", branch]);
    fs::write(work.join("work.txt"), "work\n").expect("write");
    git(&work, &["add", "work.txt"]);
    git(&work, &["commit", "-q", "-m", "work"]);
    if push_branch {
        git(&work, &["push", "-q", "origin", branch]);
    }
    work
}

fn upstream_of(work: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(work)
        .args(["rev-parse", "--abbrev-ref", &format!("{branch}@{{u}}")])
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn opts(repo: &Path, branch: &str, arm: Arm) -> Options {
    Options {
        repo: repo.to_path_buf(),
        branch: branch.to_string(),
        arm,
        quiet: true,
        issue: "1".into(),
        uncommitted: false,
    }
}

// ---------------------------------------------------------------------------
// The helper contracts the corpus cannot distinguish
// ---------------------------------------------------------------------------

/// [`git_stdout_lossy`] must return stdout **even when git fails**, because
/// `$(git … || true)` does. This is not pedantry: `git rev-parse` echoes its
/// argument back on stdout before failing on an unknown ref, so a
/// success-only helper would report an empty `wt_origin_tip` where the
/// retired shell reported the literal `origin/<branch>`.
///
/// Every scenario in the differential corpus runs this call behind a
/// `show-ref --verify` guard that guarantees the ref exists, so the corpus
/// cannot tell the two helpers apart. This test can.
#[test]
fn stdout_is_captured_even_when_git_exits_non_zero() {
    let dir = tmpdir("lossy");
    let work = repo(&dir, "feature/x", false);

    let out = git_stdout_lossy(&work, &["rev-parse", "origin/definitely-missing"]);
    assert_eq!(
        out, "origin/definitely-missing",
        "git echoes an unresolvable rev back on stdout and then fails; the \
         helper must report it, the way command substitution does"
    );
    assert_eq!(
        git_status_code(&work, &["rev-parse", "origin/definitely-missing"]),
        128,
        "…and it really did fail — otherwise the case above proves nothing"
    );
}

/// A git that cannot be spawned at all answers 127, bash's own report.
#[test]
fn a_missing_directory_does_not_panic() {
    let dir = tmpdir("missing");
    let absent = dir.join("no such repo");
    assert_ne!(git_status_code(&absent, &["rev-parse", "HEAD"]), 0);
    assert_eq!(git_stdout_lossy(&absent, &["rev-parse", "HEAD"]), "");
    // And the whole entry point degrades to a silent 0 rather than erroring.
    assert_eq!(run(&opts(&absent, "feature/x", Arm::RegisteredWorktree)), 0);
}

#[test]
fn the_two_arms_use_different_subject_nouns() {
    assert_eq!(Arm::LocalBranch.subject(), "Branch");
    assert_eq!(Arm::RegisteredWorktree.subject(), "Worktree branch");
}

// ---------------------------------------------------------------------------
// The repair, stated positively
// ---------------------------------------------------------------------------

/// #6095/#6100: a branch tracking the default branch instead of its own
/// remote branch is repointed. Both arms, because the whole reason this is
/// one module is that `worktree.sh` had two copies of this and they drifted.
#[test]
fn a_wrong_upstream_is_repointed_on_both_arms() {
    for (arm, tag) in [
        (Arm::LocalBranch, "arm-local"),
        (Arm::RegisteredWorktree, "arm-wt"),
    ] {
        let dir = tmpdir(tag);
        let work = repo(&dir, "feature/x", true);
        git(&work, &["branch", "-q", "--set-upstream-to=origin/main", "feature/x"]);
        assert_eq!(upstream_of(&work, "feature/x"), "origin/main");

        assert_eq!(run(&opts(&work, "feature/x", arm)), 0);
        assert_eq!(
            upstream_of(&work, "feature/x"),
            "origin/feature/x",
            "{arm:?} did not repoint the upstream"
        );
    }
}

/// The repair runs under `--quiet` too. `worktree.sh` gated only the
/// `print_*` lines on `$JSON_OUTPUT`, never the git commands, so a `--json`
/// invocation still fixed the branch. Silencing the repair along with the
/// narration would be a behaviour change that no message comparison could
/// catch, because under `--quiet` there are no messages to compare.
#[test]
fn quiet_silences_the_narration_not_the_repair() {
    let dir = tmpdir("quiet");
    let work = repo(&dir, "feature/x", true);
    git(&work, &["branch", "-q", "--set-upstream-to=origin/main", "feature/x"]);

    let mut o = opts(&work, "feature/x", Arm::LocalBranch);
    o.quiet = true;
    assert_eq!(run(&o), 0);
    assert_eq!(upstream_of(&work, "feature/x"), "origin/feature/x");
}

/// The retained suite's Test 4. A branch with no `origin/<branch>` ref must
/// come out of this with no upstream — the whole body sits behind the
/// `show-ref --verify` guard precisely so that nothing is invented.
#[test]
fn a_never_pushed_branch_gets_no_fabricated_upstream() {
    let dir = tmpdir("neverpushed");
    let work = repo(&dir, "feature/x", false);
    assert_eq!(upstream_of(&work, "feature/x"), "");

    assert_eq!(run(&opts(&work, "feature/x", Arm::LocalBranch)), 0);
    assert_eq!(
        upstream_of(&work, "feature/x"),
        "",
        "an upstream was fabricated for a branch origin has never seen"
    );
}

/// #7858's class. A repo whose path contains a space is repaired normally —
/// and, on the registered-worktree arm, that path is what gets interpolated
/// into three remediation hints.
#[test]
fn a_path_containing_spaces_is_handled_whole() {
    let dir = tmpdir("spaced");
    let nested = dir.join("a dir with spaces");
    fs::create_dir_all(&nested).expect("mkdir");
    let work = repo(&nested, "feature/x", true);
    git(&work, &["branch", "-q", "--set-upstream-to=origin/main", "feature/x"]);

    assert_eq!(run(&opts(&work, "feature/x", Arm::RegisteredWorktree)), 0);
    assert_eq!(upstream_of(&work, "feature/x"), "origin/feature/x");
    assert!(
        work.to_string_lossy().contains(' '),
        "fixture no longer has a space in its path; the case is vacuous"
    );
}

/// A refname carrying shell metacharacters (`;`, `'`, `$`, `` ` `` — all legal
/// in a git refname) is a branch name, not a command fragment.
///
/// None of these contain a space: `git check-ref-format` rejects spaces
/// outright, so `feature/x;touch pwned` is not a reachable input and a fixture
/// using one would fail at `git checkout -b` rather than at the property. The
/// command-injection shape survives without it — `;touch-pwned` still ends the
/// statement and names a command.
#[test]
fn a_refname_with_metacharacters_is_not_interpreted() {
    for (i, branch) in [
        "feature/x;touch-pwned",
        "feature/x'q",
        "feature/x$y",
        "feature/x`y`",
    ]
    .into_iter()
    .enumerate()
    {
        // Keyed on the index, not on the branch, because several of these
        // names are the same length and a length-keyed tmpdir would have each
        // one delete the previous case's fixture.
        let dir = tmpdir(&format!("meta-{i}"));
        let work = repo(&dir, branch, true);
        git(&work, &["branch", "-q", "--set-upstream-to=origin/main", branch]);

        assert_eq!(run(&opts(&work, branch, Arm::LocalBranch)), 0);
        assert_eq!(upstream_of(&work, branch), format!("origin/{branch}"));
        assert!(
            !work.join("pwned").exists(),
            "a metacharacter in the branch name reached a shell"
        );
    }
}

// ---------------------------------------------------------------------------
// The drift report is warn-only, and only for the behind case
// ---------------------------------------------------------------------------

/// Push one commit further than the checkout keeps, so HEAD is a strict
/// ancestor of the pushed tip.
fn make_behind(work: &Path, branch: &str) {
    fs::write(work.join("pushed.txt"), "only on origin\n").expect("write");
    git(work, &["add", "pushed.txt"]);
    git(work, &["commit", "-q", "-m", "pushed ahead"]);
    git(work, &["push", "-q", "origin", branch]);
    git(work, &["reset", "-q", "--hard", "HEAD~1"]);
}

/// #6257's report prints; it does not pull. A worktree behind its pushed tip
/// must still be behind it afterwards, and uncommitted work must survive —
/// the retained suite asserts both, and they are the difference between a
/// diagnosis and a remedy nobody asked for.
#[test]
fn the_drift_report_changes_nothing() {
    let dir = tmpdir("driftnoop");
    let work = repo(&dir, "feature/x", true);
    make_behind(&work, "feature/x");
    fs::write(work.join("work.txt"), "local WIP that must survive\n").expect("write");

    let head_before = git_stdout_lossy(&work, &["rev-parse", "HEAD"]);
    let tip = git_stdout_lossy(&work, &["rev-parse", "origin/feature/x"]);
    assert_ne!(head_before, tip, "fixture is not actually behind");

    let mut o = opts(&work, "feature/x", Arm::RegisteredWorktree);
    o.uncommitted = true;
    assert_eq!(run(&o), 0);

    assert_eq!(
        git_stdout_lossy(&work, &["rev-parse", "HEAD"]),
        head_before,
        "the drift report moved HEAD — it is warn-only"
    );
    assert_eq!(
        fs::read_to_string(work.join("work.txt")).expect("read"),
        "local WIP that must survive\n",
        "uncommitted work was modified by a warn-only check"
    );
}

/// The retained suite's Test 2, as a property of the predicate rather than of
/// a message: *ahead* is not *behind*. The check is
/// `HEAD != tip && merge-base --is-ancestor HEAD tip`, and dropping the
/// second half — the mutation a reader is most likely to think redundant
/// next to the first — turns every unpushed local commit into a false
/// "this worktree may be stale".
#[test]
fn ahead_and_diverged_are_not_ancestors_of_the_tip() {
    // Ahead: one unpushed local commit.
    let dir = tmpdir("ahead");
    let work = repo(&dir, "feature/x", true);
    fs::write(work.join("local.txt"), "unpushed\n").expect("write");
    git(&work, &["add", "local.txt"]);
    git(&work, &["commit", "-q", "-m", "unpushed"]);
    let head = git_stdout_lossy(&work, &["rev-parse", "HEAD"]);
    let tip = git_stdout_lossy(&work, &["rev-parse", "origin/feature/x"]);
    assert_ne!(head, tip, "ahead fixture must differ from the tip");
    assert_ne!(
        git_status_code(&work, &["merge-base", "--is-ancestor", &head, &tip]),
        0,
        "an AHEAD worktree read as an ancestor of the tip — the drift report \
         would false-positive on every unpushed commit"
    );

    // Diverged: both sides moved.
    let dir2 = tmpdir("diverged");
    let work2 = repo(&dir2, "feature/x", true);
    make_behind(&work2, "feature/x");
    fs::write(work2.join("local.txt"), "divergent\n").expect("write");
    git(&work2, &["add", "local.txt"]);
    git(&work2, &["commit", "-q", "-m", "divergent"]);
    let head2 = git_stdout_lossy(&work2, &["rev-parse", "HEAD"]);
    let tip2 = git_stdout_lossy(&work2, &["rev-parse", "origin/feature/x"]);
    assert_ne!(
        git_status_code(&work2, &["merge-base", "--is-ancestor", &head2, &tip2]),
        0,
        "a DIVERGED worktree read as an ancestor of the tip"
    );
}

/// The `local-branch` arm has no drift report at all — it runs before any
/// worktree exists, so there is no checkout that could be stale. Structural,
/// not cosmetic: giving both arms the report would make `worktree.sh` print a
/// staleness warning about a directory it is about to create.
#[test]
fn the_local_branch_arm_runs_no_drift_report() {
    let dir = tmpdir("nodrift");
    let work = repo(&dir, "feature/x", true);
    make_behind(&work, "feature/x");
    git(&work, &["checkout", "-q", "main"]);

    let head_before = git_stdout_lossy(&work, &["rev-parse", "HEAD"]);
    assert_eq!(run(&opts(&work, "feature/x", Arm::LocalBranch)), 0);
    assert_eq!(git_stdout_lossy(&work, &["rev-parse", "HEAD"]), head_before);
    // The correction half still ran.
    assert_eq!(upstream_of(&work, "feature/x"), "origin/feature/x");
}
