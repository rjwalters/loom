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
//! Fenced blocks (```` ``` ```` / `~~~`, opened and closed with at most 3
//! columns of indent -- a 4-space-indented fence is indented code, not a
//! fence; closed by a same-character fence at least as long; an unclosed fence
//! runs to the end), indented code blocks (not interrupting a paragraph),
//! blockquotes (blanked wholesale, conservatively, including lazy
//! continuation), and inline code spans, which may cross a newline within one
//! paragraph. An HTML comment that starts outside code claims its own line up
//! to `-->` first, so a backtick inside a real record's `reason="…"` cannot
//! hide that record.

/// `text` with every byte inside a code region (or blockquote) replaced by a
/// space (newlines kept). Byte length and every offset outside code are
/// unchanged, so a match found in the result indexes the original text.
#[must_use]
pub(super) fn blank(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(u8, usize)> = None;
    // Consecutive plain-paragraph lines, so an inline span may cross a newline.
    let mut para = String::new();
    // Inside a blockquote (until a blank line).
    let mut in_quote = false;
    for line in text.split_inclusive('\n') {
        let indent = indent_width(line);
        let trimmed = line.trim_start_matches([' ', '\t']);
        let run = |c: u8| trimmed.bytes().take_while(|b| *b == c).count();
        if let Some((c, n)) = fence {
            // A closing fence is indented at most 3 columns.
            let r = run(c);
            if indent < 4 && r >= n && trimmed[r..].trim().is_empty() {
                fence = None;
            }
            out.push_str(&spaces(line));
            continue;
        }
        let blank_line = trimmed.trim().is_empty();
        if blank_line {
            flush(&mut para, &mut out);
            in_quote = false;
            // A blank line inside an indented code run does not end it.
            out.push_str(&spaces(line));
            continue;
        }
        let opener = if indent < 4 {
            [b'`', b'~']
                .into_iter()
                .map(|c| (c, run(c)))
                .find(|&(_, r)| r >= 3)
                // A backtick fence's info string cannot contain a backtick.
                .filter(|&(c, n)| c == b'~' || !trimmed[n..].contains('`'))
        } else {
            None
        };
        // Blockquote: its content (markers and fences alike) is blanked
        // conservatively, including lazy continuation lines, but never a line
        // that starts a new HTML comment block at column 0..3.
        let quoted = indent < 4 && trimmed.starts_with('>');
        let lazy = in_quote && para.is_empty() && opener.is_none() && !trimmed.starts_with("<!--");
        if quoted || lazy {
            flush(&mut para, &mut out);
            in_quote = true;
            out.push_str(&spaces(line));
            continue;
        }
        in_quote = false;
        if let Some(o) = opener {
            flush(&mut para, &mut out);
            fence = Some(o);
            out.push_str(&spaces(line));
            continue;
        }
        // Indented code cannot interrupt a paragraph.
        if indent >= 4 && para.is_empty() {
            out.push_str(&spaces(line));
            continue;
        }
        para.push_str(line);
    }
    flush(&mut para, &mut out);
    out
}

/// Columns of leading whitespace (a tab counts as 4).
fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn flush(para: &mut String, out: &mut String) {
    if !para.is_empty() {
        out.push_str(&blank_inline(para));
        para.clear();
    }
}

/// Every byte of `s` as a space, newlines kept.
fn spaces(s: &str) -> String {
    s.bytes()
        .map(|b| if b == b'\n' { '\n' } else { ' ' })
        .collect()
}

/// One paragraph (one or more lines) with its inline code spans blanked. A backtick run of length `n`
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
            // A comment is claimed only up to the end of its own line.
            let eol = line[i..].find('\n').map_or(bytes.len(), |e| i + e);
            i = line[i + 4..eol].find("-->").map_or(eol, |e| i + 4 + e + 3);
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
