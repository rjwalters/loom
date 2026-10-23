//! Small filesystem/formatting helpers shared by the reclaim and GC modules
//! (`target_dir_gc`, `tmpfs_reclaim`, `scratch_reclaim`, `git_tmp_reclaim`,
//! `docker_image_clean`, `target_orphan_reclaim`).

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;

use chrono::{DateTime, Utc};

/// Durability/permission options for [`write_atomic`].
#[derive(Debug, Clone, Copy)]
pub struct AtomicOpts {
    /// Owner-only (`0600`) file on unix; otherwise `0644`.
    pub private: bool,
    /// `fsync` the parent directory after the rename so the rename itself
    /// survives a crash (#10831). Best-effort where a directory cannot be
    /// opened for sync.
    pub sync_dir: bool,
}

impl Default for AtomicOpts {
    /// Crash-safe state: not private, parent directory synced.
    fn default() -> Self {
        Self {
            private: false,
            sync_dir: true,
        }
    }
}

/// Write `bytes` to `path` atomically: create the parent directory, write a
/// uniquely named temp file beside `path`, `fsync` it, then `rename` it into
/// place. A reader sees the old file or the new one, never a partial write,
/// and no temp file is left behind on success.
///
/// # Errors
/// Any I/O failure creating, writing, syncing or renaming the file.
pub fn write_atomic(path: &Path, bytes: &[u8], opts: AtomicOpts) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if opts.private { 0o600 } else { 0o644 };
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if opts.sync_dir {
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Format a byte count as `B` / `K` / `M` / `G` with one decimal place.
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", b / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// Recursively find the newest mtime and cumulative size under `path`,
/// without following symlinks. Best-effort: an unreadable child is skipped
/// rather than failing the whole walk (a permissions hiccup on one nested
/// file must not abort GC of everything else).
pub(crate) fn newest_mtime_and_size(path: &Path) -> io::Result<(DateTime<Utc>, u64)> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut newest = mtime_of(&meta);
        let mut total = 0u64;
        let entries = match fs::read_dir(path) {
            Ok(e) => e,
            Err(_) => return Ok((newest, total)),
        };
        for entry in entries.flatten() {
            if let Ok((child_newest, child_size)) = newest_mtime_and_size(&entry.path()) {
                if child_newest > newest {
                    newest = child_newest;
                }
                total = total.saturating_add(child_size);
            }
        }
        Ok((newest, total))
    } else {
        Ok((mtime_of(&meta), meta.len()))
    }
}

fn mtime_of(meta: &fs::Metadata) -> DateTime<Utc> {
    meta.modified()
        .ok()
        .map_or_else(Utc::now, DateTime::<Utc>::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_parent_replaces_and_leaves_no_temp() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("a/b/state.json");
        write_atomic(&path, b"one", AtomicOpts::default()).unwrap();
        write_atomic(&path, b"two", AtomicOpts::default()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "leftover temp file: {names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_is_0600_and_default_is_0644() {
        use std::os::unix::fs::PermissionsExt as _;
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("secret");
        let opts = AtomicOpts {
            private: true,
            sync_dir: false,
        };
        write_atomic(&p, b"x", opts).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let q = td.path().join("public");
        write_atomic(&q, b"x", AtomicOpts::default()).unwrap();
        assert_eq!(fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o644);
    }
}
