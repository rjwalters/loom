//! Differential test: the Rust port of `worktree.sh`'s crash-debris cleanup —
//! the **orphan guard** — must agree with the shell it replaced on a shared
//! corpus of repo shapes (#8195 slice 5, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! The retained suite (`test-worktree-orphan-guard-spaces.sh`) is the
//! black-box evidence and it is not sufficient on its own: #8011 shipped three
//! silent divergences past a green retained suite, because a suite only proves
//! what its author thought to write down. So the corpus here is generated from
//! the *grammar the code reads* — the filesystem and cwd shapes each predicate
//! branches on — rather than from the shapes anyone remembered.
//!
//! **The corpus is written to disk once and both sides are built from those
//! same bytes.** An earlier differential in this epic had each side generate
//! its own inputs; they diverged, and the harness reported a divergence in the
//! code when nothing about the code had been measured. Here the spec is
//! serialised to a file, read back, and materialised into two trees which are
//! asserted byte-identical *before* either implementation runs — so "the
//! inputs differed" is a test failure with its own message rather than a
//! silent lie.
//!
//! The shell side runs `tests/fixtures/worktree-cleanup-retired.sh`, a frozen
//! copy of the retired function. Reading it out of the live `worktree.sh`
//! stopped being possible the moment that file started delegating.
//!
//! # What is compared
//!
//! Three things per scenario, each independently able to fail:
//!
//! 1. **stdout**, as a sorted multiset of whole lines with the tree root
//!    replaced by `<TREE>`. The warnings are the operator-visible contract
//!    (`test-worktree-orphan-guard-spaces.sh` greps them), and they carry the
//!    path the guard decided about — so comparing them compares the decision
//!    *and* the path it was made on.
//! 2. **The filesystem**, as a sorted manifest of `kind<TAB>relpath` for every
//!    surviving entry, plus the contents of every file the function can touch.
//!    This is what catches a `rm -rf` that fired when it should not have — the
//!    #7849 data-loss bug — and equally one that did not fire when it should.
//! 3. **`git worktree list --porcelain`** afterwards, which is where a
//!    divergent `git worktree prune` shows up. A prune that runs when the
//!    shell's did not (or vice versa) is invisible in 1 and 2.
//!
//! `.git` internals are excluded from the manifest apart from
//! `.git/worktrees/**` — the only part of them this function writes to — for
//! the ordinary reason that an index file records mtimes. Commit SHAs are
//! pinned identical across the two trees by fixing author/committer identity
//! and date, so the ref files that *are* compared match byte-for-byte.
//!
//! # Not compared
//!
//! stderr. The shell fixture's `2>/dev/null` suppressions and the port's
//! `Stdio::null()` are not the same shape, neither side's stderr is read by
//! `worktree.sh`, and nothing asserts on it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Corpus: the grammar each predicate branches on
// ---------------------------------------------------------------------------

/// Where the cleanup is run from. `worktree.sh` `cd`s to the main workspace
/// before calling, but "the main workspace" is reached differently on
/// different hosts — and two of these shapes are exactly the ones that made
/// the guard answer falsely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cwd {
    /// The repo root. `git rev-parse --git-common-dir` answers the RELATIVE
    /// `.git` from here, which the shell used verbatim.
    Repo,
    /// The repo root reached through a symlinked ancestor (`/tmp` →
    /// `/private/tmp` on macOS). Bash's `pwd` keeps the logical form; the
    /// porcelain reports the physical one. That mismatch is half of #7849.
    RepoViaSymlink,
    /// A subdirectory of the repo. `--git-common-dir` answers an ABSOLUTE
    /// path from here, so `dirname` of it is a different code path.
    Subdir,
    /// Inside a registered worktree of the repo — the shape `worktree.sh`
    /// auto-navigates out of, but which the function must survive being
    /// called from, since `--git-common-dir` still points at the main repo.
    LiveWorktree,
}

/// One repo shape, serialised into the corpus file and materialised twice.
///
/// Every field is populated in at least one case, and the fields are
/// deliberately *independent* — #8011's fourth divergence hid in a field the
/// corpus left empty in all 700 cases.
#[derive(Clone, Debug)]
struct Scenario {
    name: &'static str,
    /// Repo location relative to the tree root. Deliberately contains a space
    /// in several cases: that is #7858/#7849's whole subject.
    repo: &'static str,
    /// The issue argument the cleanup is invoked with.
    issue: u64,
    /// `(issue-dir-name, lock-file-name)` created under
    /// `<git-common>/worktrees/`. The dir name is a string, not a number, so a
    /// lock under a *different* issue's admin dir is expressible.
    locks: &'static [(&'static str, &'static str)],
    /// Registered worktrees to create: `(issue, uncommitted-file-name)`.
    live: &'static [(u64, &'static str)],
    /// Unregistered dirs under `<repo>/.loom/worktrees`:
    /// `(dir-name, file-inside)`. A file inside is what proves a `rm -rf`
    /// actually recursed.
    orphans: &'static [(&'static str, &'static str)],
    /// `(link-name under <repo>/.loom/worktrees, target relative to the tree
    /// root)` — an orphan candidate that is a SYMLINK to a directory.
    orphan_symlinks: &'static [(&'static str, &'static str)],
    /// Extra dirs to create relative to the tree root (symlink targets, and
    /// the `Subdir` cwd).
    extra_dirs: &'static [&'static str],
    /// `(path relative to the tree root, contents)`.
    extra_files: &'static [(&'static str, &'static str)],
    cwd: Cwd,
    /// `JSON_OUTPUT=true` on the shell side, `--quiet` on the Rust side.
    quiet: bool,
}

const fn base(name: &'static str) -> Scenario {
    Scenario {
        name,
        repo: "repo",
        issue: 42,
        locks: &[],
        live: &[],
        orphans: &[],
        orphan_symlinks: &[],
        extra_dirs: &[],
        extra_files: &[],
        cwd: Cwd::Repo,
        quiet: false,
    }
}

fn corpus() -> Vec<Scenario> {
    let mut cases = Vec::new();

    // --- Nothing to do -----------------------------------------------------
    cases.push(base("clean-repo-no-debris"));
    cases.push(Scenario {
        repo: "My Repos/repo",
        ..base("clean-repo-space-in-path")
    });

    // --- 1. Stale lock sweep ----------------------------------------------
    for lock in ["index.lock", "HEAD.lock", "gitdir.lock"] {
        cases.push(Scenario {
            locks: match lock {
                "index.lock" => &[("issue-42", "index.lock")],
                "HEAD.lock" => &[("issue-42", "HEAD.lock")],
                _ => &[("issue-42", "gitdir.lock")],
            },
            ..base(match lock {
                "index.lock" => "lock-index-only",
                "HEAD.lock" => "lock-head-only",
                _ => "lock-gitdir-only",
            })
        });
    }
    cases.push(Scenario {
        locks: &[
            ("issue-42", "index.lock"),
            ("issue-42", "HEAD.lock"),
            ("issue-42", "gitdir.lock"),
        ],
        ..base("lock-all-three")
    });
    cases.push(Scenario {
        // A lock in a DIFFERENT issue's admin dir must survive untouched.
        locks: &[("issue-42", "index.lock"), ("issue-99", "index.lock")],
        ..base("lock-other-issue-untouched")
    });
    cases.push(Scenario {
        // An unrelated file in the same admin dir must survive.
        locks: &[("issue-42", "index.lock"), ("issue-42", "not-a.lock")],
        ..base("lock-sweep-leaves-other-files")
    });
    cases.push(Scenario {
        locks: &[("issue-42", "index.lock")],
        repo: "My Repos/repo",
        cwd: Cwd::RepoViaSymlink,
        ..base("lock-via-symlinked-space-path")
    });
    cases.push(Scenario {
        locks: &[("issue-42", "index.lock")],
        quiet: true,
        ..base("lock-quiet")
    });

    // --- 2. The orphan guard: a LIVE worktree must survive -----------------
    cases.push(Scenario {
        live: &[(42, "PRECIOUS.txt")],
        ..base("live-worktree-survives")
    });
    cases.push(Scenario {
        // #7858/#7849 proper: the porcelain line carries a space.
        repo: "My Repos/repo",
        live: &[(42, "PRECIOUS.txt")],
        ..base("live-worktree-space-in-path-survives")
    });
    cases.push(Scenario {
        // The other half of #7849: logical vs physical path resolution.
        repo: "My Repos/repo",
        live: &[(42, "PRECIOUS.txt")],
        cwd: Cwd::RepoViaSymlink,
        ..base("live-worktree-via-symlinked-path-survives")
    });
    cases.push(Scenario {
        repo: "My Repos/repo",
        live: &[(42, "PRECIOUS.txt")],
        cwd: Cwd::Subdir,
        extra_dirs: &["My Repos/repo/deep/nested"],
        ..base("live-worktree-from-subdir-survives")
    });
    cases.push(Scenario {
        repo: "My Repos/repo",
        live: &[(42, "PRECIOUS.txt"), (7, "other.txt")],
        cwd: Cwd::LiveWorktree,
        ..base("live-worktree-called-from-inside-a-worktree")
    });
    cases.push(Scenario {
        // Several registered worktrees, only one of which is the argument.
        live: &[(42, "a.txt"), (43, "b.txt"), (44, "c.txt")],
        ..base("live-worktree-among-siblings")
    });

    // --- 2. The orphan guard: an ORPHAN must be removed --------------------
    cases.push(Scenario {
        orphans: &[("issue-42", "leftover.txt")],
        ..base("orphan-removed")
    });
    cases.push(Scenario {
        repo: "My Repos/repo",
        orphans: &[("issue-42", "leftover.txt")],
        ..base("orphan-removed-space-in-path")
    });
    cases.push(Scenario {
        repo: "My Repos/repo",
        orphans: &[("issue-42", "leftover.txt")],
        cwd: Cwd::RepoViaSymlink,
        ..base("orphan-removed-via-symlinked-path")
    });
    cases.push(Scenario {
        // A sibling orphan for a different issue must NOT be swept.
        orphans: &[("issue-42", "leftover.txt"), ("issue-99", "keep.txt")],
        ..base("orphan-other-issue-untouched")
    });
    cases.push(Scenario {
        // Orphan + live worktree at once: one goes, one stays.
        live: &[(7, "PRECIOUS.txt")],
        orphans: &[("issue-42", "leftover.txt")],
        ..base("orphan-removed-while-sibling-worktree-lives")
    });
    cases.push(Scenario {
        orphans: &[("issue-42", "leftover.txt")],
        quiet: true,
        ..base("orphan-quiet")
    });
    cases.push(Scenario {
        // Locks AND an orphan: both branches, one prune.
        locks: &[("issue-42", "index.lock")],
        orphans: &[("issue-42", "leftover.txt")],
        ..base("locks-and-orphan-together")
    });

    // --- 2. The orphan guard: symlinked candidates -------------------------
    cases.push(Scenario {
        // `rm -rf` unlinks a symlink; it does not follow it. Following would
        // destroy a tree outside the worktree root.
        orphan_symlinks: &[("issue-42", "elsewhere")],
        extra_dirs: &["elsewhere"],
        extra_files: &[("elsewhere/PRECIOUS.txt", "another agent's work")],
        ..base("orphan-symlink-unlinked-target-survives")
    });
    cases.push(Scenario {
        repo: "My Repos/repo",
        orphan_symlinks: &[("issue-42", "some where else")],
        extra_dirs: &["some where else"],
        extra_files: &[("some where else/PRECIOUS.txt", "another agent's work")],
        ..base("orphan-symlink-space-in-target")
    });
    cases.push(Scenario {
        // A DANGLING symlink is not a directory, so `[[ -d ]]` is false and
        // nothing happens at all.
        orphan_symlinks: &[("issue-42", "nowhere")],
        ..base("orphan-symlink-dangling-is-not-a-dir")
    });

    // --- 2. The orphan guard: shapes that are not orphans ------------------
    cases.push(Scenario {
        // A FILE at the candidate path is not a directory.
        extra_files: &[("repo/.loom/worktrees/issue-42", "not a directory")],
        extra_dirs: &["repo/.loom/worktrees"],
        ..base("candidate-is-a-regular-file")
    });
    cases.push(Scenario {
        // Empty orphan dir: removed, but there is nothing inside to prove it
        // recursed — which is why the other cases put a file in.
        orphans: &[("issue-42", "")],
        ..base("orphan-empty-dir")
    });
    cases.push(Scenario {
        // No `.loom/worktrees` at all.
        live: &[],
        ..base("no-worktree-root-dir")
    });

    cases
}

// ---------------------------------------------------------------------------
// Corpus serialisation — written ONCE, both sides read the same bytes
// ---------------------------------------------------------------------------

/// The corpus is serialised to this many bytes-on-disk before either tree is
/// built, and both trees are materialised from the deserialised form.
fn write_corpus(dir: &Path, cases: &[Scenario]) -> PathBuf {
    let path = dir.join("corpus.txt");
    let mut f = fs::File::create(&path).expect("create corpus");
    for c in cases {
        writeln!(f, "{c:?}").expect("write corpus");
    }
    f.sync_all().expect("sync corpus");
    path
}

// ---------------------------------------------------------------------------
// Materialisation
// ---------------------------------------------------------------------------

fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        // Pin identity AND dates so the two trees produce identical SHAs.
        .env("GIT_AUTHOR_NAME", "loom")
        .env("GIT_AUTHOR_EMAIL", "loom@example.invalid")
        .env("GIT_COMMITTER_NAME", "loom")
        .env("GIT_COMMITTER_EMAIL", "loom@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} in {cwd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build one tree for `case` under `root`, and return `(repo_path, cwd)`.
fn materialise(root: &Path, case: &Scenario) -> (PathBuf, PathBuf) {
    let repo = root.join(case.repo);
    fs::create_dir_all(&repo).expect("mkdir repo");
    run_git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("README.md"), b"seed\n").expect("seed");
    run_git(&repo, &["add", "README.md"]);
    run_git(&repo, &["commit", "-q", "-m", "init"]);

    for dir in case.extra_dirs {
        fs::create_dir_all(root.join(dir)).expect("mkdir extra");
    }
    for (path, contents) in case.extra_files {
        let p = root.join(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir extra parent");
        }
        fs::write(&p, contents.as_bytes()).expect("write extra");
    }

    let wt_root = repo.join(".loom").join("worktrees");
    for (issue, marker) in case.live {
        fs::create_dir_all(&wt_root).expect("mkdir wt root");
        let path = wt_root.join(format!("issue-{issue}"));
        let branch = format!("feature/issue-{issue}");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                path.to_str().expect("utf8 worktree path"),
            ],
        );
        fs::write(path.join(marker), b"uncommitted\n").expect("write marker");
    }

    for (name, inner) in case.orphans {
        let path = wt_root.join(name);
        fs::create_dir_all(&path).expect("mkdir orphan");
        if !inner.is_empty() {
            fs::write(path.join(inner), b"debris\n").expect("write debris");
        }
    }

    for (name, target) in case.orphan_symlinks {
        fs::create_dir_all(&wt_root).expect("mkdir wt root");
        symlink(root.join(target), wt_root.join(name)).expect("symlink orphan");
    }

    // Locks go under the git common dir, which is `<repo>/.git`.
    for (dir, lock) in case.locks {
        let admin = repo.join(".git").join("worktrees").join(dir);
        fs::create_dir_all(&admin).expect("mkdir admin");
        fs::write(admin.join(lock), b"").expect("write lock");
    }

    let cwd = match case.cwd {
        Cwd::Repo => repo.clone(),
        Cwd::RepoViaSymlink => {
            // A symlinked ANCESTOR, so the repo itself is reached through it.
            let alias = root.join("alias");
            symlink(root.join(first_component(case.repo)), &alias).expect("symlink alias");
            alias.join(rest_components(case.repo))
        }
        Cwd::Subdir => repo.join("deep").join("nested"),
        Cwd::LiveWorktree => wt_root
            .join(format!("issue-{}", case.live.first().map(|(i, _)| *i).unwrap_or(case.issue))),
    };

    (repo, cwd)
}

fn first_component(rel: &str) -> &str {
    rel.split('/').next().unwrap_or(rel)
}

fn rest_components(rel: &str) -> String {
    let mut parts = rel.split('/');
    parts.next();
    parts.collect::<Vec<_>>().join("/")
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// A sorted manifest of everything under `root`, with `root` (and any symlink
/// alias of it) replaced by `<TREE>` in both paths and file contents.
///
/// `.git` internals are excluded apart from `.git/worktrees/**` — the only
/// part of them this function writes to. See the module docs.
fn manifest(root: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort();
    entries
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    children.sort();
    for path in children {
        let rel = path
            .strip_prefix(root)
            .expect("under root")
            .to_string_lossy()
            .to_string();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap_or_default();
            out.push(format!("symlink\t{rel}\t{}", normalise(&target.to_string_lossy(), root)));
            continue;
        }
        if meta.is_dir() {
            out.push(format!("dir\t{rel}"));
            if is_excluded_git_internal(&rel) {
                continue;
            }
            walk(root, &path, out);
            continue;
        }
        if is_excluded_git_internal(&rel) {
            out.push(format!("file\t{rel}"));
            continue;
        }
        let contents = fs::read(&path).unwrap_or_default();
        out.push(format!("file\t{rel}\t{}", normalise(&String::from_utf8_lossy(&contents), root)));
    }
}

/// Inside a `.git` directory but outside `worktrees/`: recorded by name only
/// (an index file records mtimes, a log records timestamps).
fn is_excluded_git_internal(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let Some(idx) = parts.iter().position(|p| *p == ".git") else {
        return false;
    };
    parts.get(idx + 1).copied() != Some("worktrees")
}

fn normalise(text: &str, root: &Path) -> String {
    let root_s = root.to_string_lossy().to_string();
    let physical = fs::canonicalize(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| root_s.clone());
    let mut out = text.replace(&physical, "<TREE>").replace(&root_s, "<TREE>");
    // The `alias` symlink is a second name for the same tree; the logical cwd
    // travels through it, so warnings can carry it.
    out = out.replace("<TREE>/alias", "<TREE>/ALIAS");
    out.trim_end().to_string()
}

fn stdout_lines(raw: &[u8], root: &Path) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(raw)
        .lines()
        .map(|l| normalise(strip_ansi(l).trim(), root))
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    lines
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // Consume up to and including the final byte of a CSI sequence.
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

fn porcelain(repo: &Path, root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list");
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| normalise(l, root))
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    lines
}

// ---------------------------------------------------------------------------
// The two implementations
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn run_shell(cwd: &Path, logical: &Path, issue: u64, quiet: bool) -> Vec<u8> {
    let script = repo_root().join("loom-daemon/tests/fixtures/worktree-cleanup-retired.sh");
    assert!(script.exists(), "frozen fixture missing at {script:?}");
    let out = Command::new("bash")
        .arg(&script)
        .arg(issue.to_string())
        .current_dir(cwd)
        .env("PWD", logical)
        .env("JSON_OUTPUT", if quiet { "true" } else { "false" })
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("bash");
    assert!(
        out.status.success(),
        "the retired shell always exits 0; got {:?}",
        out.status.code()
    );
    out.stdout
}

fn run_rust(cwd: &Path, logical: &Path, issue: u64, quiet: bool) -> Vec<u8> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.arg("worktree-cleanup").arg(issue.to_string());
    if quiet {
        cmd.arg("--quiet");
    }
    let out = cmd
        .current_dir(cwd)
        .env("PWD", logical)
        // The machine-level defaults tier must not decide what a fixture
        // config says — on either side.
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env_remove("LOOM_WORKTREE_ROOT")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("loom-daemon");
    assert!(
        out.status.success(),
        "worktree-cleanup must always exit 0; got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

struct Observation {
    stdout: Vec<String>,
    manifest: Vec<String>,
    porcelain: Vec<String>,
}

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("loom-cleanup-diff-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("scratch");
    // Canonicalised so `<TREE>` substitution has one physical form to work
    // with; the symlink shapes are built INSIDE it on purpose.
    fs::canonicalize(&base).expect("canonicalise scratch")
}

/// Run both implementations on byte-identical trees and return what each did.
fn observe(case: &Scenario, scratch: &Path) -> (Observation, Observation) {
    let shell_root = scratch.join("shell");
    let rust_root = scratch.join("rust");
    fs::create_dir_all(&shell_root).expect("mkdir shell root");
    fs::create_dir_all(&rust_root).expect("mkdir rust root");

    let (shell_repo, shell_cwd) = materialise(&shell_root, case);
    let (rust_repo, rust_cwd) = materialise(&rust_root, case);

    // The inputs are asserted identical BEFORE either side runs, so "the
    // inputs differed" is its own failure rather than a silent lie.
    let before_shell = manifest(&shell_root);
    let before_rust = manifest(&rust_root);
    assert_eq!(
        before_shell, before_rust,
        "[{}] the two trees were not byte-identical before either side ran",
        case.name
    );

    let shell_physical = fs::canonicalize(&shell_cwd).expect("canonicalise shell cwd");
    let rust_physical = fs::canonicalize(&rust_cwd).expect("canonicalise rust cwd");

    let shell_stdout = run_shell(&shell_physical, &shell_cwd, case.issue, case.quiet);
    let rust_stdout = run_rust(&rust_physical, &rust_cwd, case.issue, case.quiet);

    (
        Observation {
            stdout: stdout_lines(&shell_stdout, &shell_root),
            manifest: manifest(&shell_root),
            porcelain: porcelain(&shell_repo, &shell_root),
        },
        Observation {
            stdout: stdout_lines(&rust_stdout, &rust_root),
            manifest: manifest(&rust_root),
            porcelain: porcelain(&rust_repo, &rust_root),
        },
    )
}

#[test]
fn rust_agrees_with_the_retired_shell_on_every_corpus_case() {
    let cases = corpus();
    let scratch = scratch("main");

    // Generated ONCE, on disk, before anything is materialised.
    let corpus_file = write_corpus(&scratch, &cases);
    let serialised = fs::read_to_string(&corpus_file).expect("read corpus back");
    assert_eq!(
        serialised.lines().count(),
        cases.len(),
        "the corpus round-tripped to a different number of cases"
    );

    let mut compared = 0usize;
    let mut cases_that_removed_something = 0usize;
    let mut cases_that_printed_something = 0usize;
    let mut cases_with_a_surviving_live_worktree = 0usize;

    for case in &cases {
        let dir = scratch.join(case.name);
        fs::create_dir_all(&dir).expect("case dir");
        let (shell, rust) = observe(case, &dir);

        assert_eq!(
            shell.stdout, rust.stdout,
            "[{}] stdout diverged\n  shell: {:#?}\n  rust:  {:#?}",
            case.name, shell.stdout, rust.stdout
        );
        assert_eq!(
            shell.manifest,
            rust.manifest,
            "[{}] the filesystem diverged\n  only in shell: {:#?}\n  only in rust:  {:#?}",
            case.name,
            diff(&shell.manifest, &rust.manifest),
            diff(&rust.manifest, &shell.manifest),
        );
        assert_eq!(
            shell.porcelain, rust.porcelain,
            "[{}] git's worktree registry diverged (a prune ran on one side only)",
            case.name
        );

        compared += 1;
        if !shell.stdout.is_empty() {
            cases_that_printed_something += 1;
        }
        if shell
            .stdout
            .iter()
            .any(|l| l.contains("Removing orphan worktree dir") || l.contains("Cleaned stale"))
        {
            cases_that_removed_something += 1;
        }
        if shell.manifest.iter().any(|e| e.contains("PRECIOUS.txt")) {
            cases_with_a_surviving_live_worktree += 1;
        }
    }

    // Discriminating-power floors: a harness that silently degraded to
    // comparing nothing would still be "green" without these.
    assert_eq!(compared, cases.len(), "not every case was compared");
    assert!(
        compared >= 25,
        "corpus shrank below the size that covers the grammar ({compared} cases)"
    );
    assert!(
        cases_that_printed_something >= 12,
        "only {cases_that_printed_something} cases produced any output"
    );
    assert!(
        cases_that_removed_something >= 12,
        "only {cases_that_removed_something} cases exercised a removal"
    );
    assert!(
        cases_with_a_surviving_live_worktree >= 5,
        "only {cases_with_a_surviving_live_worktree} cases proved a live worktree survived"
    );

    let _ = fs::remove_dir_all(&scratch);
}

fn diff(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|x| !b.contains(x)).cloned().collect()
}

/// The harness must be able to go RED. Every comparison it makes is proved
/// sensitive to a mutation of exactly the thing it claims to compare — a
/// green run means nothing otherwise.
#[test]
fn the_comparison_can_actually_fail() {
    let root = scratch("redcheck");

    let a = vec!["dir\tx".to_string(), "file\ty\tz".to_string()];

    // A missing entry.
    assert_ne!(a, vec!["dir\tx".to_string()]);
    // An extra entry.
    let mut extra = a.clone();
    extra.push("file\tw\t".to_string());
    assert_ne!(a, extra);
    // A one-byte content difference.
    assert_ne!(a, vec!["dir\tx".to_string(), "file\ty\tZ".to_string()]);
    // A kind change (dir → symlink).
    assert_ne!(a, vec!["symlink\tx\tt".to_string(), "file\ty\tz".to_string()]);

    // …and the manifest really does record content, kind and path.
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/f"), b"one").unwrap();
    symlink(root.join("d"), root.join("l")).unwrap();
    let before = manifest(&root);
    fs::write(root.join("d/f"), b"two").unwrap();
    assert_ne!(before, manifest(&root), "manifest() is blind to file contents");

    // ANSI stripping must not eat real text.
    assert_eq!(strip_ansi("\u{1b}[1;33m⚠ hello\u{1b}[0m"), "⚠ hello");

    // Normalisation must not collapse two different paths into one.
    let mut seen = BTreeMap::new();
    seen.insert(normalise("/a/b", Path::new("/a")), 1);
    seen.insert(normalise("/a/c", Path::new("/a")), 2);
    assert_eq!(seen.len(), 2, "normalise() collapsed distinct paths");

    let _ = fs::remove_dir_all(&root);
}
