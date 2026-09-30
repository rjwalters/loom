//! Minimal line-oriented helpers for format-preserving edits to the small,
//! hand-written block-YAML files a fleet store keeps (`repos.yml`,
//! `fleet/state.yml`). This is deliberately not a general YAML editor — it
//! only knows how to find a `key:` mapping header at a given indentation and
//! its child block, and to set/insert flat `key: value` scalar lines inside
//! that block, which is exactly the shape [`super::state_edit`] and
//! [`super::priority_edit`] touch. Every other line in the file is left
//! byte-for-byte alone, which is what keeps the diff minimal (#9599).

pub(crate) fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

pub(crate) fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// The line range of one `key:` mapping's nested block. The `key:` header
/// line itself is not part of it — every edit this module makes is inside
/// the block, so the header index is not carried around.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Block {
    /// First line of the block's content (one past the `key:` header).
    pub start: usize,
    /// One past the block's last content line.
    pub end: usize,
    /// Indentation of the block's direct children — read off its first
    /// child when the block is non-empty, `header_indent + 2` otherwise.
    pub child_indent: usize,
}

/// Find a `key:` header at exactly `indent`, searching
/// `lines[search_start..search_end]` only.
pub(crate) fn find_block(
    lines: &[String],
    search_start: usize,
    search_end: usize,
    indent: usize,
    key: &str,
) -> Option<Block> {
    let header_text = format!("{key}:");
    let header = (search_start..search_end).find(|&i| {
        !is_blank_or_comment(&lines[i])
            && indent_of(&lines[i]) == indent
            && lines[i].trim() == header_text
    })?;
    let mut end = header + 1;
    while end < search_end {
        let l = &lines[end];
        if !is_blank_or_comment(l) && indent_of(l) <= indent {
            break;
        }
        end += 1;
    }
    let child_indent = (header + 1..end)
        .find(|&i| !is_blank_or_comment(&lines[i]))
        .map_or(indent + 2, |i| indent_of(&lines[i]));
    Some(Block {
        start: header + 1,
        end,
        child_indent,
    })
}

/// Find a top-level (`indent`-level) `key:` mapping, appending an empty one
/// at the end of the document (`lines`) when it does not exist yet.
pub(crate) fn find_or_append_top_block(lines: &mut Vec<String>, indent: usize, key: &str) -> Block {
    if let Some(b) = find_block(lines, 0, lines.len(), indent, key) {
        return b;
    }
    let header = lines.len();
    lines.push(format!("{}{key}:", " ".repeat(indent)));
    Block {
        start: header + 1,
        end: header + 1,
        child_indent: indent + 2,
    }
}

/// Insert a new, empty `key:` mapping header as the last entry of `block`,
/// growing `block.end` to include it, and return the new (empty) child block.
pub(crate) fn append_mapping(lines: &mut Vec<String>, block: &mut Block, key: &str) -> Block {
    let header_indent = block.child_indent;
    let header = block.end;
    lines.insert(header, format!("{}{key}:", " ".repeat(header_indent)));
    block.end += 1;
    Block {
        start: header + 1,
        end: header + 1,
        child_indent: header_indent + 2,
    }
}

/// Set a flat `key: value` scalar inside `block`, at `block.child_indent`:
/// replaces the line if the key is already set at that level, otherwise
/// appends one as the block's last line (growing `block.end`).
pub(crate) fn set_scalar(lines: &mut Vec<String>, block: &mut Block, key: &str, value: &str) {
    let existing = (block.start..block.end).find(|&i| {
        !is_blank_or_comment(&lines[i]) && indent_of(&lines[i]) == block.child_indent && {
            let t = lines[i].trim_start();
            t == format!("{key}:") || t.starts_with(&format!("{key}: "))
        }
    });
    let new_line = format!("{}{key}: {value}", " ".repeat(block.child_indent));
    match existing {
        Some(i) => lines[i] = new_line,
        None => {
            lines.insert(block.end, new_line);
            block.end += 1;
        }
    }
}

/// [`set_scalar`] with `value` double-quoted (and its own quotes/backslashes
/// escaped) — for free-text operator input (`reason`, `by`) that may contain
/// YAML-significant characters a plain scalar cannot carry safely.
pub(crate) fn set_scalar_quoted(
    lines: &mut Vec<String>,
    block: &mut Block,
    key: &str,
    value: &str,
) {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    set_scalar(lines, block, key, &quoted);
}

#[cfg(test)]
#[path = "tests/block_tests.rs"]
mod tests;
