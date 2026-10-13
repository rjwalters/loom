//! Consumer cursors: `<stream>/cursors/<consumer>.json` holding a
//! [`Position`] — a segment identity plus an offset in it, never a bare
//! global byte offset, so rotation cannot invalidate a cursor.
//!
//! Commits are durable atomic replacements: temp file in the same directory,
//! `sync_all`, rename, then a directory fsync. Every failure surfaces.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{sync_dir, JournalError, Position};

pub(crate) const CURSOR_DIR: &str = "cursors";

#[derive(Debug, Clone)]
pub struct Cursor {
    path: PathBuf,
}

impl Cursor {
    pub(crate) fn new(path: PathBuf) -> Self {
        Cursor { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The committed position, `None` if never committed. An unreadable or
    /// unparseable cursor is an error (rotation treats it as "retain all").
    pub fn load(&self) -> Result<Option<Position>, JournalError> {
        load(&self.path)
    }

    /// Durably replace the committed position.
    pub fn commit(&self, position: Position) -> Result<(), JournalError> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("cursor has no parent"))?;
        let created = !dir.is_dir();
        std::fs::create_dir_all(dir)?;
        if created {
            if let Some(stream_dir) = dir.parent() {
                sync_dir(stream_dir)?;
            }
        }
        let bytes = serde_json::to_vec(&position).map_err(JournalError::Encode)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".cursor-")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|e| e.error)?;
        sync_dir(dir)?;
        Ok(())
    }
}

pub(crate) fn load(path: &Path) -> Result<Option<Position>, JournalError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| JournalError::Io(io::Error::new(io::ErrorKind::InvalidData, e))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Every committed cursor of a stream (`*.json` under `cursors/`; temp files
/// are ignored). Any unreadable cursor fails the whole listing.
pub(crate) fn all(stream_dir: &Path) -> Result<Vec<(String, Position)>, JournalError> {
    let dir = stream_dir.join(CURSOR_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut cursors = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(consumer) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if consumer.starts_with('.') {
            continue;
        }
        let position = load(&entry.path())?.ok_or_else(|| {
            JournalError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "cursor vanished while listing",
            ))
        })?;
        cursors.push((consumer.to_owned(), position));
    }
    cursors.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(cursors)
}
