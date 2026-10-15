//! The Loom-managed block of `.github/labels.yml`, refreshed by a resync
//! (#11033).
//!
//! The file is co-owned (#4187): Loom's labels sit between
//! [`LOOM_LABELS_START`] and [`LOOM_LABELS_END`], and a consumer may keep its
//! own labels above or below. The installer splices the block; until #11033
//! no resync did, so a label upstream retired (`loom:urgent`) stayed declared
//! in every consumer, and the forge drift check re-created it after
//! `labels migrate --apply` had deleted it.
//!
//! The rule is the same as `refresh_labels_block()` in
//! `defaults/scripts/resync-installed.sh`; keep the two equal.
//!
//! * One BEGIN and one END marker, BEGIN first: the bytes from the start of
//!   the BEGIN line to the end of the END line (its newline excluded) are
//!   replaced by the same range of the payload's file. Every other byte is
//!   kept.
//! * No marker at all: the block is appended after a blank line.
//! * No file: the payload's file is written.
//! * Anything else is [`Shape::Malformed`] and nothing is written: a second
//!   BEGIN or END, END before BEGIN, a lone marker, or a line that looks like
//!   a marker and is not exactly one.
//!
//! A marker is a line that is exactly the marker text, with trailing spaces,
//! tabs or a CR allowed. Unlike the installer's `merge_labels_block`, nothing
//! outside the markers is touched, not even a legacy duplicate of a Loom label:
//! a resync runs unattended and only ever rewrites what Loom owns.
//!
//! Only the file changes. No live forge label is ever deleted here; that is
//! `loom-daemon labels migrate --apply`.

use std::fs;
use std::io;
use std::path::Path;

use super::super::scaffolding::{LOOM_LABELS_END, LOOM_LABELS_START};

/// What a labels file's markers say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    /// No marker at all.
    None,
    /// One well-formed block: the byte range from the start of the BEGIN line
    /// to the end of the END line, its newline excluded.
    Block { start: usize, end: usize },
    /// Markers a resync must not guess at, with the reason.
    Malformed(String),
}

/// `line` is exactly `marker`, allowing trailing spaces, tabs or a CR.
fn is_exactly(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker)
        .is_some_and(|rest| rest.chars().all(|c| matches!(c, ' ' | '\t' | '\r')))
}

/// `line` reads like a marker (`#`, then `BEGIN` or `END`, `LOOM`, `LABELS`
/// separated by spaces or tabs, any indentation) without being one. The shell
/// twin's awk pattern is `^[ \t]*#[ \t]*(BEGIN|END)[ \t]+LOOM[ \t]+LABELS`.
fn looks_like_marker(line: &str) -> bool {
    let blank = |c: char| c == ' ' || c == '\t';
    let Some(rest) = line.trim_start_matches(blank).strip_prefix('#') else {
        return false;
    };
    let rest = rest.trim_start_matches(blank);
    let Some(rest) = rest
        .strip_prefix("BEGIN")
        .or_else(|| rest.strip_prefix("END"))
    else {
        return false;
    };
    let words = |s: &str, word: &str| -> Option<usize> {
        let trimmed = s.trim_start_matches(blank);
        (trimmed.len() < s.len() && trimmed.starts_with(word))
            .then(|| s.len() - trimmed.len() + word.len())
    };
    let Some(n) = words(rest, "LOOM") else {
        return false;
    };
    words(&rest[n..], "LABELS").is_some()
}

/// Classify `text`'s markers. The order of the checks is the shell twin's: a
/// near-marker line first, then the counts, then the order.
pub(crate) fn classify(text: &str) -> Shape {
    let (mut begins, mut ends) = (Vec::new(), Vec::new());
    let mut near = None;
    let mut offset = 0;
    for (index, line) in text.split('\n').enumerate() {
        let line_end = offset + line.len();
        if is_exactly(line, LOOM_LABELS_START) {
            begins.push((index + 1, offset));
        } else if is_exactly(line, LOOM_LABELS_END) {
            ends.push((index + 1, line_end));
        } else if looks_like_marker(line) {
            near = Some(index + 1);
        }
        offset = line_end + 1;
    }
    if let Some(line) = near {
        return Shape::Malformed(format!("line {line} looks like a marker but is not exactly one"));
    }
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Shape::None,
        ([(b_line, start)], [(e_line, end)]) => {
            if e_line < b_line {
                Shape::Malformed(format!(
                    "END marker (line {e_line}) comes before BEGIN (line {b_line})"
                ))
            } else {
                Shape::Block {
                    start: *start,
                    end: *end,
                }
            }
        }
        (b, e) => Shape::Malformed(format!(
            "{} BEGIN and {} END marker(s), expected one of each",
            b.len(),
            e.len()
        )),
    }
}

/// The new content of a labels file whose current content is `existing`
/// (`None`: no file), given the payload's `source`.
///
/// # Errors
/// The reason, when `existing` (or `source`) has malformed markers or
/// `source` has no block. Nothing is to be written then.
pub(crate) fn refresh(existing: Option<&str>, source: &str) -> Result<String, String> {
    let Shape::Block { start, end } = classify(source) else {
        return Err("the payload's labels.yml has no usable Loom-managed block".to_string());
    };
    let block = &source[start..end];
    let Some(existing) = existing else {
        return Ok(source.to_string());
    };
    match classify(existing) {
        Shape::Block { start, end } => {
            Ok(format!("{}{block}{}", &existing[..start], &existing[end..]))
        }
        Shape::None => {
            let mut out = existing.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(block);
            out.push('\n');
            Ok(out)
        }
        Shape::Malformed(why) => Err(why),
    }
}

/// What staging the labels file did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Staged {
    /// The file was written (it may still equal what was there).
    Wrote,
    /// Nothing to do: the payload ships no usable labels file.
    NoSource,
    /// The workspace's file cannot be read (not UTF-8, say). Left as is.
    Unusable(String),
    /// The workspace's markers are malformed. Left as is.
    Malformed(String),
}

/// Refresh the labels file at `dst` (in staging) from `src` (the payload).
///
/// # Errors
/// The payload's file exists and cannot be read, or staging cannot be
/// written.
pub(super) fn stage(src: &Path, dst: &Path) -> Result<Staged, String> {
    let source = match fs::read_to_string(src) {
        Ok(source) => source,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Staged::NoSource),
        Err(e) => return Err(format!("read {}: {e}", src.display())),
    };
    if !matches!(classify(&source), Shape::Block { .. }) {
        // A payload defect, the same in every repo: say it, write nothing.
        log::warn!(
            "resync: the payload's .github/labels.yml has no usable Loom-managed block; no \
             repo's label block is refreshed"
        );
        return Ok(Staged::NoSource);
    }
    let existing = match fs::read_to_string(dst) {
        Ok(existing) => Some(existing),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Ok(Staged::Unusable(e.to_string())),
    };
    let new = match refresh(existing.as_deref(), &source) {
        Ok(new) => new,
        Err(why) => return Ok(Staged::Malformed(why)),
    };
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(dst, new).map_err(|e| format!("write {}: {e}", dst.display()))?;
    Ok(Staged::Wrote)
}

#[cfg(test)]
#[path = "labels_block_tests.rs"]
mod tests;
