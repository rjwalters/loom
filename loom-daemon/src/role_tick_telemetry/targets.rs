//! Which issues and PRs a role tick **wrote to** (Issue #9168), read from the
//! same `tool_use` shell commands [`super::tally_command`] classifies.
//!
//! A role-runner tick is spawned without a target — the agent picks its PR or
//! issue itself — so the only record of what it acted on is its transcript.
//! This module turns one shell command into the set of `(repo, number)` pairs
//! it mutated, so the tick can join each of those stories after the fact
//! ([`super::story`]).
//!
//! # What counts
//!
//! Only **write** actions, each naming its number explicitly:
//!
//! - `gh issue {comment,edit,close,reopen,lock,unlock,pin,unpin} <N>` and
//!   `gh pr {comment,edit,merge,review,close,reopen,ready,lock,unlock} <N>`;
//!   `<N>` may be `N`, `#N` or a `https://github.com/<o>/<r>/{issues,pull}/N`
//!   URL. A branch name, a `$VAR`, or no argument at all (gh's
//!   current-branch default) names no number and yields nothing.
//! - `merge-pr.sh <N>` (not with `--dry-run`).
//! - `gh api` with an effective method of `POST`, `PATCH`, `PUT` or `DELETE`
//!   against `repos/<o>/<r>/{issues,pulls}/<N>[/…]`. The method is `-X` /
//!   `--method`, else `POST` when a field (`-f`/`-F`/`--input`) is sent, else
//!   `GET` — gh's own rule.
//!
//! Read-only commands (`view`, `list`, `diff`, `checks`, a `GET` API call)
//! never count. Whether a number is an issue or a PR is not decided here —
//! `gh api …/issues/N/labels` is routinely used on PRs — the forge answers
//! that when the target is resolved.
//!
//! # Repository
//!
//! A command names another repository with `-R`/`--repo`, a URL, or an API
//! path's explicit `<o>/<r>`; the target carries it and the caller keeps only
//! the tick's own repo — cross-repo work is refused, never guessed. API-path
//! placeholders (`{owner}/{repo}`, `:owner/:repo`) mean the current repo.
//!
//! # Parsing
//!
//! A small shell-word splitter, not a shell: quotes and backslashes group
//! words, unquoted `&&`, `||`, `;`, `|`, `&`, parentheses and newlines split
//! commands, and a here-document body (`<<EOF` … `EOF`) is skipped, so prose
//! inside a comment body cannot become a target. Anything it does not
//! understand yields no target — the result is a lower bound, like
//! [`crate::telemetry::RoleTickActions`].

use std::collections::BTreeSet;

/// One issue or PR a tick wrote to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    /// The number the command named (issue or PR — the forge decides).
    pub number: u32,
    /// An explicitly named `owner/repo`, lowercased; `None` = the tick's own.
    pub repo: Option<String>,
}

impl Target {
    /// Whether this target is in `own` (`owner/repo`, any case).
    #[must_use]
    pub fn in_repo(&self, own: &str) -> bool {
        self.repo
            .as_deref()
            .is_none_or(|repo| repo.eq_ignore_ascii_case(own))
    }
}

/// Every target `command` writes to (possibly several, for a compound
/// command), added to `out`.
pub fn collect(command: &str, out: &mut BTreeSet<Target>) {
    for words in commands(command) {
        if let Some(target) = command_target(&words) {
            out.insert(target);
        }
    }
}

/// The distinct `own`-repo numbers in `targets`, dropping (and counting)
/// every target in another repository.
#[must_use]
pub fn own_repo_numbers(targets: &BTreeSet<Target>, own: &str) -> (Vec<u32>, usize) {
    let mut numbers = BTreeSet::new();
    let mut foreign = 0;
    for target in targets {
        if target.in_repo(own) {
            numbers.insert(target.number);
        } else {
            foreign += 1;
        }
    }
    (numbers.into_iter().collect(), foreign)
}

/// The target of one simple command, given as its words.
fn command_target(words: &[String]) -> Option<Target> {
    // Leading `VAR=value` assignments and an `env` wrapper do not change
    // which program runs.
    let start = words.iter().position(|w| w != "env" && !is_assignment(w))?;
    let program = words[start].rsplit('/').next().unwrap_or_default();
    let args = &words[start + 1..];
    match program {
        "gh" => gh_target(args),
        "merge-pr.sh" => merge_pr_target(args),
        _ => None,
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    })
}

fn gh_target(args: &[String]) -> Option<Target> {
    let group = args.first()?.as_str();
    if group == "api" {
        return api_target(&args[1..]);
    }
    let verb = args.get(1)?.as_str();
    let booleans: &[&str] = match (group, verb) {
        (
            "issue",
            "comment" | "edit" | "close" | "reopen" | "lock" | "unlock" | "pin" | "unpin",
        )
        | ("pr", "comment" | "edit" | "close" | "reopen" | "ready" | "lock" | "unlock") => &[
            "e",
            "editor",
            "w",
            "web",
            "edit-last",
            "create-if-none",
            "delete-last",
            "yes",
            "remove-milestone",
            "undo",
            "d",
            "delete-branch",
        ],
        ("pr", "merge") => &[
            "admin",
            "auto",
            "d",
            "delete-branch",
            "disable-auto",
            "m",
            "merge",
            "r",
            "rebase",
            "s",
            "squash",
        ],
        ("pr", "review") => &["a", "approve", "c", "comment", "r", "request-changes"],
        _ => return None,
    };
    let parsed = Args::parse(&args[2..], booleans, &["R", "repo"]);
    if parsed.help {
        return None;
    }
    let mut target = number_argument(parsed.positionals.first()?)?;
    if let Some(repo) = parsed.value {
        // `-R` naming a different repo than the URL is contradictory: refuse.
        let repo = repo.to_ascii_lowercase();
        if target.repo.as_ref().is_some_and(|url| *url != repo) {
            return None;
        }
        target.repo = Some(repo);
    }
    Some(target)
}

fn merge_pr_target(args: &[String]) -> Option<Target> {
    if args.iter().any(|a| a == "--dry-run") {
        return None;
    }
    // Every merge-pr.sh flag but `--worktree-path <dir>` is boolean.
    let positional = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with('-') && (*i == 0 || args[i - 1] != "--worktree-path"))
        .map(|(_, a)| a)?;
    number_argument(positional).filter(|t| t.repo.is_none())
}

fn api_target(args: &[String]) -> Option<Target> {
    const VALUED: &[&str] = &[
        "X",
        "method",
        "f",
        "raw-field",
        "F",
        "field",
        "H",
        "header",
        "input",
        "q",
        "jq",
        "t",
        "template",
        "hostname",
        "cache",
        "p",
        "preview",
    ];
    let mut method: Option<String> = None;
    let mut sends_body = false;
    let mut endpoint: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let Some((name, inline)) = flag(arg) else {
            endpoint = endpoint.or(Some(arg));
            i += 1;
            continue;
        };
        if matches!(name, "h" | "help") {
            return None;
        }
        let value = if VALUED.contains(&name) {
            match inline {
                Some(v) => Some(v.to_string()),
                None => {
                    i += 1;
                    args.get(i).cloned()
                }
            }
        } else {
            None
        };
        match name {
            "X" | "method" => method = value,
            "f" | "raw-field" | "F" | "field" | "input" => sends_body = true,
            _ => {}
        }
        i += 1;
    }
    let method = method
        .unwrap_or_else(|| if sends_body { "POST" } else { "GET" }.into())
        .to_ascii_uppercase();
    if !matches!(method.as_str(), "POST" | "PATCH" | "PUT" | "DELETE") {
        return None;
    }
    api_path_target(endpoint?)
}

/// `repos/<o>/<r>/{issues,pulls}/<N>[/…]` → target N.
fn api_path_target(endpoint: &str) -> Option<Target> {
    let path = endpoint
        .strip_prefix("https://api.github.com/")
        .unwrap_or(endpoint)
        .trim_start_matches('/');
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').collect();
    let ["repos", owner, repo, "issues" | "pulls", number, ..] = parts.as_slice() else {
        return None;
    };
    let number = parse_number(number)?;
    let placeholder = |part: &str| matches!(part, "{owner}" | "{repo}" | ":owner" | ":repo");
    let repo = match (placeholder(owner), placeholder(repo)) {
        (true, true) => None,
        (false, false) => Some(format!("{owner}/{repo}").to_ascii_lowercase()),
        // Half a placeholder is not a repository we can name.
        _ => return None,
    };
    Some(Target { number, repo })
}

/// `N`, `#N`, or a `github.com/<o>/<r>/{issues,pull}/N` URL.
fn number_argument(word: &str) -> Option<Target> {
    if let Some(number) = parse_number(word.strip_prefix('#').unwrap_or(word)) {
        return Some(Target { number, repo: None });
    }
    let rest = word
        .strip_prefix("https://github.com/")
        .or_else(|| word.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = rest.split(['?', '#']).next()?.split('/').collect();
    let [owner, repo, "issues" | "pull", number, ..] = parts.as_slice() else {
        return None;
    };
    Some(Target {
        number: parse_number(number)?,
        repo: Some(format!("{owner}/{repo}").to_ascii_lowercase()),
    })
}

fn parse_number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|n| *n > 0)
}

/// `--name[=value]` / `-n[value]` → `(name, inline value)`; `None` for a
/// positional word.
fn flag(word: &str) -> Option<(&str, Option<&str>)> {
    if let Some(long) = word.strip_prefix("--") {
        if long.is_empty() {
            return None;
        }
        return Some(match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        });
    }
    let short = word.strip_prefix('-')?;
    let mut chars = short.char_indices();
    let (_, first) = chars.next()?;
    if first.is_ascii_digit() {
        return None;
    }
    let split = first.len_utf8();
    let inline = &short[split..];
    Some((&short[..split], (!inline.is_empty()).then_some(inline)))
}

/// A gh subcommand's arguments: its positionals, the value of the one flag
/// the caller asked for, and whether help was requested.
struct Args {
    positionals: Vec<String>,
    value: Option<String>,
    help: bool,
}

impl Args {
    /// Every flag not in `booleans` is taken to consume a value (the next
    /// word, unless given inline) — the conservative reading: a misread
    /// boolean can only hide a positional, never invent one.
    fn parse(args: &[String], booleans: &[&str], wanted: &[&str]) -> Self {
        let mut out = Args {
            positionals: Vec::new(),
            value: None,
            help: false,
        };
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            i += 1;
            if arg == "--" {
                out.positionals.extend(args[i..].iter().cloned());
                break;
            }
            let Some((name, inline)) = flag(arg) else {
                out.positionals.push(arg.clone());
                continue;
            };
            if matches!(name, "h" | "help") {
                out.help = true;
                continue;
            }
            if booleans.contains(&name) {
                continue;
            }
            let value = match inline {
                Some(v) => Some(v.to_string()),
                None => {
                    i += 1;
                    args.get(i - 1).cloned()
                }
            };
            if wanted.contains(&name) {
                out.value = value;
            }
        }
        out
    }
}

/// Split `script` into simple commands, each as its words. See the module
/// docs' "Parsing" section for what is and is not understood.
fn commands(script: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
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
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(q),
                    }
                }
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
            '#' if !in_word => {
                // A comment runs to the end of the line.
                while chars.peek().is_some_and(|n| *n != '\n') {
                    chars.next();
                }
            }
            '<' if !in_word && chars.peek() == Some(&'<') => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    // `<<<` here-string: an ordinary word follows.
                    chars.next();
                    continue;
                }
                let strip_tabs = chars.peek() == Some(&'-');
                if strip_tabs {
                    chars.next();
                }
                while chars.peek().is_some_and(|n| *n == ' ' || *n == '\t') {
                    chars.next();
                }
                let mut delimiter = String::new();
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || matches!(n, ';' | '&' | '|' | ')' | '(') {
                        break;
                    }
                    if n != '\'' && n != '"' && n != '\\' {
                        delimiter.push(n);
                    }
                    chars.next();
                }
                if !delimiter.is_empty() {
                    heredocs.push((delimiter, strip_tabs));
                }
            }
            '\n' => {
                finish_word(&mut word, &mut in_word, &mut words);
                finish_command(&mut words, &mut out);
                // Skip every pending here-document body, line by line.
                for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                    loop {
                        let mut line = String::new();
                        let mut ended = false;
                        for n in chars.by_ref() {
                            if n == '\n' {
                                ended = true;
                                break;
                            }
                            line.push(n);
                        }
                        let line = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if line == delimiter || !ended {
                            break;
                        }
                    }
                }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
