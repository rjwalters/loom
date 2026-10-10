//! Segment files: naming, listing, a bounded streaming line reader and
//! tail-only probes. **Nothing here loads a segment whole** (the #11045 rule):
//! reads are one line at a time, or a bounded block from the tail.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::MAX_ENTRY_BYTES;

const SUFFIX: &str = ".jsonl";
const TAIL_BLOCK: u64 = 8 * 1024;

/// A segment is named by the `seq` of its first record, zero-padded to ten
/// digits: `0000000001.jsonl`.
#[must_use]
pub(crate) fn segment_name(first_seq: u64) -> String {
    format!("{first_seq:010}{SUFFIX}")
}

pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SUFFIX)?;
    if digits.len() < 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub first_seq: u64,
    pub path: PathBuf,
}

/// The stream's segments, oldest first. A missing stream directory has none;
/// any other listing failure surfaces.
pub(crate) fn list(dir: &Path) -> io::Result<Vec<Segment>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut segments = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(first_seq) = entry.file_name().to_str().and_then(parse_segment_name) {
            segments.push(Segment {
                first_seq,
                path: entry.path(),
            });
        }
    }
    segments.sort_by_key(|s| s.first_seq);
    Ok(segments)
}

/// One line as read from a segment.
#[derive(Debug)]
pub(crate) enum Line {
    /// A newline-terminated line within the entry cap (newline included).
    Complete(Vec<u8>),
    /// A newline-terminated line over the entry cap; skipped unbuffered.
    Oversize,
    /// Bytes after the last newline: an append that has not finished (or a
    /// crash's torn tail). The reader does not advance past it.
    Torn,
}

/// Streams the lines of one segment from a byte offset, buffering at most
/// one capped entry.
pub(crate) struct LineReader {
    inner: BufReader<File>,
    offset: u64,
    done: bool,
}

impl LineReader {
    pub(crate) fn open(path: &Path, offset: u64) -> io::Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(LineReader {
            inner: BufReader::new(file),
            offset,
            done: false,
        })
    }

    /// Offset just past the last line returned as complete or oversize.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// The next line and its start offset, or `None` at end of file. After a
    /// [`Line::Torn`] the reader is exhausted.
    pub(crate) fn next_line(&mut self) -> io::Result<Option<(u64, Line)>> {
        if self.done {
            return Ok(None);
        }
        let start = self.offset;
        let mut buf = Vec::new();
        let n = (&mut self.inner)
            .take(MAX_ENTRY_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            self.done = true;
            return Ok(None);
        }
        if buf.last() == Some(&b'\n') {
            self.offset += n as u64;
            if n > MAX_ENTRY_BYTES {
                return Ok(Some((start, Line::Oversize)));
            }
            return Ok(Some((start, Line::Complete(buf))));
        }
        if n <= MAX_ENTRY_BYTES {
            // EOF before a newline.
            self.done = true;
            return Ok(Some((start, Line::Torn)));
        }
        // Over the cap and not yet terminated: skip to the newline unbuffered.
        let (skipped, terminated) = skip_to_newline(&mut self.inner)?;
        if terminated {
            self.offset += n as u64 + skipped;
            Ok(Some((start, Line::Oversize)))
        } else {
            self.done = true;
            Ok(Some((start, Line::Torn)))
        }
    }
}

fn skip_to_newline(reader: &mut BufReader<File>) -> io::Result<(u64, bool)> {
    let mut skipped = 0_u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((skipped, false));
        }
        if let Some(i) = available.iter().position(|&b| b == b'\n') {
            reader.consume(i + 1);
            return Ok((skipped + i as u64 + 1, true));
        }
        let len = available.len();
        reader.consume(len);
        skipped += len as u64;
    }
}

/// Offset just past the last newline of `file` (0 if it has none), found by
/// reading backwards in bounded blocks.
pub(crate) fn complete_len(file: &mut File) -> io::Result<u64> {
    let mut end = file.metadata()?.len();
    let mut block = vec![0_u8; TAIL_BLOCK as usize];
    while end > 0 {
        let start = end.saturating_sub(TAIL_BLOCK);
        let len = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block[..len])?;
        if let Some(i) = block[..len].iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// The last complete line ending at `end` (an offset just past a newline),
/// read from the tail only. `None` when there is no complete line or the
/// line is over the entry cap.
pub(crate) fn last_line(file: &mut File, end: u64) -> io::Result<Option<Vec<u8>>> {
    if end == 0 {
        return Ok(None);
    }
    let window = end.min(MAX_ENTRY_BYTES as u64 + 1);
    let start = end - window;
    let mut buf = vec![0_u8; window as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf)?;
    // `buf` ends with the line's own newline; find the one before it.
    let body = &buf[..buf.len() - 1];
    match body.iter().rposition(|&b| b == b'\n') {
        Some(i) => Ok(Some(buf[i + 1..].to_vec())),
        None if start == 0 => Ok(Some(buf)),
        None => Ok(None),
    }
}
