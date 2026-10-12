//! A deliberately small shell lexer for [`super`]: split a Bash command into
//! simple commands, each a list of words with quotes removed.
//!
//! It is not a shell parser and does not try to be one. It knows enough to
//! find `cargo …` and the assignments in front of it: quoting, backslash
//! escapes, `$( … )` / backtick spans kept whole, comments, redirections, and
//! heredoc bodies (skipped, so prose written to a file is never read as a
//! command). Anything it cannot make sense of becomes a word the evaluator
//! fails to resolve — and an unresolved target dir is an ALLOW, never a deny.

/// One word, quotes removed. `$( … )` and backtick spans are kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Word {
    pub text: String,
    /// A `$` or backtick came from single quotes, so it is literal text. The
    /// expander cannot tell which `$` that was, so it treats the word as
    /// unresolvable (an allow).
    pub single_quoted_dollar: bool,
}

/// Split `command` into simple commands.
pub(super) fn simple_commands(command: &str) -> Vec<Vec<Word>> {
    Lexer::new(command).run()
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    commands: Vec<Vec<Word>>,
    words: Vec<Word>,
    cur: String,
    in_word: bool,
    sq_dollar: bool,
    /// Drop the next completed word: it is a redirection target.
    drop_next: bool,
    /// Heredoc delimiters (and their `<<-` flag) whose bodies start at the
    /// next newline.
    heredocs: Vec<(String, bool)>,
    want_heredoc_word: Option<bool>,
}

impl<'a> Lexer<'a> {
    fn new(command: &'a str) -> Self {
        Self {
            chars: command.chars().peekable(),
            commands: Vec::new(),
            words: Vec::new(),
            cur: String::new(),
            in_word: false,
            sq_dollar: false,
            drop_next: false,
            heredocs: Vec::new(),
            want_heredoc_word: None,
        }
    }

    fn end_word(&mut self) {
        if !self.in_word {
            return;
        }
        let text = std::mem::take(&mut self.cur);
        let sq = std::mem::replace(&mut self.sq_dollar, false);
        self.in_word = false;
        if let Some(strip) = self.want_heredoc_word.take() {
            self.heredocs.push((text, strip));
            return;
        }
        if std::mem::replace(&mut self.drop_next, false) {
            return;
        }
        self.words.push(Word {
            text,
            single_quoted_dollar: sq,
        });
    }

    fn end_command(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.commands.push(std::mem::take(&mut self.words));
        }
        self.drop_next = false;
    }

    /// Copy a `$( … )` span (the `$(` already consumed) into the word,
    /// balancing parentheses; separators inside it do not split the command.
    fn copy_subst(&mut self) {
        self.cur.push_str("$(");
        let mut depth = 1usize;
        while let Some(c) = self.chars.next() {
            self.cur.push(c);
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                '\\' => {
                    if let Some(n) = self.chars.next() {
                        self.cur.push(n);
                    }
                }
                _ => {}
            }
        }
    }

    fn copy_backtick(&mut self) {
        self.cur.push('`');
        for c in self.chars.by_ref() {
            self.cur.push(c);
            if c == '`' {
                return;
            }
        }
    }

    /// Skip heredoc bodies queued on the line just ended.
    fn skip_heredoc_bodies(&mut self) {
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            let mut line = String::new();
            loop {
                match self.chars.next() {
                    Some('\n') | None => {
                        let candidate = if strip {
                            line.trim_start_matches('\t')
                        } else {
                            &line
                        };
                        let done = candidate == delim || self.chars.peek().is_none();
                        line.clear();
                        if done {
                            break;
                        }
                    }
                    Some(c) => line.push(c),
                }
            }
        }
    }

    fn run(mut self) -> Vec<Vec<Word>> {
        while let Some(c) = self.chars.next() {
            match c {
                ' ' | '\t' => self.end_word(),
                '\n' => {
                    self.end_command();
                    self.skip_heredoc_bodies();
                }
                ';' | '|' | '(' | ')' => self.end_command(),
                '&' => {
                    if self.chars.peek() == Some(&'>') {
                        // `&>file` / `&>>file`: a redirection, not a separator.
                        self.end_word();
                        self.chars.next();
                        if self.chars.peek() == Some(&'>') {
                            self.chars.next();
                        }
                        self.drop_next = true;
                    } else {
                        self.end_command();
                    }
                }
                '>' | '<' => {
                    // A bare fd number in front (`2>`) is part of the operator.
                    if self.in_word && self.cur.chars().all(|d| d.is_ascii_digit()) {
                        self.cur.clear();
                        self.in_word = false;
                    } else {
                        self.end_word();
                    }
                    if c == '<' && self.chars.peek() == Some(&'<') {
                        self.chars.next();
                        if self.chars.peek() == Some(&'<') {
                            // `<<<word`: a here-string, its word is data.
                            self.chars.next();
                            self.drop_next = true;
                            continue;
                        }
                        let strip = if self.chars.peek() == Some(&'-') {
                            self.chars.next();
                            true
                        } else {
                            false
                        };
                        self.want_heredoc_word = Some(strip);
                        continue;
                    }
                    if matches!(self.chars.peek(), Some('>' | '|')) {
                        self.chars.next();
                    }
                    if self.chars.peek() == Some(&'&') {
                        // `>&2` / `2>&1`: a fd duplication, its target is a fd.
                        self.chars.next();
                    }
                    self.drop_next = true;
                }
                '#' if !self.in_word => {
                    while let Some(&n) = self.chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        self.chars.next();
                    }
                }
                '\\' => match self.chars.next() {
                    Some('\n') | None => {}
                    Some(n) => {
                        self.in_word = true;
                        self.cur.push(n);
                    }
                },
                '\'' => {
                    self.in_word = true;
                    for n in self.chars.by_ref() {
                        if n == '\'' {
                            break;
                        }
                        if n == '$' || n == '`' {
                            self.sq_dollar = true;
                        }
                        self.cur.push(n);
                    }
                }
                '"' => {
                    self.in_word = true;
                    while let Some(n) = self.chars.next() {
                        match n {
                            '"' => break,
                            '\\' => {
                                if let Some(m) = self.chars.next() {
                                    if !matches!(m, '"' | '\\' | '$' | '`') {
                                        self.cur.push('\\');
                                    }
                                    if m != '\n' {
                                        self.cur.push(m);
                                    }
                                }
                            }
                            '$' if self.chars.peek() == Some(&'(') => {
                                self.chars.next();
                                self.copy_subst();
                            }
                            '`' => self.copy_backtick(),
                            _ => self.cur.push(n),
                        }
                    }
                }
                '$' if self.chars.peek() == Some(&'(') => {
                    self.chars.next();
                    self.in_word = true;
                    self.copy_subst();
                }
                '`' => {
                    self.in_word = true;
                    self.copy_backtick();
                }
                _ => {
                    self.in_word = true;
                    self.cur.push(c);
                }
            }
        }
        self.end_command();
        self.commands
    }
}
