//! Differential test: the Rust port of `worktree.sh`'s in-worktree detection
//! against the shell it replaced (#8195 slice 11, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! **No retained suite asserts either consumer's output** — not one of the 21
//! `test-worktree-*.sh` files runs `--check`, and none greps for the
//! auto-navigation banner. That absence is not incidental: it is precisely why
//! a predicate that was wrong from every position a caller can stand in shipped
//! and stayed. So this harness *is* the specification, and it is built from the
//! only grammar the code has: **position × `--json`**.
//!
//! The four positions are the primary clone's root, a subdirectory of it, a
//! linked worktree, and a subdirectory of a linked worktree. They are compared
//! against one shared fixture per side — materialised twice from the same
//! builder and asserted byte-identical *before* either implementation runs, so
//! "the inputs differed" can never masquerade as a finding.
//!
//! # What is compared
//!
//! Exit code, stdout and stderr byte for byte (after redacting the per-side
//! root), and — for the navigation arm — the working directory the caller is
//! left in, which is the arm's whole purpose and its only non-textual
//! observable.
//!
//! # This harness asserts DISAGREEMENT, mostly
//!
//! The retired predicate,
//!
//! ```text
//! [[ "$(git rev-parse --git-common-dir)" != "$(git rev-parse --show-toplevel)/.git" ]]
//! ```
//!
//! compares a path git answers *relative to the current directory* against an
//! absolute one, so it is **constant-true**: three of the four positions get
//! the wrong answer, and the fourth (a linked worktree) gets the right answer
//! for the wrong reason. Each divergence therefore has a test asserting the two
//! sides DISAGREE in the documented direction, so a regression to the shell's
//! answer fails rather than quietly re-agreeing:
//!
//! 1. `--check` in the primary clone: shell says "Current worktree:" and exits
//!    0; the port says "Not currently in a worktree" and exits 1.
//! 2. Same from a subdirectory of the primary clone.
//! 3. The navigation arm in the primary clone: the shell prints four lines and
//!    `cd`s to `dirname ".git"` = `.` — a no-op **by luck**, since git's
//!    relative answer happens to be the relative path to the repo root; the
//!    port prints nothing and navigates nowhere.
//! 4. A `--separate-git-dir` checkout, where `.git` is a *file*: the shell
//!    calls it a worktree and would navigate to the directory holding the
//!    detached git dir — a real, non-lucky wrong `cd`.
//!
//! And the one place they must AGREE is the position that matters for safety:
//! inside a linked worktree, both must report the worktree and both must leave
//! the caller in the primary clone. That agreement is what makes the port's
//! nested-worktree prevention equivalent, and it is asserted for `--json` too,
//! where the shell printed nothing at all.
//!
//! # The shell fallback is covered here too
//!
//! `worktree.sh` keeps a one-line physical fallback for a host with no daemon
//! (or one predating this subcommand), because a silent "not in a worktree"
//! there would let `git worktree add` nest a worktree inside another. It is not
//! a second implementation of the decision, and
//! [`the_shell_fallback_agrees_with_the_port_from_every_position`] is the proof:
//! it runs the *live* `worktree.sh`'s fallback expression against the port's
//! answer in all four positions plus the `--separate-git-dir` case.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

fn retired_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-check-retired.sh")
}

fn live_worktree_sh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/worktree.sh")
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The four positions a caller can stand in. Named after what `worktree.sh`
/// would be doing there, not after the directory layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    /// The primary clone's root — where every Builder invokes `worktree.sh <N>`.
    PrimaryRoot,
    /// A subdirectory of the primary clone; git answers `--git-common-dir` as
    /// `../.git` here.
    PrimarySubdir,
    /// Inside a linked worktree — the position auto-navigation exists for.
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

struct Side {
    root: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
}

impl Side {
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
    let base = std::env::temp_dir().join(format!(
        "loom-wt-check-diff-{tag}-{}-{:?}",
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

/// One side's fixture: a repo with a commit, a subdirectory, and a linked
/// worktree for `feature/issue-42` laid out the way `worktree.sh` lays one out.
fn build_side(tag: &str, side: &str) -> Side {
    let root = tmpdir(&format!("{tag}-{side}"));
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
    Side {
        root,
        repo,
        worktree,
    }
}

/// The two sides, plus the assertion that they are indistinguishable before
/// either implementation has run.
fn two_sides(tag: &str) -> (Side, Side) {
    let a = build_side(tag, "shell");
    let b = build_side(tag, "rust");
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
        // Skip the git administrative trees: they carry absolute paths and
        // per-worktree bookkeeping that is not what this comparison is about.
        if path.file_name().is_some_and(|n| n == ".git") {
            out.push(format!("{} [gitdir]", path.display()));
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

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    code: i32,
    stdout: String,
    stderr: String,
    /// Where the caller was left. `None` for the `--check` arm, which never
    /// navigates.
    cwd: Option<String>,
}

fn redact(text: &str, side: &Side) -> String {
    text.replace(side.root.to_str().expect("utf8"), "<ROOT>")
}

fn observe(out: &Output, side: &Side, want_cwd: bool) -> Observed {
    let stdout = redact(&String::from_utf8_lossy(&out.stdout), side);
    let (stdout, cwd) = if want_cwd {
        let mut lines: Vec<&str> = stdout.lines().collect();
        let cwd = lines
            .iter()
            .position(|l| l.starts_with("CWD="))
            .map(|i| lines.remove(i)["CWD=".len()..].to_string());
        (lines.join("\n"), cwd)
    } else {
        (stdout, None)
    };
    Observed {
        code: out.status.code().unwrap_or(-1),
        stdout,
        stderr: redact(&String::from_utf8_lossy(&out.stderr), side),
        cwd,
    }
}

/// The retired shell, in `cwd`.
fn run_shell(side: &Side, pos: Position, arm: &str, json: bool) -> Observed {
    let mut cmd = Command::new("bash");
    hermetic(&mut cmd);
    let out = cmd
        .arg(retired_fixture())
        .arg(arm)
        .current_dir(side.cwd(pos))
        .env("JSON_OUTPUT", if json { "true" } else { "false" })
        .output()
        .expect("run retired fixture");
    observe(&out, side, arm == "navigate")
}

/// The port, in `cwd`. The `navigate` arm is the port plus the wrapper's own
/// `cd` and its `print_success` — the two things that stay in `worktree.sh`
/// because only the calling process can change its own directory — replayed
/// here by the same `case` the script uses, so the comparison covers the whole
/// retired block rather than only the part that moved.
fn run_rust(side: &Side, pos: Position, arm: &str, json: bool) -> Observed {
    let cwd = side.cwd(pos);
    let out = if arm == "check" {
        let mut cmd = Command::new(bin());
        hermetic(&mut cmd);
        cmd.arg("worktree-check")
            .current_dir(&cwd)
            .output()
            .expect("run port")
    } else {
        let script = format!(
            r#"
set -e
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'
print_error()   {{ echo -e "${{RED}}ERROR: $1${{NC}}" >&2; }}
print_success() {{ echo -e "${{GREEN}}✓ $1${{NC}}"; }}
print_info()    {{ echo -e "${{BLUE}}ℹ $1${{NC}}"; }}
print_warning() {{ echo -e "${{YELLOW}}⚠ $1${{NC}}"; }}
if [[ "$JSON_OUTPUT" == "true" ]]; then exec 3>&1 1>&2; else exec 3>&1; fi
_WT_IN_WORKTREE=false; _WT_MAIN_WORKSPACE=""
_q=""; [[ "$JSON_OUTPUT" != "true" ]] || _q="--quiet"
_out="$("{bin}" worktree-check --porcelain $_q 2>/dev/null)" || _out=""
while IFS=$'\t' read -r _l _m; do
    case "$_l" in
        IN_WORKTREE)    _WT_IN_WORKTREE=true ;;
        MAIN_WORKSPACE) _WT_MAIN_WORKSPACE="$_m" ;;
        WARNING)        print_warning "$_m" ;;
        INFO)           print_info "$_m" ;;
        PLAIN)          echo "$_m" ;;
        BLANK)          echo "" ;;
    esac
done <<<"$_out"
if [[ "$_WT_IN_WORKTREE" == "true" ]]; then
    if [[ -z "$_WT_MAIN_WORKSPACE" ]]; then
        if [[ "$JSON_OUTPUT" == "true" ]]; then
            echo '{{"error": "Failed to find git common directory"}}' >&3
        else
            print_error "Failed to find git common directory"
        fi
        exit 1
    fi
    if cd "$_WT_MAIN_WORKSPACE" 2>/dev/null; then
        [[ "$JSON_OUTPUT" == "true" ]] || print_success "Switched to main workspace"
    else
        if [[ "$JSON_OUTPUT" == "true" ]]; then
            echo '{{"error": "Failed to change to main workspace", "mainWorkspace": "'"$_WT_MAIN_WORKSPACE"'"}}' >&3
        else
            print_error "Failed to change to main workspace: $_WT_MAIN_WORKSPACE"
            print_info "Please manually run: cd $_WT_MAIN_WORKSPACE"
        fi
        exit 1
    fi
    [[ "$JSON_OUTPUT" == "true" ]] || echo ""
fi
echo "CWD=$(pwd -P)" >&3
"#,
            bin = bin()
        );
        let mut cmd = Command::new("bash");
        hermetic(&mut cmd);
        cmd.arg("-c")
            .arg(script)
            .current_dir(&cwd)
            .env("JSON_OUTPUT", if json { "true" } else { "false" })
            .output()
            .expect("run port wrapper")
    };
    observe(&out, side, arm == "navigate")
}

// ---------------------------------------------------------------------------
// Where they must AGREE: inside a linked worktree
// ---------------------------------------------------------------------------

#[test]
fn check_agrees_inside_a_linked_worktree() {
    let (a, b) = two_sides("check-agree");
    for pos in [Position::Worktree, Position::WorktreeSubdir] {
        let shell = run_shell(&a, pos, "check", false);
        let rust = run_rust(&b, pos, "check", false);
        assert_eq!(rust, shell, "`--check` must be unchanged at {pos:?}");
        assert_eq!(shell.code, 0);
    }
}

#[test]
fn navigation_agrees_inside_a_linked_worktree() {
    // The position the whole block exists for, and the one whose behaviour is
    // load-bearing: leaving the caller in the primary clone is what stops
    // `git worktree add` from nesting a worktree inside another one.
    let (a, b) = two_sides("nav-agree");
    for pos in [Position::Worktree, Position::WorktreeSubdir] {
        for json in [false, true] {
            let shell = run_shell(&a, pos, "navigate", json);
            let rust = run_rust(&b, pos, "navigate", json);
            assert_eq!(rust, shell, "navigation must be unchanged at {pos:?} (json={json})");
            assert_eq!(
                shell.cwd.as_deref(),
                Some("<ROOT>/repo"),
                "both sides leave the caller in the primary clone"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Where they must DISAGREE: every position outside a linked worktree
// ---------------------------------------------------------------------------

#[test]
fn check_disagrees_in_the_primary_clone_and_the_port_is_right() {
    let (a, b) = two_sides("check-primary");
    for pos in [Position::PrimaryRoot, Position::PrimarySubdir] {
        let shell = run_shell(&a, pos, "check", false);
        let rust = run_rust(&b, pos, "check", false);

        assert_eq!(shell.code, 0, "the retired predicate was constant-true at {pos:?}");
        assert!(
            shell.stdout.starts_with("Current worktree:"),
            "retired output at {pos:?}: {:?}",
            shell.stdout
        );
        assert_eq!(rust.code, 1, "the port reports the main working directory at {pos:?}");
        assert_eq!(
            rust.stdout, "Not currently in a worktree (you're in the main working directory)\n",
            "the arm that was dead code is now the one that runs"
        );
        assert_ne!(rust, shell);
    }
}

#[test]
fn navigation_disagrees_in_the_primary_clone_and_the_shell_was_lucky() {
    let (a, b) = two_sides("nav-primary");
    for pos in [Position::PrimaryRoot, Position::PrimarySubdir] {
        let shell = run_shell(&a, pos, "navigate", false);
        let rust = run_rust(&b, pos, "navigate", false);

        assert!(
            shell.stdout.contains("Currently in a worktree"),
            "the retired block announced a navigation it had no reason to make, at {pos:?}: {:?}",
            shell.stdout
        );
        assert!(
            shell.stdout.contains("Found main workspace"),
            "…and named a main workspace from `dirname` of git's RELATIVE answer"
        );
        assert_eq!(
            rust.stdout, "",
            "the port prints nothing at {pos:?} — there is nothing to navigate"
        );
        assert_ne!(rust, shell);

        // Both end up in the primary clone, but only one of them by design:
        // `dirname ".git"` is `.`, and `dirname "../.git"` is `..`, which
        // happen to be the relative path to the repo root from those two
        // positions. Pinned so a future reader does not mistake the retired
        // behaviour for correct.
        assert_eq!(shell.cwd.as_deref(), Some("<ROOT>/repo"), "lucky, not correct");
        assert_eq!(
            rust.cwd.as_deref(),
            Some(if pos == Position::PrimaryRoot {
                "<ROOT>/repo"
            } else {
                "<ROOT>/repo/sub"
            }),
            "the port never moved, so the caller is exactly where it started"
        );
    }
}

#[test]
fn navigation_disagrees_in_json_mode_too_where_the_shell_printed_nothing() {
    // Under `--json` every message is suppressed on both sides, so the ONLY
    // observable is the `cd`. This is the shape that made the defect survive:
    // nothing a machine reads ever changed.
    let (a, b) = two_sides("nav-json");
    let shell = run_shell(&a, Position::PrimarySubdir, "navigate", true);
    let rust = run_rust(&b, Position::PrimarySubdir, "navigate", true);
    assert_eq!(shell.stdout, "", "no messages under --json");
    assert_eq!(rust.stdout, "", "no messages under --json");
    assert_eq!(shell.cwd.as_deref(), Some("<ROOT>/repo"));
    assert_eq!(rust.cwd.as_deref(), Some("<ROOT>/repo/sub"));
}

#[test]
fn a_separate_git_dir_checkout_is_where_the_shells_luck_runs_out() {
    // `git init --separate-git-dir` leaves `.git` as a FILE in the working
    // tree, and the common git dir somewhere else entirely — so `dirname` of
    // git's (absolute) answer is NOT the repo root, and the retired block
    // `cd`s outside the repository. This is the same predicate failing without
    // the accident that hid it everywhere else.
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

    let side = Side {
        root: root.clone(),
        repo: work.clone(),
        worktree: work.clone(),
    };
    let shell = run_shell(&side, Position::PrimaryRoot, "navigate", false);
    let rust = run_rust(&side, Position::PrimaryRoot, "navigate", false);
    assert_eq!(
        shell.cwd.as_deref(),
        Some("<ROOT>"),
        "the retired block navigated OUT of the repository — the wrong `cd` its \
         relative-path luck hid in every other position"
    );
    assert_eq!(
        rust.cwd.as_deref(),
        Some("<ROOT>/work"),
        "the port leaves the caller in the checkout, because this is not a linked worktree"
    );
}

// ---------------------------------------------------------------------------
// The live shell fallback, which must agree with the port everywhere
// ---------------------------------------------------------------------------

/// The fallback expression `worktree.sh` uses when no daemon can answer,
/// extracted from the LIVE script rather than copied, so the two cannot drift.
fn live_fallback(cwd: &Path) -> (bool, String) {
    let source = fs::read_to_string(live_worktree_sh()).expect("read worktree.sh");
    let marker = r#"    _gd="$(cd "$(git rev-parse --git-dir 2>/dev/null || echo .)" 2>/dev/null && pwd -P)" || _gd=""#;
    assert!(
        source.contains(marker),
        "worktree.sh no longer contains the fallback this test extracts; update both together"
    );
    let body = source
        .split_once(marker)
        .expect("split on marker")
        .1
        .lines()
        .take_while(|l| !l.starts_with('}'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("_WT_IN_WORKTREE=true"),
        "the extracted fallback body must still be the one that sets the flag; got:\n{body}"
    );
    // The body is re-hosted inside a real shell FUNCTION rather than spliced
    // into the top level, because its decision is expressed as an early
    // `return 0` on the "not a linked worktree" branch. Run at the top level
    // that `return` is a syntax error, and neutralising it (`return 0` -> `:`)
    // would let control fall through to `_WT_IN_WORKTREE=true` — i.e. the
    // harness would report "in a worktree" from every position, which is
    // exactly the constant-true defect this slice exists to retire. A function
    // keeps the branch meaning what `worktree.sh` means by it.
    let script = format!(
        "set -e\n\
         _WT_IN_WORKTREE=false\n\
         _WT_MAIN_WORKSPACE=\"\"\n\
         _loom_fallback_under_test() {{\n\
         \x20   local _gd _cd\n\
         {marker}\n{body}\n}}\n\
         _loom_fallback_under_test\n\
         echo \"$_WT_IN_WORKTREE $_WT_MAIN_WORKSPACE\"\n"
    );
    let mut cmd = Command::new("bash");
    hermetic(&mut cmd);
    let out = cmd
        .arg("-c")
        .arg(&script)
        .current_dir(cwd)
        .output()
        .expect("run fallback");
    assert!(
        out.status.success(),
        "fallback failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (flag, ws) = line.split_once(' ').unwrap_or((line.as_str(), ""));
    (flag == "true", ws.to_string())
}

#[test]
fn the_shell_fallback_agrees_with_the_port_from_every_position() {
    // `worktree.sh`'s no-daemon fallback is deliberately a SECOND SPELLING of
    // the same physical question, not a second decision. This is what makes
    // that claim checkable: the same answer, in every position, including the
    // `--separate-git-dir` case that defeats the `[[ -f .git ]]` shortcut.
    let (side, _unused) = two_sides("fallback");
    for pos in POSITIONS {
        let cwd = side.cwd(*pos);
        let (fallback_in_wt, fallback_ws) = live_fallback(&cwd);
        let port = loom_daemon::worktree_cli::check::locate(&cwd);
        assert_eq!(
            fallback_in_wt, port.linked_worktree,
            "fallback and port disagree about {pos:?}"
        );
        if port.linked_worktree {
            assert_eq!(
                fallback_ws,
                port.main_workspace
                    .as_ref()
                    .expect("main workspace")
                    .display()
                    .to_string(),
                "fallback and port disagree about the main workspace at {pos:?}"
            );
        }
    }
}

#[test]
fn the_shell_fallback_also_agrees_on_a_separate_git_dir_checkout() {
    let root = tmpdir("fallback-separate");
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
        return;
    }
    let (fallback_in_wt, _) = live_fallback(&work);
    assert!(!fallback_in_wt, "`.git` being a FILE is not what makes a linked worktree");
    assert!(!loom_daemon::worktree_cli::check::locate(&work).linked_worktree);
}

#[test]
fn the_shell_fallback_owns_the_decision_but_not_one_word_of_the_message_text() {
    // The degradation on a daemonless host is DELIBERATE and asymmetric: the
    // safety property (never navigate into, or add a worktree inside, another
    // worktree) is preserved exactly, while the four banner lines are lost —
    // the same trade slices 4, 5 and 9 make with their `optional` markers.
    //
    // This is what keeps it from silently becoming a second implementation
    // again: the fallback body may not name any of the port's strings, and may
    // not call the print helpers. If a future change wants those messages back
    // on a daemonless host, it has to move them somewhere both sides read —
    // not re-type them here, where nothing would pin them to the port.
    let source = fs::read_to_string(live_worktree_sh()).expect("read worktree.sh");
    let marker = r#"    _gd="$(cd "$(git rev-parse --git-dir 2>/dev/null || echo .)" 2>/dev/null && pwd -P)" || _gd=""#;
    let body: String = source
        .split_once(marker)
        .expect("split on marker")
        .1
        .lines()
        .take_while(|l| !l.starts_with('}'))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "print_warning",
        "print_info",
        "Currently in a worktree",
        "Current worktree:",
        "Found main workspace",
    ] {
        assert!(
            !body.contains(forbidden),
            "the daemonless fallback must not re-implement the port's output; found {forbidden:?}"
        );
    }
}
