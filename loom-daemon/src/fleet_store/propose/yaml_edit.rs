//! A minimal, line-targeted editor for `fleet.yml`, the store's hand-edited
//! source (#10905): set or remove one value by path, and leave every other
//! line byte-for-byte alone, so a reviewer's diff is exactly the change and
//! the file's comments survive.
//!
//! Deliberately not a YAML library. It reads only what it needs: block
//! mappings (`key:` lines at one indentation), block lists of records
//! (`- name: x`), and where each `key:`'s value ends (its nested lines, or a
//! `|`/`>` block scalar's text). It writes values in block style, with
//! strings double-quoted (JSON escapes) unless they are plainly safe.
//! fleet-gitops' `scripts/render.py` stays the only full parser: every edit
//! is rendered, and so checked, before a proposal is opened
//! ([`super::source`]).
//!
//! An inline comment on a scalar line that is replaced is kept on the new
//! line. Comments inside a value that is replaced wholesale (a list, or a
//! mapping that became a scalar) go with it; a removed key's own lines go,
//! the comments above it stay.

use anyhow::{bail, Result};
use serde_json::{Map, Value};

/// One step of a path into the document.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Seg<'a> {
    /// A mapping key.
    Key(&'a str),
    /// The record of a block list whose `field` is the string `value`.
    Item {
        /// The record's key to match on.
        field: &'a str,
        /// Its value.
        value: &'a str,
    },
}

/// A document being edited.
pub(crate) struct Doc {
    lines: Vec<String>,
    trailing_newline: bool,
}

/// A block mapping's children: `lines[start..end]`, keys at column `indent`.
#[derive(Debug, Clone, Copy)]
struct Container {
    start: usize,
    end: usize,
    indent: usize,
    /// The `key:` line whose value this is, and that key's column.
    header: Option<(usize, usize)>,
    /// For a list record: its `- ` line, whose first key follows the dash.
    dash: Option<usize>,
}

/// One `key: value` line and the lines its value spans.
#[derive(Debug, Clone)]
struct Entry {
    line: usize,
    /// Everything before the key: indentation, and `- ` on a record's first
    /// line.
    prefix: String,
    /// The key as written (quotes included).
    key_text: String,
    key: String,
    /// The key's column.
    col: usize,
    /// The inline value, comment stripped, trimmed.
    value: String,
    /// The inline comment, with the whitespace before it; empty if none.
    comment: String,
    /// One past the value's last line.
    end: usize,
}

/// What a `key:`'s value is.
enum Child {
    Map(Container),
    List {
        start: usize,
        end: usize,
        dash: usize,
    },
    /// `key:` with nothing (or only comments) under it.
    Empty,
    /// `key: {}`.
    EmptyFlow,
    /// Any other inline value.
    Inline,
}

enum Walk {
    Found(Container, Entry),
    /// `path[at]` is not in `within`; `flow` is the `key: {}` line to open
    /// first when inserting.
    Missing {
        within: Container,
        at: usize,
        flow: Option<Entry>,
    },
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn is_dash(line: &str) -> bool {
    let t = line.trim_start();
    t == "-" || t.starts_with("- ")
}

/// Index one past the closing quote of the quoted scalar `s` starts with.
fn quoted_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote == b'"' => i += 2,
            b'\'' if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') => i += 2,
            c if c == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The text of a scalar as written: a quoted one decoded, a plain one as is.
fn unquote(s: &str) -> Option<String> {
    match s.as_bytes().first() {
        Some(b'"') => serde_json::from_str(s).ok(),
        Some(b'\'') => Some(s[1..s.len().checked_sub(1)?].replace("''", "'")),
        _ => Some(s.to_string()),
    }
}

/// `(key as written, key, rest after the colon)` of `s`, a line from the key
/// on; `None` when it is not a `key:` line.
fn split_key(s: &str) -> Option<(&str, String, &str)> {
    let colon = if s.starts_with('"') || s.starts_with('\'') {
        let end = quoted_end(s)?;
        s[end..].starts_with(':').then_some(end)?
    } else {
        let mut at = None;
        for (i, c) in s.char_indices() {
            if c == ':'
                && s[i + 1..]
                    .chars()
                    .next()
                    .is_none_or(|n| n == ' ' || n == '\t')
            {
                at = Some(i);
                break;
            }
        }
        at?
    };
    let key_text = s[..colon].trim_end();
    if key_text.is_empty() || key_text.starts_with(['#', '-', '[', '{', '&', '*', '!', '|', '>']) {
        return None;
    }
    let rest = &s[colon + 1..];
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    Some((key_text, unquote(key_text)?, rest))
}

/// `(value, comment)` of the text after a key's colon.
fn split_comment(rest: &str) -> (String, String) {
    let trimmed = rest.trim_start();
    let skip = rest.len() - trimmed.len();
    let from = if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        quoted_end(trimmed).map_or(rest.len(), |e| skip + e)
    } else {
        skip
    };
    let hash = rest[from..]
        .char_indices()
        .find(|&(i, c)| c == '#' && (from + i == 0 || rest[..from + i].ends_with([' ', '\t'])))
        .map(|(i, _)| from + i);
    match hash {
        Some(h) => {
            let start = rest[..h].trim_end().len();
            (rest[..start].trim().to_string(), rest[start..].to_string())
        }
        None => (rest.trim().to_string(), String::new()),
    }
}

/// Whether `s` can be written as a plain scalar (or key) and read back as
/// the same string by any YAML reader, `render.py`'s strict one included.
fn plain_ok(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let reserved = ["y", "n", "yes", "no", "on", "off", "true", "false", "null"];
    (first.is_ascii_alphabetic() || first == '_' || first == '/')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./@+".contains(c))
        && !reserved.contains(&s.to_ascii_lowercase().as_str())
}

/// A string as a scalar: plain when safe, else double-quoted with JSON
/// escapes (a valid YAML double-quoted scalar).
fn string_scalar(s: &str) -> String {
    if plain_ok(s) {
        s.to_string()
    } else {
        Value::String(s.to_string()).to_string()
    }
}

/// A non-container value (or an empty container) as an inline scalar.
fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => string_scalar(s),
        Value::Object(_) => "{}".to_string(),
        Value::Array(_) => "[]".to_string(),
        other => other.to_string(),
    }
}

/// `key: value` in block style, the key at column `col` after `prefix`.
fn emit(prefix: &str, key_text: &str, value: &Value, col: usize) -> Result<Vec<String>> {
    let pad = " ".repeat(col + 2);
    let mut out = Vec::new();
    match value {
        Value::Object(m) if !m.is_empty() => {
            out.push(format!("{prefix}{key_text}:"));
            for (k, v) in m {
                out.extend(emit(&pad, &string_scalar(k), v, col + 2)?);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            out.push(format!("{prefix}{key_text}:"));
            for item in items {
                match item {
                    Value::Object(m) if !m.is_empty() => {
                        for (n, (k, v)) in m.iter().enumerate() {
                            let p = if n == 0 {
                                format!("{pad}- ")
                            } else {
                                " ".repeat(col + 4)
                            };
                            out.extend(emit(&p, &string_scalar(k), v, col + 4)?);
                        }
                    }
                    Value::Array(inner) if !inner.is_empty() => {
                        bail!("`{key_text}` is a list of lists; edit it in fleet.yml by hand")
                    }
                    v => out.push(format!("{pad}- {}", scalar(v))),
                }
            }
        }
        v => out.push(format!("{prefix}{key_text}: {}", scalar(v))),
    }
    Ok(out)
}

/// A path for messages: `repos[name=app].fleet_priority`.
fn label(path: &[Seg<'_>]) -> String {
    let mut out = String::new();
    for s in path {
        match s {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Item { field, value } => out.push_str(&format!("[{field}={value}]")),
        }
    }
    out
}

impl Doc {
    /// Start editing `text`.
    pub(crate) fn new(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_string).collect(),
            trailing_newline: text.is_empty() || text.ends_with('\n'),
        }
    }

    /// The edited text.
    pub(crate) fn finish(self) -> String {
        let mut out = self.lines.join("\n");
        if self.trailing_newline && !out.is_empty() {
            out.push('\n');
        }
        out
    }

    fn root(&self) -> Container {
        Container {
            start: 0,
            end: self.lines.len(),
            indent: 0,
            header: None,
            dash: None,
        }
    }

    /// The entry on line `i` with its key at column `col`.
    fn entry_at(&self, i: usize, col: usize, limit: usize) -> Option<Entry> {
        let line = &self.lines[i];
        let (key_text, key, rest) = split_key(line.get(col..)?)?;
        let (value, comment) = split_comment(rest);
        let block_scalar = value.starts_with('|') || value.starts_with('>');
        let mut last = i;
        for (j, l) in self.lines.iter().enumerate().take(limit).skip(i + 1) {
            if l.trim().is_empty() {
                continue;
            }
            let comment_line = l.trim_start().starts_with('#');
            let ind = indent_of(l);
            if ind > col {
                if block_scalar || !comment_line {
                    last = j;
                }
            } else if comment_line && !block_scalar {
                // A comment, whatever its indentation, ends nothing.
            } else if ind == col && value.is_empty() && !block_scalar && is_dash(l) {
                // A list written at its key's own indentation.
                last = j;
            } else {
                break;
            }
        }
        Some(Entry {
            line: i,
            prefix: line[..col].to_string(),
            key_text: key_text.to_string(),
            key,
            col,
            value,
            comment,
            end: last + 1,
        })
    }

    fn entries(&self, c: Container) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut i = c.start;
        while i < c.end {
            let l = &self.lines[i];
            let candidate = if c.dash == Some(i) {
                true
            } else {
                !is_blank_or_comment(l) && indent_of(l) == c.indent && !is_dash(l)
            };
            match candidate
                .then(|| self.entry_at(i, c.indent, c.end))
                .flatten()
            {
                Some(e) => {
                    i = e.end.max(i + 1);
                    out.push(e);
                }
                None => i += 1,
            }
        }
        out
    }

    fn find(&self, c: Container, key: &str) -> Option<Entry> {
        self.entries(c).into_iter().find(|e| e.key == key)
    }

    fn child(&self, e: &Entry) -> Child {
        if e.value == "{}" {
            return Child::EmptyFlow;
        }
        if !e.value.is_empty() {
            return Child::Inline;
        }
        let first = (e.line + 1..e.end).find(|&j| !is_blank_or_comment(&self.lines[j]));
        match first {
            None => Child::Empty,
            Some(j) if is_dash(&self.lines[j]) => Child::List {
                start: e.line + 1,
                end: e.end,
                dash: indent_of(&self.lines[j]),
            },
            Some(j) => Child::Map(Container {
                start: e.line + 1,
                end: e.end,
                indent: indent_of(&self.lines[j]),
                header: Some((e.line, e.col)),
                dash: None,
            }),
        }
    }

    /// The records of a block list.
    fn items(&self, start: usize, end: usize, dash: usize) -> Vec<Container> {
        let heads: Vec<usize> = (start..end)
            .filter(|&j| {
                let l = &self.lines[j];
                !is_blank_or_comment(l) && indent_of(l) == dash && is_dash(l)
            })
            .collect();
        heads
            .iter()
            .enumerate()
            .map(|(n, &h)| {
                let after = &self.lines[h][dash + 1..];
                Container {
                    start: h,
                    end: heads.get(n + 1).copied().unwrap_or(end),
                    indent: dash + 1 + (after.len() - after.trim_start().len()),
                    header: None,
                    dash: Some(h),
                }
            })
            .collect()
    }

    fn walk(&self, path: &[Seg<'_>]) -> Result<Walk> {
        if !matches!(path.last(), Some(Seg::Key(_))) {
            bail!("an edit path must end at a key");
        }
        let mut within = self.root();
        let mut flow = None;
        let mut i = 0;
        while i < path.len() {
            let Seg::Key(key) = path[i] else {
                bail!("`{}`: a list record must follow a key", label(&path[..=i]));
            };
            let Some(entry) = self.find(within, key) else {
                return Ok(Walk::Missing {
                    within,
                    at: i,
                    flow,
                });
            };
            if i + 1 == path.len() {
                return Ok(Walk::Found(within, entry));
            }
            let here = label(&path[..=i]);
            let child = self.child(&entry);
            if let Seg::Item { field, value } = path[i + 1] {
                let Child::List { start, end, dash } = child else {
                    bail!("`{here}` in fleet.yml is not a block list");
                };
                within = self
                    .items(start, end, dash)
                    .into_iter()
                    .find(|item| {
                        self.find(*item, field)
                            .is_some_and(|e| unquote(&e.value).as_deref() == Some(value))
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("fleet.yml has no {here}[] record with {field} `{value}`")
                    })?;
                flow = None;
                i += 2;
                continue;
            }
            let open = Container {
                start: entry.line + 1,
                end: entry.line + 1,
                indent: entry.col + 2,
                header: Some((entry.line, entry.col)),
                dash: None,
            };
            (within, flow) = match child {
                Child::Map(m) => (m, None),
                Child::Empty => (open, None),
                Child::EmptyFlow => (open, Some(entry)),
                Child::List { .. } | Child::Inline => {
                    bail!("`{here}` in fleet.yml is not a block mapping; edit it by hand")
                }
            };
            i += 1;
        }
        bail!("empty edit path")
    }

    /// Whether `path` names a value in the document.
    pub(crate) fn has(&self, path: &[Seg<'_>]) -> Result<bool> {
        Ok(matches!(self.walk(path)?, Walk::Found(..)))
    }

    /// Set `path` to `value`, replacing its current value or inserting it
    /// (and any missing mapping above it) after its last sibling.
    pub(crate) fn set(&mut self, path: &[Seg<'_>], value: &Value) -> Result<()> {
        match self.walk(path)? {
            Walk::Found(_, e) => {
                let mut new = emit(&e.prefix, &e.key_text, value, e.col)?;
                let inline_scalar =
                    !e.value.is_empty() && !e.value.starts_with('|') && !e.value.starts_with('>');
                if new.len() == 1 && inline_scalar {
                    new[0].push_str(&e.comment);
                }
                self.lines.splice(e.line..e.end, new);
            }
            Walk::Missing { within, at, flow } => {
                let mut nested = value.clone();
                for seg in path[at + 1..].iter().rev() {
                    let Seg::Key(k) = seg else {
                        bail!("fleet.yml has no `{}`", label(&path[..=at]));
                    };
                    let mut m = Map::new();
                    m.insert((*k).to_string(), nested);
                    nested = Value::Object(m);
                }
                let Seg::Key(key) = path[at] else {
                    bail!("`{}`: a list record must follow a key", label(path));
                };
                if let Some(f) = flow {
                    self.lines[f.line] = format!("{}{}:{}", f.prefix, f.key_text, f.comment);
                }
                let at_line = self.entries(within).last().map_or(within.start, |e| e.end);
                let pad = " ".repeat(within.indent);
                let new = emit(&pad, &string_scalar(key), &nested, within.indent)?;
                self.lines.splice(at_line..at_line, new);
            }
        }
        Ok(())
    }

    /// Remove `path` and its value's lines; `false` when it is not there. A
    /// mapping left empty is written `{}`, which still reads as a mapping.
    pub(crate) fn remove(&mut self, path: &[Seg<'_>]) -> Result<bool> {
        let Walk::Found(within, e) = self.walk(path)? else {
            return Ok(false);
        };
        if within.dash == Some(e.line) {
            bail!("`{}` opens a list record; edit it in fleet.yml by hand", label(path));
        }
        self.lines.drain(e.line..e.end);
        let left = Container {
            end: within.end - (e.end - e.line),
            ..within
        };
        if let Some((h, col)) = within.header {
            if self.entries(left).is_empty() {
                if let Some(head) = self.entry_at(h, col, h + 1) {
                    self.lines[h] =
                        format!("{}{}: {{}}{}", head.prefix, head.key_text, head.comment);
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
#[path = "tests/yaml_edit_tests.rs"]
mod tests;
