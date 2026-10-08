//! Tests for the checkout fast-forward (#10869).
//!
//! Real git, in temp dirs only: a bare `origin`, a `seed` clone that stands
//! for everyone else pushing to it, and a `host` clone that is the main
//! checkout under test. No network, no registry, no registered workspace:
//! every pass is handed its roots explicitly.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use tempfile::TempDir;

use super::git::{classify_refusal, Refusal};
use super::host::{hold_for_move, hold_for_self_update};
use super::*;
use crate::fleet_sync::FleetSyncStatus;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn identify(repo: &Path) {
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "user.email", "test@example.invalid"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

/// A bare origin, a seed clone and the host's main checkout.
struct Fixture {
    tmp: TempDir,
    origin: PathBuf,
    seed: PathBuf,
    host: PathBuf,
    branch: &'static str,
}

impl Fixture {
    fn new() -> Self {
        Self::on("main")
    }

    fn on(branch: &'static str) -> Self {
        let tmp = TempDir::new().unwrap();
        // Canonical, so a path compares equal however it was reached.
        let base = fs::canonicalize(tmp.path()).unwrap();
        let origin = base.join("origin.git");
        let seed = base.join("seed");
        let host = base.join("host");
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--bare", &format!("--initial-branch={branch}")]);
        git(&base, &["clone", "--quiet", origin.to_str().unwrap(), "seed"]);
        identify(&seed);
        write(&seed.join("README.md"), "one\n");
        write(&seed.join(".loom/scripts/spawn.sh"), "#!/bin/sh\necho old\n");
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "first"]);
        git(&seed, &["push", "--quiet", "origin", &format!("HEAD:{branch}")]);
        let fx = Self {
            tmp,
            origin,
            seed,
            host,
            branch,
        };
        fx.clone_as("host");
        fx
    }

    fn base(&self) -> PathBuf {
        fs::canonicalize(self.tmp.path()).unwrap()
    }

    /// Another checkout of the same origin.
    fn clone_as(&self, name: &str) -> PathBuf {
        git(&self.base(), &["clone", "--quiet", self.origin.to_str().unwrap(), name]);
        let path = self.base().join(name);
        identify(&path);
        path
    }

    /// Someone else lands a commit on the default branch.
    fn push(&self, path: &str, contents: &str) -> String {
        git(&self.seed, &["pull", "--quiet", "--ff-only", "origin", self.branch]);
        write(&self.seed.join(path), contents);
        git(&self.seed, &["add", "-A"]);
        git(&self.seed, &["commit", "--quiet", "-m", &format!("change {path}")]);
        git(
            &self.seed,
            &[
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:{}", self.branch),
            ],
        );
        self.origin_tip()
    }

    fn origin_tip(&self) -> String {
        git(&self.origin, &["rev-parse", self.branch])
    }

    fn head(&self) -> String {
        git(&self.host, &["rev-parse", "HEAD"])
    }

    /// A local commit on the host's checked-out branch, not pushed.
    fn commit_locally(&self, path: &str) {
        write(&self.host.join(path), "local\n");
        git(&self.host, &["add", "-A"]);
        git(&self.host, &["commit", "--quiet", "-m", "local work"]);
    }
}

/// Everything "left byte-for-byte unchanged" means: HEAD, the branch it is
/// on, the index file's bytes, and every file in the working tree.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    head: String,
    on: String,
    index: Vec<u8>,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

fn snapshot(repo: &Path) -> Snapshot {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(repo, repo, &mut files);
    let on = git_in(repo, &["symbolic-ref", "--quiet", "HEAD"]);
    Snapshot {
        head: git(repo, &["rev-parse", "HEAD"]),
        on: String::from_utf8_lossy(&on.stdout).trim().to_string(),
        index: fs::read(repo.join(".git/index")).unwrap_or_default(),
        files,
    }
}

/// The knobs of one pass. Everything defaults to "the timer, with
/// `fleet.autoApply` on, nothing else going on".
struct Knobs {
    write: bool,
    gate: Box<dyn Fn(&Path) -> bool>,
    updating: bool,
    fetched: bool,
    budget: Option<Duration>,
    elapsed: Box<dyn Fn() -> Duration>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            write: true,
            gate: Box::new(|_| false),
            updating: false,
            fetched: false,
            budget: None,
            elapsed: Box::new(|| Duration::ZERO),
        }
    }
}

fn pass_over(roots: &[PathBuf], knobs: &Knobs, memory: &mut Memory) -> CheckoutPass {
    let env = Env {
        write: knobs.write,
        gate_in_flight: &*knobs.gate,
        hold: &|_| (!knobs.updating).then(MoveHold::free),
        fetched: &|_, _| knobs.fetched,
        budget: knobs.budget,
        elapsed: &*knobs.elapsed,
        clock: &t0,
    };
    run(&env, roots, memory)
}

/// One pass over the fixture's host checkout, with fresh memory.
fn pass(fx: &Fixture, knobs: &Knobs) -> CheckoutPass {
    pass_over(std::slice::from_ref(&fx.host), knobs, &mut Memory::default())
}

fn only(pass: &CheckoutPass) -> &CheckoutReport {
    assert_eq!(pass.checkouts.len(), 1, "{pass:?}");
    &pass.checkouts[0]
}

/// Assert a pass skipped the host checkout in `state` and changed nothing.
fn assert_skipped(fx: &Fixture, knobs: &Knobs, state: CheckoutState) -> CheckoutReport {
    let before = snapshot(&fx.host);
    let found = pass(fx, knobs);
    let report = only(&found).clone();
    assert_eq!(report.state, state, "{report:?}");
    assert_eq!(snapshot(&fx.host), before, "a skipped checkout must not change at all");
    report
}

// ------------------------------------------------------------------------
// The fast-forward
// ------------------------------------------------------------------------

#[test]
fn a_clean_checkout_that_is_behind_is_fast_forwarded() {
    let fx = Fixture::new();
    let old = fx.head();
    fx.push("src/a.rs", "fn a() {}\n");
    let tip = fx.push("src/b.rs", "fn b() {}\n");

    let found = pass(&fx, &Knobs::default());

    let report = only(&found);
    assert_eq!(report.state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip, "HEAD is the origin tip");
    assert_eq!(report.branch.as_deref(), Some("main"));
    assert_eq!(report.behind, Some(0));
    assert_eq!(report.head.as_deref(), Some(&tip[..12]));
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "", "and the tree is clean");
    // One INFO line, naming the old and the new commit.
    assert_eq!(found.transitions.len(), 1);
    let t = &found.transitions[0];
    assert_eq!((t.kind, t.level), ("fast-forwarded", Level::Info));
    assert!(t.line.contains(&old[..12]) && t.line.contains(&tip[..12]), "{}", t.line);
    assert!(t.line.contains("2 commit(s)"), "{}", t.line);
}

#[test]
fn a_current_checkout_is_not_written_and_not_reported() {
    let fx = Fixture::new();
    let before = snapshot(&fx.host);

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::Current);
    assert_eq!(snapshot(&fx.host), before);
    assert!(found.transitions.is_empty(), "{:?}", found.transitions);
    assert!(lines(&found.checkouts).is_empty(), "status shows only what is not current");
}

#[test]
fn a_repo_whose_default_branch_is_not_main_is_fast_forwarded() {
    let fx = Fixture::on("master");
    let tip = fx.push("src/a.rs", "fn a() {}\n");

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded);
    assert_eq!(only(&found).branch.as_deref(), Some("master"));
    assert_eq!(fx.head(), tip);
}

#[test]
fn a_repo_whose_default_branch_cannot_be_resolved_is_skipped() {
    // No `origin` at all: nothing to resolve a default branch from, and no
    // `main` is guessed.
    let tmp = TempDir::new().unwrap();
    let repo = fs::canonicalize(tmp.path()).unwrap().join("lonely");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    identify(&repo);
    write(&repo.join("a"), "a\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "first"]);
    let before = snapshot(&repo);

    let found = pass_over(std::slice::from_ref(&repo), &Knobs::default(), &mut Memory::default());

    assert_eq!(only(&found).state, CheckoutState::NoDefaultBranch);
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn the_merge_runs_with_hooks_disabled() {
    let fx = Fixture::new();
    let hook = fx.host.join(".git/hooks/post-merge");
    write(&hook, "#!/bin/sh\necho ran >> hook-ran.txt\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let tip = fx.push("src/a.rs", "fn a() {}\n");

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip);
    assert!(!fx.host.join("hook-ran.txt").exists(), "the post-merge hook must not run");

    // The control: the same hook does run for a merge made by hand, so its
    // absence above is the daemon's doing and not a hook that never fires.
    fx.push("src/b.rs", "fn b() {}\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    git(&fx.host, &["merge", "--quiet", "--ff-only", "origin/main"]);
    assert!(fx.host.join("hook-ran.txt").exists(), "the hook is live");
}

// ------------------------------------------------------------------------
// Local work is never touched
// ------------------------------------------------------------------------

#[test]
fn a_dirty_checkout_is_left_alone_staged_or_unstaged() {
    // Unstaged.
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    write(&fx.host.join("README.md"), "edited here\n");
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(report.behind, Some(1));
    assert!(report.detail.as_deref().unwrap().contains("README.md"), "{report:?}");

    // Staged.
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    write(&fx.host.join("README.md"), "edited here\n");
    git(&fx.host, &["add", "README.md"]);
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert!(report.detail.as_deref().unwrap().contains("README.md"), "{report:?}");
}

#[test]
fn an_untracked_file_in_the_way_blocks_and_one_that_is_not_survives() {
    // In the way of an incoming file: git refuses, nothing is touched.
    let fx = Fixture::new();
    fx.push("notes.txt", "from upstream\n");
    write(&fx.host.join("notes.txt"), "mine, never committed\n");
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::WouldOverwrite);
    assert!(report.detail.as_deref().unwrap().contains("notes.txt"), "{report:?}");

    // Not in the way: it does not make the checkout dirty, and it is still
    // there afterwards.
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    write(&fx.host.join("scratch.txt"), "mine, never committed\n");
    let found = pass(&fx, &Knobs::default());
    assert_eq!(only(&found).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip);
    assert_eq!(
        fs::read_to_string(fx.host.join("scratch.txt")).unwrap(),
        "mine, never committed\n"
    );
}

#[test]
fn an_ignored_untracked_file_in_the_way_is_not_overwritten() {
    // git's default is to silently replace an ignored file a merge needs the
    // path of. This step discards nothing.
    let fx = Fixture::new();
    write(&fx.host.join(".git/info/exclude"), "local.cfg\n");
    write(&fx.host.join("local.cfg"), "mine, ignored, never committed\n");
    fx.push("local.cfg", "from upstream\n");

    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::WouldOverwrite);

    assert!(report.detail.as_deref().unwrap().contains("local.cfg"), "{report:?}");
    assert_eq!(
        fs::read_to_string(fx.host.join("local.cfg")).unwrap(),
        "mine, ignored, never committed\n"
    );
}

#[test]
fn unpushed_commits_are_ahead_or_diverged_and_left_alone() {
    // Ahead only.
    let fx = Fixture::new();
    fx.commit_locally("local.rs");
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::Ahead);
    assert_eq!((report.ahead, report.behind), (Some(1), Some(0)));

    // Ahead and behind.
    let fx = Fixture::new();
    fx.commit_locally("local.rs");
    fx.push("src/a.rs", "fn a() {}\n");
    fx.push("src/b.rs", "fn b() {}\n");
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::Diverged);
    assert_eq!((report.ahead, report.behind), (Some(1), Some(2)));
}

#[test]
fn another_branch_or_a_detached_head_is_left_alone() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["checkout", "--quiet", "-b", "feature/x"]);
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::WrongBranch);
    assert!(report.detail.as_deref().unwrap().contains("feature/x"), "{report:?}");

    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["checkout", "--quiet", "--detach"]);
    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::WrongBranch);
    assert!(report.detail.as_deref().unwrap().contains("detached"), "{report:?}");
}

#[test]
fn an_operation_in_progress_is_left_alone_whichever_it_is() {
    // One case per marker `in_special_git_state` reads, each on a checkout
    // that would otherwise be fast-forwarded.
    for marker in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "BISECT_LOG",
        "REVERT_HEAD",
    ] {
        let fx = Fixture::new();
        fx.push("src/a.rs", "fn a() {}\n");
        let path = fx.host.join(".git").join(marker);
        if marker.starts_with("rebase") {
            fs::create_dir_all(&path).unwrap();
        } else {
            // A real commit id, as git itself would have written.
            write(&path, &format!("{}\n", fx.head()));
        }
        let before = fx.head();
        let found = pass(&fx, &Knobs::default());
        assert_eq!(only(&found).state, CheckoutState::MidOperation, "{marker}");
        assert_eq!(fx.head(), before, "{marker}");
    }
}

#[test]
fn a_real_merge_in_progress_is_left_byte_for_byte_alone() {
    let fx = Fixture::new();
    // A conflicting local side branch, merged into main and left unresolved.
    git(&fx.host, &["checkout", "--quiet", "-b", "side"]);
    write(&fx.host.join("README.md"), "side\n");
    git(&fx.host, &["commit", "--quiet", "-am", "side"]);
    git(&fx.host, &["checkout", "--quiet", "main"]);
    write(&fx.host.join("README.md"), "main\n");
    git(&fx.host, &["commit", "--quiet", "-am", "main"]);
    assert!(!git_in(&fx.host, &["merge", "side"]).status.success(), "the merge conflicts");
    fx.push("src/a.rs", "fn a() {}\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::MidOperation);
}

// ------------------------------------------------------------------------
// Things that are building in the checkout
// ------------------------------------------------------------------------

#[test]
fn nothing_moves_while_a_gate_run_is_in_flight_and_it_moves_on_the_next_pass() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    let mut memory = Memory::default();
    let roots = [fx.host.clone()];
    let before = snapshot(&fx.host);

    let gated = Knobs {
        gate: Box::new(|_| true),
        ..Knobs::default()
    };
    let found = pass_over(&roots, &gated, &mut memory);
    assert_eq!(only(&found).state, CheckoutState::GateInFlight);
    assert_eq!(snapshot(&fx.host), before);
    assert!(found.transitions.is_empty(), "one pass of a transient state is not reported");

    let found = pass_over(&roots, &Knobs::default(), &mut memory);
    assert_eq!(only(&found).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip);
}

#[test]
fn a_gate_run_that_starts_after_the_first_check_still_stops_the_write() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    // Not in flight when rule 4 asks; in flight by the time the merge is due.
    let asked = Rc::new(Cell::new(0u32));
    let count = asked.clone();
    let knobs = Knobs {
        gate: Box::new(move |_| {
            count.set(count.get() + 1);
            count.get() > 1
        }),
        ..Knobs::default()
    };

    assert_skipped(&fx, &knobs, CheckoutState::GateInFlight);
    assert_eq!(asked.get(), 2, "asked at rule 4 and again right before the write");
}

#[test]
fn nothing_moves_in_a_checkout_the_self_update_is_running_in() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    let updating = Knobs {
        updating: true,
        ..Knobs::default()
    };
    assert_skipped(&fx, &updating, CheckoutState::SelfUpdateInFlight);

    // The real hold, as `auto_update` takes it around its update script.
    let other = fx.clone_as("other");
    let held = hold_for_self_update(&fx.host);
    assert!(hold_for_move(&fx.host).is_none(), "the checkout being built from is refused");
    assert!(hold_for_move(&other).is_some(), "an update elsewhere stops no other checkout");
    drop(held);
    assert!(hold_for_move(&fx.host).is_some(), "and it is released with the script's exit");

    assert_eq!(only(&pass(&fx, &Knobs::default())).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip);
}

#[test]
fn a_self_update_waits_for_an_attempt_already_in_progress_in_its_checkout() {
    let fx = Fixture::new();
    let moving = hold_for_move(&fx.host).expect("no update is running");
    let (started, done) = (std::sync::mpsc::channel(), std::sync::mpsc::channel());
    let root = fx.host.clone();
    let updater = std::thread::spawn(move || {
        started.0.send(()).unwrap();
        let held = hold_for_self_update(&root);
        done.0.send(()).unwrap();
        drop(held);
    });
    started.1.recv().unwrap();
    // The update has announced itself and is waiting on the attempt...
    assert!(done.1.recv_timeout(Duration::from_millis(300)).is_err(), "it must wait");
    // ...and no NEW attempt may start in that checkout meanwhile.
    assert!(hold_for_move(&fx.host).is_none());
    drop(moving);
    done.1
        .recv_timeout(Duration::from_secs(30))
        .expect("the update proceeds once the attempt is over");
    updater.join().unwrap();
}

// ------------------------------------------------------------------------
// The fetch
// ------------------------------------------------------------------------

#[test]
fn an_unreachable_origin_is_reported_only_on_the_third_pass_in_a_row() {
    let fx = Fixture::new();
    let gone = fx.base().join("nowhere.git");
    git(&fx.host, &["remote", "set-url", "origin", gone.to_str().unwrap()]);
    let mut memory = Memory::default();
    let roots = [fx.host.clone()];
    let before = snapshot(&fx.host);

    for n in 1..=4u32 {
        let found = pass_over(&roots, &Knobs::default(), &mut memory);
        assert_eq!(only(&found).state, CheckoutState::FetchFailed, "pass {n}");
        let expected = usize::from(n == TRANSIENT_AFTER);
        assert_eq!(found.transitions.len(), expected, "pass {n}: {:?}", found.transitions);
    }
    assert_eq!(snapshot(&fx.host), before);
}

#[test]
fn a_fetch_the_workspace_half_made_in_this_pass_is_reused() {
    // The workspace half's stand-in: it advances origin (a resync pushed from
    // a throwaway worktree of this very clone), which also moves this clone's
    // remote-tracking ref, and the pass is told the branch was fetched.
    let fx = Fixture::new();
    let worktree = fx.base().join("resync-worktree");
    git(
        &fx.host,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    write(&worktree.join(".loom/scripts/spawn.sh"), "#!/bin/sh\necho new\n");
    git(&worktree, &["commit", "--quiet", "-am", "chore(loom): resync"]);
    git(&worktree, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
    let pushed = fx.origin_tip();
    assert_ne!(fx.head(), pushed, "the push never touched the main checkout");

    // Origin is unreachable from here on: only a reused fetch can succeed.
    let gone = fx.base().join("nowhere.git");
    git(&fx.host, &["remote", "set-url", "origin", gone.to_str().unwrap()]);
    let reuse = Knobs {
        fetched: true,
        ..Knobs::default()
    };
    let found = pass(&fx, &reuse);

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{:?}", only(&found));
    assert_eq!(fx.head(), pushed, "the same pass reaches the commit the resync pushed");
}

#[test]
fn the_same_pass_reaches_a_commit_that_landed_before_its_fetch() {
    // Without a reusable fetch the checkout half fetches for itself, so it
    // still reaches whatever is on the default branch when it runs.
    let fx = Fixture::new();
    let tip = fx.push(".loom/scripts/spawn.sh", "#!/bin/sh\necho new\n");
    assert_eq!(only(&pass(&fx, &Knobs::default())).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip);
}

// ------------------------------------------------------------------------
// Reporting
// ------------------------------------------------------------------------

#[test]
fn a_skip_state_is_reported_when_entered_and_when_it_clears_not_every_pass() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    write(&fx.host.join("README.md"), "edited here\n");
    let mut memory = Memory::default();
    let roots = [fx.host.clone()];
    let knobs = Knobs::default();

    let mut entered = Vec::new();
    for _ in 0..3 {
        entered.extend(pass_over(&roots, &knobs, &mut memory).transitions);
    }
    assert_eq!(entered.len(), 1, "three passes in one state are one line: {entered:?}");
    assert_eq!((entered[0].kind, entered[0].level), ("entered", Level::Warn));
    assert_eq!(entered[0].report.since, t0());

    // The operator cleans up; the checkout fast-forwards: one more line.
    git(&fx.host, &["checkout", "--quiet", "--", "README.md"]);
    let cleared = pass_over(&roots, &knobs, &mut memory).transitions;
    assert_eq!(cleared.len(), 1, "{cleared:?}");
    assert_eq!(cleared[0].kind, "fast-forwarded");
    assert_eq!(cleared[0].previous, Some(CheckoutState::Dirty));

    // And nothing while it stays current.
    assert!(pass_over(&roots, &knobs, &mut memory)
        .transitions
        .is_empty());
}

#[test]
fn a_skip_state_that_clears_without_a_fast_forward_says_so_once() {
    // Ahead, then pushed by the operator: current, with nothing to merge.
    let fx = Fixture::new();
    fx.commit_locally("local.rs");
    let mut memory = Memory::default();
    let roots = [fx.host.clone()];
    let knobs = Knobs::default();
    assert_eq!(pass_over(&roots, &knobs, &mut memory).transitions.len(), 1);

    git(&fx.host, &["push", "--quiet", "origin", "HEAD:main"]);
    let cleared = pass_over(&roots, &knobs, &mut memory).transitions;
    assert_eq!(cleared.len(), 1, "{cleared:?}");
    assert_eq!((cleared[0].kind, cleared[0].level), ("cleared", Level::Info));
    assert_eq!(cleared[0].previous, Some(CheckoutState::Ahead));
}

#[test]
fn installed_files_behind_is_true_only_when_the_missing_commits_touch_loom_paths() {
    // Product code only.
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    write(&fx.host.join("README.md"), "edited here\n");
    let found = pass(&fx, &Knobs::default());
    assert!(!only(&found).installed_files_behind);
    assert_eq!(found.transitions[0].level, Level::Warn);

    // An installed Loom script: the skipped checkout is running a stale one.
    for path in [".loom/scripts/spawn.sh", ".claude/commands/loom/builder.md"] {
        let fx = Fixture::new();
        fx.push(path, "new\n");
        write(&fx.host.join("README.md"), "edited here\n");
        let found = pass(&fx, &Knobs::default());
        assert!(only(&found).installed_files_behind, "{path}");
        assert_eq!(found.transitions[0].level, Level::Error, "{path}");
        assert!(found.transitions[0].line.contains("installed Loom files"), "{path}");
    }
}

#[test]
fn a_standing_skip_is_reported_again_once_it_holds_back_installed_files() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    write(&fx.host.join("README.md"), "edited here\n");
    let mut memory = Memory::default();
    let roots = [fx.host.clone()];
    let knobs = Knobs::default();
    assert_eq!(pass_over(&roots, &knobs, &mut memory).transitions[0].level, Level::Warn);

    fx.push(".loom/scripts/spawn.sh", "#!/bin/sh\necho new\n");
    let louder = pass_over(&roots, &knobs, &mut memory).transitions;
    assert_eq!(louder.len(), 1, "{louder:?}");
    assert_eq!(louder[0].level, Level::Error);
    assert!(pass_over(&roots, &knobs, &mut memory)
        .transitions
        .is_empty());
}

#[test]
fn with_auto_apply_off_nothing_is_written_and_the_count_is_still_reported() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    fx.push("src/b.rs", "fn b() {}\n");
    let read_only = Knobs {
        write: false,
        ..Knobs::default()
    };
    // A tracked file whose timestamp moved but whose content did not: a plain
    // `git status` would rewrite the index to refresh it. A read must not.
    let touched = fs::File::options()
        .write(true)
        .open(fx.host.join("README.md"))
        .unwrap();
    touched
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(120))
        .unwrap();
    drop(touched);

    let report = assert_skipped(&fx, &read_only, CheckoutState::Behind);

    assert_eq!(report.behind, Some(2));
    let shown = lines(std::slice::from_ref(&report)).join("\n");
    assert!(shown.contains("behind, 2 behind"), "{shown}");
    assert!(shown.contains("fleet.autoApply is off"), "{shown}");
}

// ------------------------------------------------------------------------
// The startup budget
// ------------------------------------------------------------------------

#[test]
fn the_startup_half_stops_at_its_budget_and_leaves_the_rest_for_the_timer() {
    let fx = Fixture::new();
    let second = fx.clone_as("second");
    let third = fx.clone_as("third");
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    let roots = [fx.host.clone(), second.clone(), third.clone()];
    let mut memory = Memory::default();

    // An injected clock: the budget runs out while the first workspace is
    // being merged. The gate probe is asked twice per workspace, the second
    // time right before the merge, which is where the clock jumps.
    let asked = Rc::new(Cell::new(0u32));
    let (clock, count) = (asked.clone(), asked.clone());
    let startup = Knobs {
        budget: Some(STARTUP_BUDGET),
        elapsed: Box::new(move || {
            if clock.get() >= 2 {
                STARTUP_BUDGET + Duration::from_secs(1)
            } else {
                Duration::ZERO
            }
        }),
        gate: Box::new(move |_| {
            count.set(count.get() + 1);
            false
        }),
        ..Knobs::default()
    };
    let found = pass_over(&roots, &startup, &mut memory);
    assert_eq!(found.checkouts.len(), 1);
    assert_eq!(found.deferred, 2);
    assert_eq!(fx.head(), tip, "the first workspace's merge is never cut short");
    assert_ne!(git(&second, &["rev-parse", "HEAD"]), tip);

    // The first timer pass does the rest.
    let found = pass_over(&roots, &Knobs::default(), &mut memory);
    assert_eq!(found.deferred, 0);
    assert_eq!(found.checkouts.len(), 3);
    assert_eq!(git(&second, &["rev-parse", "HEAD"]), tip);
    assert_eq!(git(&third, &["rev-parse", "HEAD"]), tip);
}

// ------------------------------------------------------------------------
// Status
// ------------------------------------------------------------------------

fn report_in(state: CheckoutState) -> CheckoutReport {
    CheckoutReport {
        root: PathBuf::from("/repos/app"),
        state,
        branch: Some("main".to_string()),
        behind: Some(3),
        ahead: None,
        head: Some("abc123abc123".to_string()),
        detail: Some("M src/a.rs".to_string()),
        since: t0(),
        installed_files_behind: true,
    }
}

#[test]
fn status_shows_every_checkout_that_is_not_current() {
    let mut current = report_in(CheckoutState::Current);
    current.root = PathBuf::from("/repos/fine");
    let shown = lines(&[report_in(CheckoutState::Dirty), current]);
    assert_eq!(
        shown,
        vec![
            "  checkout /repos/app: dirty, 3 behind, INSTALLED LOOM FILES BEHIND (M src/a.rs), \
             since 2026-10-08T12:00:00Z"
        ]
    );
}

#[test]
fn a_snapshot_written_without_checkouts_still_reads_back() {
    // What a pre-#10869 daemon wrote.
    let old = serde_json::json!({
        "repo": "acme/fleet", "reference": "main", "host": "build-1", "pass": "timer",
        "at": "2026-10-08T12:00:00Z", "intervalSecs": 300, "autoApply": true,
        "config": {"commit": null, "cached": false, "tiers": [], "error": null},
        "roster": {"drift": [], "applied": 0, "unapplied": 0, "error": null, "skipped": null},
    });
    let mut status: FleetSyncStatus = serde_json::from_value(old).expect("reads back");
    assert!(status.checkouts.is_empty());
    // A snapshot with nothing to say writes no `checkouts` key at all.
    let written = serde_json::to_value(&status).unwrap();
    assert!(written.get("checkouts").is_none(), "{written}");

    // With a checkout it round-trips, and `status` shows it.
    status.checkouts = vec![report_in(CheckoutState::WouldOverwrite)];
    let written = serde_json::to_value(&status).unwrap();
    assert_eq!(written["checkouts"][0]["state"], "would-overwrite");
    assert_eq!(written["checkouts"][0]["installedFilesBehind"], true);
    let back: FleetSyncStatus = serde_json::from_value(written).unwrap();
    assert_eq!(back.checkouts, status.checkouts);
    let line = crate::fleet_sync::render_line(Some(&back), t0()).unwrap();
    assert!(line.contains("checkout /repos/app: would-overwrite, 3 behind"), "{line}");
}

#[test]
fn every_state_has_its_own_label_and_only_four_are_transient() {
    use CheckoutState as S;
    let all = [
        (S::Current, "current"),
        (S::FastForwarded, "fast-forwarded"),
        (S::Behind, "behind"),
        (S::NoDefaultBranch, "no-default-branch"),
        (S::WrongBranch, "wrong-branch"),
        (S::MidOperation, "mid-operation"),
        (S::GateInFlight, "gate-in-flight"),
        (S::SelfUpdateInFlight, "self-update-in-flight"),
        (S::FetchFailed, "fetch-failed"),
        (S::Ahead, "ahead"),
        (S::Diverged, "diverged"),
        (S::Dirty, "dirty"),
        (S::WouldOverwrite, "would-overwrite"),
        (S::GitFailure, "git-failure"),
    ];
    for (state, label) in all {
        assert_eq!(state.as_str(), label);
        // The snapshot spells a state the way `status` prints it.
        assert_eq!(serde_json::to_value(state).unwrap(), label);
    }
    let transient: Vec<&str> = all
        .iter()
        .filter(|(s, _)| s.is_transient())
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(
        transient,
        [
            "gate-in-flight",
            "self-update-in-flight",
            "fetch-failed",
            "git-failure"
        ]
    );
}

#[test]
fn a_refused_merge_is_read_as_a_file_in_the_way_or_as_a_failure() {
    let untracked = "error: The following untracked working tree files would be overwritten by \
                     merge:\n\tnotes.txt\n\tdocs/x.md\nPlease move or remove them before you \
                     merge.\nAborting";
    assert_eq!(
        classify_refusal(untracked),
        Refusal::WouldOverwrite("notes.txt, docs/x.md".to_string())
    );
    let other = "fatal: Not possible to fast-forward, aborting.";
    assert!(matches!(classify_refusal(other), Refusal::Other(e) if e.contains("fast-forward")));
}
