//! Differential test: the Rust port of `worktree.sh`'s sparse-checkout family
//! must agree with the shell it replaced on a shared corpus (#8195 slice 10,
//! epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! The retained suite touches this family in exactly two assertions pairs
//! (`test-worktree-sentinel-reinvoke.sh` Tests 3/4: "the re-configure arm
//! prints its success line and writes the sentinel"). They still pass,
//! unchanged, against the port — but they are nowhere near a specification,
//! so this harness is the equivalence evidence, built from the grammar the
//! code branches on: arm × mode × `--json` × starting state × the extra
//! always-include env var.
//!
//! Each scenario is materialised twice from the same builder — once per side
//! — and the two worktrees are asserted identical *before* either
//! implementation runs, so "the inputs differed" can never masquerade as a
//! finding. The shell side runs `tests/fixtures/worktree-sparse-retired.sh`,
//! a frozen copy of the retired code.
//!
//! # What is compared
//!
//! Exit code; stdout and stderr byte for byte after redacting the per-side
//! root and the `du -sh` figure; the sentinel's bytes (or absence); the files
//! present in the worktree; the resulting `sparse-checkout list`; and
//! `core.sparseCheckout` in the worktree's config.
//!
//! # Known divergences, pinned as disagreements
//!
//! Each is a defect of the retired shell, reproduced against it before the
//! port was written, and each has its own test asserting the two sides
//! DISAGREE in the documented direction — so a regression to the shell's
//! answer fails rather than quietly re-agreeing:
//!
//! 1. A cone git rejects: the shell dies with git's 128 in silence; the port
//!    exits 1 and says what git said.
//! 2. A `"` in a cone path: the shell's `--json` document does not parse.
//! 3. A repo reached through a symlink: the shell refuses a live worktree.
//! 4. An unregistered `issue-4` beside a registered `issue-44`: the shell
//!    accepts it, and writes a sentinel into it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use loom_daemon::worktree_cli::sentinel;

const ISSUE: &str = "7";
const BRANCH: &str = "feature/issue-7";

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Start {
    /// A registered worktree with everything checked out.
    Full,
    /// A registered worktree already narrowed to this cone.
    Sparse(&'static [&'static str]),
    /// `git worktree add --no-checkout`: the create arm's precondition.
    NoCheckout,
    /// A directory git does not know about.
    Unregistered,
}

struct Scenario {
    name: &'static str,
    arm: &'static str,
    start: Start,
    /// `None` = `--full`.
    cone: Option<&'static [&'static str]>,
    json: bool,
    extra_include: Option<&'static str>,
}

const CORPUS: &[Scenario] = &[
    Scenario {
        name: "reconf-full-from-sparse",
        arm: "reconfigure",
        start: Start::Sparse(&["src/lib"]),
        cone: None,
        json: false,
        extra_include: None,
    },
    Scenario {
        name: "reconf-full-from-sparse-json",
        arm: "reconfigure",
        start: Start::Sparse(&["src/lib"]),
        cone: None,
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "reconf-full-already-full",
        arm: "reconfigure",
        start: Start::Full,
        cone: None,
        json: false,
        extra_include: None,
    },
    Scenario {
        name: "reconf-full-already-full-json",
        arm: "reconfigure",
        start: Start::Full,
        cone: None,
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "reconf-sparse-one-path",
        arm: "reconfigure",
        start: Start::Full,
        cone: Some(&["src/lib"]),
        json: false,
        extra_include: None,
    },
    Scenario {
        name: "reconf-sparse-two-paths-json",
        arm: "reconfigure",
        start: Start::Full,
        cone: Some(&["src/lib", "docs"]),
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "reconf-sparse-same-cone-again",
        arm: "reconfigure",
        start: Start::Sparse(&["src/lib"]),
        cone: Some(&["src/lib"]),
        json: false,
        extra_include: None,
    },
    Scenario {
        name: "reconf-sparse-replace-cone-json",
        arm: "reconfigure",
        start: Start::Sparse(&["src/lib"]),
        cone: Some(&["docs"]),
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "reconf-sparse-extra-include",
        arm: "reconfigure",
        start: Start::Full,
        cone: Some(&["src/lib"]),
        json: false,
        extra_include: Some("vendor  src/other"),
    },
    Scenario {
        name: "reconf-sparse-extra-include-json",
        arm: "reconfigure",
        start: Start::Full,
        cone: Some(&["src/lib"]),
        json: true,
        extra_include: Some("vendor\tsrc/other\n"),
    },
    Scenario {
        name: "reconf-sparse-path-with-space",
        arm: "reconfigure",
        start: Start::Full,
        cone: Some(&["my dir"]),
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "reconf-unregistered-full",
        arm: "reconfigure",
        start: Start::Unregistered,
        cone: None,
        json: false,
        extra_include: None,
    },
    Scenario {
        name: "reconf-unregistered-sparse-json",
        arm: "reconfigure",
        start: Start::Unregistered,
        cone: Some(&["src/lib"]),
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "create-one-path",
        arm: "create",
        start: Start::NoCheckout,
        cone: Some(&["src/lib"]),
        json: false,
        extra_include: None,
    },
    Scenario {
        name: "create-two-paths-json",
        arm: "create",
        start: Start::NoCheckout,
        cone: Some(&["src/lib", "docs"]),
        json: true,
        extra_include: None,
    },
    Scenario {
        name: "create-extra-include-json",
        arm: "create",
        start: Start::NoCheckout,
        cone: Some(&["docs"]),
        json: true,
        extra_include: Some("vendor"),
    },
];

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempTree(PathBuf);

impl TempTree {
    /// The root contains a space: every scenario doubles as a #7858 case.
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("loom-sparse diff-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp tree");
        Self(fs::canonicalize(&path).expect("canonicalize"))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    hermetic(&mut cmd);
    cmd
}

/// The same git environment for fixtures and for both implementations.
fn hermetic(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("LC_ALL", "C")
        .env_remove("LOOM_WORKTREE_ALWAYS_INCLUDE");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {dir:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `<root>/repo` with files in and out of every cone the corpus uses, and
/// `<root>/repo/.loom/worktrees/issue-7` in the scenario's starting state.
fn materialize(root: &Path, start: Start) -> (PathBuf, PathBuf) {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for path in [
        "src/lib/a.txt",
        "src/other/o.txt",
        "docs/b.md",
        "top.txt",
        "scripts/s.sh",
        ".claude/c.md",
        "vendor/v.txt",
        "my dir/m.txt",
    ] {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        fs::write(&file, path).expect("write");
    }
    fs::write(repo.join(".gitignore"), ".loom/\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let wt = repo.join(".loom/worktrees/issue-7");
    let wt_s = wt.to_str().expect("utf-8").to_string();
    match start {
        Start::Full => {
            git(&repo, &["worktree", "add", "-q", "-b", BRANCH, &wt_s, "main"]);
        }
        Start::Sparse(cone) => {
            git(&repo, &["worktree", "add", "-q", "-b", BRANCH, &wt_s, "main"]);
            git(&wt, &["sparse-checkout", "init", "--cone"]);
            let mut args = vec!["sparse-checkout", "set"];
            args.extend_from_slice(cone);
            git(&wt, &args);
        }
        Start::NoCheckout => {
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--no-checkout",
                    "-b",
                    BRANCH,
                    &wt_s,
                    "main",
                ],
            );
        }
        Start::Unregistered => {
            fs::create_dir_all(&wt).expect("mkdir");
            fs::write(wt.join("precious.txt"), "not git's").expect("write");
        }
    }
    (repo, wt)
}

// ---------------------------------------------------------------------------
// Running both sides
// ---------------------------------------------------------------------------

fn run_shell(
    repo: &Path,
    wt: &Path,
    arm: &str,
    cone: Option<&[&str]>,
    json: bool,
    extra: Option<&str>,
) -> Output {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-sparse-retired.sh");
    let mut cmd = Command::new("bash");
    hermetic(&mut cmd);
    cmd.current_dir(repo)
        .arg(&fixture)
        .arg(arm)
        .arg(wt)
        .env("JSON_OUTPUT", if json { "true" } else { "false" })
        .env("ISSUE_NUMBER", ISSUE)
        .env("BRANCH_NAME", BRANCH);
    match cone {
        None => {
            cmd.arg("--full");
        }
        Some(paths) => {
            cmd.args(paths);
        }
    }
    if let Some(extra) = extra {
        cmd.env("LOOM_WORKTREE_ALWAYS_INCLUDE", extra);
    }
    cmd.output().expect("fixture runs")
}

fn run_port(
    repo: &Path,
    wt: &Path,
    arm: &str,
    cone: Option<&[&str]>,
    json: bool,
    extra: Option<&str>,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    hermetic(&mut cmd);
    cmd.current_dir(repo)
        .args([
            "worktree-sparse",
            "--arm",
            arm,
            "--issue",
            ISSUE,
            "--branch",
            BRANCH,
        ])
        .arg("--worktree")
        .arg(wt);
    if json {
        cmd.arg("--json");
    }
    match cone {
        None => {
            cmd.arg("--full");
        }
        Some(paths) => {
            cmd.arg("--").args(paths);
        }
    }
    if let Some(extra) = extra {
        cmd.env("LOOM_WORKTREE_ALWAYS_INCLUDE", extra);
    }
    cmd.output().expect("port runs")
}

#[derive(Debug, PartialEq)]
struct Observed {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    sentinel: Option<String>,
    files: Vec<String>,
    sparse_list: String,
    sparse_cfg: String,
}

fn observe(root: &Path, wt: &Path, out: &Output) -> Observed {
    let sparse_list = git_cmd(wt)
        .args(["sparse-checkout", "list"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let sparse_cfg = git_cmd(wt)
        .args(["config", "--get", "core.sparseCheckout"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    Observed {
        code: out.status.code(),
        stdout: redact(&String::from_utf8_lossy(&out.stdout), root),
        stderr: redact(&String::from_utf8_lossy(&out.stderr), root),
        sentinel: fs::read_to_string(wt.join(".loom-managed")).ok(),
        files: files_under(wt),
        sparse_list,
        sparse_cfg,
    }
}

/// Drop the per-side root and the `du -sh` figure (block allocation is not
/// this code's behaviour).
fn redact(text: &str, root: &Path) -> String {
    let root = root.to_string_lossy();
    text.lines()
        .map(|line| {
            let line = line.replace(root.as_ref(), "<ROOT>");
            if line.contains("size") {
                if let Some(pos) = line.rfind(": ") {
                    let tail = if line.ends_with("\u{1b}[0m") {
                        "\u{1b}[0m"
                    } else {
                        ""
                    };
                    return format!("{}: <SIZE>{tail}", &line[..pos]);
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut acc = Vec::new();
    walk(dir, dir, &mut acc);
    acc.sort();
    acc
}

fn walk(root: &Path, dir: &Path, acc: &mut Vec<String>) {
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
        if rel == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, acc);
        } else {
            acc.push(rel);
        }
    }
}

/// Materialise a scenario once per side; assert the inputs are identical.
fn both_sides(tag: &str, start: Start) -> (TempTree, (PathBuf, PathBuf), (PathBuf, PathBuf)) {
    let tree = TempTree::new(tag);
    let shell = materialize(&tree.0.join("shell"), start);
    let port = materialize(&tree.0.join("port"), start);
    assert_eq!(
        files_under(&shell.1),
        files_under(&port.1),
        "the two materialisations of {tag:?} differ BEFORE either side ran; the harness is broken, not the code"
    );
    (tree, shell, port)
}

// ---------------------------------------------------------------------------
// The differential
// ---------------------------------------------------------------------------

#[test]
fn shell_and_port_agree_across_the_corpus() {
    for s in CORPUS {
        let (tree, (sh_repo, sh_wt), (pt_repo, pt_wt)) = both_sides(s.name, s.start);
        let shell_out = run_shell(&sh_repo, &sh_wt, s.arm, s.cone, s.json, s.extra_include);
        let port_out = run_port(&pt_repo, &pt_wt, s.arm, s.cone, s.json, s.extra_include);
        let shell = observe(&tree.0.join("shell"), &sh_wt, &shell_out);
        let port = observe(&tree.0.join("port"), &pt_wt, &port_out);
        assert_eq!(shell, port, "scenario {:?}: shell and port disagree", s.name);
    }
}

/// The corpus must actually exercise what it claims to: at least one
/// scenario narrows a tree, one restores it, one refuses, and every JSON
/// scenario that succeeded produced parseable output. Without this, a corpus
/// on which both sides silently did nothing would pass the comparison above.
#[test]
fn the_corpus_is_not_vacuous() {
    let (mut narrowed, mut restored, mut refused) = (false, false, false);
    for s in CORPUS {
        let (_tree, _shell, (repo, wt)) = both_sides(&format!("vac-{}", s.name), s.start);
        let out = run_port(&repo, &wt, s.arm, s.cone, s.json, s.extra_include);
        let code = out.status.code();
        if code == Some(1) {
            refused = true;
            assert!(!wt.join(".loom-managed").exists(), "{}: refusal wrote a sentinel", s.name);
            continue;
        }
        assert_eq!(code, Some(0), "{}: {}", s.name, String::from_utf8_lossy(&out.stderr));
        if s.cone.is_some() && !wt.join("vendor/v.txt").exists() {
            narrowed = true;
        }
        if s.cone.is_none() && wt.join("vendor/v.txt").exists() {
            restored = true;
        }
        if s.json {
            let stdout = String::from_utf8_lossy(&out.stdout);
            serde_json::from_str::<serde_json::Value>(stdout.trim())
                .unwrap_or_else(|e| panic!("{}: stdout is not JSON ({e}): {stdout}", s.name));
        }
    }
    assert!(
        narrowed && restored && refused,
        "corpus lost coverage: narrowed={narrowed} restored={restored} refused={refused}"
    );
}

// ---------------------------------------------------------------------------
// Pinned divergences — each asserts DISAGREEMENT in the documented direction
// ---------------------------------------------------------------------------

/// Divergence 1: a cone git rejects. The retired `set -e` script exited with
/// git's 128 and printed nothing at all.
#[test]
fn a_rejected_cone_was_a_silent_128_and_is_now_a_reported_1() {
    let (_tree, (sh_repo, sh_wt), (pt_repo, pt_wt)) = both_sides("rejected", Start::Full);
    let shell = run_shell(&sh_repo, &sh_wt, "reconfigure", Some(&["src/*"]), false, None);
    let port = run_port(&pt_repo, &pt_wt, "reconfigure", Some(&["src/*"]), false, None);

    assert_eq!(shell.status.code(), Some(128), "the retired defect, as reproduced");
    assert!(shell.stderr.is_empty(), "the retired defect was silent");
    assert_eq!(port.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&port.stderr);
    assert!(
        stderr.contains("sparse-checkout set failed"),
        "port names the failed step: {stderr}"
    );
    assert!(!sh_wt.join(".loom-managed").exists() && !pt_wt.join(".loom-managed").exists());
}

/// Divergence 2: the retired `awk` cone builder did not escape.
#[test]
fn a_quote_in_a_cone_path_broke_the_json_and_no_longer_does() {
    let (_tree, (sh_repo, sh_wt), (pt_repo, pt_wt)) = both_sides("quote", Start::Full);
    let cone: &[&str] = &[r#"src/li"b"#];
    let shell = run_shell(&sh_repo, &sh_wt, "reconfigure", Some(cone), true, None);
    let port = run_port(&pt_repo, &pt_wt, "reconfigure", Some(cone), true, None);

    assert_eq!(shell.status.code(), Some(0));
    assert!(
        serde_json::from_slice::<serde_json::Value>(&shell.stdout).is_err(),
        "the retired defect, as reproduced: {}",
        String::from_utf8_lossy(&shell.stdout)
    );
    assert_eq!(port.status.code(), Some(0));
    let doc: serde_json::Value =
        serde_json::from_slice(&port.stdout).expect("port emits valid JSON");
    assert_eq!(doc["cone"][0], r#"src/li"b"#);
}

/// Divergence 3: a repo reached through a symlink. `git worktree list`
/// reports symlink-resolved paths and the retired substring match missed a
/// live worktree.
#[test]
fn a_live_worktree_behind_a_symlinked_repo_path_was_refused_and_no_longer_is() {
    let (tree, (sh_repo, _), (pt_repo, _)) = both_sides("symlinked", Start::Full);
    let sh_alias = tree.0.join("shell alias");
    let pt_alias = tree.0.join("port alias");
    std::os::unix::fs::symlink(&sh_repo, &sh_alias).expect("symlink");
    std::os::unix::fs::symlink(&pt_repo, &pt_alias).expect("symlink");
    let rel = ".loom/worktrees/issue-7";

    let shell = run_shell(&sh_alias, &sh_alias.join(rel), "reconfigure", None, false, None);
    let port = run_port(&pt_alias, &pt_alias.join(rel), "reconfigure", None, false, None);

    assert_eq!(shell.status.code(), Some(1), "the retired defect, as reproduced");
    assert!(String::from_utf8_lossy(&shell.stderr).contains("not a registered worktree"));
    assert_eq!(port.status.code(), Some(0), "{}", String::from_utf8_lossy(&port.stderr));
    assert!(pt_repo.join(rel).join(".loom-managed").exists());
}

/// Divergence 4: `grep -q ".../issue-4"` matches a registered `issue-44`. The
/// retired arm then ran git against the MAIN workspace (git resolves `-C
/// issue-4` upward) and wrote a sentinel into the unregistered directory —
/// authorizing cleanup tooling to delete it.
#[test]
fn an_unregistered_dir_beside_a_registered_superstring_was_accepted_and_no_longer_is() {
    let tree = TempTree::new("substring");
    let mut sides = Vec::new();
    for side in ["shell", "port"] {
        let (repo, _) = materialize(&tree.0.join(side), Start::Full);
        let wt44 = repo.join(".loom/worktrees/issue-44");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/issue-44",
                wt44.to_str().expect("utf-8"),
                "main",
            ],
        );
        let wt4 = repo.join(".loom/worktrees/issue-4");
        fs::create_dir_all(&wt4).expect("mkdir");
        fs::write(wt4.join("precious.txt"), "not git's").expect("write");
        sides.push((repo, wt4));
    }
    let (sh_repo, sh_wt4) = &sides[0];
    let (pt_repo, pt_wt4) = &sides[1];

    let shell = run_shell(sh_repo, sh_wt4, "reconfigure", Some(&["src/lib"]), false, None);
    let port = run_port(pt_repo, pt_wt4, "reconfigure", Some(&["src/lib"]), false, None);

    assert_eq!(shell.status.code(), Some(0), "the retired defect, as reproduced");
    assert!(
        sh_wt4.join(".loom-managed").exists(),
        "the retired arm wrote a sentinel into an unregistered directory"
    );
    assert_eq!(
        git(sh_repo, &["config", "--get", "core.sparseCheckout"]).trim(),
        "true",
        "the retired arm ran sparse-checkout against the MAIN workspace"
    );

    assert_eq!(port.status.code(), Some(1));
    assert!(
        !pt_wt4.join(".loom-managed").exists(),
        "no sentinel in an unregistered directory"
    );
    let main_cfg = git_cmd(pt_repo)
        .args(["config", "--get", "core.sparseCheckout"])
        .output()
        .expect("git");
    assert!(!main_cfg.status.success(), "the main workspace was never touched");
    assert!(pt_repo.join("vendor/v.txt").exists());
}

// ---------------------------------------------------------------------------
// The sentinel, pinned to the LIVE shell writer
// ---------------------------------------------------------------------------

fn live_worktree_sh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/worktree.sh")
}

/// Until the remaining shell call sites move, two writers produce one
/// format, and `guard-destructive-generic.sh` parses its `# Branch:` line. This
/// extracts `write_loom_sentinel` from the LIVE `worktree.sh` (not the frozen
/// fixture) so a change to either writer fails here.
#[test]
fn the_rust_sentinel_is_byte_identical_to_the_live_shell_writer() {
    let tree = TempTree::new("sentinel");
    for (issue, branch) in [
        ("42", "feature/issue-42"),
        ("007", "feature/my-custom $name"),
    ] {
        let dir = tree.0.join(format!("issue {issue}"));
        fs::create_dir_all(&dir).expect("mkdir");
        let script = "eval \"$(sed -n '/^write_loom_sentinel() {/,/^}/p' \"$1\")\"\n\
             declare -F write_loom_sentinel >/dev/null || exit 3\n\
             write_loom_sentinel \"$2\""
            .to_string();
        let status = Command::new("bash")
            .args(["-c", &script, "bash"])
            .arg(live_worktree_sh())
            .arg(&dir)
            .env("ISSUE_NUMBER", issue)
            .env("BRANCH_NAME", branch)
            .status()
            .expect("bash runs");
        assert!(
            status.success(),
            "could not extract/run write_loom_sentinel from the live worktree.sh"
        );
        assert_eq!(
            fs::read_to_string(dir.join(".loom-managed")).expect("shell wrote it"),
            sentinel::content(issue, branch),
            "Rust and live-shell sentinel writers drifted for {issue}/{branch}"
        );
    }
}

// ---------------------------------------------------------------------------
// The live worktree.sh wiring, end to end
// ---------------------------------------------------------------------------

/// A consumer-layout repo (`.loom/scripts/` copied from `defaults/scripts/`)
/// with a bare origin, as the retained suites build it.
fn consumer_repo(root: &Path) -> PathBuf {
    let origin = root.join("origin.git");
    let (repo, _) = materialize(root, Start::Unregistered);
    let _ = fs::remove_dir_all(repo.join(".loom"));
    git(
        root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().expect("utf-8"),
        ],
    );
    git(&repo, &["remote", "add", "origin", origin.to_str().expect("utf-8")]);
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    let scripts = repo.join(".loom/scripts");
    fs::create_dir_all(&scripts).expect("mkdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
    let status = Command::new("cp")
        .arg("-R")
        .arg(src.join("."))
        .arg(&scripts)
        .status()
        .expect("cp");
    assert!(status.success());
    repo
}

fn run_live(repo: &Path, self_bin: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(repo.join(".loom/scripts/worktree.sh"));
    hermetic(&mut cmd);
    cmd.current_dir(repo)
        .args(args)
        .env("LOOM_DAEMON_SELF_BIN", self_bin)
        .env("LOOM_DEFAULT_BRANCH", "main")
        .output()
        .expect("worktree.sh runs")
}

/// No usable daemon: `--sparse` and `--full` refuse with 2 BEFORE anything
/// is touched — no worktree, no branch, no lock directory, no fetch residue.
/// A daemon that predates the subcommand is the realistic case (a rolling
/// fleet upgrade), so the stand-in answers every invocation the way clap does
/// for an unknown subcommand.
#[test]
fn live_script_refuses_sparse_and_full_with_2_on_a_daemon_without_the_subcommand() {
    let tree = TempTree::new("gate");
    let repo = consumer_repo(&tree.0);
    let fake = tree.0.join("old loom-daemon");
    fs::write(&fake, "#!/bin/sh\necho \"error: unrecognized subcommand '$1'\" >&2\nexit 2\n")
        .expect("write");
    let _ = Command::new("chmod").arg("+x").arg(&fake).status();

    for args in [
        &["7", "--sparse", "src/lib"][..],
        &["--json", "7", "--sparse", "src/lib"][..],
        &["7", "--full"][..],
    ] {
        let out = run_live(&repo, &fake, args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!repo.join(".loom/worktrees").exists(), "{args:?}: nothing may be created");
        assert!(
            !repo.join(".git/loom").exists() && !repo.join(".loom/locks").exists(),
            "{args:?}: no lock taken"
        );
        assert!(
            git(&repo, &["branch", "--list", BRANCH]).is_empty(),
            "{args:?}: no branch created"
        );
        if args[0] == "--json" {
            let doc: serde_json::Value =
                serde_json::from_slice(&out.stdout).expect("one JSON document");
            assert_eq!(doc["error"], "sparse-requires-loom-daemon");
        }
    }
}

/// The create arm through the live script: stdout is exactly one valid JSON
/// document carrying the cone the port computed, and the tree is narrowed.
#[test]
fn live_script_creates_a_sparse_worktree_and_emits_one_valid_document() {
    let tree = TempTree::new("live-create");
    let repo = consumer_repo(&tree.0);
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));

    let out = run_live(&repo, &bin, &["--json", "7", "--sparse", "src/lib", r#"we"ird"#]);

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(doc["success"], true);
    assert_eq!(doc["sparse"], true);
    assert_eq!(
        doc["cone"],
        serde_json::json!([
            "src/lib",
            r#"we"ird"#,
            ".claude",
            ".loom",
            ".githooks",
            "scripts"
        ])
    );
    let wt = repo.join(".loom/worktrees/issue-7");
    assert!(wt.join("src/lib/a.txt").exists());
    assert!(!wt.join("vendor/v.txt").exists(), "out-of-cone file not materialized");
    assert!(wt.join(".loom-managed").exists());

    // And the non-sparse document still says so, with the empty cone.
    let out = run_live(&repo, &bin, &["--json", "8"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["sparse"], false);
    assert_eq!(doc["cone"], serde_json::json!([]));
}
