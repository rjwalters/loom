//! `journal verify`: a read-only structural check of a journal root.
//!
//! It reports corrupt lines, `seq` gaps and regressions, unknown envelope
//! majors, torn tails and unreadable directories, segments or cursors.
//! **There is no false green**: the report is `ok` only when the root and
//! every stream were readable end to end and nothing anomalous was found.
//! A missing, unreadable or empty root is an error, never "0 problems".
//!
//! A torn tail is reported but does not fail verification on its own: it is
//! either an append in flight (verify takes no lock) or a crash the next
//! writer repairs, and it holds no complete record. Concern-specific
//! comparisons (journal vs. legacy store) belong to later slices.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::envelope::{decode, Decoded};
use super::segment::{self, Line, LineReader};
use super::{cursor, validate_name};

#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    /// Streams to check; empty means every stream under the root.
    pub streams: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StreamReport {
    pub stream: String,
    pub segments: usize,
    pub records: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub corrupt_lines: u64,
    pub unknown_major: u64,
    /// The distinct unknown majors seen, ascending.
    pub unknown_majors: Vec<u32>,
    pub gaps: u64,
    pub regressions: u64,
    /// A `seq` regression (reuse after a restore) forces a projection rebuild.
    pub needs_rebuild: bool,
    pub torn_tail: bool,
    pub cursors: usize,
    pub errors: Vec<String>,
}

impl StreamReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
            && self.corrupt_lines == 0
            && self.unknown_major == 0
            && self.gaps == 0
            && self.regressions == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    pub root: PathBuf,
    pub ok: bool,
    pub streams: Vec<StreamReport>,
    pub errors: Vec<String>,
}

/// Verify the journal at `root`. Never writes.
#[must_use]
pub fn verify(root: &Path, options: &VerifyOptions) -> VerifyReport {
    let mut errors = Vec::new();
    let mut streams = Vec::new();
    match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => errors.push(format!("{} is not a directory", root.display())),
        Err(e) => errors.push(format!("cannot read journal root {}: {e}", root.display())),
    }
    if errors.is_empty() {
        let names = if options.streams.is_empty() {
            discover(root).unwrap_or_else(|e| {
                errors.push(format!("cannot list journal root {}: {e}", root.display()));
                Vec::new()
            })
        } else {
            options.streams.clone()
        };
        if names.is_empty() && errors.is_empty() {
            errors.push(format!("no journal streams under {}", root.display()));
        }
        for name in names {
            streams.push(verify_stream(root, &name));
        }
    }
    let ok = errors.is_empty() && !streams.is_empty() && streams.iter().all(StreamReport::is_clean);
    VerifyReport {
        root: root.to_path_buf(),
        ok,
        streams,
        errors,
    }
}

fn discover(root: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if validate_name(name).is_ok() {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    Ok(names)
}

fn verify_stream(root: &Path, name: &str) -> StreamReport {
    let mut report = StreamReport {
        stream: name.to_owned(),
        ..StreamReport::default()
    };
    if let Err(e) = validate_name(name) {
        report.errors.push(e.to_string());
        return report;
    }
    let dir = root.join(name);
    if let Err(e) = std::fs::read_dir(&dir) {
        report
            .errors
            .push(format!("cannot read stream {}: {e}", dir.display()));
        return report;
    }
    let segments = match segment::list(&dir) {
        Ok(segments) => segments,
        Err(e) => {
            report.errors.push(format!("cannot list segments: {e}"));
            return report;
        }
    };
    report.segments = segments.len();
    let mut previous: Option<u64> = None;
    for (index, seg) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        if let Err(e) = scan_segment(&seg.path, name, is_last, &mut previous, &mut report) {
            report
                .errors
                .push(format!("cannot read segment {}: {e}", seg.path.display()));
        }
    }
    report.last_seq = previous;
    match cursor::all(&dir) {
        Ok(cursors) => report.cursors = cursors.len(),
        Err(e) => report.errors.push(format!("unreadable cursor: {e}")),
    }
    report
}

fn scan_segment(
    path: &Path,
    stream: &str,
    is_last: bool,
    previous: &mut Option<u64>,
    report: &mut StreamReport,
) -> io::Result<()> {
    let mut reader = LineReader::open(path, 0)?;
    while let Some((_, line)) = reader.next_line()? {
        let decoded = match line {
            Line::Complete(bytes) => decode(&bytes),
            Line::Oversize => {
                report.corrupt_lines += 1;
                continue;
            }
            Line::Torn if is_last => {
                report.torn_tail = true;
                continue;
            }
            Line::Torn => {
                report.corrupt_lines += 1;
                continue;
            }
        };
        match &decoded {
            Decoded::Record(envelope) if envelope.stream == stream => report.records += 1,
            Decoded::UnknownMajor { v, .. } => {
                report.unknown_major += 1;
                if let Err(at) = report.unknown_majors.binary_search(v) {
                    report.unknown_majors.insert(at, *v);
                }
            }
            Decoded::Record(_) | Decoded::Corrupt => {
                report.corrupt_lines += 1;
                continue;
            }
        }
        let Some(seq) = decoded.seq() else {
            continue;
        };
        report.first_seq.get_or_insert(seq);
        match *previous {
            Some(p) if seq == p + 1 => {}
            Some(p) if seq > p => report.gaps += 1,
            Some(_) => {
                report.regressions += 1;
                report.needs_rebuild = true;
            }
            None => {}
        }
        *previous = Some(seq);
    }
    Ok(())
}
