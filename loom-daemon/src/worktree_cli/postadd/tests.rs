//! Unit tests for the post-`git worktree add` finalization port (#8195 slice
//! 16).
//!
//! The retained suites (`defaults/scripts/tests/test-worktree-hookspath.sh` for
//! `core.hooksPath`, `tests/hooks/test-post-worktree-target-dir.sh` for what the
//! hook itself does with `LOOM_WORKTREE_CARGO_TARGET_DIR`) and the differential
//! harness (`tests/worktree_postadd_differential.rs`) are the equivalence
//! evidence. These are the cases those cannot reach cheaply: the hook's exact
//! argv/cwd/environment, and the path shapes — above all a **space-bearing
//! path**, #7858's class — that a shell fixture cannot express without becoming
//! a test of the fixture.

use super::*;
use std::fs;

/// A throwaway directory that removes itself. Same shape as
/// [`super::super::link`]'s: `temp_dir()` + pid + a counter, so parallel test
/// threads never collide.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("loom-wt-postadd-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp tree");
        Self(path)
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(&p).expect("mkdir -p");
        p
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fixture: a main workspace and a real git repo standing in for the
/// worktree. `git -C <worktree> config` needs a repository; it does not need
/// that repository to be a *linked* worktree, and nothing in this slice reads
/// the link.
struct Fixture {
    _tree: TempTree,
    repo_root: PathBuf,
    worktree: PathBuf,
}

impl Fixture {
    /// `repo_rel` / `worktree_rel` are relative names so a caller can put a
    /// space in either.
    fn new(tag: &str, repo_rel: &str, worktree_rel: &str) -> Self {
        let tree = TempTree::new(tag);
        let repo_root = tree.dir(repo_rel);
        let worktree = tree.dir(worktree_rel);
        git_init(&worktree);
        Self {
            _tree: tree,
            repo_root,
            worktree,
        }
    }

    fn options(&self) -> Options {
        Options {
            repo_root: self.repo_root.clone(),
            worktree: self.worktree.clone(),
            branch: "feature/issue-42".to_string(),
            issue: "42".to_string(),
            // Quiet throughout: these assertions are about effects, and the
            // test harness's captured stdout is not the contract under test.
            quiet: true,
        }
    }

    /// Install `.loom/hooks/post-worktree.sh` recording argv, cwd and the one
    /// environment variable this slice sets, then `exit $exit_code`.
    fn install_hook(&self, exit_code: i32) -> PathBuf {
        let hook = self.repo_root.join(HOOK_REL_PATH);
        fs::create_dir_all(hook.parent().expect("hook parent")).expect("mkdir hooks");
        let record = self.repo_root.join("hook-record.txt");
        let script = format!(
            "#!/bin/sh\n\
             {{\n\
             printf 'argc=%s\\n' \"$#\"\n\
             printf 'argv1=%s\\n' \"$1\"\n\
             printf 'argv2=%s\\n' \"$2\"\n\
             printf 'argv3=%s\\n' \"$3\"\n\
             printf 'cwd=%s\\n' \"$(pwd)\"\n\
             printf 'target=%s\\n' \"${{{env}-<unset>}}\"\n\
             }} > '{record}'\n\
             exit {exit_code}\n",
            env = CARGO_TARGET_ENV,
            record = record.display(),
            exit_code = exit_code,
        );
        fs::write(&hook, script).expect("write hook");
        make_executable(&hook);
        record
    }

    /// `--local`, deliberately: the shell wrote to the repository-local config
    /// (`git config` without a scope flag does), and this dispatch host's own
    /// `~/.gitconfig` sets a `core.hooksPath` of its own — reading the merged
    /// value would make every assertion here answer about the HOST.
    fn hooks_path(&self) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.worktree)
            .args(["config", "--local", "--get", "core.hooksPath"])
            .output()
            .expect("git config --get");
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }
}

fn git_init(dir: &Path) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .status()
        .expect("git init")
        .success();
    assert!(ok, "git init failed in {}", dir.display());
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("stat").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod +x");
    }
}

fn record_field(record: &Path, key: &str) -> String {
    let body = fs::read_to_string(record).expect("hook record");
    for line in body.lines() {
        if let Some(value) = line.strip_prefix(&format!("{key}=")) {
            return value.to_string();
        }
    }
    panic!("no `{key}=` line in hook record:\n{body}");
}

// ---------------------------------------------------------------------------
// #7858's class: a space-bearing path must survive intact
// ---------------------------------------------------------------------------

/// **The regression this slice exists to make unreachable.** Both the main
/// workspace and the worktree sit under a path containing a space, and the
/// hook must receive the worktree path as ONE argument with the space in it.
///
/// In the retired shell this was `(cd "$ABS_WORKTREE_PATH" && "$POST_WORKTREE_HOOK"
/// "$ABS_WORKTREE_PATH" …)` — correct only because every expansion happened to
/// be quoted. #7858 is what one missing pair of quotes on that class of line
/// cost: an `rm -rf` on a LIVE worktree. Here there is no word-splitting stage
/// at all, and this test fails if anyone reintroduces one (e.g. by building a
/// shell command line and handing it to `sh -c`).
#[test]
fn hook_receives_argv_with_spaces_intact() {
    let fx = Fixture::new("spaces", "main work space", "issue 42 worktree");
    let record = fx.install_hook(0);

    assert_eq!(run(&fx.options()), 0);

    assert_eq!(record_field(&record, "argc"), "3", "hook must get exactly 3 arguments");
    assert_eq!(
        record_field(&record, "argv1"),
        fx.worktree.display().to_string(),
        "the worktree path must arrive whole, space included"
    );
    assert_eq!(record_field(&record, "argv2"), "feature/issue-42");
    assert_eq!(record_field(&record, "argv3"), "42");
}

/// The same property one level down: the hook runs *from inside* the worktree,
/// and a space in that path must not truncate the `cd`.
#[test]
fn hook_runs_from_inside_the_worktree() {
    let fx = Fixture::new("cwd", "main work space", "issue 42 worktree");
    let record = fx.install_hook(0);

    assert_eq!(run(&fx.options()), 0);

    let cwd = PathBuf::from(record_field(&record, "cwd"));
    assert_eq!(
        fs::canonicalize(&cwd).expect("canonicalize cwd"),
        fs::canonicalize(&fx.worktree).expect("canonicalize worktree"),
        "the hook's cwd must be the worktree"
    );
}

/// A space-bearing worktree path must also reach `git -C` whole — the other
/// interpolated-path call in this module.
#[test]
fn hooks_path_is_set_on_a_space_bearing_worktree() {
    let fx = Fixture::new("hookspath-spaces", "main work space", "issue 42 worktree");
    fs::create_dir_all(fx.repo_root.join(GITHOOKS_DIR)).expect("mkdir .githooks");

    assert_eq!(run(&fx.options()), 0);

    assert_eq!(fx.hooks_path().as_deref(), Some(HOOKS_PATH_VALUE));
}

// ---------------------------------------------------------------------------
// core.hooksPath (#3638)
// ---------------------------------------------------------------------------

/// #3638's whole point: a repo with NO tracked `.githooks/` must not have
/// `core.hooksPath` pointed at a directory that does not exist — git reads a
/// missing `hooksPath` as "no hooks at all", silently disabling any hooks
/// configured elsewhere.
#[test]
fn hooks_path_is_left_unset_without_a_githooks_dir() {
    let fx = Fixture::new("no-githooks", "main", "wt");

    assert_eq!(run(&fx.options()), 0);

    assert_eq!(fx.hooks_path(), None, "core.hooksPath must be left unset");
}

/// `[[ -d ]]` is a *directory* test that follows symlinks. A regular FILE named
/// `.githooks` is not a hooks directory and must not arm the config.
#[test]
fn a_githooks_file_is_not_a_githooks_dir() {
    let fx = Fixture::new("githooks-file", "main", "wt");
    fs::write(fx.repo_root.join(GITHOOKS_DIR), "not a directory").expect("write");

    assert_eq!(run(&fx.options()), 0);

    assert_eq!(fx.hooks_path(), None);
}

/// …and a SYMLINK to a directory is one, because `[[ -d ]]` stats through
/// links. This is the one place `symlink_metadata` would have diverged.
#[test]
#[cfg(unix)]
fn a_symlink_to_a_githooks_dir_counts() {
    let fx = Fixture::new("githooks-symlink", "main", "wt");
    let real = fx.repo_root.join("real-hooks");
    fs::create_dir_all(&real).expect("mkdir");
    std::os::unix::fs::symlink(&real, fx.repo_root.join(GITHOOKS_DIR)).expect("symlink");

    assert_eq!(run(&fx.options()), 0);

    assert_eq!(fx.hooks_path().as_deref(), Some(HOOKS_PATH_VALUE));
}

// ---------------------------------------------------------------------------
// The hook's execute guard and failure contract
// ---------------------------------------------------------------------------

/// `[[ -x ]]`: a hook file with no execute bit is not run. Most repos have no
/// hook at all, and a non-executable one is the shape a partial checkout or a
/// lost mode bit leaves behind.
#[test]
#[cfg(unix)]
fn a_non_executable_hook_is_not_run() {
    let fx = Fixture::new("hook-not-x", "main", "wt");
    let record = fx.install_hook(0);
    let hook = fx.repo_root.join(HOOK_REL_PATH);
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("stat").permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&hook, perms).expect("chmod -x");
    }

    assert_eq!(run(&fx.options()), 0);

    assert!(!record.exists(), "a non-executable hook must not run");
}

/// No hook at all — the common case — is silent and successful.
#[test]
fn a_missing_hook_is_a_no_op() {
    let fx = Fixture::new("hook-missing", "main", "wt");

    assert_eq!(run(&fx.options()), 0);
}

/// "a failed submodule warns and worktree creation still succeeds" is this
/// family's contract, and it holds for the hook too: a hook that exits
/// non-zero must NOT turn into a non-zero exit here. The call site discards
/// the code, so a regression would be invisible without this assertion.
#[test]
fn a_failing_hook_still_exits_zero() {
    let fx = Fixture::new("hook-fails", "main", "wt");
    let record = fx.install_hook(17);

    assert_eq!(run(&fx.options()), 0, "the worktree already exists; never fail the caller");

    assert!(record.exists(), "the hook must still have been run");
}

// ---------------------------------------------------------------------------
// LOOM_WORKTREE_CARGO_TARGET_DIR (#8458)
// ---------------------------------------------------------------------------

/// The scheme is **default-OFF** (#6013/#6014's lesson), so on a fixture with
/// no opt-in the variable is exported EMPTY rather than left unset — which is
/// what the shell's `export VAR="$(… || true)"` did, and what every consumer's
/// `-n` test expects.
#[test]
fn the_cargo_target_env_is_exported_empty_when_the_scheme_is_off() {
    let fx = Fixture::new("cargo-off", "main", "wt");
    let record = fx.install_hook(0);

    assert_eq!(run(&fx.options()), 0);

    assert_eq!(record_field(&record, "target"), "", "exported, and empty — not `<unset>`");
}

/// It is `LOOM_WORKTREE_CARGO_TARGET_DIR`, never `CARGO_TARGET_DIR`. Setting
/// the latter would make the hook's main-workspace binary lookup miss and
/// reintroduce #6013/#6014's rebuild storm, so the NAME is load-bearing.
#[test]
fn the_cargo_target_env_is_not_cargo_target_dir() {
    assert_eq!(CARGO_TARGET_ENV, "LOOM_WORKTREE_CARGO_TARGET_DIR");
    assert_ne!(CARGO_TARGET_ENV, "CARGO_TARGET_DIR");
}
