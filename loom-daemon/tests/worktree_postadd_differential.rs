//! Differential test: the Rust port of `worktree.sh`'s post-`git worktree add`
//! finalization must agree with the shell it replaced on a shared corpus of
//! main-workspace shapes (#8195 slice 16, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! Two retained suites touch parts of this block —
//! `defaults/scripts/tests/test-worktree-hookspath.sh` (the #3638 guard, driven
//! end-to-end through `worktree.sh`) and
//! `tests/hooks/test-post-worktree-target-dir.sh` (what the hook itself does
//! with the variable this block sets) — and both keep passing unchanged. What
//! neither covers is the hook *invocation*: its argv, its working directory,
//! the two messages and their order, and the `LOOM_WORKTREE_CARGO_TARGET_DIR`
//! hand-off. This harness is that evidence.
//!
//! **Each scenario is materialised twice from the same builder, and the two
//! trees are asserted byte-identical before either side runs.** An earlier
//! harness in this epic had each side generate its own inputs; they diverged,
//! and it reported a divergence in the code when nothing about the code had
//! been measured. `assert_trees_identical` fails with its own message rather
//! than letting "the inputs differed" masquerade as a finding.
//!
//! The shell side runs `tests/fixtures/worktree-postadd-retired.sh`, a frozen
//! copy of the retired blocks. Reading them out of the live `worktree.sh`
//! stopped being possible the moment that file started delegating.
//!
//! # What is compared, and what is deliberately not
//!
//! Compared: this command's own `ℹ`/`✓`/`⚠` lines, in order; the exit code;
//! `core.hooksPath` as git reports it afterwards; and the record the hook
//! writes (its argc/argv, its cwd, and the value of the one environment
//! variable this block sets).
//!
//! Not compared: the hook's own stdout. It is the project's, relayed
//! unchanged by both sides, and the fixture hook deliberately writes its
//! record to a file instead so the comparison is about *what the hook
//! received* rather than about how a test hook chose to print it.
//!
//! # Known divergence, pinned
//!
//! **A `git config` that fails.** The retired `git -C … config core.hooksPath`
//! ran bare under `set -e`, so a failure aborted the script — after the
//! worktree already existed, leaving no symlinks, no hook run, no success line
//! and (under `--json`) no document at all for a caller that had just been
//! handed a real worktree. The port warns and continues, which is the
//! best-effort contract its two neighbours (`worktree-link`,
//! `worktree-submodules`) already hold. Asserted as a divergence in
//! [`an_unwritable_config_is_the_pinned_divergence`], so a regression to the
//! shell's abort fails the suite rather than quietly re-agreeing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// ---------------------------------------------------------------------------
// Corpus: the shapes the retired blocks branch on
// ---------------------------------------------------------------------------

/// One main-workspace shape, materialised twice from these same fields.
struct Scenario {
    name: &'static str,
    /// Create a tracked `.githooks/` directory in the main workspace (#3638's
    /// guard).
    githooks: bool,
    /// Install `.loom/hooks/post-worktree.sh`, exiting with this code. `None`
    /// installs no hook at all — the common case.
    hook_exit: Option<i32>,
    /// Write the hook without an execute bit, so `[[ -x ]]` is false.
    hook_not_executable: bool,
    /// Put a space in both the main workspace and worktree directory names —
    /// #7858's class, the defect the parent issue leads with.
    spacey: bool,
}

/// Every branch of the retired blocks, once, plus the space-bearing shape of
/// each arm that interpolates a path.
const CORPUS: &[Scenario] = &[
    Scenario {
        name: "bare-no-githooks-no-hook",
        githooks: false,
        hook_exit: None,
        hook_not_executable: false,
        spacey: false,
    },
    Scenario {
        name: "githooks-only",
        githooks: true,
        hook_exit: None,
        hook_not_executable: false,
        spacey: false,
    },
    Scenario {
        name: "hook-succeeds",
        githooks: false,
        hook_exit: Some(0),
        hook_not_executable: false,
        spacey: false,
    },
    Scenario {
        name: "hook-fails",
        githooks: false,
        hook_exit: Some(3),
        hook_not_executable: false,
        spacey: false,
    },
    Scenario {
        name: "hook-not-executable",
        githooks: false,
        hook_exit: Some(0),
        hook_not_executable: true,
        spacey: false,
    },
    Scenario {
        name: "githooks-and-hook",
        githooks: true,
        hook_exit: Some(0),
        hook_not_executable: false,
        spacey: false,
    },
    Scenario {
        name: "spacey-githooks-and-hook",
        githooks: true,
        hook_exit: Some(0),
        hook_not_executable: false,
        spacey: true,
    },
    Scenario {
        name: "spacey-hook-fails",
        githooks: false,
        hook_exit: Some(7),
        hook_not_executable: false,
        spacey: true,
    },
];

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

/// A throwaway directory that removes itself.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("loom-postadd-diff-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp tree");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Build `<root>/<main>` + `<root>/<worktree>` for `scenario`, returning both
/// and the path the hook records into.
fn materialize(root: &Path, scenario: &Scenario) -> (PathBuf, PathBuf, PathBuf) {
    let (main_name, wt_name) = if scenario.spacey {
        ("main work space", "issue 42 worktree")
    } else {
        ("main", "issue-42")
    };
    let main = root.join(main_name);
    let worktree = root.join(wt_name);
    fs::create_dir_all(&main).expect("mkdir main");
    fs::create_dir_all(&worktree).expect("mkdir worktree");

    // `git -C <worktree> config` needs a repository. It does not need a LINKED
    // worktree: neither implementation reads the link, and a plain repo keeps
    // the fixture (and the byte-identity assertion below) free of the absolute
    // paths a real `git worktree add` writes into `.git`.
    git(&worktree, &["init", "-q"]);

    if scenario.githooks {
        fs::create_dir_all(main.join(".githooks")).expect("mkdir .githooks");
        fs::write(main.join(".githooks/pre-commit"), "#!/bin/sh\nexit 0\n").expect("write hook");
    }

    let record = root.join("hook-record.txt");
    if let Some(code) = scenario.hook_exit {
        let hook = main.join(".loom/hooks/post-worktree.sh");
        fs::create_dir_all(hook.parent().expect("hook parent")).expect("mkdir .loom/hooks");
        fs::write(&hook, hook_script(&record, code)).expect("write hook");
        set_mode(
            &hook,
            if scenario.hook_not_executable {
                0o644
            } else {
                0o755
            },
        );
    }

    (main, worktree, record)
}

/// A hook that records exactly what it received, then exits `code`.
///
/// It writes a FILE rather than printing, so the comparison is about what the
/// hook was handed — argc, each argv word, the cwd, and the one environment
/// variable this block sets — and not about a test hook's choice of output.
fn hook_script(record: &Path, code: i32) -> String {
    format!(
        "#!/bin/sh\n\
         {{\n\
         printf 'argc=%s\\n' \"$#\"\n\
         printf 'argv1=%s\\n' \"$1\"\n\
         printf 'argv2=%s\\n' \"$2\"\n\
         printf 'argv3=%s\\n' \"$3\"\n\
         printf 'cwd=%s\\n' \"$(pwd -P)\"\n\
         printf 'target=%s\\n' \"${{LOOM_WORKTREE_CARGO_TARGET_DIR-<unset>}}\"\n\
         }} > '{}'\n\
         exit {code}\n",
        record.display()
    )
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("stat").permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms).expect("chmod");
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs")
}

/// Every regular file under `root`, by relative path, with its contents —
/// except the git administrative churn neither implementation touches, and
/// with `root`'s own absolute spelling normalized out of both.
///
/// The normalization is what makes the comparison meaningful: the fixture hook
/// has its record path baked into it, so two materialisations under different
/// temp roots differ by construction and nothing else.
fn snapshot(root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut acc = std::collections::BTreeMap::new();
    walk(root, root, &mut acc);
    acc
}

fn walk(root: &Path, dir: &Path, acc: &mut std::collections::BTreeMap<String, String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `.git` holds timestamps, index churn and absolute paths; this
        // comparison is about the working tree plus the one config key, which
        // `hooks_path` reads explicitly.
        if entry.file_name() == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, acc);
        } else if let Ok(bytes) = fs::read(&path) {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .to_string();
            acc.insert(rel, normalize(&String::from_utf8_lossy(&bytes), root));
        }
    }
}

fn assert_trees_identical(a: &Path, b: &Path, scenario: &str) {
    assert_eq!(
        snapshot(a),
        snapshot(b),
        "the two materialisations of scenario {scenario:?} differ BEFORE either \
         implementation ran; the harness is broken, not the code"
    );
}

// ---------------------------------------------------------------------------
// Running the two sides
// ---------------------------------------------------------------------------

fn run_shell(main: &Path, wt: &Path) -> Output {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-postadd-retired.sh");
    Command::new("bash")
        .arg(&fixture)
        .arg(main)
        .arg(wt)
        .arg("feature/issue-42")
        .arg("42")
        .env("JSON_OUTPUT", "false")
        // The RESOLVED binary, passed in identically to both sides. The
        // retired line's `loom_locate_daemon_bin` tier is one of the port's
        // two argued changes; freezing the resolver in the fixture would make
        // this compare against whatever is installed on the host (#8176).
        .env("LOOM_FIXTURE_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
        .output()
        .expect("fixture runs")
}

fn run_port(main: &Path, wt: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("worktree-postadd")
        .arg("--repo-root")
        .arg(main)
        .arg("--worktree")
        .arg(wt)
        .args(["--branch", "feature/issue-42", "--issue", "42"])
        .args(extra)
        .output()
        .expect("port runs")
}

/// Only the lines this command owns: the coloured `ℹ`/`✓`/`⚠` ones.
fn own_lines(out: &Output) -> Vec<String> {
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(strip_ansi)
        .filter(|line| {
            line.starts_with('\u{2139}') || line.starts_with('✓') || line.starts_with('⚠')
        })
        .collect()
}

fn strip_ansi(line: &str) -> String {
    let mut acc = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            acc.push(c);
        }
    }
    acc.trim_end().to_string()
}

/// The repository-LOCAL `core.hooksPath` afterwards, or `None` when unset.
///
/// `--local` deliberately: both implementations write with a scope-less
/// `git config`, which is the local file, and a host whose own `~/.gitconfig`
/// sets `core.hooksPath` would otherwise make this read answer about the host
/// instead of about either implementation.
fn hooks_path(wt: &Path) -> Option<String> {
    let out = git(wt, &["config", "--local", "--get", "core.hooksPath"]);
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What the hook recorded, or `None` when it never ran.
fn hook_record(record: &Path) -> Option<String> {
    fs::read_to_string(record).ok()
}

// ---------------------------------------------------------------------------
// The differential
// ---------------------------------------------------------------------------

#[test]
fn shell_and_port_agree_across_the_corpus() {
    for scenario in CORPUS {
        let tree = TempTree::new(scenario.name);
        let shell_root = tree.path().join("shell");
        let port_root = tree.path().join("port");
        fs::create_dir_all(&shell_root).expect("mkdir");
        fs::create_dir_all(&port_root).expect("mkdir");

        let (shell_main, shell_wt, shell_record) = materialize(&shell_root, scenario);
        let (port_main, port_wt, port_record) = materialize(&port_root, scenario);
        assert_trees_identical(&shell_root, &port_root, scenario.name);

        let shell = run_shell(&shell_main, &shell_wt);
        let port = run_port(&port_main, &port_wt, &[]);

        assert_eq!(
            own_lines(&shell),
            own_lines(&port),
            "scenario {:?}: stdout lines differ\n--- shell ---\n{}\n--- port ---\n{}",
            scenario.name,
            String::from_utf8_lossy(&shell.stdout),
            String::from_utf8_lossy(&port.stdout),
        );
        assert_eq!(
            shell.status.code(),
            port.status.code(),
            "scenario {:?}: exit codes differ",
            scenario.name
        );
        assert_eq!(
            hooks_path(&shell_wt),
            hooks_path(&port_wt),
            "scenario {:?}: core.hooksPath differs",
            scenario.name
        );

        // The hook's record carries absolute paths, which differ between the
        // two roots by construction. Compare it with each side's own root
        // rewritten to a placeholder, so everything else — argc, the branch
        // and issue words, the cwd's relationship to the worktree, and the
        // LOOM_WORKTREE_CARGO_TARGET_DIR value — is compared literally.
        assert_eq!(
            hook_record(&shell_record).map(|r| normalize(&r, &shell_root)),
            hook_record(&port_record).map(|r| normalize(&r, &port_root)),
            "scenario {:?}: the hook received different input",
            scenario.name
        );
    }
}

/// Replace a side's own root (both its logical and its canonical spelling)
/// with `<ROOT>`, so two runs under different temp dirs are comparable.
fn normalize(record: &str, root: &Path) -> String {
    let mut out = record.replace(&root.to_string_lossy().to_string(), "<ROOT>");
    if let Ok(canonical) = fs::canonicalize(root) {
        out = out.replace(&canonical.to_string_lossy().to_string(), "<ROOT>");
    }
    out
}

/// `--quiet` is the `--json` mode the shell expressed as
/// `if [[ "$JSON_OUTPUT" != "true" ]]` around every line: the port must print
/// NONE of its own lines, while still doing all three pieces of work.
#[test]
fn quiet_suppresses_every_owned_line_but_none_of_the_work() {
    let scenario = &CORPUS[5]; // githooks-and-hook
    let tree = TempTree::new("quiet");
    let root = tree.path().join("port");
    fs::create_dir_all(&root).expect("mkdir");
    let (main, wt, record) = materialize(&root, scenario);

    let out = run_port(&main, &wt, &["--quiet"]);

    assert_eq!(out.status.code(), Some(0));
    assert!(
        own_lines(&out).is_empty(),
        "--quiet must print none of its own lines; got {:?}",
        own_lines(&out)
    );
    assert_eq!(hooks_path(&wt).as_deref(), Some(".githooks"));
    assert!(hook_record(&record).is_some(), "--quiet must still run the hook");
}

/// The pinned divergence (see the module docs). A `core.hooksPath` write that
/// cannot succeed aborts the retired shell under `set -e` — taking the hook
/// run with it — and is a warning in the port, which still runs the hook.
///
/// The failure is arranged by making the WORKTREE not a git repository at all,
/// which is the only way to make `git config` fail that does not depend on
/// running as a non-root user (CI containers often do run as root, where an
/// unwritable file is still writable).
#[test]
fn an_unwritable_config_is_the_pinned_divergence() {
    let scenario = Scenario {
        name: "config-fails",
        githooks: true,
        hook_exit: Some(0),
        hook_not_executable: false,
        spacey: false,
    };
    let tree = TempTree::new("divergence");
    let shell_root = tree.path().join("shell");
    let port_root = tree.path().join("port");
    fs::create_dir_all(&shell_root).expect("mkdir");
    fs::create_dir_all(&port_root).expect("mkdir");

    let (shell_main, shell_wt, shell_record) = materialize(&shell_root, &scenario);
    let (port_main, port_wt, port_record) = materialize(&port_root, &scenario);
    // Un-make both repositories, identically, AFTER materialisation.
    for wt in [&shell_wt, &port_wt] {
        fs::remove_dir_all(wt.join(".git")).expect("rm .git");
    }

    let shell = run_shell(&shell_main, &shell_wt);
    let port = run_port(&port_main, &port_wt, &[]);

    assert_ne!(
        shell.status.code(),
        Some(0),
        "the retired shell aborts under `set -e`; if this now exits 0 the fixture has drifted"
    );
    assert!(hook_record(&shell_record).is_none(), "the retired shell never reached the hook");

    assert_eq!(
        port.status.code(),
        Some(0),
        "the port warns and continues — the worktree already exists"
    );
    assert!(
        hook_record(&port_record).is_some(),
        "the port must still run the post-worktree hook"
    );
    assert!(
        own_lines(&port).iter().any(|l| l.starts_with('⚠')),
        "the port must SAY it could not set core.hooksPath; got {:?}",
        own_lines(&port)
    );
}
