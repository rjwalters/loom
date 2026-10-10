//! The streaming stream reader. It never writes or repairs: it opens
//! segments read-only, skips and counts corrupt / oversize / unknown-major
//! lines, and stops at the last complete record of the newest segment.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

use super::envelope::{decode, Decoded, Envelope};
use super::segment::{self, Line, LineReader, Segment};
use super::{JournalError, Position};

/// One record and where it sits. Commit [`ReadRecord::next`] to a cursor to
/// acknowledge it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRecord {
    pub envelope: Envelope,
    /// Where the record starts.
    pub position: Position,
    /// Just past the record: the cursor value acknowledging it.
    pub next: Position,
}

impl ReadRecord {
    /// The natural key, so a consumer can de-duplicate a replayed record.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.envelope.key.as_deref()
    }
}

/// What a reader skipped. Never fatal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    pub records: u64,
    /// Bad JSON, over the entry cap, wrong stream, or an unterminated line
    /// in a non-final segment.
    pub corrupt_lines: u64,
    /// Lines of an envelope major this build does not read.
    pub unknown_major: u64,
    /// The newest segment ends in an incomplete line (a crash's torn tail or
    /// an append in progress). The reader stopped before it.
    pub torn_tail: bool,
}

pub struct StreamReader {
    name: String,
    dir: PathBuf,
    from: Position,
    queue: Option<VecDeque<Segment>>,
    current: Option<(u64, LineReader, bool)>,
    stats: ReadStats,
    fused: bool,
}

impl StreamReader {
    pub(crate) fn new(name: &str, dir: &Path, from: Position) -> Self {
        StreamReader {
            name: name.to_owned(),
            dir: dir.to_path_buf(),
            from,
            queue: None,
            current: None,
            stats: ReadStats::default(),
            fused: false,
        }
    }

    #[must_use]
    pub fn stats(&self) -> ReadStats {
        self.stats
    }

    fn step(&mut self) -> Result<Option<ReadRecord>, JournalError> {
        if self.queue.is_none() {
            let segments = segment::list(&self.dir)?;
            self.queue = Some(
                segments
                    .into_iter()
                    .filter(|s| s.first_seq >= self.from.segment)
                    .collect(),
            );
        }
        loop {
            if self.current.is_none() {
                let Some(queue) = self.queue.as_mut() else {
                    return Ok(None);
                };
                let Some(next) = queue.pop_front() else {
                    return Ok(None);
                };
                let offset = if next.first_seq == self.from.segment {
                    self.from.offset
                } else {
                    0
                };
                let reader = match LineReader::open(&next.path, offset) {
                    Ok(reader) => reader,
                    // Rotated away since listing: it was already acknowledged.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                self.current = Some((next.first_seq, reader, queue.is_empty()));
            }
            let Some((segment, reader, is_last)) = self.current.as_mut() else {
                continue;
            };
            let (segment, is_last) = (*segment, *is_last);
            let Some((start, line)) = reader.next_line()? else {
                self.current = None;
                continue;
            };
            match line {
                Line::Complete(bytes) => match decode(&bytes) {
                    Decoded::Record(envelope) if envelope.stream == self.name => {
                        self.stats.records += 1;
                        let seq = envelope.seq;
                        return Ok(Some(ReadRecord {
                            envelope: *envelope,
                            position: Position {
                                segment,
                                offset: start,
                                seq: seq.saturating_sub(1),
                            },
                            next: Position {
                                segment,
                                offset: reader.offset(),
                                seq,
                            },
                        }));
                    }
                    Decoded::UnknownMajor { .. } => self.stats.unknown_major += 1,
                    Decoded::Record(_) | Decoded::Corrupt => self.stats.corrupt_lines += 1,
                },
                Line::Oversize => self.stats.corrupt_lines += 1,
                Line::Torn if is_last => {
                    self.stats.torn_tail = true;
                    self.current = None;
                }
                Line::Torn => {
                    self.stats.corrupt_lines += 1;
                    self.current = None;
                }
            }
        }
    }
}

impl Iterator for StreamReader {
    type Item = Result<ReadRecord, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.fused {
            return None;
        }
        match self.step() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.fused = true;
                None
            }
            Err(e) => {
                self.fused = true;
                Some(Err(e))
            }
        }
    }
}
