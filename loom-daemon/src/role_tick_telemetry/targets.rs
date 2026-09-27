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
//! A command that names no repository acts on whatever the shell selects, so
//! a [`Session`] follows that state through the transcript (#9180):
//!
//! - `GH_REPO` — as a command prefix (`GH_REPO=o/r gh …`), or set for the
//!   rest of the session (`GH_REPO=…`, `export`/`declare`, cleared by
//!   `unset`) — is carried on the target like `-R`.
//! - `cd`/`pushd` move the working directory; the target carries it and is
//!   kept only when it lies inside the tick's root (Loom worktrees under
//!   `.loom/worktrees/` included, any other nested checkout excluded). An
//!   unknowable directory (`cd -`, `cd $DIR`, `cd ~`, `popd`) drops every
//!   unqualified target until an absolute `cd` makes it known again.
//! - `GH_HOST` (other than `github.com`), `GIT_DIR` or `GIT_WORK_TREE`, or a
//!   `GH_REPO` that is not a plain `owner/repo`, make the repository
//!   unknowable: no target at all.
//!
//! Every rule is one-sided: when unsure, the command yields nothing.
//!
//! # Parsing
//!
//! A small shell-word splitter, not a shell ([`shell`]): quotes and
//! backslashes group words, unquoted operators and newlines split commands,
//! here-document bodies (also `cat<<EOF` and inside `"$(cat <<'EOF' … )"`)
//! are skipped, so prose inside a comment body cannot become a target.
//! Anything it does not understand yields no target — the result is a lower
//! bound, like [`crate::telemetry::RoleTickActions`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

mod shell;

/// One issue or PR a tick wrote to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    /// The number the command named (issue or PR — the forge decides).
    pub number: u32,
    /// An explicitly named `owner/repo` (flag, URL, API path or `GH_REPO`),
    /// lowercased; `None` = whatever the working directory's checkout is.
    pub repo: Option<String>,
    /// The working directory the command ran in when a `cd` moved it — as
    /// written, relative to the tick's root unless absolute; `None` = the
    /// root itself. Irrelevant (always `None`) when the command names its
    /// repository explicitly.
    pub cwd: Option<String>,
}

impl Target {
    /// A target in the tick's own checkout, named by number only.
    #[must_use]
    pub fn own(number: u32) -> Self {
        Self {
            number,
            repo: None,
            cwd: None,
        }
    }

    /// Whether this target is in `own` (`owner/repo`, any case), for a tick
    /// whose sessions started in `root`.
    #[must_use]
    pub fn in_repo(&self, own: &str, root: &Path) -> bool {
        self.repo
            .as_deref()
            .is_none_or(|repo| repo.eq_ignore_ascii_case(own))
            && self.cwd.as_deref().is_none_or(|dir| dir_in_root(root, dir))
    }
}

/// Every target `command` writes to (possibly several, for a compound
/// command), added to `out`. `session` carries the shell state (`cd`,
/// `GH_REPO`, …) from one command of a transcript to the next.
pub fn collect(command: &str, session: &mut Session, out: &mut BTreeSet<Target>) {
    for words in shell::commands(command) {
        if let Some(target) = session.command_target(&words) {
            out.insert(target);
        }
    }
}

/// The distinct `own`-repo numbers in `targets`, dropping (and counting)
/// every target in another repository or another checkout than `root`.
#[must_use]
pub fn own_repo_numbers(targets: &BTreeSet<Target>, own: &str, root: &Path) -> (Vec<u32>, usize) {
    let mut numbers = BTreeSet::new();
    let mut foreign = 0;
    for target in targets {
        if target.in_repo(own, root) {
            numbers.insert(target.number);
        } else {
            foreign += 1;
        }
    }
    (numbers.into_iter().collect(), foreign)
}

/// Whether `dir` (absolute, or relative to `root`) lies in `root`'s own
/// checkout: `root` or below it, with no other checkout (a `.git` entry)
/// in between — except Loom's own worktrees, `.loom/worktrees/<name>`.
/// Lexical, like the shell's default `cd -L`.
fn dir_in_root(root: &Path, dir: &str) -> bool {
    let root = normalize(root);
    let path = normalize(&root.join(dir));
    let Ok(below) = path.strip_prefix(&root) else {
        return false;
    };
    let mut sub = PathBuf::new();
    for (depth, part) in below.components().enumerate() {
        sub.push(part);
        let loom_worktree = depth == 2 && sub.starts_with(".loom/worktrees");
        if !loom_worktree && root.join(&sub).join(".git").exists() {
            return false;
        }
    }
    true
}

/// `path` with `.` and `..` resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The environment variables that change which repository `gh` acts on.
const SCOPE_VARS: &[&str] = &["GH_REPO", "GH_HOST", "GIT_DIR", "GIT_WORK_TREE"];

/// The shell state one transcript's commands leave behind: what an
/// unqualified `gh` command would act on. Start a fresh one per transcript.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// [`SCOPE_VARS`] set for the rest of the session.
    vars: BTreeMap<String, String>,
    cwd: Cwd,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Cwd {
    /// The tick's root.
    #[default]
    Start,
    /// A directory, absolute or relative to the root.
    Dir(String),
    /// Somewhere we cannot name.
    Unknown,
}

/// Which repository a command naming none acts on.
enum Scope {
    /// The working directory's checkout.
    Cwd,
    /// `GH_REPO`'s, lowercased.
    Named(String),
    /// Cannot be told.
    Unknown,
}

impl Session {
    /// The target of one simple command, given as its words, after folding
    /// any state it changes into `self`.
    fn command_target(&mut self, words: &[String]) -> Option<Target> {
        let target = self.fold(words);
        // A directory change anywhere but the program position (`sudo cd`,
        // `command -p cd`, `xargs cd`, …) is one we cannot follow: from here
        // on the directory is unknowable.
        let program = words.iter().position(|w| !is_prefix(w));
        if words
            .iter()
            .enumerate()
            .any(|(i, w)| Some(i) != program && matches!(w.as_str(), "cd" | "pushd" | "popd"))
        {
            self.cwd = Cwd::Unknown;
        }
        target
    }

    /// [`Self::command_target`] for a command whose program word is taken
    /// at face value.
    fn fold(&mut self, words: &[String]) -> Option<Target> {
        // Leading `VAR=value` assignments, an `env` wrapper, and keywords or
        // builtins that run the next word (`if`, `{`, `!`, `builtin`,
        // `command`, …) do not change which program runs, but a scope
        // variable among them changes what it acts on.
        let mut vars = self.vars.clone();
        let mut wrapped = false;
        let mut start = 0;
        for word in words {
            if word == "env" {
                wrapped = true;
            } else if let Some((name, value)) = assignment(word) {
                if SCOPE_VARS.contains(&name) {
                    vars.insert(name.to_owned(), value.to_owned());
                }
            } else if !PREFIX_WORDS.contains(&word.as_str()) {
                break;
            }
            start += 1;
        }
        let Some(program) = words.get(start) else {
            // Bare assignments set shell variables for the rest of the session.
            if !wrapped {
                self.vars = vars;
            }
            return None;
        };
        let program = program.rsplit('/').next().unwrap_or_default();
        let args = &words[start + 1..];
        let target = match program {
            "gh" => gh_target(args),
            "merge-pr.sh" => merge_pr_target(args),
            "cd" | "pushd" => {
                self.cwd = self.cwd.change(args);
                return None;
            }
            "popd" => {
                self.cwd = Cwd::Unknown;
                return None;
            }
            "export" | "declare" | "typeset" | "local" | "readonly" => {
                for (name, value) in args.iter().filter_map(|a| assignment(a)) {
                    if SCOPE_VARS.contains(&name) {
                        self.vars.insert(name.to_owned(), value.to_owned());
                    }
                }
                return None;
            }
            "unset" => {
                for arg in args {
                    self.vars.remove(arg.as_str());
                }
                return None;
            }
            _ => return None,
        }?;
        match scope(&vars) {
            Scope::Unknown => None,
            // gh prefers `-R`/a URL/an API path over `GH_REPO` and the cwd.
            _ if target.repo.is_some() => Some(target),
            Scope::Named(repo) => Some(Target {
                repo: Some(repo),
                cwd: self.cwd.as_target()?,
                ..target
            }),
            Scope::Cwd => Some(Target {
                cwd: self.cwd.as_target()?,
                ..target
            }),
        }
    }
}

/// Words that run the word after them as the program: shell keywords,
/// grouping, and the `builtin`/`command`/`exec`/`time` wrappers.
const PREFIX_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "do", "while", "until", "!", "{", "time", "builtin", "command",
    "exec",
];

/// A word before the program: a [`PREFIX_WORDS`] entry, `env`, or an
/// assignment.
fn is_prefix(word: &str) -> bool {
    word == "env" || PREFIX_WORDS.contains(&word) || assignment(word).is_some()
}

/// What `vars` say an unqualified command acts on.
fn scope(vars: &BTreeMap<String, String>) -> Scope {
    let host = vars.get("GH_HOST").map(|h| h.to_ascii_lowercase());
    if vars.contains_key("GIT_DIR")
        || vars.contains_key("GIT_WORK_TREE")
        || host.is_some_and(|h| h != "github.com")
    {
        return Scope::Unknown;
    }
    let Some(repo) = vars.get("GH_REPO") else {
        return Scope::Cwd;
    };
    // `OWNER/REPO` or `github.com/OWNER/REPO`, nothing symbolic.
    let repo = repo.to_ascii_lowercase();
    let repo = repo.strip_prefix("github.com/").unwrap_or(&repo);
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match repo.split('/').collect::<Vec<_>>().as_slice() {
        [owner, name] if plain(owner) && plain(name) => Scope::Named(repo.to_owned()),
        _ => Scope::Unknown,
    }
}

impl Cwd {
    /// The directory after `cd`/`pushd` with `args`.
    fn change(&self, args: &[String]) -> Cwd {
        let mut args = args.iter().map(String::as_str).peekable();
        while args
            .next_if(|a| matches!(*a, "-L" | "-P" | "-e" | "-@"))
            .is_some()
        {}
        args.next_if_eq(&"--");
        // No argument is `$HOME`; `-` is `$OLDPWD`; `+N`/`-N` a stack entry.
        let Some(dir) = args.next() else {
            return Cwd::Unknown;
        };
        if dir.is_empty()
            || dir.starts_with(['-', '+', '~'])
            || dir.contains(['$', '`', '*', '?', '[', '{'])
            || dir.contains(shell::SUBSTITUTION)
        {
            return Cwd::Unknown;
        }
        if dir.starts_with('/') {
            return Cwd::Dir(dir.to_owned());
        }
        match self {
            Cwd::Start => Cwd::Dir(dir.to_owned()),
            Cwd::Dir(base) => Cwd::Dir(format!("{}/{dir}", base.trim_end_matches('/'))),
            Cwd::Unknown => Cwd::Unknown,
        }
    }

    /// The target's `cwd`; `None` (outer) when it cannot be named.
    fn as_target(&self) -> Option<Option<String>> {
        match self {
            Cwd::Start => Some(None),
            Cwd::Dir(dir) => Some(Some(dir.clone())),
            Cwd::Unknown => None,
        }
    }
}

/// `NAME=value` → `(NAME, value)`, for a valid shell variable name.
fn assignment(word: &str) -> Option<(&str, &str)> {
    word.split_once('=').filter(|(name, _)| {
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
    Some(Target {
        number,
        repo,
        cwd: None,
    })
}

/// `N`, `#N`, or a `github.com/<o>/<r>/{issues,pull}/N` URL.
fn number_argument(word: &str) -> Option<Target> {
    if let Some(number) = parse_number(word.strip_prefix('#').unwrap_or(word)) {
        return Some(Target::own(number));
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
        cwd: None,
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
