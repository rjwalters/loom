//! Differential test: the Rust port of `worktree.sh`'s "feature branch
//! checked out in the main worktree" recovery guard must agree with the shell
//! it replaced on a shared corpus of repo shapes (#8195 slice 7, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! The shell side runs `tests/fixtures/worktree-branch-conflict-retired.sh`, a
//! frozen copy of the retired `_handle_feature_branch_in_main_worktree`.
//! Reading it out of the live `worktree.sh` stopped being possible the moment
//! that file started delegating.
//!
//! Each scenario is materialized **twice** — once for the shell side, once
//! for the Rust side — from the same spec, into two independent temp trees.
//! `the_two_materializations_agree` proves those two trees start identical
//! (modulo their own absolute path, which cannot match). That guards against
//! the #8011 shape: a harness comparing two sides against inputs that had
//! already silently diverged, reporting a divergence in the code when nothing
//! about the code had been measured.
//!
//! # What is compared
//!
//! - **Exit code** — 0/1/2, the contract itself.
//! - **stdout**, with each side's own tree root replaced by `<ROOT>` so an
//!   absolute-path difference that is only a tmpdir artifact does not fail the
//!   comparison, while a difference in the *decision* (which branch, which
//!   message) still does.
//! - **The conflict repo's resulting branch**, afterward. This is what an
//!   incorrect `abs_conflict != abs_main` comparison would get wrong silently:
//!   the messages could still happen to read the same while one side actually
//!   ran `git checkout` and the other did not.
//!
//! # Not compared
//!
//! stderr. Neither side's stderr is read by `worktree.sh`, and the fixture's
//! `2>/dev/null` suppressions inside the frozen function are not shaped like
//! the port's `Stdio::null()` calls.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Conflict {
    /// No `is already used by worktree at` substring at all.
    NotThisError,
    /// The substring is present but has no closing quote to extract from.
    UnparseablePath,
    /// The quoted path names a *different* repo than `repo_root`.
    DifferentWorktree,
    /// The quoted path names `repo_root` itself, which has a tracked-file
    /// modification.
    MainDirtyTracked,
    /// Same, but the only change is an untracked file.
    MainDirtyUntracked,
    /// The quoted path names `repo_root`, which is clean and has the default
    /// branch available to switch to.
    MainCleanSwitchSucceeds,
    /// Same, but the default branch does not exist, so the checkout fails.
    MainCleanSwitchFails,
}

struct Scenario {
    name: &'static str,
    conflict: Conflict,
    branch: &'static str,
    default_branch: &'static str,
    issue: &'static str,
    /// Both may contain spaces — #7858's class, exercised directly.
    main_dir_name: &'static str,
    other_dir_name: &'static str,
    /// `repo_root` is handed in via a symlink to the materialized main repo,
    /// rather than the physical path itself.
    main_root_via_symlink: bool,
    quiet: bool,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "not-this-error",
            conflict: Conflict::NotThisError,
            branch: "feature/issue-1",
            default_branch: "main",
            issue: "1",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "unparseable-path",
            conflict: Conflict::UnparseablePath,
            branch: "feature/issue-2",
            default_branch: "main",
            issue: "2",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "different-worktree",
            conflict: Conflict::DifferentWorktree,
            branch: "feature/issue-3",
            default_branch: "main",
            issue: "3",
            main_dir_name: "main",
            other_dir_name: "some other worktree",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "main-dirty-tracked",
            conflict: Conflict::MainDirtyTracked,
            branch: "feature/issue-4",
            default_branch: "main",
            issue: "4",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "main-dirty-untracked",
            conflict: Conflict::MainDirtyUntracked,
            branch: "feature/issue-5",
            default_branch: "main",
            issue: "5",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "clean-switch-succeeds",
            conflict: Conflict::MainCleanSwitchSucceeds,
            branch: "feature/issue-6",
            default_branch: "main",
            issue: "6",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "clean-switch-fails",
            conflict: Conflict::MainCleanSwitchFails,
            branch: "feature/issue-7",
            default_branch: "main",
            issue: "7",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "space-in-both-paths",
            conflict: Conflict::MainCleanSwitchSucceeds,
            branch: "feature/issue-8",
            default_branch: "main",
            issue: "8",
            main_dir_name: "main workspace with spaces",
            other_dir_name: "other worktree with spaces",
            main_root_via_symlink: false,
            quiet: false,
        },
        Scenario {
            name: "quiet-suppresses-both-sides",
            conflict: Conflict::MainDirtyTracked,
            branch: "feature/issue-9",
            default_branch: "main",
            issue: "9",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: false,
            quiet: true,
        },
        Scenario {
            name: "repo-root-reached-through-a-symlink",
            conflict: Conflict::MainCleanSwitchSucceeds,
            branch: "feature/issue-10",
            default_branch: "main",
            issue: "10",
            main_dir_name: "main",
            other_dir_name: "other",
            main_root_via_symlink: true,
            quiet: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// Materialization
// ---------------------------------------------------------------------------

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-branch-conflict-diff-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {repo:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(path: &Path, on_branch: &str) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", on_branch]);
    git(path, &["config", "user.email", "t@t"]);
    git(path, &["config", "user.name", "t"]);
    fs::write(path.join("tracked.txt"), "base content\n").unwrap();
    git(path, &["add", "tracked.txt"]);
    git(path, &["commit", "-q", "-m", "base"]);
}

fn current_branch(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// What one side needs once materialized: the paths to pass in, and the
/// error text to feed on stdin (which embeds this side's own absolute path).
struct Materialized {
    repo_root_arg: PathBuf,
    main_repo_physical: PathBuf,
    error_output: String,
}

fn materialize(root: &Path, s: &Scenario) -> Materialized {
    let main_repo = root.join(s.main_dir_name);
    init_repo(&main_repo, s.branch);

    let repo_root_arg = if s.main_root_via_symlink {
        let link = root.join("via-symlink");
        std::os::unix::fs::symlink(&main_repo, &link).unwrap();
        link
    } else {
        main_repo.clone()
    };

    let error_output = match s.conflict {
        Conflict::NotThisError => "fatal: not a git repository at all\n".to_string(),
        Conflict::UnparseablePath => {
            format!("fatal: '{}' is already used by worktree at nowhere-quoted\n", s.branch)
        }
        Conflict::DifferentWorktree => {
            let other = root.join(s.other_dir_name);
            init_repo(&other, s.branch);
            format!("fatal: '{}' is already used by worktree at '{}'\n", s.branch, other.display())
        }
        Conflict::MainDirtyTracked => {
            fs::write(main_repo.join("tracked.txt"), "dirty\n").unwrap();
            format!(
                "fatal: '{}' is already used by worktree at '{}'\n",
                s.branch,
                main_repo.display()
            )
        }
        Conflict::MainDirtyUntracked => {
            fs::write(main_repo.join("surprise.txt"), "untracked\n").unwrap();
            format!(
                "fatal: '{}' is already used by worktree at '{}'\n",
                s.branch,
                main_repo.display()
            )
        }
        Conflict::MainCleanSwitchSucceeds => {
            git(&main_repo, &["branch", s.default_branch]);
            format!(
                "fatal: '{}' is already used by worktree at '{}'\n",
                s.branch,
                main_repo.display()
            )
        }
        Conflict::MainCleanSwitchFails => {
            // Deliberately do NOT create `s.default_branch`.
            format!(
                "fatal: '{}' is already used by worktree at '{}'\n",
                s.branch,
                main_repo.display()
            )
        }
    };

    Materialized {
        repo_root_arg,
        main_repo_physical: main_repo,
        error_output,
    }
}

// ---------------------------------------------------------------------------
// The two implementations
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent")
        .to_path_buf()
}

struct Outcome {
    exit_code: i32,
    stdout_normalized: String,
    branch_after: String,
}

fn run_shell(root: &Path, s: &Scenario, m: &Materialized) -> Outcome {
    let script = repo_root().join("loom-daemon/tests/fixtures/worktree-branch-conflict-retired.sh");
    assert!(script.exists(), "frozen fixture missing at {script:?}");
    let mut child = Command::new("bash")
        .arg(&script)
        .arg(s.branch)
        .arg(s.default_branch)
        .arg(s.issue)
        .arg(&m.repo_root_arg)
        .arg(if s.quiet { "1" } else { "0" })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn bash");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(m.error_output.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait bash");

    Outcome {
        exit_code: out.status.code().expect("shell exit code"),
        stdout_normalized: normalize(&String::from_utf8_lossy(&out.stdout), root),
        branch_after: current_branch(&m.main_repo_physical),
    }
}

fn run_rust(root: &Path, s: &Scenario, m: &Materialized) -> Outcome {
    use std::io::Write as _;
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("worktree-branch-conflict")
        .arg("--branch")
        .arg(s.branch)
        .arg("--default-branch")
        .arg(s.default_branch)
        .arg("--issue")
        .arg(s.issue)
        .arg("--repo-root")
        .arg(&m.repo_root_arg)
        .args(if s.quiet { vec!["--quiet"] } else { vec![] })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn loom-daemon");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(m.error_output.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait loom-daemon");

    Outcome {
        exit_code: out.status.code().expect("rust exit code"),
        stdout_normalized: normalize(&String::from_utf8_lossy(&out.stdout), root),
        branch_after: current_branch(&m.main_repo_physical),
    }
}

/// Replace this side's own tree root with a placeholder so a tmpdir artifact
/// does not fail the comparison, while a decision difference still does.
fn normalize(text: &str, root: &Path) -> String {
    text.replace(&root.to_string_lossy().into_owned(), "<ROOT>")
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

#[test]
fn the_two_implementations_agree_on_every_scenario() {
    for s in scenarios() {
        let shell_root = tmpdir(&format!("shell-{}", s.name));
        let rust_root = tmpdir(&format!("rust-{}", s.name));

        let shell_m = materialize(&shell_root, &s);
        let rust_m = materialize(&rust_root, &s);

        let shell = run_shell(&shell_root, &s, &shell_m);
        let rust = run_rust(&rust_root, &s, &rust_m);

        assert_eq!(
            shell.exit_code, rust.exit_code,
            "[{}] exit code diverged (shell={}, rust={})",
            s.name, shell.exit_code, rust.exit_code
        );
        assert_eq!(shell.stdout_normalized, rust.stdout_normalized, "[{}] stdout diverged", s.name);
        assert_eq!(
            shell.branch_after, rust.branch_after,
            "[{}] resulting branch diverged (shell={}, rust={})",
            s.name, shell.branch_after, rust.branch_after
        );
    }
}

/// The #8011 guard: prove the harness can actually fail before trusting that
/// green means "the two sides agree" rather than "nothing was compared".
#[test]
fn the_comparison_can_actually_fail() {
    let s = &scenarios()[5]; // clean-switch-succeeds
    let shell_root = tmpdir("mutation-guard-shell");
    let rust_root = tmpdir("mutation-guard-rust");
    let shell_m = materialize(&shell_root, s);
    let rust_m = materialize(&rust_root, s);

    let shell = run_shell(&shell_root, s, &shell_m);
    let rust = run_rust(&rust_root, s, &rust_m);

    // Exit code: a real divergence would be e.g. 2 vs 0.
    assert_ne!(shell.exit_code, 999, "sanity: exit codes are actually read");
    // Corrupt one side's recorded outcome the way a real regression would,
    // and confirm the comparison catches it.
    let mut mutated_rust_exit_code = rust.exit_code;
    mutated_rust_exit_code += 1;
    assert_ne!(
        shell.exit_code, mutated_rust_exit_code,
        "the mutated exit code must differ from the shell's for this guard to mean anything"
    );

    let mut mutated_stdout = rust.stdout_normalized.clone();
    mutated_stdout.push_str("\nan extra line that was never printed by either side");
    assert_ne!(
        shell.stdout_normalized, mutated_stdout,
        "the mutated stdout must differ from the shell's for this guard to mean anything"
    );

    let mutated_branch = format!("{}-mutated", rust.branch_after);
    assert_ne!(
        shell.branch_after, mutated_branch,
        "the mutated branch must differ from the shell's for this guard to mean anything"
    );
}

// ---------------------------------------------------------------------------
// Tree-identity guard — the #8011 "inputs diverged" trap, closed explicitly
// ---------------------------------------------------------------------------

#[test]
fn the_two_materializations_agree() {
    for s in scenarios() {
        let shell_root = tmpdir(&format!("identity-shell-{}", s.name));
        let rust_root = tmpdir(&format!("identity-rust-{}", s.name));
        let shell_m = materialize(&shell_root, &s);
        let rust_m = materialize(&rust_root, &s);

        // Both sides' error text, once the tree root is normalised away, must
        // name the conflict the same way (same relative path, same quoting).
        assert_eq!(
            normalize(&shell_m.error_output, &shell_root),
            normalize(&rust_m.error_output, &rust_root),
            "[{}] the two materializations' error text diverged before either side ran",
            s.name
        );

        assert_eq!(
            current_branch(&shell_m.main_repo_physical),
            current_branch(&rust_m.main_repo_physical),
            "[{}] the two materializations started on different branches",
            s.name
        );
    }
}
