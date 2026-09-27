//! The shell-word splitter behind [`super::collect`] (Issues #9168, #9180).
//!
//! Not a shell: it splits a script into simple commands, each as its words,
//! and understands only as much syntax as it takes to keep text that is
//! *not* a command from being read as one:
//!
//! - quotes and backslashes group words; unquoted `&&`, `||`, `;`, `|`, `&`,
//!   parentheses and newlines split commands; `#` at a word start comments
//!   to the end of the line;
//! - `<<[-]DELIM` opens a here-document wherever it appears unquoted — also
//!   glued to a word (`cat<<EOF`) or with a quoted/escaped delimiter
//!   (`<<'EOF'`, `<<"EOF"`, `<<\EOF`) — and its body, up to the line that is
//!   exactly `DELIM`, is never parsed;
//! - `$( … )` and `` ` … ` `` are captured whole — quote- and
//!   here-document-aware, so an odd number of `"` in the body of
//!   `--body "$(cat <<'EOF' … EOF)"` cannot close the surrounding quote —
//!   and their contents are split as commands of their own. The word that
//!   held one carries [`SUBSTITUTION`] and so never reads as a number.
//!   Nesting deeper than [`MAX_DEPTH`] (or a substitution that closes with a
//!   here-document still open) hides the rest of the input: the lexer's
//!   recursion and total work stay bounded (linear in the input) whatever an
//!   agent — or forge text it copied — puts in a command (#9180).
//! - `$'…'` (ANSI-C quoting) is one quoted word whose `\'` does not end it.
//!
//! Where it and a real shell could disagree, the disagreement can only hide
//! text (e.g. an arithmetic `<<` read as a here-document skips the rest of
//! the script), never expose prose as a command.

use std::iter::Peekable;
use std::str::Chars;

type Stream<'a> = Peekable<Chars<'a>>;

/// Stands in for a command substitution inside a word: never a number, a
/// repository, or a directory.
pub(super) const SUBSTITUTION: &str = "$(…)";

/// How deep substitutions may nest before the rest of the input is hidden.
/// Each level re-lexes its contents once, so total work is at most
/// `MAX_DEPTH` times the input length, and the stack stays shallow.
pub(super) const MAX_DEPTH: usize = 16;

/// A pending here-document: its delimiter and whether `<<-` strips tabs.
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

/// Split `script` into simple commands, each as its words, in execution
/// order (a substitution's commands come before the command using it).
pub(super) fn commands(script: &str) -> Vec<Vec<String>> {
    commands_at(script, 0)
}

/// [`commands`] for text nested `depth` substitutions deep.
fn commands_at(script: &str, depth: usize) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut chars = script.chars().peekable();
    let finish_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    let finish_command = |words: &mut Vec<String>, out: &mut Vec<Vec<String>>| {
        if !words.is_empty() {
            out.push(std::mem::take(words));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                double_quoted(&mut chars, &mut word, &mut out, depth);
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                in_word = true;
                ansi_c_quoted(&mut chars, &mut word);
            }
            '\\' => {
                // A backslash-newline continues the line.
                match chars.next() {
                    Some('\n') | None => {}
                    Some(escaped) => {
                        in_word = true;
                        word.push(escaped);
                    }
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                in_word = true;
                word.push_str(SUBSTITUTION);
                if let Some(inner) = substitution(&mut chars, depth + 1) {
                    out.extend(commands_at(&inner, depth + 1));
                }
            }
            '`' => {
                in_word = true;
                word.push_str(SUBSTITUTION);
                let inner = backquoted(&mut chars);
                nested(&mut chars, &inner, depth, &mut out);
            }
            '#' if !in_word => {
                // A comment runs to the end of the line.
                while chars.peek().is_some_and(|n| *n != '\n') {
                    chars.next();
                }
            }
            '<' if chars.peek() == Some(&'<') => {
                // An operator even when glued to a word: `cat<<EOF`.
                finish_word(&mut word, &mut in_word, &mut words);
                chars.next();
                if chars.peek() == Some(&'<') {
                    // `<<<` here-string: an ordinary word follows.
                    chars.next();
                    continue;
                }
                if let Some(heredoc) = heredoc_operator(&mut chars, &mut String::new()) {
                    heredocs.push(heredoc);
                }
            }
            '\n' => {
                finish_word(&mut word, &mut in_word, &mut words);
                finish_command(&mut words, &mut out);
                skip_bodies(&mut chars, &mut heredocs, &mut String::new());
            }
            ';' | '&' | '|' | '(' | ')' => {
                finish_word(&mut word, &mut in_word, &mut words);
                finish_command(&mut words, &mut out);
            }
            c if c.is_whitespace() => finish_word(&mut word, &mut in_word, &mut words),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish_word(&mut word, &mut in_word, &mut words);
    finish_command(&mut words, &mut out);
    out
}

/// The commands of a backquoted `inner` found `depth` deep into `out`, or —
/// past [`MAX_DEPTH`] — nothing, hiding the rest of the input.
fn nested(chars: &mut Stream<'_>, inner: &str, depth: usize, out: &mut Vec<Vec<String>>) {
    if depth + 1 > MAX_DEPTH {
        chars.for_each(drop);
    } else {
        out.extend(commands_at(inner, depth + 1));
    }
}

/// The rest of a `$'…'` word (`$'` consumed) into `word`: a backslash
/// escapes the next character, so `\'` does not end it.
fn ansi_c_quoted(chars: &mut Stream<'_>, word: &mut String) {
    while let Some(q) = chars.next() {
        match q {
            '\'' => break,
            '\\' => {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            _ => word.push(q),
        }
    }
}

/// The rest of a `"…"` word (the opening quote consumed) into `word`; the
/// commands of any substitution inside it into `out`.
fn double_quoted(
    chars: &mut Stream<'_>,
    word: &mut String,
    out: &mut Vec<Vec<String>>,
    depth: usize,
) {
    while let Some(q) = chars.next() {
        match q {
            '"' => break,
            '\\' => {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                word.push_str(SUBSTITUTION);
                if let Some(inner) = substitution(chars, depth + 1) {
                    out.extend(commands_at(&inner, depth + 1));
                }
            }
            '`' => {
                word.push_str(SUBSTITUTION);
                let inner = backquoted(chars);
                nested(chars, &inner, depth, out);
            }
            _ => word.push(q),
        }
    }
}

/// The text of a `$( … )` whose `$(` is consumed, `depth` substitutions
/// deep, up to (and consuming) its matching `)`, or to the end of input.
/// Quotes, escapes, comments and here-document bodies are copied verbatim
/// but never close it. `None` — with the rest of the input consumed and
/// hidden — past [`MAX_DEPTH`], or when it closes with a here-document still
/// open (a `case` arm's `)`: where its body really ends is unknowable).
fn substitution(chars: &mut Stream<'_>, depth: usize) -> Option<String> {
    let hide = |chars: &mut Stream<'_>| {
        chars.for_each(drop);
        None
    };
    if depth > MAX_DEPTH {
        return hide(chars);
    }
    let mut text = String::new();
    let mut parens = 1usize;
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut word_start = true;
    while let Some(c) = chars.next() {
        let starts_word = word_start;
        word_start = c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')');
        match c {
            ')' => {
                parens -= 1;
                if parens == 0 {
                    return if heredocs.is_empty() {
                        Some(text)
                    } else {
                        hide(chars)
                    };
                }
                text.push(c);
            }
            '(' => {
                parens += 1;
                text.push(c);
            }
            '\'' => {
                text.push(c);
                for q in chars.by_ref() {
                    text.push(q);
                    if q == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                text.push(c);
                if !copy_double_quoted(chars, &mut text, depth) {
                    return None;
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                text.push_str("$'");
                while let Some(q) = chars.next() {
                    text.push(q);
                    match q {
                        '\'' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                text.push(escaped);
                            }
                        }
                        _ => {}
                    }
                }
            }
            '`' => {
                text.push(c);
                text.push_str(&backquoted(chars));
                text.push('`');
            }
            '\\' => {
                text.push(c);
                if let Some(escaped) = chars.next() {
                    text.push(escaped);
                }
            }
            '#' if starts_word => {
                text.push(c);
                while let Some(n) = chars.next_if(|n| *n != '\n') {
                    text.push(n);
                }
            }
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                text.push_str("<<");
                word_start = true;
                if chars.peek() == Some(&'<') {
                    chars.next();
                    text.push('<');
                    continue;
                }
                if let Some(heredoc) = heredoc_operator(chars, &mut text) {
                    heredocs.push(heredoc);
                }
            }
            '\n' => {
                text.push(c);
                skip_bodies(chars, &mut heredocs, &mut text);
            }
            _ => text.push(c),
        }
    }
    Some(text)
}

/// Copy the rest of a `"…"` (opening quote already copied) into `text`,
/// through its closing quote; a substitution inside it is copied whole.
/// `false` when a nested substitution hid the rest of the input.
fn copy_double_quoted(chars: &mut Stream<'_>, text: &mut String, depth: usize) -> bool {
    while let Some(q) = chars.next() {
        text.push(q);
        match q {
            '"' => return true,
            '\\' => {
                if let Some(escaped) = chars.next() {
                    text.push(escaped);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                text.push('(');
                let Some(inner) = substitution(chars, depth + 1) else {
                    return false;
                };
                text.push_str(&inner);
                text.push(')');
            }
            '`' => {
                text.push_str(&backquoted(chars));
                text.push('`');
            }
            _ => {}
        }
    }
    true
}

/// The text of a `` `…` `` whose opening backquote is consumed, through its
/// closing one (consumed, not returned); `\` escapes the next character.
fn backquoted(chars: &mut Stream<'_>) -> String {
    let mut text = String::new();
    while let Some(c) = chars.next() {
        match c {
            '`' => break,
            '\\' => match chars.next() {
                Some(e @ ('`' | '\\' | '$')) => text.push(e),
                Some(e) => {
                    text.push('\\');
                    text.push(e);
                }
                None => {}
            },
            _ => text.push(c),
        }
    }
    text
}

/// The delimiter after a consumed `<<`, copying what it consumes to `raw`.
/// Quotes and backslashes group it (`<<'END X'`) and are not part of it.
fn heredoc_operator(chars: &mut Stream<'_>, raw: &mut String) -> Option<Heredoc> {
    let strip_tabs = chars.next_if_eq(&'-').is_some();
    if strip_tabs {
        raw.push('-');
    }
    while let Some(blank) = chars.next_if(|n| *n == ' ' || *n == '\t') {
        raw.push(blank);
    }
    let mut delimiter = String::new();
    while let Some(&n) = chars.peek() {
        if n.is_whitespace() || matches!(n, ';' | '&' | '|' | ')' | '(' | '<' | '>') {
            break;
        }
        chars.next();
        raw.push(n);
        match n {
            '\'' | '"' => {
                for q in chars.by_ref() {
                    raw.push(q);
                    if q == n {
                        break;
                    }
                    delimiter.push(q);
                }
            }
            '\\' => {
                if let Some(escaped) = chars.next() {
                    raw.push(escaped);
                    delimiter.push(escaped);
                }
            }
            _ => delimiter.push(n),
        }
    }
    (!delimiter.is_empty()).then_some(Heredoc {
        delimiter,
        strip_tabs,
    })
}

/// Skip every pending here-document body (the line after its operator
/// already begun), copying the skipped text to `raw`. A body without its
/// terminator line runs to the end of input.
fn skip_bodies(chars: &mut Stream<'_>, heredocs: &mut Vec<Heredoc>, raw: &mut String) {
    for heredoc in std::mem::take(heredocs) {
        loop {
            let mut line = String::new();
            let mut ended = false;
            for n in chars.by_ref() {
                raw.push(n);
                if n == '\n' {
                    ended = true;
                    break;
                }
                line.push(n);
            }
            let line = if heredoc.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if line == heredoc.delimiter || !ended {
                break;
            }
        }
    }
}
