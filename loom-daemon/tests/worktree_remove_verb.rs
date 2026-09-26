//! The `remove` verb, driven through the real binary (#8195 slice 3).
//!
//! # Why a subprocess suite rather than unit tests
//!
//! The verb resolves the repo, the worktree root and the worktree itself from
//! the process's **current directory** (`git rev-parse --git-common-dir`),
//! exactly as `worktree.sh remove` did, and it reads `LOOM_WORKTREE_ROOT` from
//! the environment. Neither can be varied from a unit test without mutating
//! process-global state shared with every other parallel test. Slice 1 learned
//! this the expensive way: three defects in the ported lock were invisible to
//! twelve unit tests and only appeared when the real binary ran twice.
//!
//! # What this file is evidence FOR
//!
//! The three retained shell suites remain the behavioural specification and
//! run unchanged against this port — `test-worktree-remove.sh` 26/26,
//! `test-worktree-remove-squash-merge.sh` 10/10,
//! `test-cargo-target-dir-reclaim.sh` 44/44, each matching its pre-port
//! baseline exactly. These tests cover what those suites structurally cannot:
//!
//! - **Paths containing spaces and shell metacharacters** (#7858). Every
//!   fixture in the shell suites lives under `/tmp/loom-…` — not one space
//!   anywhere — so none of them has ever exercised the unquoted-path class
//!   that turned an orphan-guard cleanup into an `rm -rf` on a LIVE worktree.
//!   Here the worktree root, the worktree directory, the branch name and the
//!   dirty file all carry spaces, quotes, semicolons and `$(…)`.
//! - **The sentinel contract under a hostile path**, which is the exact
//!   combination (destructive operation + attacker-shaped path) #7858 was.
//! - **`--json` on a hostile path**, which the shell's raw `printf`
//!   interpolation could not produce parseable output for.
//! - **The #5177 orphaned-directory fallback**, whose three-part gate is the
//!   only `rm -rf` in the verb — pinned from both sides: it must fire on the
//!   orphan shape and must NOT fire on any other removal failure. That second
//!   half is `a_removal_that_fails_for_any_other_reason_never_falls_back_to_rm_rf`,
//!   and it is the only test in the repository that fails when the gate is
//!   widened to `Err(_)`; all 80 retained shell assertions stay green.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

/// A path shaped like every reason bash needed quoting. Used for the repo
/// directory, a worktree root override, and file names inside a worktree.
const HOSTILE: &str = "my repo (v2); rm -rf $(echo x) 'quoted'";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    /// `LOOM_WORKTREE_ROOT` for every invocation, when overridden.
    worktree_root_override: Option<PathBuf>,
}

impl Fixture {
    fn new(dirname: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join(dirname);
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("tracked.txt"), "tracked\n").expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        Self {
            _tmp: tmp,
            repo,
            worktree_root_override: None,
        }
    }

    /// Redirect the worktree base the way `LOOM_WORKTREE_ROOT` does (#3530) —
    /// here to a directory whose own name contains spaces and metacharacters.
    fn with_worktree_root(mut self, name: &str) -> Self {
        let root = self._tmp.path().join(name);
        std::fs::create_dir_all(&root).expect("mkdir root");
        self.worktree_root_override = Some(root);
        self
    }

    /// Where worktrees actually land. An override is namespaced by repo
    /// basename (`${override}/<repo-basename>`) so several workspaces can
    /// share one external volume — the port reuses `worktree_root::worktree_root`
    /// for this, so the fixture has to agree with it.
    fn worktree_root(&self) -> PathBuf {
        match &self.worktree_root_override {
            Some(root) => root.join(self.repo.file_name().expect("repo basename")),
            None => self.repo.join(".loom/worktrees"),
        }
    }

    fn worktree(&self, issue: u32) -> PathBuf {
        self.worktree_root().join(format!("issue-{issue}"))
    }

    /// Add a managed worktree the way `worktree.sh` does: a linked worktree on
    /// its own branch, carrying the `.loom-managed` sentinel.
    fn add_worktree(&self, issue: u32) -> PathBuf {
        self.add_worktree_named(issue, &format!("feature/issue-{issue}"))
    }

    fn add_worktree_named(&self, issue: u32, branch: &str) -> PathBuf {
        let path = self.worktree(issue);
        std::fs::create_dir_all(self.worktree_root()).expect("mkdir root");
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                &path.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(path.join(".loom-managed"), "# Loom-managed\n").expect("sentinel");
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_inner(args, None)
    }

    /// [`Fixture::run`] with a `git` on `PATH` that fails `worktree remove`
    /// with `message` and passes every other subcommand through to the real
    /// binary.
    ///
    /// This is the only way to reach the #5177 fallback's *classifier* from a
    /// test: every naturally reachable failure of `git worktree remove
    /// --force` on a sentinel-bearing managed worktree is the "is not a
    /// working tree" shape, so without a shim the "any OTHER failure must not
    /// trigger `rm -rf`" half of the gate is unreachable and therefore
    /// unverified.
    fn run_with_failing_worktree_remove(&self, args: &[&str], message: &str) -> Output {
        let shim_dir = self._tmp.path().join("git-shim");
        std::fs::create_dir_all(&shim_dir).expect("mkdir shim");
        let real_git = which_git();
        let shim = shim_dir.join("git");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\n\
                 # Pass everything through EXCEPT `worktree remove`, which fails\n\
                 # with a cause the #5177 classifier must not accept.\n\
                 saw_worktree=0; saw_remove=0\n\
                 for a in \"$@\"; do\n\
                 \x20 [ \"$a\" = worktree ] && saw_worktree=1\n\
                 \x20 [ \"$a\" = remove ] && saw_remove=1\n\
                 done\n\
                 if [ \"$saw_worktree\" = 1 ] && [ \"$saw_remove\" = 1 ]; then\n\
                 \x20 echo {message} >&2\n\
                 \x20 exit 1\n\
                 fi\n\
                 exec {real} \"$@\"\n",
                message = shell_quote(message),
                real = shell_quote(&real_git.to_string_lossy()),
            ),
        )
        .expect("write shim");
        make_executable(&shim);
        self.run_inner(args, Some(&shim_dir))
    }

    fn run_inner(&self, args: &[&str], path_prefix: Option<&Path>) -> Output {
        let mut cmd = Command::new(bin());
        cmd.arg("worktree-remove")
            .args(args)
            .current_dir(&self.repo)
            // The forge must never be consulted from a test: `branch_landed`'s
            // rung 3 would otherwise shell out to whatever `gh`/`loom-daemon`
            // the host has. The offline seam is the shell library's own.
            .env("LOOM_BRANCH_LANDED_OFFLINE", "1");
        if let Some(root) = &self.worktree_root_override {
            cmd.env("LOOM_WORKTREE_ROOT", root);
        }
        if let Some(prefix) = path_prefix {
            let existing = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{existing}", prefix.display()));
        }
        cmd.output().expect("run worktree-remove")
    }

    fn branch_exists(&self, branch: &str) -> bool {
        git_out(
            &self.repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .status
        .success()
    }

    fn registered(&self, path: &Path) -> bool {
        let out = stdout(&git_out(&self.repo, &["worktree", "list", "--porcelain"]));
        let want = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        out.lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .any(|p| {
                let p = PathBuf::from(p);
                p.canonicalize().unwrap_or(p) == want
            })
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

/// The real `git`, resolved before any shim is put on `PATH`.
fn which_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|c| c.is_file())
        .expect("a git on PATH")
}

/// Single-quote for `/bin/sh`. The shim is generated text, so the one thing it
/// must not do is reintroduce the quoting class this port exists to remove.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_p: &Path) {}

// ---------------------------------------------------------------------------
// #7858: paths containing spaces and shell metacharacters
// ---------------------------------------------------------------------------

/// The regression the whole port exists for, in its most dangerous form: a
/// worktree whose ROOT, whose own directory and whose branch all contain
/// spaces and shell metacharacters is removed correctly — and only it.
///
/// In bash every one of those interpolations had to be quoted at every use
/// site; here nothing word-splits, because `Command::arg` and `remove_dir_all`
/// take values, not command lines.
#[test]
fn a_worktree_under_a_hostile_path_is_removed_cleanly() {
    let f = Fixture::new(HOSTILE).with_worktree_root("wt root; with spaces & 'quotes'");
    let wt = f.add_worktree(501);
    // A sibling that must survive, placed so a word-split of the target path
    // would plausibly hit it.
    let sibling = f.add_worktree(502);

    let out = f.run(&["501"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!wt.exists(), "target worktree survived");
    assert!(sibling.exists(), "a sibling worktree was destroyed");
    assert!(!f.registered(&wt));
    assert!(f.registered(&sibling));
    assert!(
        !f.branch_exists("feature/issue-501"),
        "the attached branch should have been deleted (it is an ancestor of main)"
    );
    assert!(f.branch_exists("feature/issue-502"));
}

/// The dirty guard (#4449) must fire on a file whose NAME is hostile, and the
/// refusal must name it. A shell that word-split here would have reported the
/// wrong count and, worse, listed a path the operator could not act on.
#[test]
fn a_hostile_filename_still_trips_the_dirty_guard_and_is_named() {
    let f = Fixture::new(HOSTILE);
    let wt = f.add_worktree(503);
    let dirty = wt.join("a file; with $(metachars) 'and quotes'.txt");
    std::fs::write(&dirty, "work that must not be destroyed\n").expect("write");

    let out = f.run(&["503"]);
    assert_eq!(code(&out), 1, "must refuse");
    assert!(dirty.exists(), "uncommitted work was destroyed");
    assert!(wt.exists());
    let err = stderr(&out);
    assert!(err.contains("Refusing to remove"), "stderr: {err}");
    assert!(
        err.contains("a file; with $(metachars) 'and quotes'.txt"),
        "the refusal must name the file it found: {err}"
    );
    assert!(err.contains("1 uncommitted change(s)"), "stderr: {err}");
}

/// …and `--force` removes exactly that worktree, nothing adjacent.
#[test]
fn force_removes_a_dirty_hostile_worktree_and_only_it() {
    let f = Fixture::new(HOSTILE).with_worktree_root("root with spaces");
    let wt = f.add_worktree(504);
    let sibling = f.add_worktree(505);
    std::fs::write(wt.join("wip; $(x).txt"), "throwaway\n").expect("write");

    let out = f.run(&["504", "--force"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!wt.exists());
    assert!(sibling.exists());
}

/// A hostile path must still produce ONE parseable JSON document on stdout,
/// with every human line on stderr. The shell interpolated the path into
/// `printf '…"%s"…'` raw, so a `"` or `\` produced a document no consumer
/// could parse.
#[test]
fn json_mode_survives_a_hostile_path_and_keeps_stdout_pure() {
    let f = Fixture::new(HOSTILE).with_worktree_root(r#"root "quoted" \ and 'single'"#);
    let wt = f.add_worktree(506);

    let out = f.run(&["506", "--json"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let doc = stdout(&out);
    assert_eq!(doc.lines().count(), 1, "stdout must be exactly one JSON line, got: {doc:?}");
    let parsed: serde_json::Value = serde_json::from_str(doc.trim()).expect("parseable JSON");
    assert_eq!(parsed["success"], serde_json::json!(true));
    assert_eq!(parsed["removed"], serde_json::json!(true));
    assert_eq!(parsed["issueNumber"], serde_json::json!(506));
    assert_eq!(parsed["worktreePath"].as_str().map(PathBuf::from), Some(wt.clone()));
    // The human lines went somewhere — just not to stdout.
    assert!(stderr(&out).contains("Removing worktree"));
}

// ---------------------------------------------------------------------------
// The sentinel contract
// ---------------------------------------------------------------------------

/// Only sentinel-bearing worktrees are ever removed. Under a hostile path this
/// is the #7858 shape exactly: a destructive operation deciding on a path that
/// used to be word-split.
#[test]
fn a_sentinel_less_worktree_is_refused_even_under_a_hostile_path() {
    let f = Fixture::new(HOSTILE).with_worktree_root("root with spaces");
    let wt = f.add_worktree(507);
    std::fs::remove_file(wt.join(".loom-managed")).expect("drop the sentinel");

    let out = f.run(&["507"]);
    assert_eq!(code(&out), 1);
    assert!(wt.exists(), "a user-provisioned worktree was removed");
    assert!(stderr(&out).contains("lacks .loom-managed sentinel"));
}

/// The #5177 fallback is the verb's only `rm -rf`. It fires when git has no
/// record of the path AND the sentinel is present AND the path is under the
/// managed worktree root — all three, never on a bare removal failure.
#[test]
fn an_orphaned_sentinel_bearing_directory_is_removed_by_the_fallback() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(508);
    // Detach it from git's registry while leaving the directory on disk: the
    // exact state a stale `git worktree prune` leaves behind.
    let admin = f.repo.join(".git/worktrees/issue-508");
    std::fs::remove_dir_all(&admin).expect("drop the admin dir");
    git(&f.repo, &["worktree", "prune"]);
    assert!(wt.exists(), "precondition: the directory is still on disk");
    assert!(!f.registered(&wt), "precondition: git no longer tracks it");

    let out = f.run(&["508"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!wt.exists());
    // Human lines go to STDOUT outside `--json` mode, exactly as the shell's
    // `print_success` did.
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("Removed untracked worktree directory"), "{all}");
}

/// The negative half: the SAME orphaned shape without a sentinel must survive.
///
/// Note precisely what this does and does NOT prove. It refuses at guard 2
/// (the sentinel check) and never reaches the fallback at all, so it pins the
/// sentinel contract — not the fallback's blast radius. Deleting all three
/// proofs from `should_force_remove_orphan_dir` leaves this test GREEN; that
/// was measured, and it is why the two tests below exist.
#[test]
fn an_orphaned_directory_without_a_sentinel_survives_the_fallback() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(509);
    std::fs::remove_dir_all(f.repo.join(".git/worktrees/issue-509")).expect("drop the admin dir");
    git(&f.repo, &["worktree", "prune"]);
    std::fs::remove_file(wt.join(".loom-managed")).expect("drop the sentinel");

    let out = f.run(&["509"]);
    assert_eq!(code(&out), 1);
    assert!(wt.exists(), "a sentinel-less orphan was rm -rf'd");
}

/// **The blast-radius test.** `git worktree remove --force` failing for a
/// reason that is NOT the #5177 orphan shape must leave the directory alone.
///
/// This is the single highest-consequence assertion in the port: the fallback
/// is the only `rm -rf` in the verb, and everything that reaches it is a
/// sentinel-bearing worktree under the managed root — i.e. two of the gate's
/// three proofs are already satisfied, so the error classification is the only
/// thing standing between "git refused for some other reason" and a recursive
/// delete of a LIVE worktree. Widening the arm to `Err(_)` (the shape a
/// refactor most plausibly reaches for) passes all 26 assertions of
/// `test-worktree-remove.sh`, all 10 of `test-worktree-remove-squash-merge.sh`
/// and all 44 of `test-cargo-target-dir-reclaim.sh` — measured, not assumed.
/// Only this test fails.
#[test]
fn a_removal_that_fails_for_any_other_reason_never_falls_back_to_rm_rf() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(510);
    std::fs::write(wt.join("work.txt"), "committed work\n").expect("write");
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "work"]);

    // A registered, sentinel-bearing, contained worktree whose removal fails
    // with a cause the classifier must reject.
    let out = f.run_with_failing_worktree_remove(
        &["510"],
        "fatal: validation failed, cannot remove working tree",
    );

    assert_eq!(code(&out), 1, "a failed removal is exit 1");
    assert!(wt.exists(), "a live worktree was rm -rf'd after a non-#5177 removal failure");
    assert!(wt.join("work.txt").exists(), "committed work was destroyed");
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        all.contains("Could not remove worktree at"),
        "the failure must be reported, not silently swallowed: {all}"
    );
    assert!(
        !all.contains("Removed untracked worktree directory"),
        "the #5177 fallback must not claim to have run: {all}"
    );
}

/// The same shim, with the cause the classifier SHOULD accept — so the test
/// above cannot pass merely because the shim broke every path.
///
/// Together the two pin the classifier from both sides: this one fails if the
/// gate is tightened into never firing, its sibling fails if it is widened
/// into firing on anything.
#[test]
fn the_fallback_still_fires_on_the_orphan_shape_under_the_same_shim() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(511);

    let out =
        f.run_with_failing_worktree_remove(&["511"], "fatal: 'issue-511' is not a working tree");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!wt.exists(), "the orphan shape must still be reclaimed");
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("Removed untracked worktree directory"), "{all}");
}

// ---------------------------------------------------------------------------
// Exit-code contract
// ---------------------------------------------------------------------------

/// 0 = removed OR the idempotent no-op; 1 = refused or failed. Nothing else,
/// because 2 is reserved by the stub for "no binary could be resolved".
#[test]
fn the_exit_codes_are_the_shells_exit_codes() {
    let f = Fixture::new("plain-repo");

    // Idempotent no-op on a worktree that was never there.
    let out = f.run(&["999999"]);
    assert_eq!(code(&out), 0);
    assert!(
        stdout(&out).contains("nothing to remove") || stderr(&out).contains("nothing to remove")
    );

    // Usage errors are 1, never clap's 2 — see the stub's
    // LOOM_SCRIPT_HELPER_MISSING_RC argument.
    for args in [
        vec!["--keep-branch"],
        vec!["not-a-number"],
        vec!["1", "2"],
        vec!["1", "--nope"],
    ] {
        let out = f.run(&args);
        assert_eq!(code(&out), 1, "args {args:?} must exit 1, not clap's 2");
    }
}

/// `--dry-run` reports the plan and changes nothing — the same decision path a
/// real run takes, which is what makes the preview trustworthy.
#[test]
fn dry_run_reports_the_plan_and_changes_nothing() {
    let f = Fixture::new(HOSTILE).with_worktree_root("root with spaces");
    let wt = f.add_worktree(510);

    let out = f.run(&["510", "--dry-run", "--json"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(wt.exists(), "--dry-run removed the worktree");
    assert!(f.branch_exists("feature/issue-510"));
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&out).trim()).expect("parseable JSON");
    assert_eq!(parsed["dryRun"], serde_json::json!(true));
    assert_eq!(parsed["removed"], serde_json::json!(false));
    assert_eq!(parsed["branchStatus"], serde_json::json!("dry-run"));
    assert!(stderr(&out).contains("Would remove worktree"));
    assert!(stderr(&out).contains("Would delete local branch 'feature/issue-510'"));
}

/// `--keep-branch` removes the worktree and keeps the branch, including for a
/// CUSTOM branch name — the `worktree.sh <N> <custom-branch>` shape, which is
/// why the attached branch is read from the porcelain rather than constructed
/// from the issue number. (Git refnames cannot contain spaces, so the hostile
/// input here is the repo path around it, not the branch itself.)
#[test]
fn keep_branch_preserves_a_custom_branch_name() {
    let f = Fixture::new(HOSTILE);
    let wt = f.add_worktree_named(511, "spike/custom-branch-name");

    let out = f.run(&["511", "--keep-branch", "--json"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!wt.exists());
    assert!(f.branch_exists("spike/custom-branch-name"));
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&out).trim()).expect("parseable JSON");
    assert_eq!(parsed["branch"], serde_json::json!("spike/custom-branch-name"));
    assert_eq!(parsed["branchStatus"], serde_json::json!("kept"));
}

// ---------------------------------------------------------------------------
// The branch-delete safety rule
// ---------------------------------------------------------------------------

/// A branch carrying unmerged work is NEVER force-deleted, even though the
/// worktree holding it is removed. With the forge offline and the tree
/// differing, the verdict is `not-landed`, so the rule stays on `git branch -d`
/// — which refuses, loudly.
#[test]
fn a_branch_with_unmerged_commits_survives_the_removal() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(512);
    std::fs::write(wt.join("unmerged.txt"), "never pushed anywhere\n").expect("write");
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "unmerged work"]);

    let out = f.run(&["512"]);
    assert_eq!(code(&out), 0, "the worktree removal itself still succeeds");
    assert!(!wt.exists());
    assert!(
        f.branch_exists("feature/issue-512"),
        "unmerged work was force-deleted — the data-loss regression this rule prevents"
    );
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("may have unpushed commits"), "the refusal must say why: {all}");
}

/// The `-D` escalation requires proof. A branch whose commits ARE on main
/// (here by ancestry, the cheapest rung) is deleted, and the message says on
/// what evidence — so an operator reading the log can tell a proven
/// force-delete from a lucky one.
#[test]
fn a_landed_branch_is_deleted_and_the_evidence_is_named() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(513);
    std::fs::write(wt.join("shipped.txt"), "shipped\n").expect("write");
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "shipped work"]);
    // Fast-forward main onto it: now the branch is reachable from main.
    git(&f.repo, &["merge", "-q", "--ff-only", "feature/issue-513"]);

    let out = f.run(&["513", "--json"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!f.branch_exists("feature/issue-513"));
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("safe force-delete"), "{all}");
    assert!(all.contains("ancestor"), "the evidence rung must be named: {all}");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&out).trim()).expect("parseable JSON");
    assert_eq!(parsed["branchStatus"], serde_json::json!("deleted"));
}

/// The default branch is never deleted, however it got attached to a worktree.
/// Belt-and-suspenders, and the one refusal whose absence would be
/// catastrophic rather than merely lossy.
#[test]
fn the_default_branch_is_never_deleted() {
    let f = Fixture::new("plain-repo");
    // A worktree attached to a branch literally named `main` — the misdetection
    // this guard exists for.
    let path = f.worktree(514);
    std::fs::create_dir_all(f.worktree_root()).expect("mkdir root");
    git(&f.repo, &["branch", "spare", "main"]);
    git(&f.repo, &["checkout", "-q", "spare"]);
    git(&f.repo, &["worktree", "add", "-q", &path.to_string_lossy(), "main"]);
    std::fs::write(path.join(".loom-managed"), "# Loom-managed\n").expect("sentinel");

    let out = f.run(&["514"]);
    assert_eq!(code(&out), 0);
    assert!(!path.exists(), "the worktree itself is still removed");
    assert!(f.branch_exists("main"), "the default branch must never be deleted");
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("it is the repository's default branch"), "{all}");
}

// ---------------------------------------------------------------------------
// The removal ledger (#5950)
// ---------------------------------------------------------------------------

/// Every Loom-owned removal writes one attributable line, and the mechanism is
/// still the shell's string so one `grep`/`jq` reads pre- and post-port
/// history together.
#[test]
fn a_removal_is_recorded_in_the_ledger_under_the_shells_mechanism() {
    let f = Fixture::new(HOSTILE);
    f.add_worktree(515);
    let out = f.run(&["515"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));

    let ledger = f.repo.join(".loom/logs/worktree-removals.log");
    let text = std::fs::read_to_string(&ledger).expect("ledger written");
    let line = text.lines().last().expect("at least one entry");
    let parsed: serde_json::Value =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("ledger line is JSON: {e} in {line}"));
    assert_eq!(parsed["mechanism"], serde_json::json!("worktree.sh remove"));
    assert_eq!(parsed["reason"], serde_json::json!("explicit_remove"));
    assert_eq!(parsed["branch"], serde_json::json!("feature/issue-515"));
    // The hostile repo path round-trips through the ledger's own escaping.
    assert!(parsed["worktree"]
        .as_str()
        .is_some_and(|p| p.contains(HOSTILE)));
}

/// A REFUSED removal must leave no ledger entry: the ledger's value is
/// symmetric, so a line that did not correspond to a removal would make its
/// absence stop being evidence.
#[test]
fn a_refused_removal_writes_no_ledger_entry() {
    let f = Fixture::new("plain-repo");
    let wt = f.add_worktree(516);
    std::fs::write(wt.join("wip.txt"), "work\n").expect("write");

    let out = f.run(&["516"]);
    assert_eq!(code(&out), 1);
    let ledger = f.repo.join(".loom/logs/worktree-removals.log");
    assert!(
        !ledger.exists()
            || std::fs::read_to_string(&ledger)
                .unwrap_or_default()
                .is_empty(),
        "a refusal must not be recorded as a removal"
    );
}
