//! The `maintain-only` hold (#11186): an operator's, permanent while set.
//!
//! The W3/W4 holds are judged by the workspace pass and live in the
//! process-wide [`super::Holds`]. This one is not judged at all: it is the
//! registry entry's `maintain_only` mark
//! ([`crate::workspace_registry::MaintainOnly`]), set by the fleet store's
//! `fleet: maintain` or by `loom-daemon workspace hold`. It is read straight
//! from the registry file, so it takes effect on the next dispatch decision
//! after the file changes, with no daemon restart and no workspace pass.
//!
//! Because it never enters [`super::Holds`] it has none of that machinery's
//! side effects, on purpose:
//!
//! - it raises no roll demand ([`super::repo_ahead_min`] never sees it);
//! - it never stands as a "stuck" hold, so the 30-minute ERROR never fires
//!   for it: being held is what the operator asked for;
//! - it does not count toward a roll's held-workspace timing;
//! - it coexists with a W3/W4 hold on the same workspace, which keeps being
//!   judged, alerted and cleared exactly as before.
//!
//! The registry file is re-read only when it changes (its length, mtime and,
//! on unix, inode), so a dispatch decision costs one `stat`. A registry that
//! cannot be read keeps the last set read: an unreadable file never quietly
//! lifts an operator's hold.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::workspace_registry::{MaintainOnly, WorkspaceRegistry};

/// What identifies one version of the registry file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            inode,
        })
    }
}

/// The maintain-only marks of one registry file, re-read when it changes.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    path: Option<PathBuf>,
    /// `None`: the file did not exist when last looked at.
    stamp: Option<Stamp>,
    marks: HashMap<PathBuf, MaintainOnly>,
}

impl Cache {
    /// The mark on `root` in the registry at `path`.
    pub(crate) fn lookup(&mut self, path: &Path, root: &Path) -> Option<MaintainOnly> {
        self.refresh(path);
        if self.marks.is_empty() {
            return None;
        }
        self.marks
            .get(root)
            .or_else(|| self.marks.get(&super::normalize(root)))
            .copied()
    }

    fn refresh(&mut self, path: &Path) {
        let stamp = Stamp::of(path);
        if self.path.as_deref() == Some(path) && self.stamp == stamp {
            return;
        }
        if stamp.is_none() {
            // No registry: nothing is registered, so nothing is held.
            self.marks.clear();
        } else {
            match WorkspaceRegistry::load(path) {
                Ok(registry) => {
                    self.marks = registry
                        .workspaces
                        .iter()
                        .filter_map(|w| Some((w.root.clone(), w.maintain_only?)))
                        .collect();
                }
                Err(e) => {
                    log::warn!(
                        "workspace_hold: cannot read {} for maintain-only marks ({e:#}); \
                         keeping the {} read before",
                        path.display(),
                        self.marks.len()
                    );
                }
            }
        }
        self.path = Some(path.to_path_buf());
        self.stamp = stamp;
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// The maintain-only mark on `root` in this host's workspace registry.
#[must_use]
pub fn maintain_only_for(root: &Path) -> Option<MaintainOnly> {
    #[cfg(test)]
    if let Some(mark) = overlay(root) {
        return Some(mark);
    }
    let path = crate::workspace_registry::default_registry_path().ok()?;
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lookup(&path, root)
}

#[cfg(test)]
fn overlay_cell() -> &'static Mutex<HashMap<PathBuf, MaintainOnly>> {
    static OVERLAY: OnceLock<Mutex<HashMap<PathBuf, MaintainOnly>>> = OnceLock::new();
    OVERLAY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn overlay(root: &Path) -> Option<MaintainOnly> {
    let cell = overlay_cell().lock().ok()?;
    cell.get(root)
        .or_else(|| cell.get(&super::normalize(root)))
        .copied()
}

/// Mark `root` maintain-only (`Some`) for this process's dispatch decisions
/// without a registry file, or drop that (`None`). Keyed by root, so tests
/// do not disturb each other.
#[cfg(test)]
pub(crate) fn set_maintain_only_for_test(root: &Path, mark: Option<MaintainOnly>) {
    let mut cell = overlay_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match mark {
        Some(mark) => cell.insert(super::normalize(root), mark),
        None => cell.remove(&super::normalize(root)),
    };
}

#[cfg(test)]
#[path = "maintain_only_tests.rs"]
mod tests;
