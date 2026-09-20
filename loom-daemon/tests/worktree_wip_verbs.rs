//! The WIP-shelving verbs, driven through the real binary (#8195 slice 2).
//!
//! # Why a subprocess suite rather than unit tests
//!
//! Every one of these verbs resolves its target from the process's **current
//! directory** (via `git rev-parse --git-common-dir`), which is exactly how
//! `worktree.sh` invoked them and exactly what a unit test cannot vary without
//! mutating process-global state shared with every other parallel test. The
//! port's own regression evidence has to come from driving the binary.
//!
//! Slice 1 learned this the expensive way: three defects in the ported lock
//! were invisible to twelve unit tests and only appeared when the real binary
//! ran twice (see `worktree_lock_subprocess.rs`).
//!
//! # What this file is evidence FOR
//!
//! The retained shell suites (`test-worktree-snapshot.sh` 15 assertions,
//! `test-worktree-stash-baseline.sh` 52) remain the behavioural
//! specification and run unchanged against this port — 67/67, matching their
//! pre-port baseline exactly. These tests cover what those suites
//! structurally cannot:
//!
//! - **Paths containing spaces and shell metacharacters** (#7858). The shell
//!   suites build fixtures under `/tmp/loom-…` — no spaces anywhere — so they
//!   could not have caught the unquoted-path class that turned an orphan-guard
//!   cleanup into an `rm -rf` on a LIVE worktree. Here the worktree root, the
//!   worktree directory and the files inside it all carry spaces, quotes,
//!   semicolons and `$(…)`.
//! - **The capture-before-reset ordering** that makes `stash-push`'s
//!   `git reset --hard HEAD` safe.
//! - **JSON that survives a hostile path**, which the shell's raw `printf`
//!   interpolation did not.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

// ---------------------------------------------------------------------------
// Fixture: a repo, optionally with a hostile path, plus managed worktrees
// ---------------------------------------------------------------------------

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
}

impl Fixture {
    /// A repo at `<tmp>/<dirname>` with one committed file, so `HEAD` exists
    /// and `git stash create` has something to diff against.
    fn new(dirname: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join(dirname);
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("tracked.txt"), "tracked file\n").expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        Self { _tmp: tmp, repo }
    }

    fn worktree_root(&self) -> PathBuf {
        self.repo.join(".loom/worktrees")
    }

    fn worktree(&self, issue: u32) -> PathBuf {
        self.worktree_root().join(format!("issue-{issue}"))
    }

    /// Add a managed worktree the way `worktree.sh` does: a linked worktree on
    /// its own branch, carrying the `.loom-managed` sentinel.
    fn add_worktree(&self, issue: u32) -> PathBuf {
        let path = self.worktree(issue);
        std::fs::create_dir_all(self.worktree_root()).expect("mkdir root");
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("feature/issue-{issue}"),
                &path.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(path.join(".loom-managed"), "# Loom-managed\n").expect("sentinel");
        path
    }

    /// Run a verb from `cwd`, exactly as the stub does.
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(bin())
            .arg("worktree-wip")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run worktree-wip")
    }

    fn run_in_repo(&self, args: &[&str]) -> Output {
        self.run(&self.repo.clone(), args)
    }

    fn stash_list(&self) -> String {
        stdout(&git_out(&self.repo, &["stash", "list"]))
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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn combined(o: &Output) -> String {
    format!("{}{}", stdout(o), String::from_utf8_lossy(&o.stderr))
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn ref_exists(dir: &Path, name: &str) -> bool {
    git_out(dir, &["rev-parse", "--verify", "--quiet", name])
        .status
        .success()
}

// ---------------------------------------------------------------------------
// #7858: a path containing a space must never be re-reachable
// ---------------------------------------------------------------------------

/// The directory name every hostile-path test builds under.
///
/// A space (the #7858 trigger), a single quote, a semicolon and a `$(…)`: in
/// bash each of those needs correct quoting at *every* interpolation, and one
/// missed quote is what produced an `rm -rf` on a live worktree. `Command::arg`
/// passes an `OsStr` to `execve` without a shell in between, so none of them
/// can be re-split.
const HOSTILE: &str = "my repo (v2); rm -rf $(echo x)";

#[test]
fn snapshot_round_trips_through_a_worktree_path_containing_spaces() {
    let f = Fixture::new(HOSTILE);
    let wt = f.add_worktree(701);
    assert!(
        wt.to_string_lossy().contains(' '),
        "the fixture must actually contain a space, or this test proves nothing"
    );

    std::fs::write(wt.join("tracked.txt"), "tracked file\nmodified\n").expect("dirty");
    let out = f.run_in_repo(&["snapshot", "701"]);
    assert_eq!(code(&out), 0, "snapshot failed: {}", combined(&out));

    let patches: Vec<_> = std::fs::read_dir(f.worktree_root().join(".snapshots"))
        .expect("snapshot dir exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(patches.len(), 1, "exactly one patch: {patches:?}");
    let body = std::fs::read_to_string(&patches[0]).expect("read patch");
    assert!(body.contains("modified"), "the patch must carry the diff; got {body:?}");

    // And the worktree is still there. In #7858 it was not.
    assert!(wt.is_dir(), "the live worktree must survive a snapshot");
    assert!(wt.join(".loom-managed").is_file(), "sentinel intact");
}

#[test]
fn the_stash_pair_round_trips_files_whose_names_contain_spaces() {
    let f = Fixture::new(HOSTILE);
    let wt = f.add_worktree(702);

    // Tracked change, plus an untracked file whose own NAME has a space and a
    // quote — the manifest is a newline-delimited list, so a name that needed
    // shell quoting to survive a `while read` round trip is the exact hazard.
    std::fs::write(wt.join("tracked.txt"), "tracked file\nwip\n").expect("dirty");
    let untracked = "a file with spaces & 'quotes'.txt";
    std::fs::write(wt.join(untracked), "untracked payload\n").expect("write untracked");

    let out = f.run_in_repo(&["stash-push", "702", "--include-untracked"]);
    assert_eq!(code(&out), 0, "stash-push failed: {}", combined(&out));
    assert_eq!(
        std::fs::read_to_string(wt.join("tracked.txt")).expect("read"),
        "tracked file\n",
        "the worktree must be reset to the clean baseline"
    );
    assert!(!wt.join(untracked).exists(), "the untracked file must have been moved out");

    let out = f.run_in_repo(&["stash-pop", "702"]);
    assert_eq!(code(&out), 0, "stash-pop failed: {}", combined(&out));
    assert_eq!(
        std::fs::read_to_string(wt.join("tracked.txt")).expect("read"),
        "tracked file\nwip\n",
        "the tracked diff must come back byte-for-byte"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join(untracked)).expect("read restored"),
        "untracked payload\n",
        "the space-named untracked file must come back byte-for-byte"
    );
    assert!(
        !f.worktree_root().join(".stash-baseline/issue-702").exists(),
        "a completed round trip must leave no pending state"
    );
}

#[test]
fn a_hostile_worktree_root_still_produces_parseable_json() {
    // Under LOOM_WORKTREE_ROOT the base path is operator-supplied, and the
    // shell interpolated it into `printf '…"%s"…'` raw.
    let f = Fixture::new("quote\"and space");
    let wt = f.add_worktree(703);
    std::fs::write(wt.join("tracked.txt"), "tracked file\njson\n").expect("dirty");

    let out = f.run_in_repo(&["snapshot", "703", "--json"]);
    assert_eq!(code(&out), 0, "snapshot --json failed: {}", combined(&out));
    let doc: serde_json::Value =
        serde_json::from_str(stdout(&out).trim()).expect("stdout must be valid JSON");
    assert_eq!(doc["success"], true);
    assert_eq!(doc["hasChanges"], true);
    let patch = doc["patchPath"].as_str().expect("patchPath is a string");
    assert!(Path::new(patch).is_file(), "the reported patchPath must exist: {patch}");
}

// ---------------------------------------------------------------------------
// The invariant that makes `git reset --hard` safe
// ---------------------------------------------------------------------------

#[test]
fn a_refused_push_never_resets_the_tree() {
    // stash-push's reset is the one irreversible operation in this family. It
    // must be unreachable on every path that does not have a successful
    // capture behind it — here, the second push while one is pending.
    let f = Fixture::new("plain");
    let wt = f.add_worktree(704);

    std::fs::write(wt.join("tracked.txt"), "tracked file\nfirst\n").expect("dirty");
    assert_eq!(code(&f.run_in_repo(&["stash-push", "704"])), 0);

    std::fs::write(wt.join("tracked.txt"), "tracked file\nsecond\n").expect("dirty again");
    let out = f.run_in_repo(&["stash-push", "704"]);
    assert_eq!(code(&out), 1, "a second push must be refused");
    assert!(
        combined(&out).contains("stash-pop 704"),
        "the refusal must name the restore command: {}",
        combined(&out)
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("tracked.txt")).expect("read"),
        "tracked file\nsecond\n",
        "the refused push must leave the uncaptured edit exactly where it was"
    );
}

#[test]
fn a_failed_restore_preserves_the_capture_instead_of_dropping_it() {
    let f = Fixture::new("plain");
    let wt = f.add_worktree(705);

    std::fs::write(wt.join("tracked.txt"), "tracked file\ncaptured\n").expect("dirty");
    assert_eq!(code(&f.run_in_repo(&["stash-push", "705"])), 0);

    // Make the tree conflict with the capture, so `git stash apply` must fail.
    std::fs::write(wt.join("tracked.txt"), "totally different\n").expect("conflict");
    let out = f.run_in_repo(&["stash-pop", "705"]);
    assert_eq!(code(&out), 1, "a conflicting restore must fail loudly");
    assert!(
        ref_exists(&wt, "refs/loom/stash-baseline/issue-705"),
        "the captured baseline must be PRESERVED — losing it here is data loss"
    );
    assert!(
        combined(&out).contains("PRESERVED"),
        "the error must say the capture survived: {}",
        combined(&out)
    );
}

#[test]
fn the_shared_stash_stack_is_never_touched() {
    // The whole reason these verbs exist (#4821). A `git stash push` from
    // another worktree landing mid-sequence must neither answer our pop nor be
    // consumed by it.
    let f = Fixture::new("plain");
    let mine = f.add_worktree(706);
    let theirs = f.add_worktree(707);

    std::fs::write(mine.join("tracked.txt"), "tracked file\nmine\n").expect("dirty");
    let before = f.stash_list();
    assert_eq!(code(&f.run_in_repo(&["stash-push", "706"])), 0);
    assert_eq!(before, f.stash_list(), "stash-push wrote to refs/stash");

    // A concurrent builder pushes onto the SHARED stack.
    std::fs::write(theirs.join("tracked.txt"), "tracked file\ntheirs\n").expect("dirty");
    git(&theirs, &["stash", "push", "-q", "-m", "concurrent"]);
    let depth_before = f.stash_list().lines().count();

    assert_eq!(code(&f.run_in_repo(&["stash-pop", "706"])), 0);
    let restored = std::fs::read_to_string(mine.join("tracked.txt")).expect("read");
    assert!(restored.contains("mine"), "must restore OUR wip: {restored:?}");
    assert!(
        !restored.contains("theirs"),
        "must not restore the other worktree's wip: {restored:?}"
    );
    assert_eq!(
        depth_before,
        f.stash_list().lines().count(),
        "stash-pop consumed an entry from the shared stack"
    );
}

// ---------------------------------------------------------------------------
// Exit-code contract (what the stub's LOOM_SCRIPT_HELPER_MISSING_RC=2 sits on)
// ---------------------------------------------------------------------------

#[test]
fn every_refusal_exits_one_and_never_two() {
    // 2 is reserved for "the binary could not run at all". If a verb ever
    // returned it for an ordinary refusal, an operator could not tell a bad
    // argument from a missing install.
    let f = Fixture::new("plain");
    f.add_worktree(708);

    let cases: Vec<Vec<&str>> = vec![
        vec!["snapshot"],                      // no target
        vec!["snapshot", "not-a-number"],      // bad target
        vec!["snapshot", "999999"],            // no such worktree
        vec!["snapshot", "708", "--nonsense"], // unknown flag
        vec!["snapshot", "708", "1", "2"],     // extra positional
        vec!["stash-push"],                    // no target
        vec!["stash-push", "not-a-target"],    // bad target
        vec!["stash-push", "999999"],          // no such worktree
        vec!["stash-pop", "MAIN"],             // case-folded 'main' is not 'main'
        vec!["stash-pop", "708"],              // nothing pending
        vec!["stash-pop", "999999"],           // no such worktree
    ];
    for args in cases {
        let out = f.run_in_repo(&args);
        assert_eq!(code(&out), 1, "{args:?} must exit 1, not {}: {}", code(&out), combined(&out));
    }
}

#[test]
fn the_already_clean_chain_exits_zero_end_to_end() {
    // `stash-push && <check> && stash-pop` on a clean tree must not break the
    // chain: that stall is what #5217 removed.
    let f = Fixture::new("plain");
    f.add_worktree(709);

    assert_eq!(code(&f.run_in_repo(&["stash-push", "709"])), 0);
    assert_eq!(code(&f.run_in_repo(&["stash-pop", "709"])), 0);
    assert!(
        !f.worktree_root().join(".stash-baseline/issue-709").exists(),
        "no pending state may survive a clean round trip"
    );
}

// ---------------------------------------------------------------------------
// Loom's own markers
// ---------------------------------------------------------------------------

#[test]
fn runtime_markers_are_never_carried_out_of_a_worktree() {
    // `--include-untracked` MOVES files. Carrying `.loom-managed` away makes
    // every cleanup path refuse the worktree afterwards (#3548), so the filter
    // is a correctness gate here, not noise reduction.
    let f = Fixture::new("plain");
    let wt = f.add_worktree(710);
    for m in [".loom-in-use", ".loom-checkpoint", ".no-changes-needed"] {
        std::fs::write(wt.join(m), "").expect("marker");
    }
    std::fs::write(wt.join("real-wip.txt"), "real\n").expect("wip");

    assert_eq!(code(&f.run_in_repo(&["stash-push", "710", "--include-untracked"])), 0);
    for m in [
        ".loom-managed",
        ".loom-in-use",
        ".loom-checkpoint",
        ".no-changes-needed",
    ] {
        assert!(wt.join(m).is_file(), "{m} must stay in the worktree");
    }
    assert!(!wt.join("real-wip.txt").exists(), "real WIP must have been moved out");
    assert_eq!(code(&f.run_in_repo(&["stash-pop", "710"])), 0);
    assert!(wt.join("real-wip.txt").is_file(), "real WIP must come back");
}

// ---------------------------------------------------------------------------
// The `main` target, resolved from the git COMMON dir
// ---------------------------------------------------------------------------

#[test]
fn the_main_target_resolves_the_primary_clone_even_from_a_worktree_cwd() {
    let f = Fixture::new("plain");
    let wt = f.add_worktree(711);
    std::fs::write(f.repo.join("tracked.txt"), "tracked file\nmain wip\n").expect("dirty main");

    // Invoked from INSIDE the worktree: `--git-dir` would resolve here,
    // `--git-common-dir` resolves the primary clone. Getting this wrong would
    // `git reset --hard` the wrong tree.
    let out = f.run(&wt, &["stash-push", "main"]);
    assert_eq!(code(&out), 0, "stash-push main failed: {}", combined(&out));
    assert_eq!(
        std::fs::read_to_string(f.repo.join("tracked.txt")).expect("read"),
        "tracked file\n",
        "the PRIMARY clone must have been reset"
    );
    assert!(ref_exists(&f.repo, "refs/loom/stash-baseline/main"));

    let out = f.run(&wt, &["stash-pop", "main"]);
    assert_eq!(code(&out), 0, "stash-pop main failed: {}", combined(&out));
    assert_eq!(
        std::fs::read_to_string(f.repo.join("tracked.txt")).expect("read"),
        "tracked file\nmain wip\n"
    );
    assert!(!ref_exists(&f.repo, "refs/loom/stash-baseline/main"));
}

#[test]
fn main_and_an_issue_target_do_not_interfere() {
    let f = Fixture::new("plain");
    let wt = f.add_worktree(712);
    std::fs::write(f.repo.join("tracked.txt"), "tracked file\nmain\n").expect("dirty main");
    std::fs::write(wt.join("tracked.txt"), "tracked file\nissue\n").expect("dirty issue");

    assert_eq!(code(&f.run_in_repo(&["stash-push", "main"])), 0);
    assert_eq!(
        code(&f.run_in_repo(&["stash-push", "712"])),
        0,
        "a pending main capture must not block an issue capture"
    );
    assert_eq!(code(&f.run_in_repo(&["stash-pop", "712"])), 0);
    assert_eq!(code(&f.run_in_repo(&["stash-pop", "main"])), 0);

    assert!(std::fs::read_to_string(f.repo.join("tracked.txt"))
        .expect("read")
        .contains("main"));
    assert!(std::fs::read_to_string(wt.join("tracked.txt"))
        .expect("read")
        .contains("issue"));
}

#[test]
fn json_reports_a_null_issue_number_for_main_and_a_numeric_one_for_an_issue() {
    let f = Fixture::new("plain");
    let wt = f.add_worktree(713);
    std::fs::write(f.repo.join("tracked.txt"), "tracked file\nm\n").expect("dirty");
    std::fs::write(wt.join("tracked.txt"), "tracked file\ni\n").expect("dirty");

    let out = f.run_in_repo(&["stash-push", "main", "--json"]);
    assert_eq!(code(&out), 0, "{}", combined(&out));
    assert_eq!(
        stdout(&out).lines().count(),
        1,
        "stdout must be exactly one JSON line: {:?}",
        stdout(&out)
    );
    let doc: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("valid JSON");
    assert_eq!(doc["target"], "main");
    assert!(doc["issueNumber"].is_null(), "main reports a null issueNumber");
    assert_eq!(doc["ref"], "refs/loom/stash-baseline/main");

    let out = f.run_in_repo(&["stash-push", "713", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("valid JSON");
    assert_eq!(doc["issueNumber"], 713, "issue targets stay numeric");

    let _ = f.run_in_repo(&["stash-pop", "713"]);
    let _ = f.run_in_repo(&["stash-pop", "main"]);
}

// ---------------------------------------------------------------------------
// Snapshot specifics
// ---------------------------------------------------------------------------

#[test]
fn snapshot_leaves_the_index_exactly_as_it_found_it() {
    // `--include-untracked` folds untracked files in via a temporary
    // `git add -N`. If the reset afterwards missed anything, the caller's
    // worktree would silently change state behind a read-only-sounding verb.
    let f = Fixture::new("plain");
    let wt = f.add_worktree(714);
    std::fs::write(wt.join("untracked.txt"), "brand new\n").expect("write");

    let before = stdout(&git_out(&wt, &["status", "--porcelain"]));
    assert_eq!(code(&f.run_in_repo(&["snapshot", "714", "--include-untracked"])), 0);
    let after = stdout(&git_out(&wt, &["status", "--porcelain"]));
    assert_eq!(before, after, "snapshot must not change the worktree's git status");
    assert!(
        before.contains("?? untracked.txt"),
        "the file must still be untracked: {before:?}"
    );
}

#[test]
fn a_clean_worktree_still_gets_an_empty_snapshot_rather_than_an_error() {
    let f = Fixture::new("plain");
    f.add_worktree(715);
    let out = f.run_in_repo(&["snapshot", "715", "--json"]);
    assert_eq!(code(&out), 0, "{}", combined(&out));
    let doc: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("valid JSON");
    assert_eq!(doc["hasChanges"], false);
    assert_eq!(doc["bytes"], 0);
    assert!(Path::new(doc["patchPath"].as_str().expect("str")).is_file());
}

#[test]
fn snapshot_does_not_accept_the_main_target() {
    // `snapshot` is issue-only; `main` has no `.snapshots` contract. Accepting
    // it would silently write `issue-main-….patch`.
    let f = Fixture::new("plain");
    let out = f.run_in_repo(&["snapshot", "main"]);
    assert_eq!(code(&out), 1);
    assert!(combined(&out).contains("must be numeric"), "got: {}", combined(&out));
}

// ---------------------------------------------------------------------------
// LOOM_WORKTREE_ROOT redirection
// ---------------------------------------------------------------------------

#[test]
fn an_overridden_worktree_root_redirects_snapshots_and_baselines_with_it() {
    // The override exists so the worktree base can live on another volume
    // (#3530); snapshots and baselines must follow it rather than stay behind
    // in the default `.loom/worktrees` path.
    let f = Fixture::new("plain");
    let ext = f._tmp.path().join("external root");
    std::fs::create_dir_all(&ext).expect("mkdir external");

    // `worktree_root` namespaces an override by repo basename.
    let base = ext.join("plain");
    let wt = base.join("issue-716");
    std::fs::create_dir_all(&base).expect("mkdir base");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/issue-716",
            &wt.to_string_lossy(),
            "main",
        ],
    );
    std::fs::write(wt.join("tracked.txt"), "tracked file\nredirected\n").expect("dirty");

    let out = Command::new(bin())
        .args(["worktree-wip", "snapshot", "716"])
        .env("LOOM_WORKTREE_ROOT", &ext)
        .current_dir(&f.repo)
        .output()
        .expect("run");
    assert_eq!(code(&out), 0, "{}", combined(&out));

    assert!(
        base.join(".snapshots").is_dir(),
        "the snapshot must land under the overridden root"
    );
    assert!(
        !f.worktree_root().join(".snapshots").exists(),
        "nothing may leak into the default path"
    );
}
