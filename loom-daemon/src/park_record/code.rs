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
//! runs to the end), including fences opened inside a list item
//! (bulleted, numbered, or nested: the item's content column replaces column
//! 0, and a line dedented past it ends the fence), indented code blocks (not interrupting a paragraph),
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
    // An open fence: its character, its length, and the content column of the
    // list item it sits in (0 outside a list).
    let mut fence: Option<(u8, usize, usize)> = None;
    // Content column of the enclosing list item, 0 outside a list.
    let mut list_col = 0;
    // Consecutive plain-paragraph lines, so an inline span may cross a newline.
    let mut para = String::new();
    // Inside a blockquote (until a blank line).
    let mut in_quote = false;
    for line in text.split_inclusive('\n') {
        if let Some((c, n, fcol)) = fence {
            if line.trim().is_empty() {
                out.push_str(&spaces(line));
                continue;
            }
            // A line dedented past its list item ends the item, and the fence.
            if indent_width(line) >= fcol {
                // A closing fence is indented at most 3 columns past the item.
                let trimmed = line.trim_start_matches([' ', '\t']);
                let r = trimmed.bytes().take_while(|b| *b == c).count();
                if indent_width(line) - fcol < 4 && r >= n && trimmed[r..].trim().is_empty() {
                    fence = None;
                }
                out.push_str(&spaces(line));
                continue;
            }
            fence = None;
            list_col = 0;
        }
        // A lone `-` under a top-level paragraph is a setext underline, not an
        // empty list item (which cannot interrupt a paragraph).
        if !para.is_empty() && list_col == 0 && indent_width(line) < 4 && line.trim() == "-" {
            flush(&mut para, &mut out);
            out.push_str(&spaces(line));
            continue;
        }
        let (col, rest, item) = strip_lists(line, list_col);
        let indent = indent_width(rest);
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let run = |c: u8| trimmed.bytes().take_while(|b| *b == c).count();
        let blank_line = trimmed.trim().is_empty();
        if blank_line {
            flush(&mut para, &mut out);
            in_quote = false;
            if item {
                list_col = col;
            }
            // A blank line inside an indented code run does not end it.
            out.push_str(&spaces(line));
            continue;
        }
        if item {
            flush(&mut para, &mut out);
        }
        if col > 0 {
            list_col = col;
        } else if para.is_empty() {
            list_col = 0;
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
            if col == 0 {
                list_col = 0;
            }
            out.push_str(&spaces(line));
            continue;
        }
        in_quote = false;
        if let Some((c, n)) = opener {
            flush(&mut para, &mut out);
            fence = Some((c, n, col));
            out.push_str(&spaces(line));
            continue;
        }
        // Indented code cannot interrupt a paragraph.
        if indent >= 4 && para.is_empty() {
            out.push_str(&spaces(line));
            continue;
        }
        // A heading, thematic break, or setext underline is not an open
        // paragraph: it closes the one it ends, so indented code may follow.
        let closes = ends_paragraph(trimmed, !para.is_empty());
        para.push_str(line);
        if closes {
            flush(&mut para, &mut out);
        }
    }
    flush(&mut para, &mut out);
    out
}

/// Whether `trimmed` (a non-blank line, indent removed) is a single-line block
/// that leaves no paragraph open: an ATX heading, a thematic break, or a
/// setext underline under an open paragraph.
fn ends_paragraph(trimmed: &str, para_open: bool) -> bool {
    let t = trimmed.trim_end();
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes)
        && (t.len() == hashes || matches!(t.as_bytes()[hashes], b' ' | b'\t'))
    {
        return true;
    }
    let Some(c) = t.bytes().next() else {
        return false;
    };
    let marks = t.bytes().filter(|b| *b == c).count();
    let only = t.bytes().all(|b| b == c || b == b' ' || b == b'\t');
    match c {
        // Under an open paragraph a hyphen run of any length (no inner spaces)
        // is a setext underline; otherwise it must be a 3+ thematic break.
        b'-' => only && (marks >= 3 || (para_open && !t.contains([' ', '\t']))),
        b'*' | b'_' => only && marks >= 3,
        b'=' => only && para_open && !t.contains([' ', '\t']),
        _ => false,
    }
}

/// Width of the list marker at the start of `t` (`-`, `*`, `+`, or up to nine
/// digits then `.` / `)`), when whitespace or the end of the line follows it.
fn marker_width(t: &str) -> Option<usize> {
    let b = t.as_bytes();
    let w = match b.first()? {
        b'-' | b'*' | b'+' => 1,
        b'0'..=b'9' => {
            let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if d > 9 || !matches!(b.get(d), Some(b'.' | b')')) {
                return None;
            }
            d + 1
        }
        _ => return None,
    };
    matches!(b.get(w), None | Some(b' ' | b'\t' | b'\r' | b'\n')).then_some(w)
}

/// Peels list-item containers off the front of `line`: first the continuation
/// indent of the enclosing item (`list_col`), then any markers opening further
/// (nested) items. Returns the content column, the rest of the line, and
/// whether a marker opened an item on this line.
fn strip_lists(line: &str, list_col: usize) -> (usize, &str, bool) {
    let (mut col, mut rest, mut item) = (0, line, false);
    if list_col > 0 && indent_width(line) >= list_col {
        (col, rest) = (list_col, skip_cols(line, list_col));
    }
    while indent_width(rest) < 4 {
        let t = rest.trim_start_matches([' ', '\t']);
        let Some(w) = marker_width(t) else { break };
        let after = &t[w..];
        let gap = indent_width(after);
        let content = after.trim_start_matches([' ', '\t']);
        // Five or more spaces after the marker: the content is indented code.
        let pad = if content.trim().is_empty() || gap > 4 {
            1
        } else {
            gap.max(1)
        };
        col += indent_width(rest) + w + pad;
        rest = if pad == gap {
            content
        } else {
            after.get(1..).unwrap_or("")
        };
        item = true;
    }
    (col, rest, item)
}

/// `s` without up to `n` columns of leading whitespace (a tab counts as 4).
fn skip_cols(s: &str, n: usize) -> &str {
    let mut w = 0;
    for (i, c) in s.char_indices() {
        if w >= n || !matches!(c, ' ' | '\t') {
            return &s[i..];
        }
        w += if c == '\t' { 4 } else { 1 };
    }
    ""
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
        // A backslash escapes the next byte, so a run of them pairs off: an odd
        // run escapes a following backtick (a literal, never a span opener); an
        // even run leaves it active.
        if bytes[i] == b'\\' {
            let n = bytes[i..].iter().take_while(|b| **b == b'\\').count();
            i += n;
            if n % 2 == 1 && bytes.get(i) == Some(&b'`') {
                i += 1;
            }
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
