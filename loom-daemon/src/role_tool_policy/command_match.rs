//! Tool-call-time capability matching for `role-tool-policy check` (#8256).
//!
//! The spawn-time half of the per-role restriction ([`super::deny_specs_for`])
//! is a list of Claude Code `--disallowedTools` specs. Those are prefix
//! globs over the Bash tool's command text, so `bash -c 'ssh …'`, `env ssh …`
//! or a compound command walks straight past them, and the Codex CLI has no
//! such flag at all. This module is the **backstop** the guard hooks consult
//! on every tool call: it maps one Bash command line (or one Edit/Write
//! target path) to the sensitive capabilities it would exercise, so
//! [`super::RoleToolPolicy::check_command`] can deny the ones the acting
//! role's JSON does not declare.
//!
//! # What is matched
//!
//! | Capability | Matched when |
//! |---|---|
//! | `remote-shell` | a **command word** is one of [`REMOTE_SHELL_PROGRAMS`] |
//! | `cloud-cli` | a **command word** is one of [`CLOUD_CLI_PROGRAMS`] |
//! | `forge-secrets` | `gh secret …`, `gh variable …`, `gh auth token/login/refresh/logout/setup-git`, `gh auth status --show-token`, or `gh api` against a `…/secrets` / `…/variables` endpoint |
//! | `credential-store` | **any word** (argument, redirect target, assignment value) names a path inside a home credential store ([`CREDENTIAL_DIRS`]) |
//!
//! "Command word" is resolved through a small shell lexer, not a substring
//! scan: quotes and backslashes are honoured (so `"s"sh` is `ssh`, and the
//! prose `gh pr comment --body "use ssh"` is not a command), `;`/`&&`/`|`/
//! newlines/`(`/`)` separate commands, `$(…)`, backticks and `<(…)` bodies are
//! analyzed as commands of their own, and the common wrappers (`sudo`, `env`,
//! `timeout`, `xargs`, `nohup`, `find -exec`, `eval`, `bash -c`, a here-string
//! fed to a shell, …) are looked through.
//!
//! # What is deliberately NOT matched
//!
//! This is a backstop, not a sandbox. A command assembled at run time
//! (`p=ss; ${p}h host`), a script file the session wrote and then ran, or text
//! piped into a shell's stdin is not visible to any static matcher; neither is
//! the SSH transport `git fetch` uses under the hood (forge transport is not a
//! remote shell). The credential-store matcher anchors on a home directory
//! (`~`, `$HOME`, the hook's own `$HOME`, `/root`, `/home/<u>`, `/Users/<u>`)
//! or a relative path that *starts* with a credential directory, so a path
//! built from variables is not seen either. Those limits are stated in
//! `guard-hooks.md`; over-denying an obfuscated command is preferred to
//! under-denying a plain one, never the reverse.

/// Prefix of every deny answer `role-tool-policy check` prints. The hooks
/// match on it, so an exit 1 from something else is never mistaken for a
/// verdict.
pub const CHECK_DENY_PREFIX: &str = "BLOCKED [role-tool-policy]";

/// Programs that are the `remote-shell` capability when they are a command
/// word — the same set as the `Bash(<prog>:*)` specs.
pub const REMOTE_SHELL_PROGRAMS: [&str; 9] = [
    "ssh",
    "scp",
    "sftp",
    "ssh-add",
    "ssh-agent",
    "ssh-keygen",
    "ssh-keyscan",
    "ssh-copy-id",
    "autossh",
];

/// Programs that are the `cloud-cli` capability when they are a command word.
pub const CLOUD_CLI_PROGRAMS: [&str; 10] = [
    "aws", "gcloud", "az", "doctl", "flyctl", "fly", "wrangler", "heroku", "kubectl", "eksctl",
];

/// Home-relative credential locations (`credential-store`), as component
/// lists. A path is inside one when its home-relative components start with
/// the entry's components.
pub const CREDENTIAL_DIRS: [&[&str]; 12] = [
    &[".ssh"],
    &[".aws"],
    &[".gnupg"],
    &[".netrc"],
    &[".git-credentials"],
    &[".kube"],
    &[".azure"],
    &[".config", "gh"],
    &[".config", "gcloud"],
    &[".docker", "config.json"],
    &[".loom", "tokens"],
    &[".claude", ".credentials.json"],
];

/// Shells whose `-c` argument (or here-string) is itself a command line.
const SHELLS: [&str; 7] = ["sh", "bash", "zsh", "dash", "ksh", "mksh", "fish"];

/// Reserved words and grouping tokens that can precede a command word.
const KEYWORDS: [&str; 13] = [
    "!", "{", "}", "if", "then", "else", "elif", "do", "while", "until", "time", "coproc", "in",
];

/// Wrappers that run their remaining arguments as a command after skipping
/// their own `-`options. Option VALUES are handled per wrapper below where a
/// wrapper takes one.
const PLAIN_WRAPPERS: [&str; 9] = [
    "builtin",
    "exec",
    "nohup",
    "setsid",
    "unbuffer",
    "caffeinate",
    "chronic",
    "doas",
    "sudo",
];

/// Nesting cap for `$(…)` / `bash -c` / `eval` recursion. Real commands nest
/// two or three deep; the cap only bounds a pathological input, and a command
/// past it is matched on what was seen so far.
const MAX_DEPTH: usize = 16;

/// One sensitive capability a command or path would exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// A name from [`super::CAPABILITY_NAMESPACE`].
    pub capability: &'static str,
    /// What matched, for the deny message (`ssh`, `gh secret`, a path, …).
    pub evidence: String,
}

impl Hit {
    fn new(capability: &'static str, evidence: impl Into<String>) -> Self {
        Self {
            capability,
            evidence: evidence.into(),
        }
    }
}

/// Every capability hit in a Bash command line, in source order.
///
/// `home` is the hook process's `$HOME` (the same user the command runs as),
/// used to recognize an absolute path into the home directory.
#[must_use]
pub fn command_hits(command: &str, home: Option<&str>) -> Vec<Hit> {
    let mut hits = Vec::new();
    analyze_str(command, home, 0, &mut hits);
    hits
}

/// The `credential-store` hit for an Edit/Write target path, if any.
#[must_use]
pub fn path_hit(path: &str, home: Option<&str>) -> Option<Hit> {
    is_credential_path(path, home).then(|| Hit::new("credential-store", path))
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Plain,
    /// The target of `<`, `>`, `>>`, … — a path, never a command word.
    Redirect,
    /// The operand of `<<<` — text a command reads on stdin.
    HereString,
}

#[derive(Debug, Clone)]
struct Word {
    text: String,
    kind: WordKind,
}

struct Lexed {
    segments: Vec<Vec<Word>>,
    /// Bodies of `$(…)`, backticks and `<(…)`/`>(…)`: commands of their own.
    nested: Vec<String>,
}

/// Split a command line into simple commands (word lists).
// The trailing `end_segment!()` resets state a final time that nothing reads.
#[allow(unused_assignments)]
fn lex(input: &str) -> Lexed {
    let c: Vec<char> = input.chars().collect();
    let mut out = Lexed {
        segments: Vec::new(),
        nested: Vec::new(),
    };
    let mut seg: Vec<Word> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut pending = WordKind::Plain;
    let mut i = 0;

    macro_rules! end_word {
        () => {
            if in_word {
                seg.push(Word {
                    text: std::mem::take(&mut cur),
                    kind: pending,
                });
                pending = WordKind::Plain;
                in_word = false;
            }
        };
    }
    macro_rules! end_segment {
        () => {
            end_word!();
            pending = WordKind::Plain;
            if !seg.is_empty() {
                out.segments.push(std::mem::take(&mut seg));
            }
        };
    }

    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        match ch {
            '\\' => {
                match next {
                    Some('\n') => {}
                    Some(n) => {
                        cur.push(n);
                        in_word = true;
                    }
                    None => {}
                }
                i += 2;
                continue;
            }
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    cur.push(c[i]);
                    i += 1;
                }
                in_word = true;
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    match c[i] {
                        '\\' if matches!(c.get(i + 1), Some('$' | '`' | '"' | '\\' | '\n')) => {
                            if c[i + 1] != '\n' {
                                cur.push(c[i + 1]);
                            }
                            i += 2;
                            continue;
                        }
                        '$' if c.get(i + 1) == Some(&'(') => {
                            let (body, end) = take_balanced(&c, i + 2);
                            out.nested.push(body);
                            i = end;
                            continue;
                        }
                        '$' if c.get(i + 1) == Some(&'{') => {
                            let end = take_brace(&c, i + 2);
                            cur.extend(&c[i..end]);
                            i = end;
                            continue;
                        }
                        '`' => {
                            let (body, end) = take_backtick(&c, i + 1);
                            out.nested.push(body);
                            i = end;
                            continue;
                        }
                        other => cur.push(other),
                    }
                    i += 1;
                }
                in_word = true;
            }
            '$' if next == Some('(') => {
                let (body, end) = take_balanced(&c, i + 2);
                out.nested.push(body);
                in_word = true;
                i = end;
                continue;
            }
            '$' if next == Some('{') => {
                let end = take_brace(&c, i + 2);
                cur.extend(&c[i..end]);
                in_word = true;
                i = end;
                continue;
            }
            '$' if next == Some('\'') => {
                // ANSI-C quoting: kept literally, escapes undecoded.
                i += 2;
                while i < c.len() && c[i] != '\'' {
                    if c[i] == '\\' && i + 1 < c.len() {
                        cur.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    cur.push(c[i]);
                    i += 1;
                }
                in_word = true;
            }
            '`' => {
                let (body, end) = take_backtick(&c, i + 1);
                out.nested.push(body);
                in_word = true;
                i = end;
                continue;
            }
            '#' if !in_word => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ' ' | '\t' => end_word!(),
            '\n' | ';' | '&' | '|' | '(' | ')' => {
                end_segment!();
            }
            '<' | '>' if next == Some('(') => {
                end_word!();
                let (body, end) = take_balanced(&c, i + 2);
                out.nested.push(body);
                i = end;
                continue;
            }
            '<' | '>' => {
                end_word!();
                let mut run = 1;
                while matches!(c.get(i + run), Some('<' | '>')) {
                    run += 1;
                }
                pending = if ch == '<' && run >= 3 {
                    WordKind::HereString
                } else {
                    WordKind::Redirect
                };
                i += run;
                continue;
            }
            other => {
                cur.push(other);
                in_word = true;
            }
        }
        i += 1;
    }
    end_segment!();
    out
}

/// The body of a `(`-opened region starting at `start` (just past the `(`),
/// and the index just past its matching `)`. Quotes inside are skipped so a
/// `)` in a string does not close it. An unterminated region runs to the end.
fn take_balanced(c: &[char], start: usize) -> (String, usize) {
    let mut depth = 1usize;
    let mut i = start;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (c[start..i].iter().collect(), i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (c[start.min(c.len())..].iter().collect(), c.len())
}

/// Index just past the `}` closing a `${…}` whose body starts at `start`.
fn take_brace(c: &[char], start: usize) -> usize {
    let mut depth = 1usize;
    let mut i = start;
    while i < c.len() {
        match c[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    c.len()
}

/// The body of a backtick substitution starting at `start`, and the index
/// just past its closing backtick.
fn take_backtick(c: &[char], start: usize) -> (String, usize) {
    let mut body = String::new();
    let mut i = start;
    while i < c.len() && c[i] != '`' {
        if c[i] == '\\' && i + 1 < c.len() {
            body.push(c[i + 1]);
            i += 2;
            continue;
        }
        body.push(c[i]);
        i += 1;
    }
    (body, (i + 1).min(c.len()))
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

fn analyze_str(command: &str, home: Option<&str>, depth: usize, hits: &mut Vec<Hit>) {
    if depth > MAX_DEPTH {
        return;
    }
    let lexed = lex(command);
    for seg in &lexed.segments {
        for w in seg {
            // Every word, wherever it sits, can name a credential store —
            // including `--key=~/.ssh/id` and `host:~/.ssh/x`.
            for piece in w.text.split(['=', ':']) {
                if is_credential_path(piece, home) {
                    hits.push(Hit::new("credential-store", w.text.clone()));
                    break;
                }
            }
        }
        for w in seg.iter().filter(|w| w.kind == WordKind::HereString) {
            // `<<<` feeds text to stdin; when that stdin is a shell it is a
            // command line. Analyzing it for any command over-denies only
            // a here-string that itself mentions a capability.
            if seg_runs_shell(seg) {
                analyze_str(&w.text, home, depth + 1, hits);
            }
        }
        let argv: Vec<String> = seg
            .iter()
            .filter(|w| w.kind == WordKind::Plain)
            .map(|w| w.text.clone())
            .collect();
        analyze_argv(&argv, home, depth, hits);
    }
    for body in &lexed.nested {
        analyze_str(body, home, depth + 1, hits);
    }
}

fn seg_runs_shell(seg: &[Word]) -> bool {
    seg.iter()
        .find(|w| w.kind == WordKind::Plain && !is_assignment(&w.text))
        .is_some_and(|w| SHELLS.contains(&basename(&w.text)))
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Index of the first word after a run of `-`options, where each option in
/// `with_value` consumes the following word too. `--` ends the options.
fn skip_options(argv: &[String], with_value: &[&str]) -> usize {
    let mut j = 0;
    while j < argv.len() {
        let w = argv[j].as_str();
        if w == "--" {
            return j + 1;
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        j += if with_value.contains(&w) { 2 } else { 1 };
    }
    j.min(argv.len())
}

#[allow(clippy::too_many_lines)]
fn analyze_argv(argv: &[String], home: Option<&str>, depth: usize, hits: &mut Vec<Hit>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut i = 0;
    while i < argv.len() && (is_assignment(&argv[i]) || KEYWORDS.contains(&argv[i].as_str())) {
        i += 1;
    }
    let Some(word) = argv.get(i) else { return };
    let prog = basename(word);
    let rest = &argv[i + 1..];
    let recurse = |from: usize, hits: &mut Vec<Hit>| {
        if from < rest.len() {
            analyze_argv(&rest[from..], home, depth + 1, hits);
        }
    };

    if PLAIN_WRAPPERS.contains(&prog) {
        let j = skip_options(rest, &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U", "-a"]);
        recurse(j, hits);
        return;
    }
    match prog {
        "command" => {
            // `command -v ssh` / `command -V ssh` only LOOK UP a name.
            if rest
                .first()
                .is_some_and(|o| o.starts_with('-') && (o.contains('v') || o.contains('V')))
            {
                return;
            }
            recurse(skip_options(rest, &[]), hits);
        }
        "env" => {
            let mut j = 0;
            while j < rest.len() {
                let w = rest[j].as_str();
                if w == "-S" || w == "--split-string" {
                    analyze_str(&rest[j + 1..].join(" "), home, depth + 1, hits);
                    return;
                }
                if let Some(s) = w
                    .strip_prefix("--split-string=")
                    .or_else(|| w.strip_prefix("-S").filter(|s| !s.is_empty()))
                {
                    let mut line = s.to_string();
                    for more in &rest[j + 1..] {
                        line.push(' ');
                        line.push_str(more);
                    }
                    analyze_str(&line, home, depth + 1, hits);
                    return;
                }
                if matches!(w, "-u" | "--unset" | "-C" | "--chdir") {
                    j += 2;
                } else if w == "--" {
                    j += 1;
                    break;
                } else if w.starts_with('-') || is_assignment(w) {
                    j += 1;
                } else {
                    break;
                }
            }
            recurse(j, hits);
        }
        "nice" | "ionice" | "stdbuf" | "chrt" | "taskset" => {
            let j = skip_options(rest, &["-n", "-c", "-p", "-i", "-o", "-e"]);
            // chrt/taskset take a priority / mask operand before the command.
            let j = if matches!(prog, "chrt" | "taskset") {
                j + 1
            } else {
                j
            };
            recurse(j, hits);
        }
        "timeout" => {
            // Options, then the DURATION operand, then the command.
            let j = skip_options(rest, &["-s", "-k", "--signal", "--kill-after"]);
            recurse(j + 1, hits);
        }
        "xargs" => {
            let j = skip_options(
                rest,
                &[
                    "-I",
                    "-n",
                    "-P",
                    "-L",
                    "-s",
                    "-d",
                    "-E",
                    "-a",
                    "--max-args",
                    "--max-procs",
                    "--max-lines",
                    "--delimiter",
                    "--arg-file",
                    "--replace",
                ],
            );
            recurse(j, hits);
        }
        "watch" => {
            let j = skip_options(rest, &["-n", "--interval", "-d"]);
            if j < rest.len() {
                analyze_str(&rest[j..].join(" "), home, depth + 1, hits);
            }
        }
        "flock" => {
            let j = skip_options(rest, &["-w", "--timeout", "-E", "--conflict-exit-code"]);
            // flock <lockfile> -c "<cmd>"  |  flock <lockfile> <cmd> …
            match rest.get(j + 1).map(String::as_str) {
                Some("-c" | "--command") => {
                    if let Some(s) = rest.get(j + 2) {
                        analyze_str(s, home, depth + 1, hits);
                    }
                }
                _ => recurse(j + 1, hits),
            }
        }
        "eval" => analyze_str(&rest.join(" "), home, depth + 1, hits),
        "find" => {
            let mut k = 0;
            while k < rest.len() {
                if matches!(rest[k].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                    let end = rest[k + 1..]
                        .iter()
                        .position(|w| w == ";" || w == "+")
                        .map_or(rest.len(), |p| k + 1 + p);
                    analyze_argv(&rest[k + 1..end], home, depth + 1, hits);
                    k = end;
                }
                k += 1;
            }
        }
        "gh" => {
            if let Some(evidence) = gh_forge_secret(rest) {
                hits.push(Hit::new("forge-secrets", evidence));
            }
        }
        p if SHELLS.contains(&p) => {
            // `bash -c '<cmd>'`, `sh -ec '<cmd>'`, `bash -o pipefail -c …`.
            let mut j = 0;
            while j < rest.len() {
                let w = rest[j].as_str();
                if matches!(w, "-o" | "+o" | "-O" | "+O") {
                    j += 2;
                    continue;
                }
                if w.starts_with('-') && !w.starts_with("--") && w[1..].contains('c') {
                    if let Some(s) = rest.get(j + 1) {
                        analyze_str(s, home, depth + 1, hits);
                    }
                    return;
                }
                if !w.starts_with('-') && !w.starts_with('+') {
                    break;
                }
                j += 1;
            }
        }
        p if REMOTE_SHELL_PROGRAMS.contains(&p) => hits.push(Hit::new("remote-shell", p)),
        p if CLOUD_CLI_PROGRAMS.contains(&p) => hits.push(Hit::new("cloud-cli", p)),
        _ => {}
    }
}

/// The `forge-secrets` evidence for `gh <rest…>`, if it is one.
fn gh_forge_secret(rest: &[String]) -> Option<String> {
    let mut words = rest
        .iter()
        .map(String::as_str)
        .filter(|w| !w.starts_with('-'));
    let sub = words.next()?;
    match sub {
        "secret" | "variable" => Some(format!("gh {sub}")),
        "auth" => {
            let verb = words.next().unwrap_or("");
            let shows_token = rest
                .iter()
                .any(|w| w == "-t" || w == "--show-token" || w == "--show-token=true");
            match verb {
                "token" | "login" | "refresh" | "logout" | "setup-git" => {
                    Some(format!("gh auth {verb}"))
                }
                "status" if shows_token => Some("gh auth status --show-token".to_string()),
                _ => None,
            }
        }
        "api" => rest
            .iter()
            .find(|w| {
                let lower = w.to_ascii_lowercase();
                lower.contains("/secrets") || lower.contains("/variables")
            })
            .map(|w| format!("gh api {w}")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Credential paths
// ---------------------------------------------------------------------------

/// `true` when `word` names a path inside a home credential store.
fn is_credential_path(word: &str, home: Option<&str>) -> bool {
    let Some((rel, anchored)) = home_relative(word, home) else {
        return false;
    };
    let comps = normalize(rel);
    CREDENTIAL_DIRS.iter().any(|dir| {
        dir.len() <= comps.len()
            && dir.iter().zip(&comps).all(|(want, got)| {
                // Glob widening only below an explicit home anchor: a bare
                // relative `*` or `.*` is far more often a repo glob than a
                // reach into `$HOME`.
                if anchored {
                    glob_match(got, want)
                } else {
                    got == want
                }
            })
    })
}

/// The part of `word` below a home directory plus `true`, or the whole word
/// plus `false` when it is a relative path (resolved against an unknown cwd,
/// which is `$HOME` often enough — `cd && cat .ssh/id_rsa` — to count).
fn home_relative<'a>(word: &'a str, home: Option<&str>) -> Option<(&'a str, bool)> {
    if word.is_empty() {
        return None;
    }
    for prefix in ["~/", "$HOME/", "${HOME}/"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            return Some((rest, true));
        }
    }
    if let Some(rest) = word.strip_prefix('~') {
        // `~user/…`
        return rest.split_once('/').map(|(_, r)| (r, true));
    }
    if let Some(h) = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty())
    {
        if let Some(rest) = word.strip_prefix(h).and_then(|r| r.strip_prefix('/')) {
            return Some((rest, true));
        }
    }
    if let Some(rest) = word.strip_prefix("/root/") {
        return Some((rest, true));
    }
    for base in ["/home/", "/Users/"] {
        if let Some(rest) = word.strip_prefix(base) {
            return rest.split_once('/').map(|(_, r)| (r, true));
        }
    }
    if word.starts_with('/') || word.starts_with('$') {
        return None;
    }
    Some((word, false))
}

/// Lexically normalize a relative path into components (`.`/empty dropped,
/// `..` pops).
fn normalize(rel: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Shell-glob match of one path component: `*`, `?`, and `[…]` as one char.
/// A literal component compares exactly, so this only widens what a glob can
/// reach (`~/.s*h`, `~/.*/id_rsa`), never what a plain name does.
fn glob_match(pattern: &str, name: &str) -> bool {
    // As in the shell, a wildcard never matches a leading dot.
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && p[pi] == '[' {
            if let Some(close) = p[pi..].iter().position(|&c| c == ']') {
                pi += close + 1;
                ni += 1;
                continue;
            }
        }
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "command_match_tests.rs"]
mod tests;
