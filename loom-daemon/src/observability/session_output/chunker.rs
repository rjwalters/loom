//! The S3 write path for `session.output` (2am#5933, design 2am#5480): batch
//! the already-scrubbed records into immutable, content-addressed **chunks**
//! and announce each one with a small **chunk-pointer** event.
//!
//! ```text
//!   post-scrub record ──▶ Chunker::offer ──▶ open chunk (one per stream)
//!                                              │ 256 KB or 5 s
//!                                              ▼
//!                              zstd(ndjson) ──▶ retry queue ──▶ ChunkStore::put
//!                                                                   │ ok
//!                                                                   ▼
//!                                                       chunk-pointer event (OTLP)
//! ```
//!
//! # Why chunks and pointers
//!
//! Objects are immutable and keyed by the hash of their bytes
//! (`logs/<host>/<yyyy-mm>/<sha256>.zst`, built by `artifact-broker`), so a
//! live stream cannot be one growing object. The key carries no repo, issue or
//! stream, so the reader finds chunks through the pointer events, which are
//! bounded structured logs and stay in SigNoz. A pointer holds **no record
//! text** — the text lives only in the chunk.
//!
//! # Redaction
//!
//! Nothing downstream scrubs these bytes (they never cross the captain edge),
//! so [`Chunker::offer`] runs [`redact::scrub`] over every free-text field
//! again before serializing. The record constructors have already scrubbed
//! `text`; scrubbing is idempotent, so this is cheap defence in depth, and it
//! is what lets the chunk body stand on its own.
//!
//! # What is never written
//!
//! A record whose `loom.repo` is unresolved, malformed, or names a
//! `firewall: true` repo (`notebook`, `corporate`, any case) produces neither
//! a chunk nor a pointer. This mirrors the edge's `filter/live-output`; the
//! broker refuses the same repos, so this is the producer half of that
//! control, not a new one.
//!
//! # Failure
//!
//! The store is behind [`ChunkStore`]. A failing `put` leaves the chunk in a
//! bounded retry queue; on overflow the **oldest** chunk is dropped and the
//! loss is declared as `dropped_events` on the next chunk and pointer. The
//! OTLP export never waits on any of this.
//!
//! # Replay
//!
//! After a restart the producer republishes the retained tail with identical
//! `event_id`s. Identical records serialize to identical bytes, so they hash to
//! the same key and the `put` is a no-op. The chunker does not de-duplicate.

use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::telemetry::kinds::session_output::{redact, SessionOutputRecord};

/// Close a chunk once its uncompressed ndjson reaches this size.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Close a chunk this long after its first record, however small it is. This
/// bounds the freshness of the live view.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(5);

/// Sealed chunks held for retry before the oldest is dropped.
pub const DEFAULT_QUEUE_CAP: usize = 64;

/// The `daemon.event` topic a chunk pointer is published under.
pub const POINTER_TOPIC: &str = "session.output.chunk";

/// zstd level for chunks. Low: the producer runs on a shared host and the
/// text is small and repetitive.
const ZSTD_LEVEL: i32 = 3;

/// A sealed chunk, ready for the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The writing host (the broker fixes the key's host segment to the
    /// caller's own).
    pub host: String,
    /// `yyyy-mm` of the writing time.
    pub month: String,
    /// Lowercase hex SHA-256 of [`bytes`](Self::bytes) — the object's name.
    pub sha256: String,
    /// The zstd-compressed ndjson.
    pub bytes: Vec<u8>,
}

/// Where sealed chunks go: a put-only `kind: log` call through
/// `artifact-broker`.
///
/// A trait so this producer does not duplicate the broker client the archiver
/// (2am#5563) introduces; that client implements it. Called from a blocking
/// context, never from the async tick directly.
pub trait ChunkStore: Send + Sync {
    /// Put `chunk`. Idempotent by construction (same bytes, same key).
    ///
    /// # Errors
    /// Any failure; the chunk stays queued and is retried on the next pass.
    fn put(&self, chunk: &Chunk) -> Result<(), String>;
}

/// The facts a reader needs to find and use one chunk. Serialized as the
/// payload of a [`POINTER_TOPIC`] `daemon.event`. Contains no record text.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChunkPointer {
    #[serde(rename = "loom.repo")]
    pub repo: String,
    #[serde(rename = "loom.issue")]
    pub issue: Option<u32>,
    #[serde(rename = "loom.session_id")]
    pub session_id: Option<String>,
    pub stream_id: String,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub first_source_at: DateTime<Utc>,
    pub last_source_at: DateTime<Utc>,
    pub host: String,
    pub month: String,
    pub sha256: String,
    /// Compressed object size in bytes.
    pub size: u64,
    /// Records in the chunk.
    pub records: u64,
    /// Records lost to retry-queue overflow since the previous chunk. `0` when
    /// nothing was lost.
    pub dropped_events: u64,
}

/// What groups records into one chunk: same stream, same owner. A stream whose
/// repo is resolved mid-run starts a new chunk rather than mixing owners.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    repo: String,
    issue: Option<u32>,
    session_id: Option<String>,
}

#[derive(Debug)]
struct OpenChunk {
    stream_id: String,
    owner: Owner,
    opened_at: DateTime<Utc>,
    /// `(sequence, source_at, line)`; sorted by sequence when sealed.
    lines: Vec<(u64, DateTime<Utc>, String)>,
    uncompressed: usize,
}

#[derive(Debug)]
struct Sealed {
    pointer: ChunkPointer,
    chunk: Chunk,
}

/// Batches records into chunks. Pure and clock-injected: every method takes
/// `now`, so tests drive time directly.
#[derive(Debug)]
pub struct Chunker {
    host: String,
    max_bytes: usize,
    max_age: Duration,
    queue_cap: usize,
    open: Vec<OpenChunk>,
    queue: VecDeque<Sealed>,
    /// Records lost to overflow and not yet reported on a chunk.
    unreported_dropped: u64,
}

/// `owner/repo` of the edge's accepted shape (`^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`)
/// that is not a `firewall: true` repo (`/(notebook|corporate)$`, any case).
fn valid_repo(repo: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    };
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    part(owner)
        && part(name)
        && !name.eq_ignore_ascii_case("notebook")
        && !name.eq_ignore_ascii_case("corporate")
}

impl Chunker {
    #[must_use]
    pub fn new(host: impl Into<String>) -> Self {
        Self::with_limits(host, DEFAULT_MAX_BYTES, DEFAULT_MAX_AGE, DEFAULT_QUEUE_CAP)
    }

    #[must_use]
    pub fn with_limits(
        host: impl Into<String>,
        max_bytes: usize,
        max_age: Duration,
        queue_cap: usize,
    ) -> Self {
        Chunker {
            host: host.into(),
            max_bytes,
            max_age,
            queue_cap: queue_cap.max(1),
            open: Vec::new(),
            queue: VecDeque::new(),
            unreported_dropped: 0,
        }
    }

    /// Whether a record may be written to the artifact store at all.
    #[must_use]
    pub fn admits(record: &SessionOutputRecord) -> bool {
        record.identity.repo.as_deref().is_some_and(valid_repo)
    }

    /// Take one post-scrub record. Records that are not [admitted](Self::admits)
    /// are ignored without a trace: no chunk, no pointer.
    pub fn offer(&mut self, record: &SessionOutputRecord, now: DateTime<Utc>) {
        if !Self::admits(record) {
            return;
        }
        let mut record = record.clone();
        if let Some(text) = record.text.take() {
            record.text = Some(redact::scrub(&text));
        }
        if let Some(tool) = record.tool.take() {
            record.tool = Some(redact::scrub(&tool));
        }
        if let Some(reason) = record.gap_reason.take() {
            record.gap_reason = Some(redact::scrub(&reason));
        }
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        let owner = Owner {
            repo: record.identity.repo.clone().unwrap_or_default(),
            issue: record.identity.issue,
            session_id: record.identity.session_id.clone(),
        };
        if let Some(position) = self
            .open
            .iter()
            .position(|c| c.stream_id == record.stream_id)
        {
            if self.open[position].owner != owner {
                self.seal(position, now);
            }
        }
        let position = match self
            .open
            .iter()
            .position(|c| c.stream_id == record.stream_id)
        {
            Some(position) => position,
            None => {
                self.open.push(OpenChunk {
                    stream_id: record.stream_id.clone(),
                    owner,
                    opened_at: now,
                    lines: Vec::new(),
                    uncompressed: 0,
                });
                self.open.len() - 1
            }
        };
        let chunk = &mut self.open[position];
        chunk.uncompressed += line.len() + 1;
        chunk.lines.push((record.sequence, record.source_at, line));
        if chunk.uncompressed >= self.max_bytes {
            self.seal(position, now);
        }
    }

    /// Seal every open chunk that has been open for `max_age`.
    pub fn flush_due(&mut self, now: DateTime<Utc>) {
        while let Some(position) = self.open.iter().position(|c| {
            now.signed_duration_since(c.opened_at)
                .to_std()
                .is_ok_and(|age| age >= self.max_age)
        }) {
            self.seal(position, now);
        }
    }

    /// Seal everything (shutdown).
    pub fn flush_all(&mut self, now: DateTime<Utc>) {
        while !self.open.is_empty() {
            self.seal(0, now);
        }
    }

    /// Sealed chunks waiting for the store.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Try to put every queued chunk, oldest first. Stops at the first
    /// failure, leaving that chunk and everything behind it queued for the
    /// next pass. Returns the pointers of the chunks that were stored.
    pub fn pump(&mut self, store: &dyn ChunkStore) -> Vec<ChunkPointer> {
        let mut stored = Vec::new();
        while let Some(front) = self.queue.front() {
            if let Err(error) = store.put(&front.chunk) {
                log::warn!(
                    "session.output: chunk put failed ({} queued, will retry): {error}",
                    self.queue.len()
                );
                break;
            }
            if let Some(done) = self.queue.pop_front() {
                stored.push(done.pointer);
            }
        }
        stored
    }

    fn seal(&mut self, position: usize, now: DateTime<Utc>) {
        let mut open = self.open.remove(position);
        if open.lines.is_empty() {
            return;
        }
        open.lines.sort_by_key(|(sequence, _, _)| *sequence);
        let records = open.lines.len() as u64;
        let (first_sequence, first_source_at, _) = open.lines[0].clone();
        let (last_sequence, last_source_at, _) = open.lines[open.lines.len() - 1].clone();
        let dropped_events = std::mem::take(&mut self.unreported_dropped);
        let mut ndjson = String::with_capacity(open.uncompressed + 64);
        if dropped_events > 0 {
            // Declared loss rides in the chunk itself so the object is
            // self-describing; the reader surfaces it like a gap record.
            ndjson
                .push_str(&format!("{{\"chunk_meta\":{{\"dropped_events\":{dropped_events}}}}}\n"));
        }
        for (_, _, line) in &open.lines {
            ndjson.push_str(line);
            ndjson.push('\n');
        }
        let Ok(bytes) = zstd::stream::encode_all(ndjson.as_bytes(), ZSTD_LEVEL) else {
            // Compression of an in-memory buffer does not fail in practice;
            // if it ever does, the loss is declared rather than silent.
            self.unreported_dropped = self
                .unreported_dropped
                .saturating_add(records + dropped_events);
            return;
        };
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let month = now.format("%Y-%m").to_string();
        let pointer = ChunkPointer {
            repo: open.owner.repo,
            issue: open.owner.issue,
            session_id: open.owner.session_id,
            stream_id: open.stream_id,
            first_sequence,
            last_sequence,
            first_source_at,
            last_source_at,
            host: self.host.clone(),
            month: month.clone(),
            sha256: sha256.clone(),
            size: bytes.len() as u64,
            records,
            dropped_events,
        };
        let chunk = Chunk {
            host: self.host.clone(),
            month,
            sha256,
            bytes,
        };
        self.queue.push_back(Sealed { pointer, chunk });
        while self.queue.len() > self.queue_cap {
            if let Some(lost) = self.queue.pop_front() {
                // Oldest first: the live view cares about the newest text.
                self.unreported_dropped = self
                    .unreported_dropped
                    .saturating_add(lost.pointer.records + lost.pointer.dropped_events);
            }
        }
    }
}

impl ChunkPointer {
    /// The payload of the `daemon.event` that announces this chunk.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(test)]
#[path = "chunker_tests.rs"]
mod tests;
