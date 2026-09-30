//! Differential test: the Rust port of `worktree.sh`'s "the worktree directory
//! already exists" arm against the shell it replaced (#8195 slice 12, epic
//! #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! One retained suite does drive this arm —
//! `test-worktree-existing-dir-drift-check.sh` — but only through *registered*
//! worktrees whose paths contain no symlink, no space and no lookalike sibling.
//! That is the method's ceiling stated exactly: the retired probe
//!
//! ```text
//! git worktree list | grep -q "$WORKTREE_PATH"
//! ```
//!
//! is wrong in both directions and **every** assertion in that suite is blind to
//! both, because nobody writes down the input they did not imagine. So the
//! corpus here comes from the probe's own grammar — *what can a path be?* —
//! rather than from the shapes the incident reports happened to use:
//! canonical, symlinked, space-bearing, regex-metacharacter-bearing, and a
//! proper prefix of a registered sibling.
//!
//! # What is compared
//!
//! Exit code, stdout and stderr byte for byte (after redacting the per-side
//! root), **and the post-state**: the worktree's `HEAD` subject, its
//! `git status --porcelain`, and the presence and exact bytes of the
//! `.loom-managed` sentinel. The last is not decoration — the sentinel is the
//! marker `rm -rf` tooling reads as authorization (#3334), so "did this arm
//! write one, and where" is a safety observable, not a cosmetic one.
//!
//! # What is held identical on both sides, and why
//!
//! The arm already delegated three of its steps to `loom-daemon` before this
//! slice: `worktree-upstream` (slice 9), `worktree-stale-ref` (#8354) and
//! `worktree-reset` (slice 6). The frozen fixture invokes those same
//! subcommands from the same binary the port calls in-process, so a difference
//! this harness reports is attributable to the arm's own control flow. Both
//! sides also run with `LOOM_BRANCH_LANDED_OFFLINE=1`: the landed ladder's top
//! rung is a forge round-trip, and a test whose answer depends on the network
//! is not a differential test.
//!
//! # This harness asserts DISAGREEMENT for two classes, deliberately
//!
//! 1. **False negative.** `git worktree list` prints symlink-RESOLVED paths;
//!    `$WORKTREE_PATH` is concatenated and unresolved. Reached through a
//!    symlinked root — every macOS checkout under `/tmp` — the shell refuses a
//!    LIVE worktree, exit 1, advising `rm -rf` on it. The port keeps it.
//! 2. **False positive.** The match is an unanchored substring *and* a regex, so
//!    `…/issue-4` matches the line for a registered `…/issue-44`. The shell then
//!    runs the whole preserve/reset path against a directory git knows nothing
//!    about — where `git -C` resolves to the PARENT repo — and finishes by
//!    writing a `.loom-managed` sentinel into crash debris. The port refuses it
//!    and writes nothing.
//!
//! Each is asserted as a disagreement in the documented direction, with the
//! post-state checked too, so a regression to the shell's answer fails rather
//! than quietly re-agreeing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

fn retired_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-existing-retired.sh")
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// How the caller reaches the repo and the worktree — the probe's whole
/// grammar, since the only thing it consumes is a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// The canonical path, no symlink, no space, no metacharacter. The only
    /// shape the retained shell suite ever uses.
    Canonical,
    /// Both paths reached through a symlinked root (`/tmp` -> `/private/tmp`).
    ViaSymlink,
    /// Repo and worktree directory names contain spaces.
    WithSpaces,
    /// Repo directory name contains regex metacharacters (`+`, `.`).
    WithMetacharacters,
}

/// What state the existing worktree (or directory) is in when the arm runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// A committed change the base does not have: "preserve existing work".
    CommitsAhead,
    /// A modified tracked file, no commits ahead.
    DirtyTracked,
    /// An untracked file only — `git status --porcelain` reports it,
    /// `git diff HEAD` does not.
    Untracked,
    /// Clean and level with the base: stale, reset to the base.
    StaleLevel,
    /// Clean and one commit behind `origin/main`: stale, reset to the base.
    StaleBehind,
    /// Clean and level with the base, but `origin/<branch>` is live and ahead —
    /// the #8287 shape: the reference (and the reset target) must be the remote
    /// branch, not the base.
    StaleWithLiveRemote,
    /// The directory exists but is not a worktree at all (crash debris).
    Unregistered,
    /// Crash debris whose path is a proper prefix of a REGISTERED sibling's —
    /// `issue-4` beside `issue-44`.
    UnregisteredPrefixOfSibling,
}

struct Side {
    /// The per-side temp root, redacted out of every comparison.
    root: PathBuf,
    /// The path handed to the arm as `$WORKTREE_REPO_ROOT` (may be symlinked).
    repo: PathBuf,
    /// The path handed to the arm as `$WORKTREE_PATH` (may be symlinked).
    worktree: PathBuf,
    /// The canonical worktree path, for reading post-state back.
    worktree_real: PathBuf,
}

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-wt-existing-diff-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn hermetic(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("LC_ALL", "C")
        .env("TERM", "dumb")
        // The landed ladder's top rung is a forge round-trip. Both sides skip
        // it: a differential test whose answer depends on the network is not
        // one.
        .env("LOOM_BRANCH_LANDED_OFFLINE", "1")
}

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    hermetic(&mut cmd);
    let out = cmd.arg("-C").arg(dir).args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    hermetic(&mut cmd);
    let out = cmd.arg("-C").arg(dir).args(args).output().expect("run git");
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// The directory names each [`Reach`] uses. The repo name is what a
/// metacharacter has to live in for the retired `grep` (not `grep -F`) to read
/// it as pattern syntax.
fn names(reach: Reach) -> (&'static str, &'static str) {
    match reach {
        Reach::Canonical => ("repo", "issue-42"),
        Reach::ViaSymlink => ("repo", "issue-42"),
        Reach::WithSpaces => ("my repo", "issue 42"),
        Reach::WithMetacharacters => ("c++.repo", "issue-42"),
    }
}

/// One side's fixture: a bare origin, a clone with a `main` commit, and a
/// linked worktree on `feature/issue-42` laid out the way `worktree.sh` lays
/// one out — plus whatever [`State`] asks for on top.
fn build_side(tag: &str, side: &str, reach: Reach, state: State) -> Side {
    let root = tmpdir(&format!("{tag}-{side}"));
    let (repo_name, wt_name) = names(reach);

    let origin = root.join("origin.git");
    let mut init = Command::new("git");
    hermetic(&mut init);
    assert!(init
        .args(["init", "-q", "--bare", "--initial-branch=main"])
        .arg(&origin)
        .status()
        .expect("git init --bare")
        .success());

    let repo = root.join(repo_name);
    fs::create_dir_all(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q", "--initial-branch=main", "."]);
    fs::write(repo.join("base.txt"), "base\n").expect("write base");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["remote", "add", "origin", origin.to_str().expect("utf8")]);
    git(&repo, &["push", "origin", "main"]);

    let worktrees = repo.join(".loom/worktrees");
    fs::create_dir_all(&worktrees).expect("mkdir worktrees");
    let worktree = worktrees.join(wt_name);

    // The registered sibling the prefix case is a prefix OF. Created first so
    // it is in the porcelain before the lookalike directory exists.
    if state == State::UnregisteredPrefixOfSibling {
        let sibling = worktrees.join(format!("{wt_name}4"));
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/issue-424",
                sibling.to_str().expect("utf8"),
                "main",
            ],
        );
    }

    if matches!(state, State::Unregistered | State::UnregisteredPrefixOfSibling) {
        fs::create_dir_all(&worktree).expect("mkdir debris");
        fs::write(worktree.join("debris.txt"), "left behind\n").expect("write debris");
    } else {
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/issue-42",
                worktree.to_str().expect("utf8"),
                "main",
            ],
        );
        match state {
            State::CommitsAhead => {
                fs::write(worktree.join("work.txt"), "real work\n").expect("write work");
                git(&worktree, &["add", "-A"]);
                git(&worktree, &["commit", "-qm", "real work"]);
            }
            State::DirtyTracked => {
                fs::write(worktree.join("base.txt"), "locally edited\n").expect("edit base");
            }
            State::Untracked => {
                fs::write(worktree.join("scratch.txt"), "notes\n").expect("write scratch");
            }
            State::StaleLevel => {}
            State::StaleBehind => {
                // Advance origin/main past the worktree without touching it.
                fs::write(repo.join("newer.txt"), "newer\n").expect("write newer");
                git(&repo, &["add", "-A"]);
                git(&repo, &["commit", "-qm", "newer"]);
                git(&repo, &["push", "origin", "main"]);
                git(&worktree, &["fetch", "origin", "main"]);
            }
            State::StaleWithLiveRemote => {
                // A real commit on the branch, pushed, then the local branch
                // rewound to main: 0 ahead of the base, but origin/<branch>
                // still carries the branch's only work (#8147/#8190).
                fs::write(worktree.join("pushed.txt"), "pushed work\n").expect("write pushed");
                git(&worktree, &["add", "-A"]);
                git(&worktree, &["commit", "-qm", "pushed work"]);
                git(&worktree, &["push", "origin", "feature/issue-42"]);
                git(&worktree, &["reset", "--hard", "main"]);
            }
            State::Unregistered | State::UnregisteredPrefixOfSibling => unreachable!(),
        }
    }

    let worktree_real = worktree.clone();
    let (repo, worktree) = if reach == Reach::ViaSymlink {
        let link = root.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &link).expect("symlink");
        (link.join(repo_name), link.join(repo_name).join(".loom/worktrees").join(wt_name))
    } else {
        (repo, worktree)
    };

    Side {
        root,
        repo,
        worktree,
        worktree_real,
    }
}

/// The two sides, plus the assertion that they are indistinguishable before
/// either implementation has run — so "the inputs differed" can never
/// masquerade as a finding.
fn two_sides(tag: &str, reach: Reach, state: State) -> (Side, Side) {
    let a = build_side(tag, "shell", reach, state);
    let b = build_side(tag, "rust", reach, state);
    assert_eq!(
        tree_listing(&a.root).replace(a.root.to_str().expect("utf8"), "<ROOT>"),
        tree_listing(&b.root).replace(b.root.to_str().expect("utf8"), "<ROOT>"),
        "the two sides' fixtures must be identical before either implementation runs"
    );
    (a, b)
}

fn tree_listing(root: &Path) -> String {
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    found.join("\n")
}

fn walk(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip the git administrative trees and the bare origin: they carry
        // absolute paths, object hashes and per-worktree bookkeeping that are
        // not what this comparison is about.
        if path
            .file_name()
            .is_some_and(|n| n == ".git" || n == "origin.git")
        {
            out.push(format!("{} [gitdir]", path.display()));
            continue;
        }
        // Record a symlink, never follow it: the `ViaSymlink` fixture points a
        // link at its own root, and following it is an unbounded walk.
        if entry.file_type().is_ok_and(|t| t.is_symlink()) {
            out.push(format!("{} [symlink]", path.display()));
            continue;
        }
        if path.is_dir() {
            out.push(format!("{}/", path.display()));
            walk(&path, out);
        } else {
            out.push(path.display().to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// The two implementations
// ---------------------------------------------------------------------------

/// Everything either side can be observed doing.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    code: i32,
    stdout: String,
    stderr: String,
    /// The worktree's `HEAD` subject after the arm ran — the only way to see a
    /// reset that happened (or did not).
    head: String,
    /// `git status --porcelain` after the arm ran: did anything get discarded?
    status: String,
    /// The `.loom-managed` sentinel's bytes, or `None` when absent. Presence is
    /// the `rm -rf` authorization (#3334), so it is a safety observable.
    sentinel: Option<String>,
}

fn redact(text: &str, side: &Side) -> String {
    let mut out = text.replace(side.root.to_str().expect("utf8"), "<ROOT>");
    // A symlinked side is reached through `<ROOT>/link`, and git answers some
    // questions with the resolved path; collapse both spellings to one.
    out = out.replace("<ROOT>/link", "<ROOT>");
    redact_shas(&out)
}

/// Collapse every 40-hex object name to `<SHA>`.
///
/// The two sides are built independently, so identical *content* still hashes
/// differently (committer timestamps differ by milliseconds). The drift report
/// quotes two object names verbatim, and comparing those would assert that the
/// two fixtures are the same repository — which is not the property under test
/// and is not even true. Everything else about the line, including which of the
/// two positions each hash occupies, is still compared byte for byte.
fn redact_shas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let run = chars[i..]
            .iter()
            .take_while(|c| c.is_ascii_hexdigit())
            .count();
        let boundary_before = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        let boundary_after = i + run >= chars.len() || !chars[i + run].is_ascii_alphanumeric();
        if run == 40 && boundary_before && boundary_after {
            out.push_str("<SHA>");
            i += run;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn post_state(side: &Side) -> (String, String, Option<String>) {
    let head = git_out(&side.worktree_real, &["log", "-1", "--format=%s"]);
    let status = git_out(&side.worktree_real, &["status", "--porcelain"]);
    let sentinel = fs::read_to_string(side.worktree_real.join(".loom-managed")).ok();
    (head, status, sentinel)
}

fn observe(out: &Output, side: &Side) -> Observed {
    let (head, status, sentinel) = post_state(side);
    Observed {
        code: out.status.code().unwrap_or(-1),
        stdout: redact(&String::from_utf8_lossy(&out.stdout), side),
        stderr: redact(&String::from_utf8_lossy(&out.stderr), side),
        head,
        status,
        sentinel: sentinel.map(|s| redact(&s, side)),
    }
}

/// The env both sides read the same values from — `worktree.sh`'s own
/// variables at the point the arm runs.
fn arm_env(cmd: &mut Command, side: &Side, json: bool) {
    hermetic(cmd);
    cmd.env("WORKTREE_PATH", &side.worktree)
        .env("WORKTREE_REPO_ROOT", &side.repo)
        .env("ISSUE_NUMBER", "42")
        .env("BRANCH_NAME", "feature/issue-42")
        .env("DEFAULT_BRANCH", "main")
        .env("BASE_REF", "origin/main")
        .env("BASE_DISPLAY", "main")
        .env("BASE_BRANCH", "")
        .env("JSON_OUTPUT", if json { "true" } else { "false" })
        .env("LOOM_DAEMON_SELF_BIN", bin());
}

/// The retired shell.
fn run_shell(side: &Side, json: bool) -> Observed {
    let mut cmd = Command::new("bash");
    arm_env(&mut cmd, side, json);
    let out = cmd
        .arg(retired_fixture())
        .current_dir(&side.repo)
        .output()
        .expect("run retired fixture");
    observe(&out, side)
}

/// The port, invoked exactly as `worktree.sh`'s `_worktree_existing` wrapper
/// invokes it — same flags, same order, same `--quiet` gate.
fn run_rust(side: &Side, json: bool) -> Observed {
    let mut cmd = Command::new(bin());
    arm_env(&mut cmd, side, json);
    cmd.arg("worktree-existing")
        .arg("--worktree")
        .arg(&side.worktree)
        .arg("--repo")
        .arg(&side.repo)
        .args(["--issue", "42"])
        .args(["--branch", "feature/issue-42"])
        .args(["--default-branch", "main"])
        .args(["--base-ref", "origin/main"])
        .args(["--base-display", "main"])
        .args(["--base-branch", ""])
        .args(["--ignore-pid", &std::process::id().to_string()])
        .current_dir(&side.repo);
    if json {
        cmd.arg("--quiet");
    }
    // No stdout redirection is needed to mirror `worktree.sh`'s `exec 1>&2`:
    // under `--quiet` the port writes NOTHING to stdout (every message is
    // either gated off or already an `eprintln`), which is exactly what the
    // shell's redirection achieved. Asserting that emptiness is part of the
    // `--json` cases below.
    let out = cmd.output().expect("run port");
    observe(&out, side)
}

/// Run both sides of one case and return `(shell, rust)`.
fn both(tag: &str, reach: Reach, state: State, json: bool) -> (Observed, Observed) {
    let (a, b) = two_sides(tag, reach, state);
    let shell = run_shell(&a, json);
    let rust = run_rust(&b, json);
    (shell, rust)
}

fn assert_same(tag: &str, shell: &Observed, rust: &Observed) {
    assert_eq!(shell, rust, "[{tag}] the port diverged from the retired shell");
}

// ---------------------------------------------------------------------------
// 1. The cases the retained suite already covers — both sides must AGREE
// ---------------------------------------------------------------------------

#[test]
fn commits_ahead_agree() {
    let (shell, rust) = both("ahead", Reach::Canonical, State::CommitsAhead, false);
    assert_same("ahead", &shell, &rust);
    assert_eq!(shell.code, 0);
    assert!(
        shell.stdout.contains("preserving existing work"),
        "the shell's own preserve message must be what we matched: {}",
        shell.stdout
    );
    assert!(rust.sentinel.is_some(), "#3548 back-fill did not happen");
    assert_eq!(rust.head, "real work", "the commit ahead was discarded");
}

#[test]
fn dirty_tracked_agrees() {
    let (shell, rust) = both("dirty", Reach::Canonical, State::DirtyTracked, false);
    assert_same("dirty", &shell, &rust);
    assert_eq!(shell.code, 0);
    assert!(rust.stdout.contains("uncommitted changes - preserving"), "{}", rust.stdout);
    assert!(!rust.status.is_empty(), "the uncommitted edit must still be there");
}

/// An untracked file counts as "uncommitted" for this arm — `git status
/// --porcelain` reports it, `git diff HEAD` (the #6334 guard's own signal) does
/// not — so the two questions genuinely differ and both sides must answer this
/// one with the broader reading.
#[test]
fn untracked_only_agrees() {
    let (shell, rust) = both("untracked", Reach::Canonical, State::Untracked, false);
    assert_same("untracked", &shell, &rust);
    assert_eq!(shell.code, 0);
    assert!(rust.stdout.contains("preserving existing work"));
}

#[test]
fn stale_level_agrees_and_both_reset() {
    let (shell, rust) = both("stale", Reach::Canonical, State::StaleLevel, false);
    assert_same("stale", &shell, &rust);
    assert_eq!(shell.code, 0);
    assert!(rust.stdout.contains("Stale worktree detected"), "{}", rust.stdout);
    assert!(rust.sentinel.is_some());
}

#[test]
fn stale_behind_agrees_and_both_reset_to_the_base() {
    let (shell, rust) = both("behind", Reach::Canonical, State::StaleBehind, false);
    assert_same("behind", &shell, &rust);
    assert_eq!(shell.code, 0);
    assert_eq!(rust.head, "newer", "the stale worktree must end up at origin/main's tip");
}

/// The #8287 shape: 0 commits ahead of the base, but `origin/<branch>` is live
/// and carries the branch's only work. Both sides must measure and reset at the
/// REMOTE tip — resetting to main here is the #8147/#8190 incident.
#[test]
fn a_live_remote_branch_is_the_reference_on_both_sides() {
    let (shell, rust) = both("remote", Reach::Canonical, State::StaleWithLiveRemote, false);
    assert_same("remote", &shell, &rust);
    assert_eq!(rust.head, "pushed work", "reset to main, not to the remote tip");
    assert!(
        rust.stdout.contains("origin/feature/issue-42"),
        "the reference must be named in the message: {}",
        rust.stdout
    );
    // The drift report quotes two object names. They are redacted (the sides
    // are independent repos), so assert the redaction actually fired here —
    // otherwise a future change that stopped emitting the report would look
    // like agreement.
    assert_eq!(
        rust.stdout.matches("<SHA>").count(),
        2,
        "the behind-the-pushed-tip drift report and its two object names are missing: {}",
        rust.stdout
    );
}

#[test]
fn an_unregistered_directory_agrees_and_gets_no_sentinel() {
    let (shell, rust) = both("unreg", Reach::Canonical, State::Unregistered, false);
    assert_same("unreg", &shell, &rust);
    assert_eq!(shell.code, 1);
    assert!(shell.stderr.contains("not a registered worktree"));
    assert_eq!(
        rust.sentinel, None,
        "a sentinel in crash debris is an rm -rf authorization (#3334)"
    );
}

#[test]
fn a_path_with_spaces_agrees() {
    let (shell, rust) = both("spaces", Reach::WithSpaces, State::DirtyTracked, false);
    assert_same("spaces", &shell, &rust);
    assert_eq!(shell.code, 0);
    assert!(
        !rust.status.is_empty(),
        "#7858 was a live rm -rf on a space-bearing path; the edit must survive"
    );
}

#[test]
fn a_regex_metacharacter_in_the_repo_name_agrees() {
    let (shell, rust) = both("meta", Reach::WithMetacharacters, State::StaleLevel, false);
    assert_same("meta", &shell, &rust);
    assert_eq!(shell.code, 0);
}

// ---------------------------------------------------------------------------
// 2. `--json`: the shell gated its registered-arm messages and left the
//    unregistered arm's ungated. Both must still agree.
// ---------------------------------------------------------------------------

#[test]
fn json_mode_suppresses_the_same_messages_on_both_sides() {
    let (shell, rust) = both("json-preserve", Reach::Canonical, State::CommitsAhead, true);
    assert_same("json-preserve", &shell, &rust);
    assert!(
        shell.stdout.is_empty(),
        "--json must leave the real stdout untouched: {:?}",
        shell.stdout
    );
    assert!(
        !shell.stderr.contains("preserving existing work"),
        "the registered arm's messages were gated on --json: {:?}",
        shell.stderr
    );
}

#[test]
fn json_mode_keeps_the_unregistered_refusal_on_both_sides() {
    let (shell, rust) = both("json-unreg", Reach::Canonical, State::Unregistered, true);
    assert_same("json-unreg", &shell, &rust);
    assert_eq!(shell.code, 1);
    assert!(
        shell.stderr.contains("not a registered worktree"),
        "the unregistered arm's messages were NOT gated: {:?}",
        shell.stderr
    );
    assert!(shell.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// 3. The two defect classes — asserted as DISAGREEMENTS in a named direction
// ---------------------------------------------------------------------------

/// **False negative.** `git worktree list` prints symlink-resolved paths;
/// `$WORKTREE_PATH` does not. Reached through a symlinked root the shell
/// refuses a LIVE worktree and tells the caller to `rm -rf` it; the port keeps
/// it. A regression to a textual comparison re-agrees here and fails this test.
#[test]
fn a_symlinked_root_makes_the_shell_refuse_a_live_worktree() {
    let (shell, rust) = both("symlink", Reach::ViaSymlink, State::DirtyTracked, false);

    assert_eq!(shell.code, 1, "the shell's false negative is gone?");
    assert!(shell.stderr.contains("not a registered worktree"), "{:?}", shell.stderr);
    assert!(
        shell.stdout.contains("rm -rf"),
        "the shell advised rm -rf on a live worktree: {:?}",
        shell.stdout
    );

    assert_eq!(rust.code, 0, "the port must keep a live worktree");
    assert!(rust.stdout.contains("preserving existing work"), "{:?}", rust.stdout);
    assert!(
        !rust.status.is_empty(),
        "the uncommitted work the shell would have had rm -rf'd must survive"
    );
    assert_ne!(shell, rust, "the classes must not silently re-agree");
}

/// **False positive.** `…/issue-42` is a proper prefix of the registered
/// `…/issue-424`, so the unanchored substring match answers "registered" for
/// crash debris. The shell then runs the whole arm with `git -C <debris>` —
/// which resolves to the PARENT repo — and ends by writing a `.loom-managed`
/// sentinel into it, the exact marker `rm -rf` tooling reads as authorization.
#[test]
fn a_prefix_of_a_registered_path_makes_the_shell_accept_debris() {
    let (shell, rust) = both("prefix", Reach::Canonical, State::UnregisteredPrefixOfSibling, false);

    assert_eq!(shell.code, 0, "the shell's false positive is gone? (it accepted the debris)");
    assert!(
        shell.sentinel.is_some(),
        "the shell wrote a sentinel into crash debris — that IS the defect"
    );

    assert_eq!(rust.code, 1, "the port must refuse unregistered debris");
    assert!(rust.stderr.contains("not a registered worktree"), "{:?}", rust.stderr);
    assert_eq!(
        rust.sentinel, None,
        "the port must never authorize rm -rf on crash debris (#3334)"
    );
    assert_ne!(shell, rust, "the classes must not silently re-agree");
}

// ---------------------------------------------------------------------------
// 4. The shell fallback `worktree.sh` keeps for a host with no daemon
// ---------------------------------------------------------------------------

/// `_worktree_existing`'s no-daemon arm answers the registration question with
/// one physical comparison and always PRESERVES. It is not a second
/// implementation of the verdict, and this is the proof that its *predicate*
/// agrees with the port's in every shape the corpus covers — including the two
/// the retired `grep` got wrong.
#[test]
fn the_shell_fallback_predicate_agrees_with_the_port() {
    for (reach, state, registered) in [
        (Reach::Canonical, State::StaleLevel, true),
        (Reach::ViaSymlink, State::StaleLevel, true),
        (Reach::WithSpaces, State::StaleLevel, true),
        (Reach::WithMetacharacters, State::StaleLevel, true),
        (Reach::Canonical, State::Unregistered, false),
        (Reach::Canonical, State::UnregisteredPrefixOfSibling, false),
    ] {
        let side = build_side("fallback", "shell", reach, state);

        // The live fallback expression, lifted from `worktree.sh` by name so a
        // change to it there fails here rather than silently diverging.
        let script = fallback_expression();
        let mut cmd = Command::new("bash");
        hermetic(&mut cmd);
        let out = cmd
            .arg("-c")
            .arg(&script)
            .arg("bash")
            .arg(&side.worktree)
            .current_dir(&side.repo)
            .output()
            .expect("run fallback");
        let fallback_says_registered = out.status.success();

        assert_eq!(
            fallback_says_registered, registered,
            "the no-daemon fallback disagreed with the port for {reach:?}/{state:?}"
        );
    }
}

/// The fallback's predicate, read out of the LIVE `worktree.sh` rather than
/// copied — a copy would let the script's own expression change without this
/// test noticing.
fn fallback_expression() -> String {
    let script = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/worktree.sh"),
    )
    .expect("read worktree.sh");
    let marker = "    _t=\"$(cd \"$WORKTREE_PATH\"";
    let start = script
        .find(marker)
        .expect("worktree.sh no longer contains _worktree_existing's fallback assignment");
    let rest = &script[start..];
    let end = rest
        .find("\n    print_error")
        .expect("worktree.sh's fallback no longer ends in a print_error");
    format!(
        "set -e\nWORKTREE_PATH=\"$1\"\n{}\nexit 1\n",
        &rest[..end].replace("return 0", "exit 0")
    )
}
