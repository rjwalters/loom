//! Incremental line reader for append-only usage stores (Issue #8930).
//!
//! Each burn sample reads only the bytes appended since the previous one: a
//! per-file cursor is the byte offset just past the last complete line. A
//! trailing line with no newline yet is left for the next poll, so a record
//! that is mid-write is never decoded half-written.
//!
//! - **Files are keyed by canonical path**, so a store reachable through a
//!   symlinked directory (a pooled Codex profile's `sessions/`, #8694) is read
//!   once.
//! - **Truncation or replacement resets the cursor.** A file shorter than its
//!   cursor, or (on Unix) one whose device/inode changed, is read again from
//!   the start with fresh per-file state. The caller's event filter and
//!   message-id memory keep that from re-counting.
//! - **Only recently written files are tracked.** A file not modified within
//!   `active_since` is dropped from the set, which bounds memory to the files
//!   in use. If it is written again it comes back as a new file and is read
//!   from the start; its old records are older than the caller's event filter.
//! - **A line longer than [`MAX_LINE_BYTES`] is skipped**, never held whole.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// Longest line decoded. A usage record is small; a transcript line holding a
/// huge tool result is not usage and is skipped rather than buffered.
pub const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;

/// Where one file has been read up to.
#[derive(Debug, Default)]
struct Cursor {
    offset: u64,
    identity: Option<(u64, u64)>,
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Per-file cursors plus caller state `S` (e.g. Codex's running counters).
#[derive(Debug)]
pub struct TailSet<S> {
    files: HashMap<PathBuf, (Cursor, S)>,
}

impl<S> Default for TailSet<S> {
    fn default() -> Self {
        Self {
            files: HashMap::new(),
        }
    }
}

impl<S: Default> TailSet<S> {
    /// The files currently tracked, so a caller whose discovery is bounded
    /// (Codex's date directories) can keep polling a long-running file.
    pub fn tracked(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys()
    }

    /// Read every complete line appended to `paths` since the previous poll,
    /// calling `on_line(state, line)` for each. Files modified before
    /// `active_since`, missing, or no longer listed are dropped.
    pub fn poll(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        active_since: DateTime<Utc>,
        mut on_line: impl FnMut(&mut S, &str),
    ) {
        let mut next: HashMap<PathBuf, (Cursor, S)> = HashMap::new();
        for path in paths {
            // Stat before resolving: most files are idle, and skipping them
            // costs one `stat` rather than a path canonicalisation.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let recent = meta
                .modified()
                .is_ok_and(|modified| DateTime::<Utc>::from(modified) >= active_since);
            if !meta.is_file() || !recent {
                continue;
            }
            let key = path.canonicalize().unwrap_or(path);
            if next.contains_key(&key) {
                continue;
            }
            let (mut cursor, mut state) = self.files.remove(&key).unwrap_or_default();
            let id = identity(&meta);
            if meta.len() < cursor.offset || (cursor.identity.is_some() && cursor.identity != id) {
                cursor = Cursor::default();
                state = S::default();
            }
            cursor.identity = id;
            if meta.len() > cursor.offset {
                // An unreadable file keeps its cursor and is retried next poll.
                let _ = read_lines_from(&key, &mut cursor.offset, MAX_LINE_BYTES, |line| {
                    on_line(&mut state, line);
                });
            }
            next.insert(key, (cursor, state));
        }
        self.files = next;
    }
}

/// Decode complete lines of `path` from `*offset`, advancing it past the last
/// newline read.
fn read_lines_from(
    path: &std::path::Path,
    offset: &mut u64,
    max_line: u64,
    mut on_line: impl FnMut(&str),
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*offset))?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut position = *offset;
    let mut skipping = false;
    loop {
        buf.clear();
        let read = (&mut reader).take(max_line).read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        position += read as u64;
        if buf.last() != Some(&b'\n') {
            if read as u64 >= max_line {
                // Over-long: discard it through its newline.
                skipping = true;
                continue;
            }
            // A partial last line, still being written.
            break;
        }
        *offset = position;
        if std::mem::take(&mut skipping) {
            continue;
        }
        if let Ok(line) = std::str::from_utf8(&buf) {
            on_line(line.trim_end());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tail_tests.rs"]
mod tests;
