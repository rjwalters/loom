//! The shared bounded journal core (Epic #9908, Phase 2a, #11345).
//!
//! One writer/reader implementation for every concern journal
//! (`docs/design/persistence-inventory.md` §9.1). It generalises
//! `telemetry/trace/journal.rs` (which stays as it is). **Nothing in the
//! daemon calls this module yet**: no producer is migrated or wired, and no
//! production journal root is chosen (Q1 stays open for 2b). The only entry
//! points are this API and `loom-daemon journal verify`.
//!
//! # Layout
//!
//! ```text
//! <root>/<stream>/.lock                  advisory writer lock (never rotated)
//! <root>/<stream>/<first_seq:010>.jsonl  segments, named by their first seq
//! <root>/<stream>/cursors/<consumer>.json
//! ```
//!
//! # Contract
//!
//! - **One writer code path.** Every append takes an advisory lock with a
//!   bounded retry ([`DEFAULT_LOCK_RETRY`]); contention past it is a typed
//!   [`JournalError::Busy`] and nothing is written.
//!   *Deviation from §9.1:* the lock is a stable per-stream `.lock` file, not
//!   the active segment. With rotation the active segment changes identity,
//!   so two writers could hold locks on different segments across a roll.
//! - **Envelope**: versioned ([`ENVELOPE_MAJOR`]), a monotonic per-stream
//!   `seq`, and the writer identity (crate version + full commit SHA). Each
//!   encoded line is capped at [`MAX_ENTRY_BYTES`]; an oversize entry is
//!   rejected before anything is written.
//! - **Durability**: the whole line is written, then `sync_data`. Creating a
//!   segment (or the stream directory) also fsyncs the parent directory.
//!   Cursor commits use temp file + fsync + rename + directory fsync.
//! - **Torn tails** are repaired (truncated to the last newline) only by the
//!   lock holder. Readers never write and stop at the last complete record.
//! - **Rotation** ([`Stream::rotate`]) deletes only segments strictly older
//!   than the one holding the oldest consumer cursor. No cursors, or any
//!   unreadable cursor, retains everything.
//! - **Natural keys**: [`Stream::append_if_new`] de-duplicates `(kind, key)`
//!   within a bounded window of the newest
//!   [`JournalOptions::dedup_window_segments`] segments, and every
//!   [`ReadRecord`] exposes its key so consumers can de-duplicate replays.
//!
//! **Producers must never pass credential values** (tokens, keys, passwords)
//! in `data` or `key`; the journal stores what it is given verbatim.
//!
//! Quarantine copies of corrupt segments (§11.2) are deferred to the first
//! consumer slice; corrupt lines are skipped and counted here.

mod cursor;
mod envelope;
mod lock;
mod reader;
mod segment;
mod verify;
mod writer;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use cursor::Cursor;
pub use envelope::{Envelope, WriterIdentity, ENVELOPE_MAJOR};
pub use lock::StreamLock;
pub use reader::{ReadRecord, ReadStats, StreamReader};
pub use verify::{verify, StreamReport, VerifyOptions, VerifyReport};
pub use writer::{AppendOutcome, Appended, RotateOutcome};

/// Largest encoded journal line, newline included.
pub const MAX_ENTRY_BYTES: usize = 32 * 1024;
/// Default segment bound; a new segment starts before one would exceed it.
pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
/// Default bounded lock retry budget.
pub const DEFAULT_LOCK_RETRY: Duration = Duration::from_millis(1000);
/// Default natural-key de-duplication window, in newest segments.
pub const DEFAULT_DEDUP_WINDOW_SEGMENTS: usize = 2;

/// Journal errors. I/O failures surface; they are never swallowed.
#[derive(Debug)]
pub enum JournalError {
    /// The writer lock was not acquired within the retry budget.
    Busy,
    /// The encoded entry exceeds [`MAX_ENTRY_BYTES`]; nothing was written.
    EntryTooLarge {
        bytes: usize,
    },
    /// A stream or consumer name is not a plain file-name component.
    InvalidName(String),
    /// A record could not be encoded.
    Encode(serde_json::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Busy => write!(f, "journal busy: lock retry budget exhausted"),
            JournalError::EntryTooLarge { bytes } => {
                write!(f, "journal entry is {bytes} bytes (cap {MAX_ENTRY_BYTES})")
            }
            JournalError::InvalidName(name) => write!(f, "invalid journal name {name:?}"),
            JournalError::Encode(e) => write!(f, "journal encode failed: {e}"),
            JournalError::Io(e) => write!(f, "journal I/O failed: {e}"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Encode(e) => Some(e),
            JournalError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for JournalError {
    fn from(e: std::io::Error) -> Self {
        JournalError::Io(e)
    }
}

/// Tunables. The defaults match the trace journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalOptions {
    pub max_segment_bytes: u64,
    pub lock_retry: Duration,
    pub dedup_window_segments: usize,
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
            lock_retry: DEFAULT_LOCK_RETRY,
            dedup_window_segments: DEFAULT_DEDUP_WINDOW_SEGMENTS,
        }
    }
}

/// A position in a stream: the segment (its first `seq`) and a byte offset
/// within it. `seq` is the last record consumed (0 for none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub segment: u64,
    pub offset: u64,
    pub seq: u64,
}

impl Position {
    /// The beginning of the stream.
    pub const START: Position = Position {
        segment: 0,
        offset: 0,
        seq: 0,
    };
}

/// A journal root. Opening performs no I/O.
#[derive(Debug, Clone)]
pub struct Journal {
    root: PathBuf,
    options: JournalOptions,
}

impl Journal {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self::with_options(root, JournalOptions::default())
    }

    pub fn with_options(root: impl Into<PathBuf>, options: JournalOptions) -> Self {
        Journal {
            root: root.into(),
            options,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A handle on one stream. The name must be a plain component.
    pub fn stream(&self, name: &str) -> Result<Stream, JournalError> {
        validate_name(name)?;
        Ok(Stream {
            name: name.to_owned(),
            dir: self.root.join(name),
            options: self.options,
        })
    }
}

/// One stream of a journal: a directory of segments, a lock and cursors.
#[derive(Debug, Clone)]
pub struct Stream {
    name: String,
    dir: PathBuf,
    options: JournalOptions,
}

impl Stream {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Take the writer lock (bounded retry) without appending. Holding the
    /// guard blocks every writer of this stream, in any process.
    pub fn lock(&self) -> Result<StreamLock, JournalError> {
        StreamLock::acquire(&self.dir, self.options.lock_retry)
    }

    /// Stream records from `from`. Never writes and never repairs.
    #[must_use]
    pub fn reader(&self, from: Position) -> StreamReader {
        StreamReader::new(&self.name, &self.dir, from)
    }

    /// The cursor of one consumer.
    pub fn cursor(&self, consumer: &str) -> Result<Cursor, JournalError> {
        validate_name(consumer)?;
        Ok(Cursor::new(
            self.dir
                .join(cursor::CURSOR_DIR)
                .join(format!("{consumer}.json")),
        ))
    }
}

/// Stream and consumer names: `[A-Za-z0-9._-]`, 1..=64 chars, not starting
/// with `.` — never a path separator, never `.`/`..`, never a hidden file.
pub(crate) fn validate_name(name: &str) -> Result<(), JournalError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(JournalError::InvalidName(name.to_owned()))
    }
}

/// fsync a directory so entries created or removed in it are durable.
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(test)]
    tests::count_dir_sync();
    Ok(())
}
