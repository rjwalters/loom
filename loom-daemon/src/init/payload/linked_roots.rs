//! Resync roots reached through a symlinked directory (#11426).
//!
//! A repo may commit a directory above a payload root as a symlink: miniset
//! links `.claude/commands -> ../.agents/commands`, so Loom's commands root
//! `.claude/commands/loom` physically lives at `.agents/commands/loom`.
//! Writing and staging the root by its logical path would make `git add`
//! fail ("pathspec ... is beyond a symbolic link"), and skipping it would
//! leave Loom's role prompts there stale for good.
//!
//! So each root is located first:
//! * no symlinked directory above it: resynced as is;
//! * the link resolves **inside** the repo: resynced at the resolved real
//!   path, so every write and every staged path is a real one and the link
//!   itself is left alone;
//! * the link points **outside** the repo, dangles, loops, or resolves onto
//!   another root: skipped, reported once per root, and, for a payload tree
//!   (`.loom`, `.claude/commands/loom`), the install stamp is held back (see
//!   [`super::apply`]), so `loom_version` never claims prompts it could not
//!   write are current.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// The most links followed for one root before it is called a loop.
const MAX_HOPS: usize = 8;

/// Where one resync root lives in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootPath {
    /// No directory above it is a symlink.
    Direct,
    /// A directory above it is a symlink resolving inside the repo; the root
    /// is resynced at this repo-relative real path.
    InRepo(String),
    /// A directory above it is a symlink that cannot be resynced through:
    /// `link` is the symlinked directory, `why` says where it points.
    Unreachable { link: String, why: String },
}

/// Locate `rel` in `dest`, following every symlinked directory above it.
pub(crate) fn locate(dest: &Path, rel: &str) -> RootPath {
    let mut cur = rel.to_string();
    let mut hops = 0;
    loop {
        let Some(link) = first_linked_ancestor(dest, &cur) else {
            return if hops == 0 {
                RootPath::Direct
            } else {
                RootPath::InRepo(cur)
            };
        };
        hops += 1;
        let unreachable = |why: String| RootPath::Unreachable {
            link: link.clone(),
            why,
        };
        if hops > MAX_HOPS {
            return unreachable("a symlink loop".to_string());
        }
        let Ok(target) = fs::read_link(dest.join(&link)) else {
            return unreachable("an unreadable symlink".to_string());
        };
        let Some(real) = resolve(dest, &link, &target) else {
            return unreachable(format!("a symlink to {} (outside the repo)", target.display()));
        };
        if fs::symlink_metadata(dest.join(&real)).is_err() {
            return unreachable(format!("a dangling symlink to {}", target.display()));
        }
        let rest = cur.strip_prefix(link.as_str()).unwrap_or_default();
        cur = format!("{real}{rest}");
    }
}

/// The shallowest directory strictly above `rel` that is a symlink in `dest`.
fn first_linked_ancestor(dest: &Path, rel: &str) -> Option<String> {
    let mut ancestors: Vec<&Path> = Path::new(rel)
        .ancestors()
        .skip(1)
        .filter(|a| !a.as_os_str().is_empty())
        .collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .find(|dir| fs::symlink_metadata(dest.join(dir)).is_ok_and(|m| m.file_type().is_symlink()))
        .map(|dir| dir.to_string_lossy().into_owned())
}

/// The repo-relative path the symlink at `link` (repo-relative) points to,
/// or `None` when it leaves the repo. Relative targets are resolved
/// lexically from the link's own directory; an absolute one counts only when
/// it lies under the repo's canonical path.
fn resolve(dest: &Path, link: &str, target: &Path) -> Option<String> {
    if target.is_absolute() {
        let root = dest.canonicalize().ok()?;
        let real = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());
        return real
            .strip_prefix(&root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty());
    }
    let mut parts: Vec<String> = Path::new(link)
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    for component in target.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The repo-relative path of a symlink's target, resolved as [`resolve`]
/// does but from a link and target read out of git rather than a
/// filesystem: `None` when it leaves the repo or is absolute. For the
/// default-branch export (`fleet_sync::workspace_resync`), which must ship
/// the link and its in-repo target so a diff there sees what a resync sees.
pub(crate) fn resolve_relative(link: &str, target: &str) -> Option<String> {
    let target = PathBuf::from(target);
    if target.is_absolute() {
        return None;
    }
    resolve(Path::new("/"), link, &target)
}

/// `path` is `root` or lies under it.
pub(crate) fn within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
#[path = "linked_roots_tests.rs"]
mod tests;
