//! Small filesystem/formatting helpers shared by the reclaim and GC modules
//! (`target_dir_gc`, `tmpfs_reclaim`, `scratch_reclaim`, `git_tmp_reclaim`,
//! `docker_image_clean`, `target_orphan_reclaim`).

use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};

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
