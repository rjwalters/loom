//! `worktree.sh --check`'s in-worktree predicate vs. `loom-daemon
//! worktree-check` (#9425) — and vs. the string comparison it retired.
//!
//! # What was wrong
//!
//! `worktree.sh --check` (and, until #13118, the retired `worktree-return.sh`)
//! carried a copy of the predicate #8195 slice 11 (PR #9424) retired from
//! `defaults/scripts/worktree.sh` as constant-true:
//!
//! ```text
//! git_dir=$(git rev-parse --git-common-dir)
//! work_dir=$(git rev-parse --show-toplevel)
//! [[ "$git_dir" != "$work_dir/.git" ]]        # => "in a worktree"
//! ```
//!
//! `--show-toplevel` is always **absolute**; `--git-common-dir` is **relative
//! to the current directory** whenever it can be. So in the primary clone the
//! comparison read `.git` != `/repo/.git` — true — and it answered *"in a
//! worktree"* there, in every subdirectory of it, and inside a real linked
//! worktree alike. It had **no reachable false branch**, so the *"Not currently
//! in a worktree"* arm was dead code.
//!
//! # What this file pins
//!
//! Three things, each of which fails if the string comparison comes back:
//!
//! 1. **The live `worktree.sh --check`, run as a script**, answers correctly
//!    from all four positions a caller can stand in, and its previously-dead
//!    arm is reached from two of them —
//!    [`the_previously_dead_arm_is_reachable_from_the_primary_clone`],
//!    [`the_live_script_agrees_with_the_port_from_every_position`].
//! 2. **The retired comparison is constant-true**, extracted *live* from the
//!    one frozen copy in the tree (`tests/fixtures/worktree-check-retired.sh`,
//!    slice 11's fixture) rather than re-typed here:
//!    [`the_retired_comparison_is_constant_true_which_is_the_disagreement`].
//! 3. **The live script does not carry it**, and the legacy `scripts/worktree.sh`
//!    fork that held another copy stays retired:
//!    [`no_live_script_carries_the_retired_comparison`],
//!    [`the_legacy_root_fork_is_retired_and_pnpm_worktree_points_at_the_canonical_script`].
//!
//! The script is run **as a script** because the predicate is a hard
//! delegation to `loom-daemon worktree-check`; the `LOOM_DAEMON_SELF_BIN` seam
//! pins every run to the binary built from *this* tree, never to whatever the
//! host has installed.

mod common;

use common::git;
use common::hermetic_git as hermetic;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The live script under test — the one `pnpm worktree --check` runs.
fn live_worktree_sh() -> PathBuf {
    repo_root().join("defaults/scripts/worktree.sh")
}

/// Slice 11's frozen copy of the retired shell. The *only* copy of the defect
/// left in the tree, and the one this file extracts its baseline from.
fn retired_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-check-retired.sh")
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The four positions a caller can stand in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    /// The primary clone's root.
    PrimaryRoot,
    /// A subdirectory of the primary clone; git answers `--git-common-dir` as
    /// `../.git` here.
    PrimarySubdir,
    /// Inside a linked worktree.
    Worktree,
    /// A subdirectory of a linked worktree.
    WorktreeSubdir,
}

const POSITIONS: &[Position] = &[
    Position::PrimaryRoot,
    Position::PrimarySubdir,
    Position::Worktree,
    Position::WorktreeSubdir,
];

struct Fixture {
    repo: PathBuf,
    worktree: PathBuf,
}

impl Fixture {
    fn cwd(&self, pos: Position) -> PathBuf {
        match pos {
            Position::PrimaryRoot => self.repo.clone(),
            Position::PrimarySubdir => self.repo.join("sub"),
            Position::Worktree => self.worktree.clone(),
            Position::WorktreeSubdir => self.worktree.join("nested"),
        }
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    common::unique_tmpdir("loom-wt-return-diff", tag)
}

/// A repo with a commit, a subdirectory, and a linked worktree laid out the way
/// `worktree.sh` lays one out.
fn build_fixture(tag: &str) -> Fixture {
    let root = tmpdir(tag);
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q", "--initial-branch=main", "."]);
    fs::write(repo.join("f"), "hi\n").expect("write f");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    fs::create_dir_all(repo.join("sub")).expect("mkdir sub");
    let worktree = repo.join(".loom/worktrees/issue-42");
    fs::create_dir_all(worktree.parent().expect("parent")).expect("mkdir worktrees");
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
    fs::create_dir_all(worktree.join("nested")).expect("mkdir nested");
    Fixture { repo, worktree }
}

// ---------------------------------------------------------------------------
// Running the live script
// ---------------------------------------------------------------------------

/// What the script concluded about the caller's position. Three values, not
/// two, because the delegation's whole point is that "could not run" is
/// readable as *neither* answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    /// Exit 0 — it is inside a linked worktree.
    InWorktree,
    /// Exit 1 — the main working directory (the previously-dead arm).
    MainWorkingDirectory,
    /// Exit 2: `loom-daemon worktree-check` could not be run at all.
    CouldNotRun,
}

struct Run {
    code: i32,
    stdout: String,
}

/// Run the live script's `--check` verb in `cwd`, pinned to THIS tree's binary.
///
/// `LOOM_DAEMON_SELF_BIN` is `lib/script-helper.sh`'s documented harness seam
/// ("which binary IMPLEMENTS this stub", checked before the normal resolution
/// chain), so the answer under test can never come from a stale machine
/// install. `LOOM_DAEMON_BIN` is cleared alongside it so a leaked ambient value
/// cannot reach the resolver either.
fn run_live(cwd: &Path) -> Run {
    let mut cmd = Command::new("bash");
    hermetic(&mut cmd);
    let out = cmd
        .arg(live_worktree_sh())
        .arg("--check")
        .current_dir(cwd)
        .env("LOOM_DAEMON_SELF_BIN", bin())
        .env_remove("LOOM_DAEMON_BIN")
        .output()
        .expect("run worktree.sh --check");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
    }
}

/// The script's answer, read off its exit code (0 in a worktree, 1 the main
/// working directory, 2 could not run). 1 and 2 are the two refusal codes, and
/// reading a 2 as a 1 is exactly the confusion LOOM_SCRIPT_HELPER_MISSING_RC=2
/// exists to prevent.
fn live_answer(cwd: &Path) -> Answer {
    let run = run_live(cwd);
    match run.code {
        0 => Answer::InWorktree,
        1 => Answer::MainWorkingDirectory,
        2 => Answer::CouldNotRun,
        other => panic!("unexpected exit {other}: {:?}", run.stdout),
    }
}

// ---------------------------------------------------------------------------
// The retired comparison, extracted from the one frozen copy in the tree
// ---------------------------------------------------------------------------

/// `check_if_in_worktree`'s retired body, lifted out of slice 11's frozen
/// fixture rather than re-typed here, and run in `cwd`.
///
/// Extracting rather than copying is the whole point: the baseline this file
/// asserts against is the same bytes slice 11's own differential harness
/// compares to, so the two cannot drift into disagreeing about what the defect
/// was.
fn retired_comparison_says_in_worktree(cwd: &Path) -> bool {
    let source = fs::read_to_string(retired_fixture()).expect("read frozen fixture");
    let marker = "check_if_in_worktree() {";
    let body = source
        .split_once(marker)
        .expect("the frozen fixture must still define check_if_in_worktree")
        .1
        .lines()
        .take_while(|l| !l.starts_with('}'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains(r#"[[ "$git_dir" != "$work_dir/.git" ]]"#),
        "the extracted body must still be the string comparison; got:\n{body}"
    );
    // Re-hosted in a real shell FUNCTION, because the decision is expressed as
    // `return`. Spliced into a top level, `return` is a syntax error and
    // neutralising it would let control fall through — reproducing the
    // constant-true defect inside the harness meant to detect it (the bug
    // slice 11's own harness shipped with and had to fix).
    let script = format!("set -e\n_retired_under_test() {{\n{body}\n}}\nif _retired_under_test; then echo true; else echo false; fi\n");
    let mut cmd = Command::new("bash");
    hermetic(&mut cmd);
    let out = cmd
        .arg("-c")
        .arg(&script)
        .current_dir(cwd)
        .output()
        .expect("run retired comparison");
    assert!(
        out.status.success(),
        "retired comparison failed to run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim() == "true"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_live_script_agrees_with_the_port_from_every_position() {
    let fx = build_fixture("positions");
    for pos in POSITIONS {
        let cwd = fx.cwd(*pos);
        let expected = if loom_daemon::worktree_cli::check::locate(&cwd).linked_worktree {
            Answer::InWorktree
        } else {
            Answer::MainWorkingDirectory
        };
        assert_eq!(
            live_answer(&cwd),
            expected,
            "worktree.sh --check and worktree-check disagree about {pos:?}"
        );
    }
}

#[test]
fn the_previously_dead_arm_is_reachable_from_the_primary_clone() {
    // The whole defect, stated as a test: before #9425 NO position reached
    // this arm. Both primary-clone positions must reach it now, with the exact
    // human text and exit code the arm has always carried.
    let fx = build_fixture("dead-arm");
    for pos in [Position::PrimaryRoot, Position::PrimarySubdir] {
        let cwd = fx.cwd(pos);
        let run = run_live(&cwd);
        assert_eq!(run.code, 1, "{pos:?} must exit 1");
        assert!(
            run.stdout.contains("Not currently in a worktree"),
            "{pos:?} must reach the not-in-a-worktree arm; got: {:?}",
            run.stdout
        );
    }
}

#[test]
fn the_retired_comparison_is_constant_true_which_is_the_disagreement() {
    // Asserted as a DISAGREEMENT in the documented direction, not as a match:
    // if the live script ever reverts to the string comparison, the agreement
    // test above goes red rather than quietly re-agreeing with a baseline that
    // had drifted along with it.
    let fx = build_fixture("constant-true");
    for pos in POSITIONS {
        let cwd = fx.cwd(*pos);
        assert!(
            retired_comparison_says_in_worktree(&cwd),
            "the retired comparison is constant-true; it must say \"in a worktree\" at {pos:?} too"
        );
    }
    // …and in two of those four positions that answer is simply wrong, which
    // is what made the arm unreachable.
    for pos in [Position::PrimaryRoot, Position::PrimarySubdir] {
        let cwd = fx.cwd(pos);
        assert!(
            !loom_daemon::worktree_cli::check::locate(&cwd).linked_worktree,
            "{pos:?} is not a linked worktree"
        );
        assert_eq!(live_answer(&cwd), Answer::MainWorkingDirectory);
    }
}

#[test]
fn no_live_script_carries_the_retired_comparison() {
    // A source-level guard, because an expression can be reintroduced into a
    // *different* call site than the ones the behavioural tests drive. The
    // frozen fixture is deliberately exempt — it exists to hold the defect.
    //
    // Comment-only lines are stripped first: the script *quotes* the retired
    // comparison in the comment explaining why it is gone, and a guard that
    // could not tell code from prose would forbid documenting the defect.
    let scripts = ["defaults/scripts/worktree.sh"];
    for rel in scripts {
        let path = repo_root().join(rel);
        let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let code: String = source
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            r#"!= "$work_dir/.git""#,
            r#"!= "$worktree/.git""#,
            r#"!= "$(git rev-parse --show-toplevel)/.git""#,
        ] {
            assert!(
                !code.contains(forbidden),
                "{rel} has regrown the constant-true string comparison: {forbidden}"
            );
        }
    }
}

#[test]
fn the_legacy_root_fork_is_retired_and_pnpm_worktree_points_at_the_canonical_script() {
    // `scripts/worktree.sh` was a stale 240-line fork of
    // `defaults/scripts/worktree.sh` holding a third copy of the predicate. It
    // was reachable only through `pnpm worktree`, and what it created was an
    // unmanaged worktree — no `.loom-managed` sentinel, no claim lease, none of
    // the branch-reuse guards — which CLAUDE.md forbids. #9425 retires it
    // rather than correcting a predicate inside it.
    assert!(
        !repo_root().join("scripts/worktree.sh").exists(),
        "the legacy fork is back; its copy of the predicate comes back with it"
    );
    let pkg = fs::read_to_string(repo_root().join("package.json")).expect("read package.json");
    assert!(
        pkg.contains(r#""worktree": "./.loom/scripts/worktree.sh""#),
        "`pnpm worktree` must run the canonical managed script, as docs/workflows.md already says it does"
    );
}

#[test]
fn a_separate_git_dir_checkout_is_not_a_worktree() {
    // `.git` as a FILE is what defeats the tempting `[[ -f .git ]]` shortcut,
    // and it is where slice 11's string comparison lost even its luck. The
    // delegation inherits the port's correctness here for free — this test is
    // the proof that it does, through the script rather than through the lib.
    let root = tmpdir("separate-git-dir");
    let work = root.join("work");
    let gitdir = root.join("elsewhere.git");
    fs::create_dir_all(&work).expect("mkdir work");
    let mut cmd = Command::new("git");
    hermetic(&mut cmd);
    let out = cmd
        .arg("init")
        .arg("-q")
        .arg("--initial-branch=main")
        .arg(format!("--separate-git-dir={}", gitdir.display()))
        .arg(&work)
        .output()
        .expect("run git init");
    if !out.status.success() {
        return; // git too old for --separate-git-dir on init
    }
    assert!(work.join(".git").is_file(), "fixture precondition: .git is a file");
    assert_eq!(live_answer(&work), Answer::MainWorkingDirectory);
    assert!(
        retired_comparison_says_in_worktree(&work),
        "the retired comparison called this a worktree — the disagreement this case exists for"
    );
}

#[test]
fn a_repo_reached_through_a_symlink_is_not_a_worktree() {
    // The hazard slice 10 found this same comparison failing on: canonicalizing
    // both sides is what makes the answer physical rather than textual.
    let root = tmpdir("symlinked");
    let real = root.join("real");
    fs::create_dir_all(&real).expect("mkdir real");
    git(&real, &["init", "-q", "--initial-branch=main", "."]);
    fs::write(real.join("f"), "hi\n").expect("write f");
    git(&real, &["add", "-A"]);
    git(&real, &["commit", "-qm", "init"]);
    let link = root.join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    #[cfg(not(unix))]
    return;
    assert_eq!(live_answer(&link), Answer::MainWorkingDirectory);
}

#[test]
fn an_unresolvable_daemon_is_neither_answer() {
    // `LOOM_SCRIPT_HELPER_MISSING_RC=2`, argued rather than defaulted: 0 and 1
    // are the verb's two ANSWERS and the call site branches on them, so a host
    // that cannot run the verb must not present as either. Read as 1 here, a
    // daemonless host would look like a considered "you are in the main working
    // directory" from inside a worktree.
    let fx = build_fixture("no-daemon");
    let empty = tmpdir("no-daemon-bin");
    let mut cmd = Command::new("bash");
    hermetic(&mut cmd);
    let out = cmd
        .arg(live_worktree_sh())
        .arg("--check")
        .current_dir(fx.cwd(Position::Worktree))
        .env_remove("LOOM_DAEMON_SELF_BIN")
        .env_remove("LOOM_DAEMON_BIN")
        // The repo-local-build tier probes $CARGO_TARGET_DIR, which a builder
        // session exports; leaving it set would resolve a real daemon.
        .env_remove("CARGO_TARGET_DIR")
        // Starve every resolver tier: the machine-level install directory, then
        // $PATH (bash's own interpreter is already running, so a PATH with no
        // loom-daemon on it is enough), then $HOME, which the installed-binary
        // tier falls back to.
        .env("LOOM_DAEMON_BIN_DIR", &empty)
        .env("HOME", &empty)
        .env("PATH", format!("{}:/usr/bin:/bin", empty.display()))
        .output()
        .expect("run worktree.sh --check");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a host that cannot run the verb must exit 2, not 0 or 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
