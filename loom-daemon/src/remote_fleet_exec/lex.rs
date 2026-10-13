//! A small, quote-aware shell lexer for the remote-fleet-exec guard (#11568).
//!
//! It is deliberately not a shell parser. It knows exactly what the guard
//! needs: words (with quotes removed, and a flag for "contains an expansion
//! the guard cannot resolve"), the control operators that end a simple
//! command or join a pipeline, and the stdin redirections (`< file`, heredoc
//! bodies, here-strings) whose contents a remote shell would execute.
//! Anything it does not understand degrades to a plain word, never a panic.

/// One shell word, quotes removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    /// Contains a `$` expansion, a command substitution or a backtick span
    /// outside single quotes — its runtime value is unknown here.
    pub dynamic: bool,
}

/// Where a simple command's stdin comes from, when redirected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdin {
    File(Word),
    /// A heredoc body; `expands` when the delimiter is unquoted, so `$( … )`,
    /// backticks and `$VAR` in the body are expanded locally before it is fed.
    Heredoc {
        body: String,
        expands: bool,
    },
    HereString(Word),
}

/// One simple command: its words and stdin redirection (the last one wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Simple {
    pub words: Vec<Word>,
    pub stdin: Option<Stdin>,
}

/// A pipeline: stages joined by `|` / `|&`.
pub type Pipeline = Vec<Simple>;

#[derive(Debug)]
enum Tok {
    Word(Word),
    /// `|` or `|&`.
    Pipe,
    /// `;`, `&`, `&&`, `||`, newline, `(`, `)`.
    End,
    In(Word),
    /// Index into the heredoc body table.
    Heredoc(usize),
    HereString(Word),
    /// An output redirection; its target word was consumed.
    Out,
}

struct Lexer<'a> {
    s: &'a [char],
    i: usize,
    toks: Vec<Tok>,
    /// (delimiter, strip-tabs, body index) awaiting the next newline.
    pending: Vec<(String, bool, usize)>,
    bodies: Vec<String>,
    /// Per body: was the delimiter unquoted (the body expands)?
    expands: Vec<bool>,
}

/// Split `command` into pipelines.
#[must_use]
pub fn pipelines(command: &str) -> Vec<Pipeline> {
    let chars: Vec<char> = command.chars().collect();
    let mut lx = Lexer {
        s: &chars,
        i: 0,
        toks: Vec::new(),
        pending: Vec::new(),
        bodies: Vec::new(),
        expands: Vec::new(),
    };
    lx.run();
    let bodies = std::mem::take(&mut lx.bodies);
    let expands = std::mem::take(&mut lx.expands);
    let mut out = Vec::new();
    let mut pipeline: Pipeline = Vec::new();
    let mut cur = Simple::default();
    for tok in lx.toks {
        match tok {
            Tok::Word(w) => cur.words.push(w),
            Tok::In(w) => cur.stdin = Some(Stdin::File(w)),
            Tok::HereString(w) => cur.stdin = Some(Stdin::HereString(w)),
            Tok::Heredoc(n) => {
                cur.stdin = Some(Stdin::Heredoc {
                    body: bodies.get(n).cloned().unwrap_or_default(),
                    expands: expands.get(n).copied().unwrap_or(true),
                });
            }
            Tok::Out => {}
            Tok::Pipe => pipeline.push(std::mem::take(&mut cur)),
            Tok::End => {
                if !cur.words.is_empty() || cur.stdin.is_some() {
                    pipeline.push(std::mem::take(&mut cur));
                }
                if !pipeline.is_empty() {
                    out.push(std::mem::take(&mut pipeline));
                }
            }
        }
    }
    if !cur.words.is_empty() || cur.stdin.is_some() {
        pipeline.push(cur);
    }
    if !pipeline.is_empty() {
        out.push(pipeline);
    }
    out
}

impl Lexer<'_> {
    fn peek(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '\\' if self.peek(1) == Some('\n') => self.i += 2,
                ' ' | '\t' | '\r' => self.i += 1,
                '\n' => {
                    self.i += 1;
                    self.toks.push(Tok::End);
                    self.read_heredoc_bodies();
                }
                '#' => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                ';' | '(' | ')' => {
                    self.i += 1;
                    self.toks.push(Tok::End);
                }
                '&' => {
                    if self.peek(1) == Some('>') {
                        self.i += if self.peek(2) == Some('>') { 3 } else { 2 };
                        self.out_target();
                    } else {
                        self.i += if self.peek(1) == Some('&') { 2 } else { 1 };
                        self.toks.push(Tok::End);
                    }
                }
                '|' => match self.peek(1) {
                    Some('|') => {
                        self.i += 2;
                        self.toks.push(Tok::End);
                    }
                    Some('&') => {
                        self.i += 2;
                        self.toks.push(Tok::Pipe);
                    }
                    _ => {
                        self.i += 1;
                        self.toks.push(Tok::Pipe);
                    }
                },
                '<' | '>' => self.redirection(),
                c if c.is_ascii_digit() && self.fd_prefixed_redirection() => {}
                _ => {
                    let w = self.word();
                    self.toks.push(Tok::Word(w));
                }
            }
        }
    }

    /// `2>file`, `0<file` — a digit run directly followed by `<`/`>`.
    fn fd_prefixed_redirection(&mut self) -> bool {
        let mut k = 0;
        while self.peek(k).is_some_and(|c| c.is_ascii_digit()) {
            k += 1;
        }
        if matches!(self.peek(k), Some('<' | '>')) {
            self.i += k;
            self.redirection();
            true
        } else {
            false
        }
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.i += 1;
        }
    }

    fn out_target(&mut self) {
        self.skip_blanks();
        if self
            .peek(0)
            .is_some_and(|c| !matches!(c, '\n' | ';' | '|' | '&'))
        {
            let _ = self.word();
        }
        self.toks.push(Tok::Out);
    }

    fn redirection(&mut self) {
        let c = self.peek(0);
        if c == Some('>') {
            self.i += 1;
            if matches!(self.peek(0), Some('>' | '|')) {
                self.i += 1;
            }
            if self.peek(0) == Some('&') {
                self.i += 1;
            }
            self.out_target();
            return;
        }
        // `<`
        self.i += 1;
        match (self.peek(0), self.peek(1)) {
            (Some('<'), Some('<')) => {
                self.i += 2;
                self.skip_blanks();
                let w = self.word();
                self.toks.push(Tok::HereString(w));
            }
            (Some('<'), _) => {
                self.i += 1;
                let strip = self.peek(0) == Some('-');
                if strip {
                    self.i += 1;
                }
                self.skip_blanks();
                let start = self.i;
                let delim = self.word().text;
                let quoted = self.s[start..self.i]
                    .iter()
                    .any(|c| matches!(c, '\'' | '"' | '\\'));
                let n = self.bodies.len();
                self.bodies.push(String::new());
                self.expands.push(!quoted);
                self.pending.push((delim, strip, n));
                self.toks.push(Tok::Heredoc(n));
            }
            (Some('&' | '>'), _) => {
                self.i += 1;
                self.out_target();
            }
            _ => {
                self.skip_blanks();
                let w = self.word();
                self.toks.push(Tok::In(w));
            }
        }
    }

    fn read_heredoc_bodies(&mut self) {
        for (delim, strip, n) in std::mem::take(&mut self.pending) {
            let mut body = String::new();
            loop {
                if self.i >= self.s.len() {
                    break;
                }
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                let line: String = self.s[start..self.i].iter().collect();
                if self.peek(0) == Some('\n') {
                    self.i += 1;
                }
                let cmp = if strip {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if cmp == delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if let Some(b) = self.bodies.get_mut(n) {
                *b = body;
            }
        }
    }

    /// Consume a balanced `(...)` starting at the current `(`, appending the
    /// raw text (parens included) to `out`.
    fn balanced(&mut self, open: char, close: char, out: &mut String) {
        let mut depth = 0usize;
        while let Some(c) = self.peek(0) {
            self.i += 1;
            out.push(c);
            if c == open {
                depth += 1;
            } else if c == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return;
                }
            } else if c == '\'' {
                while let Some(q) = self.peek(0) {
                    self.i += 1;
                    out.push(q);
                    if q == '\'' {
                        break;
                    }
                }
            }
        }
    }

    fn dollar(&mut self, out: &mut String) {
        out.push('$');
        self.i += 1;
        match self.peek(0) {
            Some('(') => self.balanced('(', ')', out),
            Some('{') => self.balanced('{', '}', out),
            _ => {}
        }
    }

    fn word(&mut self) -> Word {
        let mut text = String::new();
        let mut dynamic = false;
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
                '\\' => {
                    self.i += 1;
                    match self.peek(0) {
                        Some('\n') => self.i += 1,
                        Some(e) => {
                            text.push(e);
                            self.i += 1;
                        }
                        None => {}
                    }
                }
                '\'' => {
                    self.i += 1;
                    while let Some(q) = self.peek(0) {
                        self.i += 1;
                        if q == '\'' {
                            break;
                        }
                        text.push(q);
                    }
                }
                '"' => {
                    self.i += 1;
                    while let Some(q) = self.peek(0) {
                        match q {
                            '"' => {
                                self.i += 1;
                                break;
                            }
                            '\\' if matches!(self.peek(1), Some('"' | '\\' | '$' | '`')) => {
                                text.push(self.s[self.i + 1]);
                                self.i += 2;
                            }
                            '$' => {
                                dynamic = true;
                                self.dollar(&mut text);
                            }
                            '`' => {
                                dynamic = true;
                                self.i += 1;
                                text.push('`');
                                while let Some(b) = self.peek(0) {
                                    self.i += 1;
                                    text.push(b);
                                    if b == '`' {
                                        break;
                                    }
                                }
                            }
                            _ => {
                                text.push(q);
                                self.i += 1;
                            }
                        }
                    }
                }
                '$' => {
                    dynamic = true;
                    self.dollar(&mut text);
                }
                '`' => {
                    dynamic = true;
                    self.i += 1;
                    text.push('`');
                    while let Some(b) = self.peek(0) {
                        self.i += 1;
                        text.push(b);
                        if b == '`' {
                            break;
                        }
                    }
                }
                _ => {
                    text.push(c);
                    self.i += 1;
                }
            }
        }
        Word { text, dynamic }
    }
}

/// The inner text of every `$( … )` and backtick span in `text`, so an
/// invocation nested inside a substitution is still examined.
#[must_use]
pub fn substitutions(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
            let mut depth = 0usize;
            let start = i + 2;
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            out.push(chars[start..j.min(chars.len())].iter().collect());
            i = j + 1;
        } else if chars[i] == '`' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j] != '`' {
                j += 1;
            }
            out.push(chars[start..j].iter().collect());
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}
