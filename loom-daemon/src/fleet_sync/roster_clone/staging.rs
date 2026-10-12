//! The staging directory a roster clone is made in (#11218).
//!
//! A clone is never made at its final path: it goes into a fresh,
//! uniquely-named sibling (`.<dir>.loom-clone-<random>`, created with
//! [`tempfile`]) whose first content is a marker file this module writes, and
//! only the finished repo inside it is renamed into place. Dropping a
//! [`Staging`] removes the directory, so a failed clone leaves nothing.
//!
//! A sibling left behind by a daemon that died mid-clone is reclaimed on a
//! later pass **only when it carries the marker** ([`sweep`]). One that
//! matches the name but has no marker (or is a symlink, or not a directory) is
//! not this module's to delete: it is left untouched and the clone is refused
//! with a report, so an operator decides.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The marker file's name inside a staging directory.
pub const MARKER: &str = ".loom-clone-staging";
/// The marker file's exact content.
pub const MARKER_TEXT: &str =
    "loom-daemon roster clone staging directory (#11218); reclaimed by the next pass\n";

/// The name prefix of every staging directory for clone directory `dir`.
#[must_use]
pub fn prefix(dir: &OsStr) -> String {
    format!(".{}.loom-clone-", dir.to_string_lossy())
}

/// Whether `path` is a real directory (not a symlink) holding this module's
/// marker as a regular file with the expected content.
#[must_use]
pub fn is_marked(path: &Path) -> bool {
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let marker = path.join(MARKER);
    is_dir
        && std::fs::symlink_metadata(&marker).is_ok_and(|m| m.is_file())
        && std::fs::read_to_string(&marker).is_ok_and(|t| t == MARKER_TEXT)
}

/// Reclaim marked leftovers for `dir` under `parent`. `Err` (nothing about
/// them is touched) when a sibling with the staging prefix is not marked.
pub fn sweep(parent: &Path, dir: &OsStr) -> Result<(), String> {
    let prefix = prefix(dir);
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot list {}: {e}", parent.display())),
    };
    let mut foreign = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        if is_marked(&path) {
            std::fs::remove_dir_all(&path).map_err(|e| {
                format!("removing the leftover staging directory {}: {e}", path.display())
            })?;
        } else {
            foreign.push(path.display().to_string());
        }
    }
    if foreign.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} is named like a clone staging directory but carries no Loom marker; left \
         untouched and not cloned — remove it by hand if it is not wanted",
        foreign.join(", ")
    ))
}

/// One clone's staging directory, removed with everything in it on drop.
pub struct Staging {
    dir: tempfile::TempDir,
}

impl Staging {
    /// Create a fresh, uniquely-named staging directory for `dir` under
    /// `parent`, marker first.
    pub fn create(parent: &Path, dir: &OsStr) -> Result<Self, String> {
        let made = tempfile::Builder::new()
            .prefix(&prefix(dir))
            .tempdir_in(parent)
            .map_err(|e| format!("creating a staging directory in {}: {e}", parent.display()))?;
        std::fs::write(made.path().join(MARKER), MARKER_TEXT)
            .map_err(|e| format!("marking {}: {e}", made.path().display()))?;
        Ok(Self { dir: made })
    }

    /// The staging directory itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where the clone goes: a not-yet-existing directory inside it.
    #[must_use]
    pub fn dest(&self) -> PathBuf {
        self.dir.path().join("repo")
    }
}
