//! A strict, dependency-free reader for the block-YAML subset a fleet store's
//! `repos.yml` and `fleet/state.yml` are written in, producing a
//! [`serde_json::Value`].
//!
//! # Why not a YAML crate
//!
//! `loom-daemon` carries no YAML dependency, and the obvious candidate
//! (`serde_yaml`) is archived and flagged unmaintained, which `cargo audit
//! --deny warnings` in `security.yml` would refuse. The store files are
//! hand-edited, operator-owned manifests in a deliberately flat shape, so a
//! small reader is enough — and it is written to **fail closed**: anything
//! outside the subset is an error, never a best-effort guess. `repos.yml`
//! carries the fleet's firewall inputs, and a reader that silently mis-parsed
//! a record would narrow or widen the fleet without anyone noticing.
//!
//! # The subset
//!
//! - Block mappings (`key: value`) and block sequences (`- item`, including
//!   `- key: value` items that start a mapping), nested by space indentation.
//!   A sequence may sit at the same indentation as its parent key.
//! - Plain, `'single'` and `"double"`-quoted scalars; `true`/`false`, `null`/`~`
//!   and decimal integers/floats resolve to JSON types, everything else is a
//!   string (so `2026-09-30T01:30Z` stays a string).
//! - Flow sequences and mappings on one line (`[a, b]`, `{k: v}`).
//! - Literal (`|`) and folded (`>`) block scalars, with `-`/`+` chomping.
//! - `#` comments, full-line or after whitespace; a leading `---`.
//!
//! Refused: anchors, aliases, tags, directives, multiple documents, multi-line
//! plain scalars, tabs in indentation, and duplicate mapping keys.

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Number, Value};

/// Parse `text` into a JSON value. An empty document is `null`.
pub fn parse(text: &str) -> Result<Value> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // A leading document marker is harmless; anything else document-level is not.
    let mut seen_doc_start = false;
    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim_end();
        if t.starts_with('%') {
            bail!("line {}: YAML directives are not supported", i + 1);
        }
        if t == "---" || t.starts_with("--- ") {
            if seen_doc_start || !is_blank_before(i, text) {
                bail!("line {}: multiple YAML documents are not supported", i + 1);
            }
            seen_doc_start = true;
            line.clear();
        } else if t == "..." {
            line.clear();
        }
    }
    let mut p = Parser { lines, pos: 0 };
    let Some((indent, _)) = p.peek()? else {
        return Ok(Value::Null);
    };
    let value = p.node(indent)?;
    if let Some((_, _)) = p.peek()? {
        bail!("line {}: unexpected content after the document", p.pos + 1);
    }
    Ok(value)
}

/// Whether every line before line index `i` of `text` is blank or a comment.
fn is_blank_before(i: usize, text: &str) -> bool {
    text.lines()
        .take(i)
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
}

struct Parser {
    lines: Vec<String>,
    pos: usize,
}

impl Parser {
    /// The next significant line as `(indent, content)`, skipping blank and
    /// comment-only lines. Does not consume it.
    fn peek(&mut self) -> Result<Option<(usize, String)>> {
        while self.pos < self.lines.len() {
            let line = &self.lines[self.pos];
            let content = line.trim();
            if content.is_empty() || content.starts_with('#') {
                self.pos += 1;
                continue;
            }
            let indent = line.len() - line.trim_start_matches(' ').len();
            if line[indent..].starts_with('\t') {
                bail!("line {}: tabs are not allowed in indentation", self.pos + 1);
            }
            return Ok(Some((indent, line.trim_end().to_string())));
        }
        Ok(None)
    }

    fn err(&self, what: &str) -> anyhow::Error {
        anyhow!("line {}: {what}", self.pos + 1)
    }

    /// Parse the node whose first line is the next significant line, which
    /// must be indented exactly `indent`.
    fn node(&mut self, indent: usize) -> Result<Value> {
        let (_, line) = self.peek()?.ok_or_else(|| self.err("expected a value"))?;
        let content = &line[indent..];
        if is_seq_item(content) {
            self.sequence(indent)
        } else if mapping_colon(content).is_some() {
            self.mapping(indent)
        } else {
            let (value, rest) = scalar_or_flow(content).map_err(|e| self.err(&e))?;
            ensure_only_comment(rest).map_err(|e| self.err(&e))?;
            self.pos += 1;
            Ok(value)
        }
    }

    fn mapping(&mut self, indent: usize) -> Result<Value> {
        let mut map = Map::new();
        while let Some((ind, line)) = self.peek()? {
            if ind < indent {
                break;
            }
            if ind > indent {
                return Err(self.err("unexpected indentation"));
            }
            let content = &line[indent..];
            if is_seq_item(content) {
                break;
            }
            let colon = mapping_colon(content).ok_or_else(|| self.err("expected `key: value`"))?;
            let key = parse_key(&content[..colon]).map_err(|e| self.err(&e))?;
            if map.contains_key(&key) {
                return Err(self.err(&format!("duplicate key `{key}`")));
            }
            let rest = content[colon + 1..].trim_start();
            let value = self.value_after_indicator(indent, rest, true)?;
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }

    fn sequence(&mut self, indent: usize) -> Result<Value> {
        let mut items = Vec::new();
        while let Some((ind, line)) = self.peek()? {
            if ind < indent {
                break;
            }
            if ind > indent {
                return Err(self.err("unexpected indentation"));
            }
            let content = &line[indent..];
            if !is_seq_item(content) {
                break;
            }
            let after_dash = &content[1..];
            let rest = after_dash.trim_start();
            if !rest.is_empty()
                && !rest.starts_with('#')
                && (is_seq_item(rest) || mapping_colon(rest).is_some())
            {
                // `- key: v` / `- - x`: blank out the dash and re-read the line
                // as a nested node at the column its content starts in.
                let col = indent + 1 + (after_dash.len() - rest.len());
                let mut rewritten = " ".repeat(col);
                rewritten.push_str(rest);
                self.lines[self.pos] = rewritten;
                items.push(self.node(col)?);
            } else {
                items.push(self.value_after_indicator(indent, rest, false)?);
            }
        }
        Ok(Value::Array(items))
    }

    /// The value following a `key:` or `- ` on the current line. `rest` is the
    /// line's remainder; an empty (or comment-only) remainder means the value
    /// is the nested block below, or null. `in_mapping` allows a sequence at
    /// the parent's own indentation (`key:\n- a`).
    fn value_after_indicator(
        &mut self,
        indent: usize,
        rest: &str,
        in_mapping: bool,
    ) -> Result<Value> {
        if rest.starts_with('|') || rest.starts_with('>') {
            let header = rest.to_string();
            self.pos += 1;
            return self.block_scalar(indent, &header);
        }
        if !rest.is_empty() && !rest.starts_with('#') {
            let (value, tail) = scalar_or_flow(rest).map_err(|e| self.err(&e))?;
            ensure_only_comment(tail).map_err(|e| self.err(&e))?;
            self.pos += 1;
            return Ok(value);
        }
        self.pos += 1;
        match self.peek()? {
            Some((ind, _)) if ind > indent => self.node(ind),
            Some((ind, line)) if in_mapping && ind == indent && is_seq_item(&line[ind..]) => {
                self.sequence(ind)
            }
            _ => Ok(Value::Null),
        }
    }

    /// A `|` / `>` block scalar whose header (already consumed) was on a line
    /// indented `parent`.
    fn block_scalar(&mut self, parent: usize, header: &str) -> Result<Value> {
        let spec = header[1..].split('#').next().unwrap_or("").trim();
        let literal = header.starts_with('|');
        let mut chomp = 'c';
        let mut explicit: Option<usize> = None;
        for ch in spec.chars() {
            match ch {
                '-' | '+' => chomp = ch,
                d if d.is_ascii_digit() && d != '0' => {
                    explicit = Some(parent + d.to_digit(10).unwrap_or(1) as usize);
                }
                _ => return Err(self.err("malformed block scalar header")),
            }
        }
        let mut raw: Vec<String> = Vec::new();
        let mut content_indent = explicit;
        while self.pos < self.lines.len() {
            let line = &self.lines[self.pos];
            if line.trim().is_empty() {
                raw.push(String::new());
                self.pos += 1;
                continue;
            }
            let ind = line.len() - line.trim_start_matches(' ').len();
            let want = *content_indent.get_or_insert(ind);
            if ind <= parent || ind < want {
                break;
            }
            raw.push(line[want..].trim_end().to_string());
            self.pos += 1;
        }
        // Trailing blank lines belong to chomping, not content.
        let body_len = raw.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
        let trailing = raw.len() - body_len;
        raw.truncate(body_len);
        let mut out = if literal {
            raw.join("\n")
        } else {
            let mut s = String::new();
            for (i, l) in raw.iter().enumerate() {
                if l.is_empty() {
                    s.push('\n');
                } else {
                    if i > 0 && !raw[i - 1].is_empty() {
                        s.push(' ');
                    }
                    s.push_str(l);
                }
            }
            s
        };
        match chomp {
            '-' => {}
            '+' => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&"\n".repeat(trailing));
            }
            _ => {
                if !out.is_empty() {
                    out.push('\n');
                }
            }
        }
        Ok(Value::String(out))
    }
}

fn is_seq_item(content: &str) -> bool {
    content == "-" || content.starts_with("- ")
}

/// Byte index of the `:` that makes `content` a `key: value` line, or `None`.
/// The key is either quoted or plain; a plain key ends at the first `:` that
/// is followed by a space or the end of the line.
fn mapping_colon(content: &str) -> Option<usize> {
    let first = content.chars().next()?;
    if matches!(first, '[' | '{' | '#' | '|' | '>') {
        return None;
    }
    let start = if first == '"' || first == '\'' {
        let (_, len) = quoted(content).ok()?;
        len
    } else {
        0
    };
    let bytes = content.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ') {
            return Some(i);
        }
        if start > 0 && bytes[i] != b' ' {
            // A quoted key must be followed directly by its colon.
            return None;
        }
        if bytes[i] == b' ' && bytes.get(i + 1) == Some(&b'#') {
            return None;
        }
        i += 1;
    }
    None
}

fn parse_key(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.starts_with('"') || raw.starts_with('\'') {
        let (v, _) = quoted(raw)?;
        return Ok(v);
    }
    if raw.is_empty() {
        return Err("empty mapping key".to_string());
    }
    reject_unsupported(raw)?;
    Ok(raw.to_string())
}

fn reject_unsupported(plain: &str) -> Result<(), String> {
    match plain.chars().next() {
        Some('&') => Err("anchors are not supported".to_string()),
        Some('*') => Err("aliases are not supported".to_string()),
        Some('!') => Err("tags are not supported".to_string()),
        Some('@' | '`') => Err(format!("a plain scalar may not start with `{plain}`")),
        _ => Ok(()),
    }
}

/// Anything after a value on its line must be whitespace or a comment.
fn ensure_only_comment(rest: &str) -> Result<(), String> {
    let t = rest.trim_start();
    if t.is_empty() || t.starts_with('#') {
        Ok(())
    } else {
        Err(format!("unexpected trailing content `{t}`"))
    }
}

/// Parse one inline value (quoted, flow collection or plain) from the start of
/// `s`, returning it and the unconsumed remainder of the line.
fn scalar_or_flow(s: &str) -> Result<(Value, &str), String> {
    match s.chars().next() {
        Some('"' | '\'') => {
            let (v, len) = quoted(s)?;
            Ok((Value::String(v), &s[len..]))
        }
        Some('[' | '{') => {
            let mut f = Flow { s, i: 0 };
            let v = f.value()?;
            Ok((v, &s[f.i..]))
        }
        _ => {
            // A plain scalar ends at a ` #` comment.
            let end = s.find(" #").unwrap_or(s.len());
            let plain = s[..end].trim();
            reject_unsupported(plain)?;
            Ok((resolve_plain(plain), &s[end..]))
        }
    }
}

/// A quoted scalar at the start of `s`: its value and its byte length.
fn quoted(s: &str) -> Result<(String, usize), String> {
    let q = s.chars().next().ok_or("expected a quote")?;
    let mut out = String::new();
    let mut chars = s.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if c == q {
            if q == '\'' && chars.peek().map(|&(_, n)| n) == Some('\'') {
                chars.next();
                out.push('\'');
                continue;
            }
            return Ok((out, i + 1));
        }
        if q == '"' && c == '\\' {
            let (_, e) = chars.next().ok_or("unterminated escape")?;
            out.push(match e {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                ' ' => ' ',
                'u' => {
                    let hex: String = (0..4)
                        .filter_map(|_| chars.next().map(|(_, h)| h))
                        .collect();
                    u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| format!("bad \\u escape `{hex}`"))?
                }
                other => return Err(format!("unsupported escape `\\{other}`")),
            });
            continue;
        }
        out.push(c);
    }
    Err("unterminated quoted string".to_string())
}

/// YAML 1.2 core-schema resolution of a plain scalar.
fn resolve_plain(plain: &str) -> Value {
    match plain {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    let digits = plain.strip_prefix(['-', '+']).unwrap_or(plain);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = plain.parse::<i64>() {
            return Value::Number(n.into());
        }
    }
    let is_float = digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && digits.bytes().filter(|&b| b == b'.').count() == 1
        && digits.bytes().any(|b| b.is_ascii_digit());
    if is_float {
        if let Some(n) = plain.parse::<f64>().ok().and_then(Number::from_f64) {
            return Value::Number(n);
        }
    }
    Value::String(plain.to_string())
}

/// One-line flow collections: `[a, "b", {k: v}]`.
struct Flow<'a> {
    s: &'a str,
    i: usize,
}

impl Flow<'_> {
    fn skip_ws(&mut self) {
        while self.s[self.i..].starts_with(' ') {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.s[self.i..].starts_with(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        let rest = &self.s[self.i..];
        match rest.chars().next() {
            Some('[') => {
                self.i += 1;
                let mut items = Vec::new();
                if self.eat(']') {
                    return Ok(Value::Array(items));
                }
                loop {
                    items.push(self.value()?);
                    if self.eat(']') {
                        return Ok(Value::Array(items));
                    }
                    if !self.eat(',') {
                        return Err("expected `,` or `]` in flow sequence".to_string());
                    }
                    if self.eat(']') {
                        return Ok(Value::Array(items));
                    }
                }
            }
            Some('{') => {
                self.i += 1;
                let mut map = Map::new();
                if self.eat('}') {
                    return Ok(Value::Object(map));
                }
                loop {
                    let key = match self.value()? {
                        Value::String(k) => k,
                        other => other.to_string(),
                    };
                    if !self.eat(':') {
                        return Err("expected `:` in flow mapping".to_string());
                    }
                    let v = self.value()?;
                    if map.insert(key.clone(), v).is_some() {
                        return Err(format!("duplicate key `{key}`"));
                    }
                    if self.eat('}') {
                        return Ok(Value::Object(map));
                    }
                    if !self.eat(',') {
                        return Err("expected `,` or `}` in flow mapping".to_string());
                    }
                }
            }
            Some('"' | '\'') => {
                let (v, len) = quoted(rest)?;
                self.i += len;
                Ok(Value::String(v))
            }
            Some(_) => {
                let end = rest.find([',', ']', '}', ':']).unwrap_or(rest.len());
                let plain = rest[..end].trim();
                if plain.is_empty() {
                    return Err("empty flow entry".to_string());
                }
                reject_unsupported(plain)?;
                self.i += end;
                Ok(resolve_plain(plain))
            }
            None => Err("unterminated flow collection".to_string()),
        }
    }
}

#[cfg(test)]
#[path = "tests/yaml_tests.rs"]
mod tests;
