//! Machine-generated content that a quarantine stash must never rescue and
//! stash retirement may always drop (#5690, #11075). Two classes:
//!
//! - **Name-based** ([`is_generated_artifact`]): path components no project
//!   authors by hand (`.venv/`, `__pycache__/`, ...). Moved here verbatim from
//!   `stash_retirement` (#11075) so that over-threshold file stays frozen.
//! - **Content-verified build trees** ([`is_build_tree_marker`]): a directory
//!   holding a cargo marker whose *bytes* prove cargo wrote it — `CACHEDIR.TAG`
//!   with the cache-dir-tagging signature, or `.rustc_info.json` (a JSON object
//!   with `rustc_fingerprint`), which cargo writes at every target root.
//!   Either suffices: 2 of worker-1's 16 bloated stashes held target trees with
//!   no tag (`.loom/target-issue-9748/`, `.loom/target-issue-9989/`). A
//!   directory *name* is never trusted — an unmarked `target/` may be real work.
//!
//! The repository root is never a build tree, even when it holds a marker:
//! that would class every path as generated, so a quarantine would rescue
//! nothing and retirement could drop real work. Discovery reads markers from
//! disk independent of ignore rules: an ignored `CACHEDIR.TAG` is invisible to
//! `ls-files --exclude-standard` while its siblings are still stashable.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};

/// Path components that are unambiguously machine-generated: a virtualenv, a
/// dependency tree, or an interpreter/tool cache. These normally never reach
/// a stash at all (they are gitignored, and `git stash push --include-untracked`
/// does not stash ignored files) — they land in a quarantine stash only in a
/// repo that forgot to ignore them, which is exactly #5690's worst case (one
/// stash of 1,749 files, 1,743 of them `.venv/` and `__pycache__`).
///
/// Deliberately **narrower** than "things a `.gitignore` usually lists":
/// `dist/`, `build/`, `out/`, and `target/` are excluded because each is a
/// plausible hand-authored source directory in some project, and a false
/// "generated" call here deletes real work. Every entry below is a name no
/// project authors by hand.
///
/// This class is used only by stash retirement and is **not** added to
/// [`crate::main_health_gate`]'s dirty-tree ignore list: that list decides
/// whether the gate may hard-reset a live working tree, a different question
/// with a different blast radius.
const GENERATED_ARTIFACT_COMPONENTS: &[&str] = &[
    "__pycache__",
    ".venv",
    "venv",
    "node_modules",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".ipynb_checkpoints",
];

/// Suffixes of a whole path component that mark it generated — `.egg-info`
/// directories (setuptools metadata) are named `<pkg>.egg-info`, so they need
/// a suffix match rather than an exact one.
const GENERATED_ARTIFACT_COMPONENT_SUFFIXES: &[&str] = &[".egg-info"];

/// Basename suffixes that mark a single generated file (compiled Python
/// bytecode) rather than a whole directory.
const GENERATED_ARTIFACT_FILE_SUFFIXES: &[&str] = &[".pyc", ".pyo"];

/// Exact basenames that are always OS/tooling droppings.
const GENERATED_ARTIFACT_BASENAMES: &[&str] = &[".DS_Store"];

/// Whether `path` is provably machine-generated content
/// ([`GENERATED_ARTIFACT_COMPONENTS`] and friends) — the "no real work at
/// all" class that made up 58 of #5690's 148 stashes.
#[must_use]
pub(crate) fn is_generated_artifact(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    if GENERATED_ARTIFACT_BASENAMES.contains(&basename)
        || GENERATED_ARTIFACT_FILE_SUFFIXES
            .iter()
            .any(|s| basename.ends_with(s))
    {
        return true;
    }
    path.split('/').any(|component| {
        GENERATED_ARTIFACT_COMPONENTS.contains(&component)
            || GENERATED_ARTIFACT_COMPONENT_SUFFIXES
                .iter()
                .any(|s| component.ends_with(s))
    })
}

/// First bytes of every cargo-written `CACHEDIR.TAG` (cache-dir tagging spec).
const CACHEDIR_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Basenames of the cargo markers a target-tree root carries.
const BUILD_TREE_MARKERS: [&str; 2] = ["CACHEDIR.TAG", ".rustc_info.json"];

/// Whether `bytes` are genuine cargo content for the marker named `name`.
#[must_use]
pub(crate) fn is_build_tree_marker(name: &str, bytes: &[u8]) -> bool {
    match name {
        "CACHEDIR.TAG" => bytes.starts_with(CACHEDIR_TAG_SIGNATURE),
        ".rustc_info.json" => serde_json::from_slice::<serde_json::Value>(bytes)
            .is_ok_and(|v| v.get("rustc_fingerprint").is_some()),
        _ => false,
    }
}

/// `(dir, marker name)` when `path` is a marker strictly below the root.
fn marker_dir(path: &str) -> Option<(&str, &str)> {
    let (dir, name) = path.rsplit_once('/')?;
    (!dir.is_empty() && BUILD_TREE_MARKERS.contains(&name)).then_some((dir, name))
}

/// Build-tree directories (no trailing slash, never the root) proven by the
/// marker blobs among a stash's `paths`; `read_blob` returns a path's bytes.
pub(crate) fn build_tree_dirs_in(
    paths: &[String],
    mut read_blob: impl FnMut(&str) -> Option<Vec<u8>>,
) -> BTreeSet<String> {
    paths
        .iter()
        .filter_map(|path| {
            let (dir, name) = marker_dir(path)?;
            let bytes = read_blob(path)?;
            is_build_tree_marker(name, &bytes).then(|| dir.to_string())
        })
        .collect()
}

/// Whether `path` is, or lies under, one of `dirs`.
#[must_use]
pub(crate) fn is_under_build_tree(path: &str, dirs: &BTreeSet<String>) -> bool {
    let mut cur = path.trim_end_matches('/');
    loop {
        if dirs.contains(cur) {
            return true;
        }
        match cur.rsplit_once('/') {
            Some((parent, _)) => cur = parent,
            None => return false,
        }
    }
}

/// Whether the on-disk directory `dir` holds a content-verified cargo marker.
fn dir_has_marker(dir: &Path) -> bool {
    BUILD_TREE_MARKERS
        .iter()
        .any(|name| std::fs::read(dir.join(name)).is_ok_and(|b| is_build_tree_marker(name, &b)))
}

/// Worktree-relative build-tree directories containing anything a quarantine
/// stash would capture (untracked, modified or staged). Every ancestor
/// directory of those files is checked on disk, so a marker that is ignored
/// (or tracked, or absent from `git status`) is still found.
pub(crate) fn worktree_build_tree_dirs(worktree: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut checked: HashSet<String> = HashSet::new();
    for args in [
        &["ls-files", "-z", "--others", "--exclude-standard"][..],
        &["diff", "-z", "--name-only"],
        &["diff", "-z", "--name-only", "--cached"],
    ] {
        let Ok(out) = Command::new("git")
            .args(args)
            .current_dir(worktree)
            .output()
        else {
            continue;
        };
        for file in String::from_utf8_lossy(&out.stdout).split('\0') {
            let mut dir = file;
            // Walk up until a directory already checked (its ancestors were
            // checked with it); the root itself is never a candidate.
            while let Some((parent, _)) = dir.rsplit_once('/') {
                dir = parent;
                if !checked.insert(dir.to_string()) {
                    break;
                }
                if dir_has_marker(&worktree.join(dir)) {
                    found.insert(dir.to_string());
                }
            }
        }
    }
    found
}

fn literal_pathspecs(dirs: &BTreeSet<String>, magic: &str) -> Vec<String> {
    dirs.iter()
        .map(|d| format!(":({magic}literal,top){d}"))
        .collect()
}

/// The `git stash push` argv for a quarantine of `worktree` that never writes
/// a build tree into `refs/stash` (#11075). Staged entries under a build tree
/// are unstaged first (content stays on disk): an exclude pathspec only limits
/// the worktree diff, so the stash's index commit would still carry them.
pub(crate) fn quarantine_stash_args(worktree: &Path, msg: &str) -> Vec<String> {
    let mut args: Vec<String> = ["stash", "push", "--include-untracked", "-m", msg]
        .map(String::from)
        .to_vec();
    let trees = worktree_build_tree_dirs(worktree);
    if trees.is_empty() {
        return args;
    }
    let _ = Command::new("git")
        .args(["reset", "-q", "--"])
        .args(literal_pathspecs(&trees, ""))
        .current_dir(worktree)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    args.extend(["--".to_string(), ":/".to_string()]);
    args.extend(literal_pathspecs(&trees, "exclude,"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";
    const RUSTC_INFO: &str = r#"{"rustc_fingerprint":123,"outputs":{}}"#;

    #[test]
    fn generated_artifact_matches_the_no_real_work_class() {
        for path in [
            "sim/.venv/lib/python3.12/site-packages/numpy/__init__.py",
            "venv/bin/activate",
            "tools/__pycache__/helper.cpython-312.pyc",
            "src/thing.pyc",
            "web/node_modules/left-pad/index.js",
            "src/mypkg.egg-info/PKG-INFO",
            ".pytest_cache/v/cache/lastfailed",
            ".mypy_cache/3.12/foo.data.json",
            ".ruff_cache/content",
            "notebooks/.ipynb_checkpoints/x-checkpoint.ipynb",
            "docs/.DS_Store",
        ] {
            assert!(is_generated_artifact(path), "expected generated: {path}");
        }
    }

    #[test]
    fn generated_artifact_does_not_swallow_plausible_source_directories() {
        // `dist/`, `build/`, `out/`, `target/` are deliberately NOT in the
        // set: each is a real hand-authored source directory somewhere, and a
        // false "generated" call here deletes work irrecoverably.
        for path in [
            "dist/index.js",
            "build/Makefile",
            "out/report.txt",
            "target/spec.md",
            "src/venv_helpers.py",
            "src/node_modules_shim.ts",
            "docs/pycache-notes.md",
        ] {
            assert!(!is_generated_artifact(path), "must not be treated as generated: {path}");
        }
    }

    #[test]
    fn build_tree_marker_requires_cargo_content() {
        assert!(is_build_tree_marker("CACHEDIR.TAG", SIG.as_bytes()));
        assert!(!is_build_tree_marker("CACHEDIR.TAG", b"not a signature"));
        assert!(is_build_tree_marker(".rustc_info.json", RUSTC_INFO.as_bytes()));
        assert!(!is_build_tree_marker(".rustc_info.json", br#"{"other":1}"#));
        assert!(!is_build_tree_marker(".rustc_info.json", b"rustc_fingerprint"));
        assert!(!is_build_tree_marker("README.md", SIG.as_bytes()));
    }

    #[test]
    fn stash_build_tree_dirs_accept_either_marker_and_never_the_root() {
        let paths: Vec<String> = [
            "target-x/CACHEDIR.TAG",
            "target-x/debug/a.o",
            ".loom/target-issue-9748/.rustc_info.json",
            ".loom/target-issue-9748/debug/b.o",
            "fake/CACHEDIR.TAG",
            "target/CACHEDIR.TAG.bak",
            // A repo-root marker must NOT make every path a build tree.
            "CACHEDIR.TAG",
            ".rustc_info.json",
        ]
        .map(String::from)
        .to_vec();
        let dirs = build_tree_dirs_in(&paths, |p| match p.rsplit('/').next() {
            _ if p.starts_with("fake/") => Some(b"nope".to_vec()),
            Some(".rustc_info.json") => Some(RUSTC_INFO.as_bytes().to_vec()),
            _ => Some(SIG.as_bytes().to_vec()),
        });
        let want: BTreeSet<String> = ["target-x", ".loom/target-issue-9748"]
            .map(String::from)
            .into();
        assert_eq!(dirs, want);
        assert!(is_under_build_tree("target-x/debug/a.o", &dirs));
        assert!(is_under_build_tree(".loom/target-issue-9748/debug/b.o", &dirs));
        assert!(!is_under_build_tree("target/spec.md", &dirs));
        assert!(!is_under_build_tree("fake/x.rs", &dirs));
        assert!(!is_under_build_tree("src/main.rs", &dirs));
        assert!(!is_under_build_tree("target-xy/a.o", &dirs));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn seeded_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
        dir
    }

    /// Every file in the stash's worktree, index and untracked commits.
    fn stash_files(dir: &Path) -> String {
        ["refs/stash", "refs/stash^2", "refs/stash^3"]
            .iter()
            .filter_map(|rev| {
                let out = Command::new("git")
                    .args(["ls-tree", "-r", "--name-only", rev])
                    .current_dir(dir)
                    .output()
                    .ok()?;
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            })
            .collect()
    }

    #[test]
    fn quarantine_never_stashes_build_trees_even_with_ignored_markers() {
        let repo = seeded_repo();
        let root = repo.path();
        // Ignored markers: invisible to `ls-files --exclude-standard`.
        write(root, ".gitignore", "CACHEDIR.TAG\n.rustc_info.json\n");
        git(root, &["add", ".gitignore"]);
        git(root, &["commit", "-q", "-m", "ignore markers"]);
        write(root, "target-x/CACHEDIR.TAG", SIG);
        write(root, "target-x/debug/artifact.o", "bin");
        // Untagged tree carrying only `.rustc_info.json` (#9748 shape).
        write(root, ".loom/target-issue-9748/.rustc_info.json", RUSTC_INFO);
        write(root, ".loom/target-issue-9748/debug/b.o", "bin");
        // Staged artifacts under an ignored-marker tree.
        write(root, "target-staged/CACHEDIR.TAG", SIG);
        write(root, "target-staged/a.o", "bin");
        git(root, &["add", "target-staged"]);
        // A repo-root marker must not exclude everything.
        write(root, "CACHEDIR.TAG", SIG);
        // Real work, including an unmarked (hand-authored) target/.
        write(root, "target/hand.txt", "keep");
        write(root, "real.txt", "work");

        crate::worktree_ops::clean::quarantine_dirty_worktree(root, "issue=11075 reason=test")
            .expect("real dirt must still be quarantined");
        let files = stash_files(root);
        assert!(files.contains("real.txt"), "{files}");
        assert!(files.contains("target/hand.txt"), "{files}");
        for tree in ["target-x/", "target-issue-9748", "target-staged"] {
            assert!(!files.contains(tree), "{tree} leaked into refs/stash: {files}");
        }
        assert!(root.join("target-staged/a.o").exists(), "build tree content stays on disk");
    }

    #[test]
    fn worktree_build_tree_dirs_requires_marker_content() {
        let repo = seeded_repo();
        let root = repo.path();
        write(root, "fake/CACHEDIR.TAG", "not a signature");
        write(root, "fake/x.o", "bin");
        write(root, "real/CACHEDIR.TAG", SIG);
        write(root, "real/x.o", "bin");
        write(root, "info/.rustc_info.json", r#"{"no_fingerprint":1}"#);
        write(root, "CACHEDIR.TAG", SIG);
        let want: BTreeSet<String> = ["real".to_string()].into();
        assert_eq!(worktree_build_tree_dirs(root), want);
    }
}
