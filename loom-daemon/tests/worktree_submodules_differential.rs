//! Differential test: the Rust port of `worktree.sh`'s submodule
//! initialization must agree with the shell it replaced on a shared corpus of
//! superproject layouts (#8195 slice 8, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! No retained suite covers this block at all — `test-worktree-*.sh` mentions
//! submodules exactly once, in a comment about stdout purity — so unlike the
//! earlier slices there is no black-box evidence to inherit. This harness IS
//! the evidence, and it is built from the grammar the code reads: the
//! superproject shapes each branch of the retired pipeline reacts to.
//!
//! **Each scenario is materialised twice from the same builder, and the two
//! trees are asserted byte-identical before either side runs.** A first
//! attempt in this epic had each side generate its own inputs; they diverged,
//! and the harness reported a divergence in the code when nothing about the
//! code had been measured. Here `assert_trees_identical` fails with its own
//! message rather than letting "the inputs differed" masquerade as a finding.
//!
//! The shell side runs `tests/fixtures/worktree-submodules-retired.sh`, a
//! frozen copy of the retired block. Reading it out of the live `worktree.sh`
//! stopped being possible the moment that file started delegating.
//!
//! # What is compared, and what is deliberately not
//!
//! Compared: this command's own `ℹ`/`✓`/`⚠` lines, in order; the exit code;
//! and the resulting on-disk state (which submodule working trees got
//! populated).
//!
//! Not compared: the child `git submodule update`'s own output. It carries
//! absolute temp paths, progress lines whose presence depends on whether
//! stdout is a tty, and — in the retired shell — a "Retry scheduled" line
//! whose count depends on git's internal retry policy. None of that is this
//! code's behaviour. The port relays those bytes verbatim (see
//! `submodules::relay`), which is the property that matters and which the
//! unit tests pin directly.
//!
//! # Known divergences, pinned
//!
//! 1. **A submodule path containing a space.** The shell's
//!    `awk '{print $2}'` truncates `vendor/my lib` to `vendor`, hands git a
//!    pathspec matching nothing, and reports a failure. The port initializes
//!    it. **The port is the correct side** — this is #7858's class, the
//!    data-loss defect the parent issue leads with, and removing it is the
//!    stated reason this slice exists. Asserted as a divergence, in
//!    [`space_bearing_path_is_the_pinned_divergence`], so a regression to the
//!    shell's answer fails the suite rather than quietly re-agreeing.
//!
//! 2. **`--reference` object borrowing.** The shell's `[[ -d "$ref_path" ]]`
//!    test ran from inside the worktree against git's RELATIVE `.git` answer,
//!    so it was false on every host and the borrow never happened. The port
//!    anchors the path and borrows. Nothing observable changes — same lines,
//!    same order, same exit code — so the shared corpus below still agrees;
//!    the borrow itself is asserted separately by
//!    [`port_borrows_the_main_workspace_object_store_where_the_shell_did_not`].
//!
//! # Why `GIT_ALLOW_PROTOCOL=file`
//!
//! Since CVE-2022-39253 git refuses a `file://` submodule transport unless
//! the *invoking* process opts in, and a repo-local `protocol.file.allow` is
//! not read by the `git clone` subprocess the submodule machinery spawns (it
//! starts outside any repository). These fixtures must clone locally — a unit
//! test has no network — so the opt-in is set on the environment of BOTH
//! sides, identically, and nothing in either implementation sets it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// ---------------------------------------------------------------------------
// Corpus: the shapes the retired pipeline branches on
// ---------------------------------------------------------------------------

/// One superproject shape, materialised twice from these same fields.
struct Scenario {
    name: &'static str,
    /// Submodule paths inside the superproject. Each gets its own upstream
    /// repo with one file in it.
    submodules: &'static [&'static str],
    /// Delete each submodule's upstream repo (and the superproject's copy of
    /// its object store) before running, so the initialization must fail.
    break_upstreams: bool,
}

/// Every branch of the retired block, once.
///
/// The space-bearing shape is deliberately NOT here: it is the pinned
/// divergence and has its own test, because a corpus entry asserting equality
/// on it would have to assert the *wrong* answer.
const CORPUS: &[Scenario] = &[
    Scenario {
        // `UNINIT_SUBMODULES -gt 0` false: the whole block is skipped and
        // nothing at all is printed.
        name: "no-submodules",
        submodules: &[],
        break_upstreams: false,
    },
    Scenario {
        // The ordinary path: one entry, success line.
        name: "one-submodule",
        submodules: &["vendor/lib"],
        break_upstreams: false,
    },
    Scenario {
        // The count in the `ℹ` line is plural, and the loop runs twice.
        name: "three-submodules",
        submodules: &["vendor/a", "vendor/b", "deps/c"],
        break_upstreams: false,
    },
    Scenario {
        // The failure summary: `⚠` plus two `ℹ` lines, still exit 0.
        name: "unreachable-upstream",
        submodules: &["vendor/lib"],
        break_upstreams: true,
    },
    Scenario {
        // Every entry fails, not just the first — the loop must not stop at
        // the first failure on either side.
        name: "two-unreachable-upstreams",
        submodules: &["vendor/a", "vendor/b"],
        break_upstreams: true,
    },
    Scenario {
        // A nested path several levels down, so the `modules/<path>`
        // concatenation has more than one component to get wrong.
        name: "deeply-nested-path",
        submodules: &["a/b/c/lib"],
        break_upstreams: false,
    },
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("loom-submod-diff-{tag}-{}-{n}", std::process::id()));
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

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {dir:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

/// Build `<root>/upstreams/<n>` for each submodule plus `<root>/main`
/// embedding them, then add `<root>/wt` as a linked worktree of `main`.
///
/// Returns `(main_workspace, worktree)`.
fn materialize(root: &Path, scenario: &Scenario) -> (PathBuf, PathBuf) {
    let main = root.join("main");
    init_repo(&main);
    fs::write(main.join("README"), "root\n").expect("write");
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "root"]);

    for (i, sub) in scenario.submodules.iter().enumerate() {
        let upstream = root.join("upstreams").join(i.to_string());
        init_repo(&upstream);
        fs::write(upstream.join("file.txt"), format!("sub {i}\n")).expect("write");
        git(&upstream, &["add", "-A"]);
        git(&upstream, &["commit", "-qm", "init"]);

        git(
            &main,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                upstream.to_str().expect("utf8"),
                sub,
            ],
        );
    }
    if !scenario.submodules.is_empty() {
        git(&main, &["commit", "-qm", "add submodules"]);
    }

    if scenario.break_upstreams {
        fs::remove_dir_all(root.join("upstreams")).expect("rm upstreams");
        let _ = fs::remove_dir_all(main.join(".git/modules"));
    }

    let wt = root.join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            wt.to_str().expect("utf8"),
            "HEAD",
        ],
    );

    (main, wt)
}

/// Every regular file and directory under `dir`, relative, with file contents
/// for the small text files the fixtures write. Symlinks are recorded by
/// target. `.git` internals are excluded — they carry absolute paths and
/// timestamps and are not what the two sides are being compared on.
fn snapshot(dir: &Path, redact: &Path) -> BTreeMap<String, String> {
    let mut acc = BTreeMap::new();
    walk(dir, dir, &mut acc);
    // The two trees live at different absolute paths by construction, and
    // `.gitmodules` records its submodule URLs absolutely. Redacting the
    // per-side root is what makes "identical inputs" a checkable claim at
    // all; without it the assertion could only ever fail.
    let redact = redact.to_string_lossy().into_owned();
    acc.into_iter()
        .map(|(k, v)| (k, v.replace(&redact, "<ROOT>")))
        .collect()
}

fn walk(root: &Path, dir: &Path, acc: &mut BTreeMap<String, String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("under root")
            .to_string_lossy()
            .into_owned();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if rel.ends_with(".git") || rel.contains("/.git/") {
                acc.insert(format!("{rel}/"), "<gitdir>".into());
                continue;
            }
            acc.insert(format!("{rel}/"), String::new());
            walk(root, &path, acc);
        } else if meta.is_symlink() {
            let target = fs::read_link(&path).unwrap_or_default();
            acc.insert(rel, format!("-> {}", target.display()));
        } else if rel.ends_with(".git") {
            // A submodule's `.git` pointer file names an absolute gitdir;
            // record presence, not content.
            acc.insert(rel, "<gitfile>".into());
        } else {
            let bytes = fs::read(&path).unwrap_or_default();
            acc.insert(rel, String::from_utf8_lossy(&bytes).into_owned());
        }
    }
}

fn assert_trees_identical(a: &Path, b: &Path, scenario: &str) {
    let (sa, sb) = (
        snapshot(a, a.parent().expect("side root")),
        snapshot(b, b.parent().expect("side root")),
    );
    assert_eq!(
        sa, sb,
        "the two materialisations of scenario {scenario:?} differ BEFORE either \
         implementation ran; the harness is broken, not the code"
    );
}

/// Only the lines this command owns: the coloured `ℹ`/`✓`/`⚠` ones. Everything
/// else on the stream is the child git's, which is not compared (see the
/// module docs).
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

fn run_shell(main: &Path, wt: &Path) -> Output {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-submodules-retired.sh");
    Command::new("bash")
        .arg(&fixture)
        .arg(main)
        .arg(wt)
        .env("JSON_OUTPUT", "false")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("LOOM_SUBMODULE_TIMEOUT", "120")
        .output()
        .expect("fixture runs")
}

fn run_port(main: &Path, wt: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("worktree-submodules")
        .arg("--repo-root")
        .arg(main)
        .arg("--worktree")
        .arg(wt)
        .args(["--timeout", "120"])
        .args(extra)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .expect("port runs")
}

/// Which submodule working trees actually got populated.
fn populated(wt: &Path, scenario: &Scenario) -> Vec<&'static str> {
    scenario
        .submodules
        .iter()
        .copied()
        .filter(|sub| wt.join(sub).join("file.txt").exists())
        .collect()
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

        let (shell_main, shell_wt) = materialize(&shell_root, scenario);
        let (port_main, port_wt) = materialize(&port_root, scenario);
        assert_trees_identical(&shell_wt, &port_wt, scenario.name);

        let shell = run_shell(&shell_main, &shell_wt);
        let port = run_port(&port_main, &port_wt, &[]);

        assert_eq!(
            own_lines(&shell),
            own_lines(&port),
            "scenario {:?}: stdout lines differ\n--- shell ---\n{}\n--- port ---\n{}\n\
             --- shell stderr ---\n{}\n--- port stderr ---\n{}",
            scenario.name,
            String::from_utf8_lossy(&shell.stdout),
            String::from_utf8_lossy(&port.stdout),
            String::from_utf8_lossy(&shell.stderr),
            String::from_utf8_lossy(&port.stderr),
        );
        assert_eq!(
            shell.status.code(),
            port.status.code(),
            "scenario {:?}: exit codes differ",
            scenario.name
        );
        assert_eq!(
            populated(&shell_wt, scenario),
            populated(&port_wt, scenario),
            "scenario {:?}: different submodules ended up populated",
            scenario.name
        );
    }
}

/// Divergence 1, pinned. The shell truncates the path at its first space and
/// fails; the port initializes the submodule. This test asserts they DISAGREE,
/// and names the port as correct — so a regression that re-adopts the shell's
/// answer fails here instead of silently re-agreeing.
#[test]
fn space_bearing_path_is_the_pinned_divergence() {
    const SCENARIO: Scenario = Scenario {
        name: "space-in-path",
        submodules: &["vendor/my lib"],
        break_upstreams: false,
    };

    let tree = TempTree::new("space");
    let shell_root = tree.path().join("shell");
    let port_root = tree.path().join("port");
    fs::create_dir_all(&shell_root).expect("mkdir");
    fs::create_dir_all(&port_root).expect("mkdir");

    let (shell_main, shell_wt) = materialize(&shell_root, &SCENARIO);
    let (port_main, port_wt) = materialize(&port_root, &SCENARIO);
    assert_trees_identical(&shell_wt, &port_wt, SCENARIO.name);

    let shell = run_shell(&shell_main, &shell_wt);
    let port = run_port(&port_main, &port_wt, &[]);

    assert!(
        populated(&shell_wt, &SCENARIO).is_empty(),
        "the retired shell is expected to leave a space-bearing submodule \
         EMPTY (awk truncates the path); if it no longer does, this pin and \
         the port's module docs both need revisiting.\n--- shell stdout ---\n{}\
         \n--- shell stderr ---\n{}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert_eq!(
        populated(&port_wt, &SCENARIO),
        vec!["vendor/my lib"],
        "the port must initialize a space-bearing submodule path — this is \
         #7858's class and the reason this slice exists.\n--- port stdout ---\n{}\
         \n--- port stderr ---\n{}",
        String::from_utf8_lossy(&port.stdout),
        String::from_utf8_lossy(&port.stderr),
    );

    // Both still exit 0 — best-effort by contract, on either side.
    assert_eq!(shell.status.code(), Some(0));
    assert_eq!(port.status.code(), Some(0));
}

/// Divergence 2, pinned from both sides. `--reference` records the borrowed
/// object store in the new clone's `objects/info/alternates`; the retired
/// shell never passed it (its `[[ -d ]]` test ran from the worktree against
/// git's relative `.git`), the port does. The shell side is asserted too, so
/// this test also documents what was actually happening before.
#[test]
fn port_borrows_the_main_workspace_object_store_where_the_shell_did_not() {
    const SCENARIO: Scenario = Scenario {
        name: "borrow",
        submodules: &["vendor/lib"],
        break_upstreams: false,
    };

    let tree = TempTree::new("borrow");
    let shell_root = tree.path().join("shell");
    let port_root = tree.path().join("port");
    fs::create_dir_all(&shell_root).expect("mkdir");
    fs::create_dir_all(&port_root).expect("mkdir");

    let (shell_main, shell_wt) = materialize(&shell_root, &SCENARIO);
    let (port_main, port_wt) = materialize(&port_root, &SCENARIO);
    assert_trees_identical(&shell_wt, &port_wt, SCENARIO.name);

    run_shell(&shell_main, &shell_wt);
    run_port(&port_main, &port_wt, &[]);

    assert!(
        alternates(&shell_wt.join("vendor/lib")).is_empty(),
        "the retired shell is expected to borrow NOTHING — if it now does, \
         the port's `--reference` divergence note needs revisiting"
    );
    assert!(
        !alternates(&port_wt.join("vendor/lib")).is_empty(),
        "the port must record a borrowed object store; an empty alternates \
         file means `git_common_dir` regressed to the relative path"
    );
}

/// The contents of `<submodule-gitdir>/objects/info/alternates`, or empty.
fn alternates(submodule: &Path) -> String {
    let Ok(pointer) = fs::read_to_string(submodule.join(".git")) else {
        return String::new();
    };
    let Some(gitdir) = pointer.trim().strip_prefix("gitdir: ") else {
        return String::new();
    };
    fs::read_to_string(submodule.join(gitdir).join("objects/info/alternates"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `--quiet` is the `--json`-mode suppression: none of this command's own
/// lines, on either stream, whatever the outcome.
#[test]
fn quiet_suppresses_every_owned_line_on_both_outcomes() {
    for scenario in [
        Scenario {
            name: "quiet-success",
            submodules: &["vendor/lib"],
            break_upstreams: false,
        },
        Scenario {
            name: "quiet-failure",
            submodules: &["vendor/lib"],
            break_upstreams: true,
        },
    ] {
        let tree = TempTree::new(scenario.name);
        let root = tree.path().join("port");
        fs::create_dir_all(&root).expect("mkdir");
        let (main, wt) = materialize(&root, &scenario);

        let out = run_port(&main, &wt, &["--quiet"]);
        assert_eq!(out.status.code(), Some(0));
        assert!(
            own_lines(&out).is_empty(),
            "scenario {:?}: --quiet must print none of this command's own lines, got {:?}",
            scenario.name,
            own_lines(&out)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        for marker in ['\u{2139}', '✓', '⚠'] {
            assert!(
                !stderr.contains(marker),
                "scenario {:?}: --quiet must not reroute its own lines to stderr either, got:\n{stderr}",
                scenario.name
            );
        }
    }
}
