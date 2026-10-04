//! A deliberately small YAML reader for GitHub Actions workflow files.
//!
//! The workspace carries no YAML crate, and the merge-group audit needs only
//! the subset workflow files actually use: block mappings, block sequences
//! (including `- key: value` mapping items), plain / single- / double-quoted
//! scalars, `|` / `>` block scalars, and one-line (or bracket-balanced
//! multi-line) flow sequences and mappings. Anchors, aliases, tags, multi-doc
//! streams and complex keys are not supported — a workflow using them parses
//! to whatever the subset can see, and the audit fails closed on anything it
//! cannot read (an unparseable condition is `Unknown`, never "covered").
//!
//! Every mapping entry records its 1-based source line so findings can name
//! the exact line, and so job blocks can be located for `# merge-group-audit:`
//! marker comments, which this parser otherwise strips like any comment.

/// A parsed YAML value.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Scalar(String),
    Seq(Vec<Node>),
    Map(Vec<Entry>),
}

/// One `key: value` pair of a mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    /// 1-based source line of the key.
    pub line: usize,
    pub value: Node,
}

impl Node {
    /// The value under `key`, when this is a mapping that has it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entries()
            .iter()
            .find(|e| e.key == key)
            .map(|e| &e.value)
    }

    /// The scalar text, when this is a scalar.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// Mapping entries (empty for anything that is not a mapping).
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        match self {
            Node::Map(e) => e,
            _ => &[],
        }
    }

    /// Sequence items (empty for anything that is not a sequence).
    #[must_use]
    pub fn items(&self) -> &[Node] {
        match self {
            Node::Seq(i) => i,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone)]
struct Line {
    indent: usize,
    /// Content after the indent, comment-stripped and right-trimmed.
    text: String,
    /// 1-based line number.
    no: usize,
}

struct Parser<'a> {
    raw: Vec<&'a str>,
    /// `None` for blank and comment-only lines.
    lines: Vec<Option<Line>>,
    pos: usize,
}

/// Parse one workflow document.
///
/// # Errors
///
/// When the document's top level is not a mapping, or indentation is
/// inconsistent in a way the subset cannot recover from.
pub fn parse(src: &str) -> Result<Node, String> {
    let raw: Vec<&str> = src.lines().collect();
    let lines = raw
        .iter()
        .enumerate()
        .map(|(i, l)| significant(l, i + 1))
        .collect();
    let mut p = Parser { raw, lines, pos: 0 };
    let Some(first) = p.peek() else {
        return Ok(Node::Map(Vec::new()));
    };
    let indent = first.indent;
    let node = p.block(indent)?;
    if let Some(rest) = p.peek() {
        return Err(format!(
            "line {}: unexpected content at indent {} (expected {indent})",
            rest.no, rest.indent
        ));
    }
    if !matches!(node, Node::Map(_)) {
        return Err("the workflow's top level is not a mapping".to_string());
    }
    Ok(node)
}

/// Classify one raw line: `None` for blank / comment-only.
fn significant(raw: &str, no: usize) -> Option<Line> {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    let body = trimmed.trim_end();
    if body.is_empty() || body.starts_with('#') || body == "---" {
        return None;
    }
    Some(Line {
        indent,
        text: strip_comment(body),
        no,
    })
}

/// Remove a trailing ` # comment`, respecting a quoted scalar that begins the
/// value (after an optional `- ` and an optional `key:`).
fn strip_comment(s: &str) -> String {
    let bytes = s.as_bytes();
    // Skip a leading `- ` sequence marker (possibly repeated).
    let mut start = 0;
    while s[start..].starts_with("- ") {
        start += 2;
        while bytes.get(start) == Some(&b' ') {
            start += 1;
        }
    }
    // Skip a `key:` if the remainder has one.
    let value_start = match split_key(&s[start..]) {
        Some((_, v)) => s.len() - v.len(),
        None => start,
    };
    let mut search_from = value_start;
    if let Some(q @ (b'\'' | b'"')) = bytes.get(value_start).copied() {
        if let Some(end) = closing_quote(&s[value_start..], q) {
            search_from = value_start + end + 1;
        }
    }
    let cut = s[search_from..]
        .char_indices()
        .map(|(i, c)| (search_from + i, c))
        .find(|&(i, c)| c == '#' && i > 0 && s[..i].chars().last().is_some_and(char::is_whitespace))
        .map(|(i, _)| i);
    match cut {
        Some(i) => s[..i].trim_end().to_string(),
        None => s.to_string(),
    }
}

/// Index of the quote closing a scalar that starts with `q` at index 0.
fn closing_quote(s: &str, q: u8) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        if q == b'"' && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Split `key: value` / `key:` at the first mapping colon. `None` when the
/// text is not a mapping entry.
fn split_key(s: &str) -> Option<(String, &str)> {
    let b = s.as_bytes();
    if b.is_empty() || matches!(b[0], b'[' | b'{' | b'|' | b'>') {
        return None;
    }
    if matches!(b[0], b'\'' | b'"') {
        let end = closing_quote(s, b[0])?;
        let after = &s[end + 1..];
        let after_trim = after.trim_start();
        if let Some(rest) = after_trim.strip_prefix(':') {
            if rest.is_empty() || rest.starts_with(' ') {
                return Some((unquote(&s[..=end]), rest.trim_start()));
            }
        }
        return None;
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ') {
            let key = s[..i].trim_end();
            if key.is_empty() || key.contains("${{") {
                return None;
            }
            return Some((key.to_string(), s[i + 1..].trim_start()));
        }
        if b[i] == b' ' && b.get(i + 1) == Some(&b'#') {
            return None;
        }
        i += 1;
    }
    None
}

/// Strip YAML quoting from a scalar.
#[must_use]
pub fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    s.to_string()
}

fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn is_block_scalar_header(v: &str) -> bool {
    let mut c = v.chars();
    matches!(c.next(), Some('|' | '>')) && c.all(|ch| ch == '+' || ch == '-' || ch.is_ascii_digit())
}

impl Parser<'_> {
    fn peek_index(&self) -> Option<usize> {
        (self.pos..self.lines.len()).find(|&i| self.lines[i].is_some())
    }

    fn peek(&self) -> Option<Line> {
        self.peek_index().and_then(|i| self.lines[i].clone())
    }

    /// Parse the block starting at the next significant line, which must sit
    /// at exactly `indent`.
    fn block(&mut self, indent: usize) -> Result<Node, String> {
        match self.peek() {
            Some(l) if l.indent == indent && is_seq_item(&l.text) => self.seq(indent),
            Some(l) if l.indent == indent => self.map(indent),
            _ => Ok(Node::Null),
        }
    }

    fn map(&mut self, indent: usize) -> Result<Node, String> {
        let mut entries = Vec::new();
        while let Some(idx) = self.peek_index() {
            let line = self.lines[idx].clone().unwrap_or_else(|| unreachable!());
            if line.indent != indent || is_seq_item(&line.text) {
                break;
            }
            let Some((key, value)) = split_key(&line.text) else {
                return Err(format!("line {}: expected `key: value` in a mapping", line.no));
            };
            let value = value.to_string();
            self.pos = idx + 1;
            let node = self.value(indent, &value)?;
            entries.push(Entry {
                key,
                line: line.no,
                value: node,
            });
        }
        Ok(Node::Map(entries))
    }

    fn seq(&mut self, indent: usize) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(idx) = self.peek_index() {
            let line = self.lines[idx].clone().unwrap_or_else(|| unreachable!());
            if line.indent != indent || !is_seq_item(&line.text) {
                break;
            }
            let rest = line.text[1..].trim_start().to_string();
            if rest.is_empty() {
                self.pos = idx + 1;
                let child = match self.peek() {
                    Some(n) if n.indent > indent => self.block(n.indent)?,
                    _ => Node::Null,
                };
                items.push(child);
                continue;
            }
            let offset = line.text.len() - rest.len();
            if split_key(&rest).is_some() || is_seq_item(&rest) {
                // `- key: value` — the item is a mapping (or a nested
                // sequence) whose first line sits at indent + offset. Rewrite
                // this line in place so the block parser sees it there.
                self.lines[idx] = Some(Line {
                    indent: indent + offset,
                    text: rest,
                    no: line.no,
                });
                items.push(self.block(indent + offset)?);
                continue;
            }
            self.pos = idx + 1;
            items.push(self.value(indent, &rest)?);
        }
        Ok(Node::Seq(items))
    }

    /// Parse the value text following a key (or a scalar sequence item) whose
    /// owner sits at `indent`.
    fn value(&mut self, indent: usize, v: &str) -> Result<Node, String> {
        if v.is_empty() {
            return match self.peek() {
                Some(n) if n.indent > indent => self.block(n.indent),
                // A sequence may sit at the same indent as its mapping key.
                Some(n) if n.indent == indent && is_seq_item(&n.text) => self.seq(indent),
                _ => Ok(Node::Null),
            };
        }
        if is_block_scalar_header(v) {
            return Ok(Node::Scalar(self.block_scalar(indent, v)));
        }
        if v.starts_with('[') || v.starts_with('{') {
            let text = self.balanced_flow(v);
            return Ok(parse_flow(&text));
        }
        if v.starts_with('\'') || v.starts_with('"') {
            return Ok(Node::Scalar(unquote(v)));
        }
        // Plain scalar, possibly continued on more-indented lines.
        let mut text = v.to_string();
        while let Some(n) = self.peek() {
            if n.indent <= indent {
                break;
            }
            text.push(' ');
            text.push_str(n.text.trim());
            self.pos = self.peek_index().map_or(self.pos, |i| i + 1);
        }
        Ok(Node::Scalar(text))
    }

    /// Consume a `|` / `>` block scalar's body from the raw lines.
    fn block_scalar(&mut self, indent: usize, header: &str) -> String {
        let folded = header.starts_with('>');
        let strip = header.contains('-');
        let mut body: Vec<String> = Vec::new();
        let mut content_indent: Option<usize> = None;
        while self.pos < self.raw.len() {
            let raw = self.raw[self.pos];
            let trimmed = raw.trim_start_matches(' ');
            let ind = raw.len() - trimmed.len();
            if trimmed.trim().is_empty() {
                body.push(String::new());
                self.pos += 1;
                continue;
            }
            if ind <= indent {
                break;
            }
            let ci = *content_indent.get_or_insert(ind);
            if ind < ci {
                break;
            }
            body.push(raw[ci..].trim_end().to_string());
            self.pos += 1;
        }
        while body.last().is_some_and(String::is_empty) {
            body.pop();
        }
        let mut text = if folded {
            body.iter()
                .filter(|l| !l.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            body.join("\n")
        };
        if !strip {
            text.push('\n');
        }
        text
    }

    /// Extend a flow collection across lines until its brackets balance.
    fn balanced_flow(&mut self, first: &str) -> String {
        let mut text = first.to_string();
        while depth(&text) > 0 && self.pos < self.raw.len() {
            if let Some(l) = significant(self.raw[self.pos], self.pos + 1) {
                text.push(' ');
                text.push_str(&l.text);
            }
            self.pos += 1;
        }
        text
    }
}

/// Bracket depth of a flow fragment, ignoring brackets in quotes.
fn depth(s: &str) -> i32 {
    let mut d = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '[' | '{') => d += 1,
            (None, ']' | '}') => d -= 1,
            _ => {}
        }
    }
    d
}

/// Split a flow body on top-level commas.
fn split_top(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut d = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), _) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, '[' | '{') => {
                d += 1;
                cur.push(c);
            }
            (None, ']' | '}') => {
                d -= 1;
                cur.push(c);
            }
            (None, ',') if d == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    parts.into_iter().map(|p| p.trim().to_string()).collect()
}

fn parse_flow(s: &str) -> Node {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return Node::Seq(split_top(inner).iter().map(|p| parse_flow(p)).collect());
    }
    if let Some(inner) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let entries = split_top(inner)
            .iter()
            .filter_map(|p| {
                split_key(p).map(|(k, v)| Entry {
                    key: k,
                    line: 0,
                    value: if v.is_empty() {
                        Node::Null
                    } else {
                        parse_flow(v)
                    },
                })
            })
            .collect();
        return Node::Map(entries);
    }
    Node::Scalar(unquote(s))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn mappings_sequences_and_scalars() {
        let doc = parse(
            "name: CI\non:\n  push:\n    branches: [main]\n  pull_request:\n    types: [opened, 'synchronize']\n  merge_group:\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4 # pinned\n        with:\n          fetch-depth: 0\n      - name: 'x # not a comment'\n        run: |\n          echo a: b\n          # inside the script\n          echo c\n      - run: echo done\n",
        )
        .unwrap();
        assert_eq!(doc.get("name").unwrap().as_str(), Some("CI"));
        let on = doc.get("on").unwrap();
        assert_eq!(
            on.get("push").unwrap().get("branches").unwrap().items(),
            &[Node::Scalar("main".into())]
        );
        assert_eq!(on.get("merge_group"), Some(&Node::Null));
        let steps = doc
            .get("jobs")
            .unwrap()
            .get("a")
            .unwrap()
            .get("steps")
            .unwrap();
        assert_eq!(steps.items().len(), 3);
        assert_eq!(steps.items()[0].get("uses").unwrap().as_str(), Some("actions/checkout@v4"));
        assert_eq!(
            steps.items()[0]
                .get("with")
                .unwrap()
                .get("fetch-depth")
                .unwrap()
                .as_str(),
            Some("0")
        );
        assert_eq!(steps.items()[1].get("name").unwrap().as_str(), Some("x # not a comment"));
        assert_eq!(
            steps.items()[1].get("run").unwrap().as_str(),
            Some("echo a: b\n# inside the script\necho c\n")
        );
        assert_eq!(steps.items()[2].get("run").unwrap().as_str(), Some("echo done"));
    }

    #[test]
    fn sequence_at_key_indent_and_folded_scalars() {
        let doc = parse("a:\n- x\n- y\nb: >-\n  one\n  two\nc: plain\n  continued\n").unwrap();
        assert_eq!(doc.get("a").unwrap().items().len(), 2);
        assert_eq!(doc.get("b").unwrap().as_str(), Some("one two"));
        assert_eq!(doc.get("c").unwrap().as_str(), Some("plain continued"));
    }

    #[test]
    fn entries_record_their_line() {
        let doc = parse("# header\n\njobs:\n  build:\n    name: Build\n").unwrap();
        let jobs = doc.get("jobs").unwrap();
        assert_eq!(doc.entries()[0].line, 3);
        assert_eq!(jobs.entries()[0].line, 4);
    }

    #[test]
    fn non_mapping_top_level_is_an_error() {
        assert!(parse("- a\n- b\n").is_err());
    }
}
