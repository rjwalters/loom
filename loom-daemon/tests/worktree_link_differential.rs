//! Differential test: the Rust port of `worktree.sh`'s symlink provisioning
//! must agree with the shell it replaced on a shared corpus of worktree
//! layouts (#8195 slice 4, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! The retained suite (`test-worktree-nested-symlinks.sh`) is the black-box
//! evidence and it is not sufficient on its own: #8011 shipped three silent
//! divergences past a 104/104-green retained suite, because a suite only
//! proves what its author thought to write down. So the corpus here is
//! generated from the *grammar the code reads* — the filesystem shapes each
//! predicate branches on — rather than from the shapes anyone remembered.
//!
//! **The corpus is written to disk once and both sides are built from those
//! same bytes.** A first attempt at a differential in this epic had each side
//! generate its own inputs; they diverged, and the harness reported a
//! divergence in the code when nothing about the code had been measured. Here
//! the spec is serialised to a file, read back, and materialised into two
//! trees which are then asserted byte-identical *before* either
//! implementation runs — so "the inputs differed" is a test failure with its
//! own message rather than a silent lie.
//!
//! The shell side runs
//! `tests/fixtures/worktree-link-retired.sh`, a frozen copy of the retired
//! block. Reading it out of the live `worktree.sh` stopped being possible the
//! moment that file started delegating.
//!
//! # What is compared, and what is deliberately not
//!
//! Compared: the set of symlinks created (path → target), the worktree's
//! `info/exclude` as a sorted multiset of byte-exact lines, and the stdout
//! lines as a SORTED multiset.
//!
//! Not compared: the ORDER of stdout lines, nor of `info/exclude` entries —
//! one nondeterminism seen from two angles. Both are emitted in nested-
//! `node_modules` discovery order, which on the shell side is `find`'s readdir
//! order: filesystem-dependent and not reproducible. The port sorts. A test
//! that compared order would be flaky for a reason that has nothing to do with
//! the code. Everything else about both is still compared byte-exactly — see
//! [`exclude_lines`] for why a multiset rather than a set, and what that still
//! catches. The order that *is* observable and stable — the four families, in
//! sequence — is asserted separately by
//! [`family_order_is_root_then_nested_then_configured_then_mcp`], which does
//! not need the shell to say anything about it.
//!
//! # Known divergence, pinned
//!
//! A path containing a NEWLINE appends identically the first time and
//! diverges on a second append of the same entry: the shell's
//! `grep -qxF "$entry"` treats a multi-line fixed pattern as several
//! patterns, so it reads the entry as already present when any ONE of its
//! lines is, and skips. The port compares whole lines byte-exactly and
//! appends — i.e. it records an entry the shell silently dropped. The
//! port is the correct side (the shell silently dropped the entry), and the
//! case is unreachable from the live flow — a second run finds the
//! destination already linked and never appends at all — so it is recorded
//! rather than reproduced. The corpus exercises the first append only, which
//! both sides agree on.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Corpus: the grammar each predicate branches on
// ---------------------------------------------------------------------------

/// One filesystem shape, serialised into the corpus file and materialised
/// twice.
///
/// Every field is populated in at least one case, and the fields are
/// deliberately *independent* — #8011's fourth divergence hid in a field the
/// corpus left empty in all 700 cases.
struct Scenario {
    name: &'static str,
    /// Directories under the main workspace.
    main_dirs: &'static [&'static str],
    /// `(path, contents)` regular files under the main workspace.
    main_files: &'static [(&'static str, &'static str)],
    /// Directories under the worktree.
    worktree_dirs: &'static [&'static str],
    /// `(path, contents)` regular files under the worktree.
    worktree_files: &'static [(&'static str, &'static str)],
    /// `(link, raw target)` symlinks pre-created in the worktree. The target
    /// is written verbatim, so a dangling link is expressible.
    worktree_links: &'static [(&'static str, &'static str)],
    /// Contents of `<main>/.loom/config.json`, if any.
    config: Option<&'static str>,
    /// Lines pre-seeded into the worktree's `info/exclude`.
    exclude_seed: Option<&'static str>,
}

const EMPTY_DIRS: &[&str] = &[];
const EMPTY_FILES: &[(&str, &str)] = &[];

const fn base(name: &'static str) -> Scenario {
    Scenario {
        name,
        main_dirs: EMPTY_DIRS,
        main_files: EMPTY_FILES,
        worktree_dirs: EMPTY_DIRS,
        worktree_files: EMPTY_FILES,
        worktree_links: EMPTY_FILES,
        config: None,
        exclude_seed: None,
    }
}

fn corpus() -> Vec<Scenario> {
    vec![
        // --- degenerate ---
        base("nothing-to-link"),
        Scenario {
            main_dirs: &["node_modules"],
            ..base("root-node-modules-but-worktree-has-no-package-json")
        },
        Scenario {
            worktree_files: &[("package.json", "{}")],
            ..base("worktree-package-json-but-no-main-node-modules")
        },
        // --- 1. root node_modules ---
        Scenario {
            main_dirs: &["node_modules"],
            worktree_files: &[("package.json", "{}")],
            ..base("root-node-modules-happy-path")
        },
        Scenario {
            main_dirs: &["node_modules"],
            worktree_dirs: &["node_modules"],
            worktree_files: &[("package.json", "{}")],
            ..base("root-destination-is-a-real-directory")
        },
        Scenario {
            main_dirs: &["node_modules"],
            worktree_files: &[("package.json", "{}"), ("node_modules", "a regular file")],
            ..base("root-destination-is-a-regular-file")
        },
        Scenario {
            main_dirs: &["node_modules"],
            worktree_files: &[("package.json", "{}")],
            worktree_links: &[("node_modules", "/nonexistent-dangling-target")],
            ..base("root-destination-is-a-dangling-symlink")
        },
        Scenario {
            main_files: &[("node_modules", "not a directory")],
            worktree_files: &[("package.json", "{}")],
            ..base("main-node-modules-is-a-file-not-a-directory")
        },
        // --- 2. nested per-package node_modules: the `find` predicate ---
        Scenario {
            main_dirs: &["node_modules", "apps/web/node_modules"],
            main_files: &[("apps/web/package.json", "{}")],
            worktree_dirs: &["apps/web"],
            worktree_files: &[("package.json", "{}")],
            ..base("nested-depth-3")
        },
        Scenario {
            main_dirs: &["node_modules", "packages/node_modules"],
            main_files: &[("packages/package.json", "{}")],
            worktree_dirs: &["packages"],
            ..base("nested-depth-2-mindepth-boundary")
        },
        Scenario {
            main_dirs: &["node_modules", "a/b/c/node_modules"],
            main_files: &[("a/b/c/package.json", "{}")],
            worktree_dirs: &["a/b/c"],
            ..base("nested-depth-4-beyond-maxdepth")
        },
        Scenario {
            main_dirs: &["node_modules/.pnpm/node_modules"],
            main_files: &[("node_modules/.pnpm/package.json", "{}")],
            worktree_dirs: &["node_modules/.pnpm"],
            ..base("nested-under-a-node-modules-ancestor")
        },
        Scenario {
            main_dirs: &["node_modules", "apps/web/node_modules"],
            worktree_dirs: &["apps/web"],
            ..base("nested-without-a-sibling-package-json")
        },
        Scenario {
            main_dirs: &["node_modules", "apps/web/node_modules"],
            main_files: &[("apps/web/package.json", "{}")],
            ..base("nested-but-the-worktree-lacks-the-package-dir")
        },
        Scenario {
            main_dirs: &["node_modules", "apps/web/node_modules"],
            main_files: &[("apps/web/package.json", "{}")],
            worktree_dirs: &["apps/web/node_modules"],
            ..base("nested-destination-already-exists")
        },
        Scenario {
            main_dirs: &[
                "node_modules",
                "apps/web/node_modules",
                "apps/api/node_modules",
                "libs/ui/node_modules",
            ],
            main_files: &[
                ("apps/web/package.json", "{}"),
                ("apps/api/package.json", "{}"),
                ("libs/ui/package.json", "{}"),
            ],
            worktree_dirs: &["apps/web", "apps/api", "libs/ui"],
            ..base("nested-several-at-once")
        },
        Scenario {
            main_dirs: &["node_modules", "apps/my web app/node_modules"],
            main_files: &[("apps/my web app/package.json", "{}")],
            worktree_dirs: &["apps/my web app"],
            ..base("nested-package-dir-with-spaces")
        },
        Scenario {
            main_dirs: &["node_modules", "apps/$(touch pwned);`id`/node_modules"],
            main_files: &[("apps/$(touch pwned);`id`/package.json", "{}")],
            worktree_dirs: &["apps/$(touch pwned);`id`"],
            ..base("nested-package-dir-with-shell-metacharacters")
        },
        Scenario {
            main_dirs: &["node_modules", "apps/*/node_modules"],
            main_files: &[("apps/*/package.json", "{}")],
            worktree_dirs: &["apps/*"],
            ..base("nested-package-dir-named-with-a-glob")
        },
        // --- 3. worktree.linkPaths: jq's grammar ---
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/wasm"]}}"#),
            ..base("linkpaths-simple")
        },
        Scenario {
            main_dirs: &["gen/deep/nested/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/deep/nested/wasm"]}}"#),
            ..base("linkpaths-destination-parent-must-be-created")
        },
        Scenario {
            config: Some(r#"{"worktree":{"linkPaths":["gen/missing"]}}"#),
            ..base("linkpaths-source-does-not-exist")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            worktree_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/wasm"]}}"#),
            ..base("linkpaths-destination-already-exists")
        },
        Scenario {
            main_dirs: &["etc"],
            config: Some(r#"{"worktree":{"linkPaths":["/etc"]}}"#),
            ..base("linkpaths-absolute-entry-must-not-escape-the-workspace")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":["./gen/wasm"]}}"#),
            ..base("linkpaths-dot-relative-entry")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/../gen/wasm"]}}"#),
            ..base("linkpaths-entry-with-a-parent-traversal")
        },
        Scenario {
            main_dirs: &["gen/a b c"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/a b c"]}}"#),
            ..base("linkpaths-entry-with-spaces")
        },
        Scenario {
            main_dirs: &["gen/$(touch pwned)"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/$(touch pwned)"]}}"#),
            ..base("linkpaths-entry-with-shell-metacharacters")
        },
        Scenario {
            main_dirs: &["gen/wasm", "gen/other"],
            config: Some(r#"{"worktree":{"linkPaths":[null,"gen/wasm",false,"","gen/other"]}}"#),
            ..base("linkpaths-falsy-and-empty-entries-are-dropped")
        },
        Scenario {
            main_dirs: &["gen/wasm", "gen/other"],
            config: Some(r#"{"worktree":{"linkPaths":{"a":"gen/wasm","b":"gen/other"}}}"#),
            ..base("linkpaths-object-form-iterates-values")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":"gen/wasm"}}"#),
            ..base("linkpaths-scalar-is-not-iterable")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":"not-an-object"}"#),
            ..base("linkpaths-parent-is-not-an-object")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":[]}}"#),
            ..base("linkpaths-empty-array")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some("{ this is not json"),
            ..base("linkpaths-malformed-config-json")
        },
        Scenario {
            main_dirs: &["gen/wasm"],
            config: Some(r#"{"worktree":{"linkPaths":["gen/wasm","gen/wasm"]}}"#),
            ..base("linkpaths-duplicate-entries")
        },
        // --- 4. .mcp.json ---
        Scenario {
            main_files: &[(".mcp.json", r#"{"mcpServers":{}}"#)],
            ..base("mcp-json-happy-path")
        },
        Scenario {
            main_files: &[(".mcp.json", "{}")],
            worktree_files: &[(".mcp.json", "{}")],
            ..base("mcp-json-destination-already-exists")
        },
        Scenario {
            main_dirs: &[".mcp.json"],
            ..base("mcp-json-in-main-is-a-directory-not-a-file")
        },
        Scenario {
            main_files: &[(".mcp.json", "{}")],
            worktree_links: &[(".mcp.json", "/nonexistent-dangling-target")],
            ..base("mcp-json-destination-is-a-dangling-symlink")
        },
        // --- info/exclude bookkeeping ---
        Scenario {
            main_dirs: &["node_modules"],
            main_files: &[(".mcp.json", "{}")],
            worktree_files: &[("package.json", "{}")],
            exclude_seed: Some("node_modules\n"),
            ..base("exclude-already-carries-one-of-the-entries")
        },
        Scenario {
            main_dirs: &["node_modules"],
            worktree_files: &[("package.json", "{}")],
            exclude_seed: Some("node_modules_x\napps/web/node_modules\n"),
            ..base("exclude-carries-near-miss-lines-only")
        },
        Scenario {
            main_dirs: &["node_modules"],
            worktree_files: &[("package.json", "{}")],
            exclude_seed: Some("# comment\n\n\n"),
            ..base("exclude-has-blank-lines")
        },
        // --- everything at once, through space-bearing paths (#7858's class) ---
        Scenario {
            main_dirs: &["node_modules", "apps/my web app/node_modules", "gen/a b c"],
            main_files: &[("apps/my web app/package.json", "{}"), (".mcp.json", "{}")],
            worktree_dirs: &["apps/my web app"],
            worktree_files: &[("package.json", "{}")],
            config: Some(r#"{"worktree":{"linkPaths":["gen/a b c"]}}"#),
            ..base("all-four-families-through-space-bearing-paths")
        },
    ]
}

// ---------------------------------------------------------------------------
// Corpus serialisation — one file, read by the materialiser
// ---------------------------------------------------------------------------

/// Render the corpus to a stable text form. Both trees for a scenario are
/// materialised from the bytes READ BACK from this file, never from the
/// in-memory literals, so the two sides cannot be fed different inputs.
fn render_corpus(scenarios: &[Scenario]) -> String {
    let mut out = String::new();
    for s in scenarios {
        out.push_str(&format!("SCENARIO\t{}\n", s.name));
        for d in s.main_dirs {
            out.push_str(&format!("MAINDIR\t{d}\n"));
        }
        for (p, c) in s.main_files {
            out.push_str(&format!("MAINFILE\t{p}\t{c}\n"));
        }
        for d in s.worktree_dirs {
            out.push_str(&format!("WTDIR\t{d}\n"));
        }
        for (p, c) in s.worktree_files {
            out.push_str(&format!("WTFILE\t{p}\t{c}\n"));
        }
        for (l, t) in s.worktree_links {
            out.push_str(&format!("WTLINK\t{l}\t{t}\n"));
        }
        if let Some(cfg) = s.config {
            out.push_str(&format!("CONFIG\t{cfg}\n"));
        }
        if let Some(seed) = s.exclude_seed {
            out.push_str(&format!("EXCLUDE\t{}\n", seed.replace('\n', "\\n")));
        }
        out.push_str("END\n");
    }
    out
}

/// A scenario as read back off disk — deliberately a different type from
/// [`Scenario`] so nothing can accidentally materialise from the literals.
#[derive(Default, Debug)]
struct ParsedScenario {
    name: String,
    main_dirs: Vec<String>,
    main_files: Vec<(String, String)>,
    worktree_dirs: Vec<String>,
    worktree_files: Vec<(String, String)>,
    worktree_links: Vec<(String, String)>,
    config: Option<String>,
    exclude_seed: Option<String>,
}

fn parse_corpus(text: &str) -> Vec<ParsedScenario> {
    let mut out: Vec<ParsedScenario> = Vec::new();
    let mut current = ParsedScenario::default();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let tag = parts.next().unwrap_or("");
        match tag {
            "SCENARIO" => current.name = parts.next().unwrap_or("").to_string(),
            "MAINDIR" => current
                .main_dirs
                .push(parts.next().unwrap_or("").to_string()),
            "MAINFILE" => current.main_files.push((
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            )),
            "WTDIR" => current
                .worktree_dirs
                .push(parts.next().unwrap_or("").to_string()),
            "WTFILE" => current.worktree_files.push((
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            )),
            "WTLINK" => current.worktree_links.push((
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            )),
            "CONFIG" => current.config = Some(parts.next().unwrap_or("").to_string()),
            "EXCLUDE" => {
                current.exclude_seed = Some(parts.next().unwrap_or("").replace("\\n", "\n"))
            }
            "END" => out.push(std::mem::take(&mut current)),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Materialisation + observation
// ---------------------------------------------------------------------------

/// Build `<root>/main` and `<root>/wt` for one scenario. `wt` is a real git
/// repo, because `git rev-parse --git-path info/exclude` is how both sides
/// find the exclude file.
fn materialise(root: &Path, s: &ParsedScenario) -> (PathBuf, PathBuf) {
    let main = root.join("main workspace");
    let worktree = root.join("work trees/issue 42");
    std::fs::create_dir_all(&main).expect("main");
    std::fs::create_dir_all(&worktree).expect("worktree");

    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&worktree)
        .status()
        .expect("git init");
    assert!(ok.success(), "git init failed in {worktree:?}");

    for d in &s.main_dirs {
        std::fs::create_dir_all(main.join(d)).expect("main dir");
    }
    for (p, c) in &s.main_files {
        let path = main.join(p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("main file parent");
        }
        std::fs::write(&path, c).expect("main file");
    }
    if let Some(cfg) = &s.config {
        std::fs::create_dir_all(main.join(".loom")).expect(".loom");
        std::fs::write(main.join(".loom/config.json"), cfg).expect("config");
    }
    for d in &s.worktree_dirs {
        std::fs::create_dir_all(worktree.join(d)).expect("wt dir");
    }
    for (p, c) in &s.worktree_files {
        let path = worktree.join(p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("wt file parent");
        }
        std::fs::write(&path, c).expect("wt file");
    }
    for (l, t) in &s.worktree_links {
        let path = worktree.join(l);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("wt link parent");
        }
        std::os::unix::fs::symlink(t, &path).expect("wt link");
    }
    if let Some(seed) = &s.exclude_seed {
        let exclude = worktree.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).expect("info dir");
        std::fs::write(&exclude, seed).expect("exclude seed");
    } else {
        // `git init` writes a default info/exclude; normalise it away so the
        // byte comparison below is about what the implementations appended.
        let _ = std::fs::write(worktree.join(".git/info/exclude"), "");
    }

    (main, worktree)
}

/// Everything observable about a worktree after a run.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
    /// `worktree-relative path` → raw symlink target.
    links: BTreeMap<String, String>,
    /// `info/exclude`'s lines, each byte-exact, as a SORTED multiset — see
    /// [`exclude_lines`] and the module docs.
    exclude: Vec<Vec<u8>>,
    /// stdout lines, SORTED — see the module docs.
    stdout: Vec<String>,
}

fn observe(worktree: &Path, stdout: &str) -> Observation {
    let mut links = BTreeMap::new();
    collect_links(worktree, worktree, &mut links);
    let exclude =
        exclude_lines(&std::fs::read(worktree.join(".git/info/exclude")).unwrap_or_default());
    let mut lines: Vec<String> = stdout.lines().map(str::to_string).collect();
    lines.sort();
    Observation {
        links,
        exclude,
        stdout: lines,
    }
}

/// `info/exclude`'s contents as a sorted multiset of byte-exact lines.
///
/// Sorted for the *same* reason stdout is (see the module docs), and it is the
/// same nondeterminism seen from a second angle: both the printed lines and the
/// exclude entries are emitted in nested-`node_modules` discovery order, which
/// on the shell side is `find`'s readdir order. `nested-several-at-once`
/// proved it — the two sides created an identical set of symlinks and printed
/// an identical set of lines, yet a byte comparison of the exclude file failed
/// because `find` happened to return `apps/web` before `apps/api` while the
/// port sorts. Nothing reads these entries in order (they are independent
/// whole-line ignore patterns, no negations), so the order carries no meaning
/// to preserve — but it *is* filesystem-dependent, so comparing it would make
/// this harness flaky for a reason that has nothing to do with the code.
///
/// Deliberately a sorted **multiset**, not a set, and deliberately **whole
/// byte-exact lines**, not a lossy string: a duplicate entry (the exact
/// `grep -qxF` idempotency bug the port must not reintroduce), a missing
/// entry, an extra entry, and a near-miss entry that differs by one byte all
/// still fail the comparison. The only thing this discards is the order.
fn exclude_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    // `split` on a trailing-newline-terminated file yields a final empty
    // element; drop only that one, never an interior blank line (the
    // `exclude-has-blank-lines` scenario asserts those survive).
    let mut lines: Vec<Vec<u8>> = bytes.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines.sort();
    lines
}

fn collect_links(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `.git` holds the exclude file, which is compared separately.
        if path.file_name().map(|n| n == ".git").unwrap_or(false) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.insert(rel, target);
        } else if meta.is_dir() {
            collect_links(root, &path, out);
        }
    }
}

/// A symlink target is an absolute path into a per-side temp tree, so the two
/// sides' raw targets can never be equal as strings. Rewrite each side's own
/// root prefix to a placeholder before comparing.
fn normalise(observation: &mut Observation, root: &Path) {
    let prefix = root.to_string_lossy().into_owned();
    for target in observation.links.values_mut() {
        *target = target.replace(&prefix, "<ROOT>");
    }
    for line in &mut observation.stdout {
        *line = line.replace(&prefix, "<ROOT>");
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

fn run_shell(main: &Path, worktree: &Path) -> String {
    let root = repo_root();
    let script = root.join("loom-daemon/tests/fixtures/worktree-link-retired.sh");
    assert!(script.exists(), "frozen fixture missing at {script:?}");
    let out = Command::new("bash")
        .arg(&script)
        .arg(main)
        .arg(worktree)
        .arg(root.join("defaults/scripts/lib"))
        // The machine-level defaults tier must not decide what a fixture
        // config says — on either side.
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .output()
        .expect("bash");
    assert!(
        out.status.success(),
        "the retired shell failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_rust(main: &Path, worktree: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("worktree-link")
        .arg("--repo-root")
        .arg(main)
        .arg("--worktree")
        .arg(worktree)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .output()
        .expect("loom-daemon");
    assert!(
        out.status.success(),
        "worktree-link must always exit 0; got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// Tree-identity guard — the #8011 "inputs diverged" trap, closed explicitly
// ---------------------------------------------------------------------------

/// A canonical description of a tree: every entry's relative path, kind and
/// (for regular files) contents.
fn fingerprint(root: &Path) -> Vec<String> {
    let mut acc = Vec::new();
    walk_fingerprint(root, root, &mut acc);
    acc.sort();
    acc
}

fn walk_fingerprint(root: &Path, dir: &Path, acc: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        // `.git` differs between two `git init`s (hashes, timestamps); its
        // only relevant content is info/exclude, which is named explicitly.
        if rel == ".git" {
            acc.push(format!(
                "file\t.git/info/exclude\t{}",
                String::from_utf8_lossy(
                    &std::fs::read(path.join("info/exclude")).unwrap_or_default()
                )
            ));
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            acc.push(format!(
                "link\t{rel}\t{}",
                std::fs::read_link(&path).unwrap_or_default().display()
            ));
        } else if meta.is_dir() {
            acc.push(format!("dir\t{rel}"));
            walk_fingerprint(root, &path, acc);
        } else {
            acc.push(format!(
                "file\t{rel}\t{}",
                String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default())
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("loom-wt-link-diff-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("scratch");
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_port_agrees_with_the_retired_shell_on_every_scenario() {
    let scratch = Scratch::new("main");
    let corpus_path = scratch.0.join("corpus.tsv");
    {
        let mut f = std::fs::File::create(&corpus_path).expect("corpus file");
        f.write_all(render_corpus(&corpus()).as_bytes())
            .expect("write corpus");
    }

    // Read the corpus BACK off disk. Nothing below may touch the literals.
    let text = std::fs::read_to_string(&corpus_path).expect("read corpus");
    let scenarios = parse_corpus(&text);
    assert_eq!(
        scenarios.len(),
        corpus().len(),
        "the corpus file lost scenarios in the round trip — a harness bug, and \
         exactly how a differential quietly compares nothing"
    );

    let mut compared = 0usize;
    let mut with_output = 0usize;
    let mut with_links = 0usize;
    let mut divergences: Vec<String> = Vec::new();

    for scenario in &scenarios {
        let shell_root = scratch.0.join(format!("shell/{}", scenario.name));
        let rust_root = scratch.0.join(format!("rust/{}", scenario.name));
        std::fs::create_dir_all(&shell_root).expect("shell root");
        std::fs::create_dir_all(&rust_root).expect("rust root");

        let (shell_main, shell_wt) = materialise(&shell_root, scenario);
        let (rust_main, rust_wt) = materialise(&rust_root, scenario);

        // Both sides start from the same bytes, asserted rather than assumed.
        assert_eq!(
            fingerprint(&shell_root),
            fingerprint(&rust_root),
            "[{}] the two input trees differ BEFORE either implementation ran",
            scenario.name
        );

        let shell_stdout = run_shell(&shell_main, &shell_wt);
        let rust_stdout = run_rust(&rust_main, &rust_wt);

        let mut shell_obs = observe(&shell_wt, &shell_stdout);
        let mut rust_obs = observe(&rust_wt, &rust_stdout);
        normalise(&mut shell_obs, &shell_root);
        normalise(&mut rust_obs, &rust_root);

        compared += 1;
        if !shell_obs.stdout.is_empty() {
            with_output += 1;
        }
        if !shell_obs.links.is_empty() {
            with_links += 1;
        }

        if shell_obs != rust_obs {
            divergences.push(format!(
                "[{}]\n  shell: {:?}\n  rust:  {:?}",
                scenario.name, shell_obs, rust_obs
            ));
        }
    }

    assert!(
        divergences.is_empty(),
        "the port disagrees with the retired shell on {} of {} scenario(s):\n{}",
        divergences.len(),
        scenarios.len(),
        divergences.join("\n")
    );

    // Discriminating power, not just size. A corpus in which the shell never
    // linked anything would agree trivially with an implementation that does
    // nothing at all — which is precisely the green-number-measuring-nothing
    // failure §6 catalogues as Cause 1.
    assert_eq!(compared, scenarios.len(), "every scenario must be compared");
    assert!(
        with_links >= 15,
        "corpus lost its discriminating power: only {with_links} scenario(s) \
         produced any symlink at all"
    );
    assert!(
        with_output >= 15,
        "corpus lost its discriminating power: only {with_output} scenario(s) \
         produced any stdout"
    );
}

/// The one ordering property worth pinning, asserted without the shell:
/// families run root → nested → configured → `.mcp.json`. Operators read this
/// output, and `find`'s readdir order makes the shell unable to testify about
/// order at all (see the module docs).
#[test]
fn family_order_is_root_then_nested_then_configured_then_mcp() {
    let scratch = Scratch::new("order");
    let spec = ParsedScenario {
        name: "order".into(),
        main_dirs: vec![
            "node_modules".into(),
            "apps/web/node_modules".into(),
            "gen/wasm".into(),
        ],
        main_files: vec![
            ("apps/web/package.json".into(), "{}".into()),
            (".mcp.json".into(), "{}".into()),
        ],
        worktree_dirs: vec!["apps/web".into()],
        worktree_files: vec![("package.json".into(), "{}".into())],
        worktree_links: Vec::new(),
        config: Some(r#"{"worktree":{"linkPaths":["gen/wasm"]}}"#.into()),
        exclude_seed: None,
    };
    let (main, worktree) = materialise(&scratch.0, &spec);
    let stdout = run_rust(&main, &worktree);

    let positions: Vec<usize> = [
        "node_modules symlinked",
        "Symlinked apps/web/node_modules",
        "Symlinked gen/wasm",
        ".mcp.json symlinked",
    ]
    .iter()
    .map(|needle| {
        stdout
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in:\n{stdout}"))
    })
    .collect();

    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "families printed out of order: {positions:?}\n{stdout}"
    );

    // The exclude file records them in the same order, one line each.
    let exclude = std::fs::read_to_string(worktree.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude, "node_modules\napps/web/node_modules\ngen/wasm\n.mcp.json\n");
}

/// Cause 1's counter-check, executable: prove the harness can go RED. If a
/// deliberately wrong observation still compares equal, every green run above
/// is worthless.
#[test]
fn the_comparison_can_actually_fail() {
    let scratch = Scratch::new("red");
    let spec = ParsedScenario {
        name: "red".into(),
        main_dirs: vec!["node_modules".into()],
        main_files: Vec::new(),
        worktree_dirs: Vec::new(),
        worktree_files: vec![("package.json".into(), "{}".into())],
        worktree_links: Vec::new(),
        config: None,
        exclude_seed: None,
    };
    let (main, worktree) = materialise(&scratch.0, &spec);
    let stdout = run_rust(&main, &worktree);
    let real = observe(&worktree, &stdout);

    // The exclude comparison discards ORDER and nothing else. Prove each of
    // the three things it must still catch, since dropping order is the one
    // deliberate weakening in this harness (see `exclude_lines`).
    assert!(!real.exclude.is_empty(), "the probe scenario must record an exclude entry");

    let mut mutated = observe(&worktree, &stdout);
    mutated.exclude.push(b"extra".to_vec());
    assert_ne!(real, mutated, "an extra exclude entry must not compare equal");

    let mut mutated = observe(&worktree, &stdout);
    mutated.exclude.pop();
    assert_ne!(real, mutated, "a missing exclude entry must not compare equal");

    let mut mutated = observe(&worktree, &stdout);
    mutated.exclude.push(real.exclude[0].clone());
    mutated.exclude.sort();
    assert_ne!(
        real, mutated,
        "a DUPLICATED exclude entry must not compare equal — a multiset, not a set"
    );

    let mut mutated = observe(&worktree, &stdout);
    mutated.exclude[0].push(b'x');
    mutated.exclude.sort();
    assert_ne!(real, mutated, "a one-byte-different exclude entry must not compare equal");

    let mut mutated = observe(&worktree, &stdout);
    mutated.links.clear();
    assert_ne!(real, mutated, "a missing symlink must not compare equal");
    assert!(!real.links.is_empty(), "the probe scenario must link something");

    let mut mutated = observe(&worktree, &stdout);
    mutated.stdout.push("extra".into());
    assert_ne!(real, mutated, "an extra stdout line must not compare equal");
}
