//! Name-independent Cargo build trees inside a worktree (#11193).
//!
//! Agents pick their own `CARGO_TARGET_DIR` names (`.loom-target/`,
//! `.loom/target-doctor-<N>/`, `target-wt/`, `.cargo-target-doctor-<N>/`), so
//! neither the fixed-name artifact list (`target/`, `node_modules/`, ...) nor
//! the five Loom bookkeeping markers recognise them. Left alone, one such
//! directory pins a merged `pr-<N>` worktree as "untracked user work" forever.
//!
//! This module answers two questions, both by content and never by name:
//!
//! 1. **Discovery** ([`discover_build_roots`]): which directories below a
//!    worktree carry a content-verified cargo marker ([`super::is_build_tree_marker`]).
//!    The walk is bounded, never descends through a symlink, and only accepts
//!    a marker that is a regular file.
//! 2. **Proof** ([`prove_build_tree`]): whether such a directory holds nothing
//!    but cargo's own layout. A marker proves cargo *wrote* in the directory;
//!    it does not prove every sibling is disposable. The proof is an
//!    allow-list: every top-level entry must be a cargo root file, a
//!    cargo/tool-owned output directory, a profile directory (carries a
//!    `.fingerprint/` and only profile-shaped entries), or a target-triple
//!    directory holding only profile directories. Anything else — a `.rs`
//!    file, a `src/`, a symlink, a stray note — fails the proof and the
//!    directory is kept as user work. The proof stops at cargo-owned
//!    namespaces (`deps/`, `build/`, `.fingerprint/`, `incremental/`, ...):
//!    nothing a person writes lives there.
//!
//! Neither answer authorizes removing a worktree by itself. The `pr-<N>`
//! classifier uses [`has_untracked_user_work`] only to stop proven build
//! output from *counting as* untracked user work; every other gate (sentinel,
//! processes, editable installs, PR state, grace, tracked changes) still runs.

use std::collections::HashMap;
use std::fs::{self, DirEntry};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{is_build_tree_marker, BUILD_TREE_MARKERS};

/// Deepest build root discovery reports: `.loom/target-doctor-1` is depth 2.
const DISCOVERY_MAX_DEPTH: usize = 3;

/// Directory entries discovery visits before giving up (an unfound root is
/// simply kept, the safe direction).
const DISCOVERY_MAX_ENTRIES: usize = 50_000;

/// Never descended into: git metadata and the fixed-name dependency trees.
const DISCOVERY_SKIP: &[&str] = &[".git", "node_modules", ".venv"];

/// Regular files cargo writes at a target root.
const ROOT_FILES: &[&str] = &[
    "CACHEDIR.TAG",
    ".rustc_info.json",
    ".future-incompat-report.json",
    ".cargo-lock",
    ".package-cache",
];

/// Output directories cargo (or a cargo subcommand) owns at a target root.
const OPAQUE_ROOT_DIRS: &[&str] = &[
    "doc",
    "package",
    "tmp",
    "nextest",
    "criterion",
    "llvm-cov-target",
];

/// Directories inside a profile directory (`debug/`, `release/`, `<custom>/`).
const PROFILE_SUBDIRS: &[&str] = &[".fingerprint", "build", "deps", "examples", "incremental"];

/// Regular files inside a profile directory with no extension to judge by.
const PROFILE_FILES: &[&str] = &[".cargo-lock", ".cargo-artifact-lock"];

/// Extensions of uplifted cargo outputs in a profile directory.
const PROFILE_FILE_EXTS: &[&str] = &[
    "d", "rlib", "rmeta", "so", "dylib", "a", "dll", "lib", "pdb", "exp", "wasm", "exe",
];

/// A directory below a worktree that carries a content-verified cargo marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRoot {
    /// Worktree-relative path, `/`-separated, no trailing slash.
    pub rel: String,
    /// The entry itself is a symlink: reported so it can be kept with a
    /// reason, never descended into or removed.
    pub symlink: bool,
}

/// Whether `dir` holds a cargo marker that is a regular file (not a symlink
/// that could point outside the worktree) with genuine cargo content.
fn has_regular_marker(dir: &Path) -> bool {
    BUILD_TREE_MARKERS.iter().any(|name| {
        let path = dir.join(name);
        fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
            && fs::read(&path).is_ok_and(|bytes| is_build_tree_marker(name, &bytes))
    })
}

/// Whether the symlink at `path` points at a marked directory. Used only to
/// report a symlinked root (so it is kept with a reason): the link is never
/// walked, measured or removed.
fn symlink_targets_build_tree(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
        && BUILD_TREE_MARKERS
            .iter()
            .any(|name| fs::read(path.join(name)).is_ok_and(|b| is_build_tree_marker(name, &b)))
}

/// Every marked directory below `worktree` (never the worktree itself), up
/// to [`DISCOVERY_MAX_DEPTH`] deep, sorted by path. A found root is not
/// descended into, so nested markers under it are not reported separately.
#[must_use]
pub fn discover_build_roots(worktree: &Path) -> Vec<BuildRoot> {
    let mut found = Vec::new();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(worktree.to_path_buf(), String::new(), 0)];
    let mut visited = 0usize;
    'walk: while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > DISCOVERY_MAX_ENTRIES {
                break 'walk;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if DISCOVERY_SKIP.contains(&name.as_str()) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let child_rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let path = entry.path();
            if file_type.is_symlink() {
                if symlink_targets_build_tree(&path) {
                    found.push(BuildRoot {
                        rel: child_rel,
                        symlink: true,
                    });
                }
                continue;
            }
            if !file_type.is_dir() {
                continue;
            }
            if has_regular_marker(&path) {
                found.push(BuildRoot {
                    rel: child_rel,
                    symlink: false,
                });
            } else if depth + 1 < DISCOVERY_MAX_DEPTH {
                stack.push((path, child_rel, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| a.rel.cmp(&b.rel));
    found
}

fn is_executable_file(entry: &DirEntry) -> bool {
    entry
        .metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// One entry of a profile directory: cargo's own subdirectories, its lock
/// files, uplifted artifacts by extension, an executable binary, or a
/// `.dSYM` bundle (a symlink into `deps/` on macOS).
fn profile_entry_ok(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy().into_owned();
    let Ok(file_type) = entry.file_type() else {
        return false;
    };
    if file_type.is_symlink() {
        return name.ends_with(".dSYM");
    }
    if file_type.is_dir() {
        return PROFILE_SUBDIRS.contains(&name.as_str()) || name.ends_with(".dSYM");
    }
    if !file_type.is_file() {
        return false;
    }
    if PROFILE_FILES.contains(&name.as_str()) {
        return true;
    }
    match Path::new(&name).extension().and_then(|e| e.to_str()) {
        Some(ext) => PROFILE_FILE_EXTS.contains(&ext),
        None => is_executable_file(entry),
    }
}

/// A cargo profile directory: a real `.fingerprint/` and nothing but
/// profile-shaped entries.
fn is_profile_dir(dir: &Path) -> bool {
    if !fs::symlink_metadata(dir.join(".fingerprint")).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    fs::read_dir(dir)
        .is_ok_and(|mut entries| entries.all(|e| e.is_ok_and(|e| profile_entry_ok(&e))))
}

/// A target-triple directory (`x86_64-unknown-linux-gnu/`): non-empty, and
/// only profile directories or `doc/` inside it.
fn is_triple_dir(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut any = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        any = true;
        let real_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !real_dir || !(entry.file_name() == "doc" || is_profile_dir(&entry.path())) {
            return false;
        }
    }
    any
}

/// Prove that `dir` is nothing but a cargo build tree, or say why not.
///
/// # Errors
///
/// A human-readable reason the directory must be kept: it is a symlink or not
/// a directory, carries no regular content-verified marker, or holds an entry
/// outside cargo's target layout (mixed content).
pub fn prove_build_tree(dir: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("cannot stat: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("is a symlink: not followed".to_string());
    }
    if !meta.is_dir() {
        return Err("is not a directory".to_string());
    }
    if !has_regular_marker(dir) {
        return Err(
            "holds no content-verified cargo marker (CACHEDIR.TAG / .rustc_info.json)".to_string()
        );
    }
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot list: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot stat {name}: {e}"))?;
        let ok = if file_type.is_symlink() {
            false
        } else if file_type.is_file() {
            ROOT_FILES.contains(&name.as_str())
        } else if file_type.is_dir() {
            let path = entry.path();
            OPAQUE_ROOT_DIRS.contains(&name.as_str())
                || is_profile_dir(&path)
                || is_triple_dir(&path)
        } else {
            false
        };
        if !ok {
            return Err(format!(
                "holds `{name}`, which is not part of a cargo target layout — mixed content is kept"
            ));
        }
    }
    Ok(())
}

/// `Some(true)` when git tracks any file under `rel`, `Some(false)` when it
/// tracks none, `None` when git cannot answer (callers keep the directory).
#[must_use]
pub fn tracks_files_under(worktree: &Path, rel: &str) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-z", "--"])
        .arg(format!(":(literal,top){rel}"))
        .output()
        .ok()?;
    out.status.success().then_some(!out.stdout.is_empty())
}

/// The outermost proper ancestor directory of `path` (worktree-relative)
/// carrying a regular, content-verified marker.
fn outermost_marked_ancestor<'p>(
    worktree: &Path,
    path: &'p str,
    marked: &mut HashMap<String, bool>,
) -> Option<&'p str> {
    path.match_indices('/')
        .map(|(i, _)| &path[..i])
        .find(|dir| {
            *marked
                .entry((*dir).to_string())
                .or_insert_with(|| has_regular_marker(&worktree.join(dir)))
        })
}

/// True if `worktree` holds untracked, non-ignored files that are neither
/// Loom's own sentinels ([`crate::worktree_ops::safety::is_loom_own_untracked_path`])
/// nor inside a proven, untracked-only cargo build tree ([`prove_build_tree`]).
///
/// The `pr-<N>` twin of [`crate::worktree_ops::safety::has_untracked_files`]
/// (#11193). A file under a marked directory whose proof fails — a `.rs`
/// beside a valid `CACHEDIR.TAG`, a truncated signature, a symlink — still
/// counts as user work. A `git` failure reports `false`, the same
/// fail-toward-proceed convention (and the same bounding gates) as
/// `has_untracked_files`.
#[must_use]
pub fn has_untracked_user_work(worktree: &Path) -> bool {
    if !worktree.is_dir() {
        return false;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-z", "--others", "--exclude-standard"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut marked: HashMap<String, bool> = HashMap::new();
    let mut proven: HashMap<String, bool> = HashMap::new();
    for path in stdout.split('\0').filter(|p| !p.is_empty()) {
        if crate::worktree_ops::safety::is_loom_own_untracked_path(path) {
            continue;
        }
        let Some(root) = outermost_marked_ancestor(worktree, path, &mut marked) else {
            return true;
        };
        let ok = *proven.entry(root.to_string()).or_insert_with(|| {
            prove_build_tree(&worktree.join(root)).is_ok()
                && tracks_files_under(worktree, root) == Some(false)
        });
        if !ok {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[path = "build_tree_tests.rs"]
pub(crate) mod tests;
