//! The one writer code path: append, natural-key append and rotation, all
//! under the stream lock.

use std::fs::OpenOptions;
use std::io::{self, Write};

use chrono::Utc;
use serde_json::Value;

use super::envelope::{decode, encode, Decoded, Envelope, WriterIdentity, ENVELOPE_MAJOR};
use super::lock::StreamLock;
use super::segment::{self, Line, LineReader, Segment};
use super::{cursor, sync_dir, JournalError, Position, Stream};

/// A durable append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    pub seq: u64,
    /// The record's position: segment and start offset.
    pub position: Position,
}

/// The result of a natural-key append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended(Appended),
    /// A record with the same `(kind, key)` is already in the window.
    Duplicate {
        seq: u64,
    },
}

/// What a rotation did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotateOutcome {
    /// A new (empty) active segment was started.
    pub rolled: bool,
    /// Segments deleted, by first seq.
    pub deleted: Vec<u64>,
    /// Why nothing (more) was deleted, when retention was conservative.
    pub retained_reason: Option<String>,
}

impl Stream {
    /// Append one record: lock, repair a torn tail, take `seq = last + 1`,
    /// write the whole line, `sync_data`. An entry over the cap or a busy
    /// lock writes nothing.
    pub fn append(
        &self,
        kind: &str,
        key: Option<&str>,
        data: Value,
    ) -> Result<Appended, JournalError> {
        let lock = self.lock()?;
        let segments = self.prepare(&lock)?;
        self.append_locked(&lock, &segments, kind, key, data)
    }

    /// Append unless a record with the same `(kind, key)` already exists in
    /// the newest [`super::JournalOptions::dedup_window_segments`] segments.
    /// That window is the de-duplication bound: a replay older than it is
    /// appended again, and consumers de-duplicate on [`super::ReadRecord::key`].
    pub fn append_if_new(
        &self,
        kind: &str,
        key: &str,
        data: Value,
    ) -> Result<AppendOutcome, JournalError> {
        let lock = self.lock()?;
        let segments = self.prepare(&lock)?;
        let window = segments
            .len()
            .saturating_sub(self.options.dedup_window_segments);
        for segment in &segments[window..] {
            if let Some(seq) = find_key(segment, &self.name, kind, key)? {
                return Ok(AppendOutcome::Duplicate { seq });
            }
        }
        self.append_locked(&lock, &segments, kind, Some(key), data)
            .map(AppendOutcome::Appended)
    }

    /// Start a new segment if the active one has reached the bound, then
    /// delete only segments strictly older than the one holding the oldest
    /// consumer cursor. No cursors, or any unreadable cursor, deletes nothing.
    /// The active segment is never deleted.
    pub fn rotate(&self) -> Result<RotateOutcome, JournalError> {
        let lock = self.lock()?;
        let mut segments = self.prepare(&lock)?;
        let mut outcome = RotateOutcome::default();
        if let Some(active) = segments.last() {
            if std::fs::metadata(&active.path)?.len() >= self.options.max_segment_bytes {
                let first_seq = lock.last_seq(&segments)? + 1;
                let path = self.dir.join(segment::segment_name(first_seq));
                create_segment(&path)?;
                sync_dir(&self.dir)?;
                segments.push(Segment { first_seq, path });
                outcome.rolled = true;
            }
        }
        let cursors = match cursor::all(&self.dir) {
            Ok(cursors) => cursors,
            Err(e) => {
                outcome.retained_reason = Some(format!("unreadable cursor: {e}"));
                return Ok(outcome);
            }
        };
        let Some(oldest) = cursors.iter().map(|(_, p)| p.segment).min() else {
            outcome.retained_reason = Some("no consumer cursors".to_owned());
            return Ok(outcome);
        };
        // The segment holding the oldest position: the newest one that
        // starts at or before it.
        let keep_from = segments
            .iter()
            .rev()
            .find(|s| s.first_seq <= oldest)
            .map_or(0, |s| s.first_seq);
        let active = segments.last().map_or(0, |s| s.first_seq);
        for segment in &segments {
            if segment.first_seq < keep_from && segment.first_seq < active {
                match std::fs::remove_file(&segment.path) {
                    Ok(()) => outcome.deleted.push(segment.first_seq),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if !outcome.deleted.is_empty() {
            sync_dir(&self.dir)?;
        }
        Ok(outcome)
    }

    /// Under the lock: list segments and repair the newest one's tail.
    fn prepare(&self, lock: &StreamLock) -> Result<Vec<Segment>, JournalError> {
        let segments = segment::list(&self.dir)?;
        if let Some(active) = segments.last() {
            lock.repair_tail(&active.path)?;
        }
        Ok(segments)
    }

    fn append_locked(
        &self,
        lock: &StreamLock,
        segments: &[Segment],
        kind: &str,
        key: Option<&str>,
        data: Value,
    ) -> Result<Appended, JournalError> {
        let seq = lock.last_seq(segments)? + 1;
        let envelope = Envelope {
            v: ENVELOPE_MAJOR,
            stream: self.name.clone(),
            seq,
            kind: kind.to_owned(),
            ts: Utc::now(),
            writer: WriterIdentity::current(),
            key: key.map(str::to_owned),
            data,
        };
        let bytes = encode(&envelope)?;
        // Roll to a new segment rather than push the active one past the
        // bound (an empty active segment always takes the record).
        let fits = |s: &Segment| -> io::Result<bool> {
            let len = std::fs::metadata(&s.path)?.len();
            Ok(len == 0 || len + bytes.len() as u64 <= self.options.max_segment_bytes)
        };
        let active = match segments.last() {
            Some(active) if fits(active)? => active.clone(),
            _ => Segment {
                first_seq: seq,
                path: self.dir.join(segment::segment_name(seq)),
            },
        };
        let mut file = create_segment(&active.path)?;
        let offset = file.metadata()?.len();
        if offset == 0 {
            // The first record of a segment: durably link the segment
            // before the record that a reader reaches through it.
            sync_dir(&self.dir)?;
        }
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(Appended {
            seq,
            position: Position {
                segment: active.first_seq,
                offset,
                seq,
            },
        })
    }
}

fn create_segment(path: &std::path::Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Stream one segment for a `(kind, key)` record; its seq if present.
fn find_key(
    segment: &Segment,
    stream: &str,
    kind: &str,
    key: &str,
) -> Result<Option<u64>, JournalError> {
    let mut reader = match LineReader::open(&segment.path, 0) {
        Ok(reader) => reader,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    while let Some((_, line)) = reader.next_line()? {
        if let Line::Complete(bytes) = line {
            if let Decoded::Record(envelope) = decode(&bytes) {
                if envelope.stream == stream
                    && envelope.kind == kind
                    && envelope.key.as_deref() == Some(key)
                {
                    return Ok(Some(envelope.seq));
                }
            }
        }
    }
    Ok(None)
}
