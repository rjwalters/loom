//! Code regions of a Markdown body, blanked so a park marker *quoted* inside
//! one is never read as a declaration (#10837).
//!
//! # The incident
//!
//! #10837's body quotes three loom-ui park records verbatim inside a fenced
//! block, as evidence. An operator later parked #10837 itself with no record
//! of its own. The release pass (#10556) read the quoted records as #10837's
//! declared blockers, found local #1689/#1692/#1693 closed, and released the
//! operator's hold (restoring `loom:issue`, so the issue was re-dispatched).
//!
//! An HTML comment inside a code block or code span renders as visible text,
//! not as a comment, so it is not a park record by the grammar's own premise
//! (an *invisible* sentinel). [`blank`] makes the parser agree.
//!
//! # Scope
//!
//! Fenced blocks (```` ``` ```` / `~~~`, closed by a same-character fence at
//! least as long; an unclosed fence runs to the end, as in CommonMark) and
//! single-line inline code spans. An HTML comment that starts outside code
//! claims its own line up to `-->` first, so a backtick inside a real record's
//! `reason="…"` cannot hide that record. Indented code blocks and fences
//! inside blockquotes are not recognised.

/// `text` with every byte inside a code region replaced by a space (newlines
/// kept). Byte length and every offset outside code are unchanged, so a match
/// found in the result indexes the original text exactly.
#[must_use]
pub(super) fn blank(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(u8, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        let run = |c: u8| trimmed.bytes().take_while(|b| *b == c).count();
        if let Some((c, n)) = fence {
            let r = run(c);
            if r >= n && trimmed[r..].trim().is_empty() {
                fence = None;
            }
            out.push_str(&spaces(line));
            continue;
        }
        let opener = [b'`', b'~']
            .into_iter()
            .map(|c| (c, run(c)))
            .find(|&(_, r)| r >= 3);
        match opener {
            // A backtick fence's info string cannot contain a backtick
            // (CommonMark); such a line is inline code, not a fence.
            Some((c, n)) if c == b'~' || !trimmed[n..].contains('`') => {
                fence = Some((c, n));
                out.push_str(&spaces(line));
            }
            _ => out.push_str(&blank_inline(line)),
        }
    }
    out
}

/// Every byte of `s` as a space, newlines kept.
fn spaces(s: &str) -> String {
    s.bytes()
        .map(|b| if b == b'\n' { '\n' } else { ' ' })
        .collect()
}

/// One line with its inline code spans blanked. A backtick run of length `n`
/// opens a span closed by the next run of exactly `n`; an unmatched run is
/// literal.
fn blank_inline(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let (mut i, mut last) = (0, 0);
    while i < bytes.len() {
        // Byte-wise: `i` steps over non-ASCII text one byte at a time, so it
        // is not always a char boundary. Every slice below starts at an ASCII
        // byte (`<` or a backtick), which always is.
        if bytes[i..].starts_with(b"<!--") {
            i = line[i + 4..]
                .find("-->")
                .map_or(bytes.len(), |e| i + 4 + e + 3);
            continue;
        }
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let n = bytes[i..].iter().take_while(|b| **b == b'`').count();
        let mut j = i + n;
        let mut close = None;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let m = bytes[j..].iter().take_while(|b| **b == b'`').count();
                if m == n {
                    close = Some(j + m);
                    break;
                }
                j += m;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out.push_str(&line[last..i]);
                out.push_str(&spaces(&line[i..end]));
                (i, last) = (end, end);
            }
            None => i += n,
        }
    }
    out.push_str(&line[last..]);
    out
}
