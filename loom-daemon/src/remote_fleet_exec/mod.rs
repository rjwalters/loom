//! `loom-daemon guards remote-exec --for-command` (#11568) — the classifier
//! behind the `loom:remote-fleet-exec` `PreToolUse` rule in
//! `defaults/hooks/guard-loom-workflow.sh`.
//!
//! The incident: a session wrote `rebase --abort` / `reset --hard` into a
//! scratch script and ran it as `ssh ubuntu@<tailnet-ip> 'bash -s' < fix.sh`
//! against three fleet workers. No guard fired — the local guards see neither
//! the remote target nor the script's contents.
//!
//! This verb finds every `ssh` / `tailscale ssh` / `scp` / `rsync` / `aws ssm
//! send-command|start-session` invocation in a command (through `bash -c`,
//! `eval`, wrappers, `xargs` / `parallel` fan-out, `$( … )`, heredocs fed
//! to a local shell, and the remote command of another hop — `ssh bastion
//! ssh <fleet> …`), works out what it
//! would run or change remotely — the inline remote command, and for a remote
//! shell reading stdin, the `< file`, heredoc, here-string or `cat file |`
//! script it is fed — and answers ASK when that payload is not read-only (or
//! cannot be resolved, including one built at run time — see [`dynamic`]) **and** the target is a fleet host (see [`roster`]).
//! A push of files onto a fleet host (`scp`/`rsync` with a remote
//! destination) is an outward change and asks too.
//!
//! The hook turns exit 1 + [`ASK_PREFIX`] into a real permission prompt (an
//! `ask` decision), never a model judgment. Exit 0 allows. A daemon too old to
//! have the verb exits 2, which the hook treats as allow.

pub mod dynamic;
pub mod lex;
pub mod roster;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use lex::{Pipeline, Simple, Stdin, Word};
use roster::Roster;

/// The first bytes of every ask; the hook asks only on this prefix.
pub const ASK_PREFIX: &str = "ASK [remote-fleet-exec]";
/// The category toggle (`guards.remoteFleetExec`, default on).
pub const TOGGLE_KEY: &str = "guards.remoteFleetExec";
/// The toggle's env override (Claude Code settings `env`).
pub const TOGGLE_ENV: &str = "LOOM_GUARD_REMOTE_FLEET_EXEC";

/// Largest script file read for classification; a bigger one is unresolvable.
const MAX_SCRIPT_BYTES: u64 = 1 << 20;
/// How deep `bash -c` / `eval` / `$( … )` / nested-hop recursion goes. A
/// remote hop nested deeper than this is undeterminable (see [`Scan::text`]).
const MAX_DEPTH: usize = 6;

/// Where an invocation is aimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Host(String),
    /// A host spelled with a variable or substitution — unknowable here, so
    /// it is treated as a fleet host.
    Dynamic(String),
    Instances(Vec<String>),
    /// `aws ssm … --targets Key=…` — resolved by AWS at run time.
    TagTargets,
}

/// What the remote side would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// A destructive pattern: label and where it was found.
    Destructive {
        pattern: &'static str,
        source: String,
    },
    /// A script the remote side executes whose contents cannot be read.
    Unresolvable(String),
    /// Files copied onto the remote host.
    Push,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub transport: &'static str,
    pub target: Target,
    pub payload: Payload,
}

/// Inputs the classifier needs from the environment.
pub struct Context<'a> {
    pub cwd: &'a Path,
    pub home: Option<PathBuf>,
}

/// The category toggle: env (`0|false|no` / `1|true|yes`) beats config
/// (`guards.remoteFleetExec`: `false` or `"false"` disables); default on.
#[must_use]
pub fn toggle_enabled(env: Option<&str>, config: Option<&Value>) -> bool {
    match env {
        Some("0" | "false" | "no") => return false,
        Some("1" | "true" | "yes") => return true,
        _ => {}
    }
    !matches!(config, Some(Value::Bool(false)))
        && !matches!(config, Some(Value::String(s)) if s == "false")
}

// ===========================================================================
// Destructive patterns
// ===========================================================================

#[allow(clippy::expect_used)]
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let span = r"[^\n;&|]*";
    vec![
        ("reset --hard", re(&format!(r"\breset\b{span}\s--hard\b"))),
        ("clean -f", re(&format!(r"\bclean\b{span}\s(?:-[A-Za-z]*f[A-Za-z]*|--force)\b"))),
        (
            "rm -r",
            re(&format!(r"(?:^|[\s;&|(])rm\b{span}\s(?:-[A-Za-z]*[rR][A-Za-z]*|--recursive)\b")),
        ),
        ("rebase --abort", re(&format!(r"\brebase\b{span}\s--abort\b"))),
        ("stash drop|clear", re(r"\bstash\s+(?:drop|clear)\b")),
        (
            "branch -D",
            re(&format!(r"\bbranch\b{span}\s(?:-D\b|--delete\s+--force\b|-df\b|-fd\b)")),
        ),
        (
            "push --force",
            re(&format!(
                r"\bpush\b{span}\s(?:--force(?:-with-lease)?\b|-f\b|\+[A-Za-z0-9_./-])"
            )),
        ),
        (
            "systemctl stop|disable|mask",
            re(&format!(r"\bsystemctl\b{span}\s(?:stop|disable|mask|kill)\b")),
        ),
        (
            "launchctl bootout|unload|disable",
            re(&format!(r"\blaunchctl\b{span}\s(?:bootout|unload|disable|remove)\b")),
        ),
        ("reboot|shutdown|poweroff", re(r"(?:^|[\s;&|(])(?:reboot|shutdown|poweroff)\b")),
    ]
});

/// `~/.loom`, `~/loom-daemon` (and `$HOME` / absolute-home spellings).
const PROTECTED: &str = r#"["']?(?:~|\$HOME|\$\{HOME\}|/home/[^/\s]+|/Users/[^/\s]+|/root)/(?:\.loom|loom-daemon)(?:/|\b)"#;

static REDIRECT_INTO_PROTECTED: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r">>?\s*{PROTECTED}")));
static PROTECTED_PATH: LazyLock<Regex> = LazyLock::new(|| re(PROTECTED));
static WRITE_VERB: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?:^|[\s;&|(])(?:tee|cp|mv|rm|install|ln|truncate|chmod|chown|unlink|rsync|sed\s+(?:-[A-Za-z]*\s+)*-i)\b",
    )
});

/// The first destructive pattern in `text`, ignoring `#` comment lines.
#[must_use]
pub fn destructive_pattern(text: &str) -> Option<&'static str> {
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        if let Some((label, _)) = PATTERNS.iter().find(|(_, r)| r.is_match(line)) {
            return Some(label);
        }
        if REDIRECT_INTO_PROTECTED.is_match(line)
            || line
                .split([';', '&', '|'])
                .any(|seg| PROTECTED_PATH.is_match(seg) && WRITE_VERB.is_match(seg))
        {
            return Some("write under ~/.loom or ~/loom-daemon");
        }
    }
    None
}

// ===========================================================================
// Finding invocations
// ===========================================================================

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

fn is_assignment(t: &str) -> bool {
    t.split_once('=').is_some_and(|(n, _)| {
        !n.is_empty()
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !n.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Index of the command word after assignments and prefix wrappers.
fn command_index(words: &[Word]) -> Option<usize> {
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let t = w.text.as_str();
        let takes: &[&str] = match base(t) {
            "sudo" | "doas" => &["-u", "-g", "-C", "-h", "-p", "-D"],
            "env" => &["-u", "-C", "-S", "--unset", "--chdir"],
            "nice" | "ionice" => &["-n", "-c"],
            "timeout" => &["-s", "-k", "--signal", "--kill-after"],
            "stdbuf" => &["-i", "-o", "-e"],
            "nohup" | "exec" | "command" | "time" | "caffeinate" | "builtin" => &[],
            _ if is_assignment(t) || matches!(t, "!" | "{" | "}" | "then" | "do" | "else") => {
                i += 1;
                continue;
            }
            _ => return Some(i),
        };
        let wrapper = base(t).to_string();
        i += 1;
        while let Some(a) = words.get(i).map(|w| w.text.as_str()) {
            if takes.contains(&a) {
                i += 2;
            } else if a.starts_with('-') || (wrapper == "env" && is_assignment(a)) {
                i += 1;
            } else {
                break;
            }
        }
        if wrapper == "timeout" && words.get(i).is_some() {
            i += 1;
        }
    }
    None
}

/// A shell invocation that reads its script from stdin (`bash`, `bash -s`,
/// `sh -l -s -- a b`).
fn is_stdin_shell(words: &[Word]) -> bool {
    let Some(ci) = command_index(words) else {
        return false;
    };
    if !SHELLS.contains(&base(&words[ci].text)) {
        return false;
    }
    for w in &words[ci + 1..] {
        let a = w.text.as_str();
        if a == "-s"
            || (a.len() > 1 && a.starts_with('-') && !a.starts_with("--") && a[1..].contains('s'))
        {
            return true;
        }
        if a == "--" || !(a.starts_with('-') || a.starts_with('+')) {
            return false;
        }
    }
    true
}

/// The script word of `sh -c SCRIPT` (any short-option cluster holding `c`).
fn shell_c_script(rest: &[Word]) -> Option<&Word> {
    let pos = rest.iter().position(|a| {
        a.text.len() > 1
            && a.text.starts_with('-')
            && !a.text.starts_with("--")
            && a.text[1..].contains('c')
    })?;
    rest.get(pos + 1)
}

/// Does any simple command in `text` read a shell script from stdin?
fn text_runs_stdin_shell(text: &str) -> bool {
    lex::pipelines(text)
        .iter()
        .flatten()
        .any(|s| is_stdin_shell(&s.words))
}

struct Scan<'a> {
    ctx: &'a Context<'a>,
    findings: Vec<Finding>,
    /// The script an enclosing hop feeds on stdin, already classified
    /// (`None` = nothing, or read-only). A nested command with no stdin of
    /// its own inherits it: `ssh bastion ssh <fleet> bash -s < fix.sh` runs
    /// `fix.sh` on the fleet host, not on the bastion.
    fed: Option<Payload>,
    /// Scanning a command that runs on an intermediate host: its files are
    /// that host's, so none can be read here.
    remote: bool,
}

/// A remote-execution word, for the too-deep check.
static TRANSPORT_WORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b(?:ssh|scp|rsync|ssm|xargs|parallel)\b"));

/// Every remote invocation in `command` whose payload is not read-only.
#[must_use]
pub fn findings(command: &str, ctx: &Context<'_>) -> Vec<Finding> {
    let mut scan = Scan {
        ctx,
        findings: Vec::new(),
        fed: None,
        remote: false,
    };
    scan.text(command, 0);
    scan.findings
}

impl Scan<'_> {
    fn text(&mut self, text: &str, depth: usize) {
        if depth > MAX_DEPTH {
            // Too deep to follow: if it could still reach a host, it is
            // undeterminable — and any hop could be a fleet host.
            if TRANSPORT_WORD.is_match(text) {
                self.push(
                    "nested remote command",
                    Target::Dynamic(format!("a hop nested more than {MAX_DEPTH} levels deep")),
                    Payload::Unresolvable(format!("`{}`", text.trim())),
                );
            }
            return;
        }
        for pipeline in lex::pipelines(text) {
            self.pipeline(&pipeline, depth);
        }
    }

    fn pipeline(&mut self, p: &Pipeline, depth: usize) {
        for (i, stage) in p.iter().enumerate() {
            for w in stage.words.iter().filter(|w| w.dynamic) {
                for inner in lex::substitutions(&w.text) {
                    self.text(&inner, depth + 1);
                }
            }
            let producer = i.checked_sub(1).and_then(|j| p.get(j));
            self.simple(stage, producer, depth);
        }
    }

    fn simple(&mut self, stage: &Simple, producer: Option<&Simple>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(ci) = command_index(&stage.words) else {
            return;
        };
        let word = base(&stage.words[ci].text);
        let rest = &stage.words[ci + 1..];
        match word {
            "eval" => {
                let joined: Vec<&str> = rest.iter().map(|w| w.text.as_str()).collect();
                self.text(&joined.join(" "), depth + 1);
            }
            w if SHELLS.contains(&w) => {
                if let Some(script) = shell_c_script(rest) {
                    self.text(&script.text, depth + 1);
                } else if is_stdin_shell(&stage.words) {
                    match &stage.stdin {
                        Some(Stdin::Heredoc { body, .. }) => self.text(body, depth + 1),
                        Some(Stdin::HereString(w)) => self.text(&w.text, depth + 1),
                        _ => {}
                    }
                }
            }
            "ssh" => self.ssh("ssh", rest, stage, producer, depth),
            "tailscale" => {
                // `tailscale [--socket=…] ssh [ssh options] dest cmd` —
                // Tailscale's own ssh, the native one on a tailnet fleet.
                let sub = rest.iter().position(|w| !w.text.starts_with('-'));
                if let Some(sub) = sub.filter(|&k| rest[k].text == "ssh") {
                    self.ssh("tailscale ssh", &rest[sub + 1..], stage, producer, depth);
                }
            }
            "xargs" | "parallel" => {
                // Fan-out: the per-input command, its placeholders made
                // dynamic (hosts or commands read from stdin). Its stdin
                // is the input list, not a script — unless `xargs -a`.
                if let Some(fan) = dynamic::fan_out(word, rest) {
                    let stdin = stage.stdin.clone().filter(|_| fan.passes_stdin);
                    let inner = Simple {
                        words: fan.words,
                        stdin,
                    };
                    // Without `-a`, the per-input command does not get
                    // `xargs`'s stdin, so it inherits no fed script either.
                    let fed = if fan.passes_stdin {
                        self.fed.clone()
                    } else {
                        None
                    };
                    let outer = std::mem::replace(&mut self.fed, fed);
                    self.simple(&inner, None, depth + 1);
                    self.fed = outer;
                }
            }
            "scp" => self.copy(
                "scp",
                rest,
                &["-c", "-F", "-i", "-J", "-l", "-o", "-P", "-S", "-D", "-X"],
            ),
            "rsync" => self.copy("rsync", rest, RSYNC_ARG_FLAGS),
            "aws" => self.aws(rest, depth),
            _ => {}
        }
    }

    fn push(&mut self, transport: &'static str, target: Target, payload: Payload) {
        self.findings.push(Finding {
            transport,
            target,
            payload,
        });
    }

    fn ssh(
        &mut self,
        transport: &'static str,
        args: &[Word],
        stage: &Simple,
        producer: Option<&Simple>,
        depth: usize,
    ) {
        let (i, terminated) = skip_ssh_options(args, 0);
        let Some(dest) = args.get(i) else {
            return;
        };
        // OpenSSH parses options after the destination too (`ssh host -t
        // cmd`), unless a `--` ended them.
        let start = if terminated {
            i + 1
        } else {
            skip_ssh_options(args, i + 1).0
        };
        let target = host_target(dest, true);
        let local: Vec<&Word> = args.get(start..).unwrap_or_default().iter().collect();
        let remote: Vec<&str> = local.iter().map(|w| w.text.as_str()).collect();
        let remote = remote.join(" ");
        // What this hop feeds the remote side on stdin, for both the remote
        // shell here and any hop nested inside the remote command.
        let fed = self.input_payload(stage, producer);
        self.nested(&remote, fed.clone(), depth);
        if let Some(pattern) = destructive_pattern(&remote) {
            let source = "the inline remote command".to_string();
            self.push(transport, target, Payload::Destructive { pattern, source });
            return;
        }
        if remote.trim().is_empty() || text_runs_stdin_shell(&remote) {
            if let Some(payload) = fed {
                self.push(transport, target, payload);
                return;
            }
        }
        if let Some(why) = dynamic::undeterminable(&remote, &local) {
            let src = format!("the inline remote command ({why})");
            self.push(transport, target, Payload::Unresolvable(src));
        }
    }

    /// Scan a command that runs on another host as a command line of its
    /// own, whatever that host is: `ssh bastion ssh <fleet> '…'` and
    /// `ssh devvm "tailscale ssh <fleet> …"` reach a fleet host through a
    /// hop that is not one. `fed` is what the outer hop feeds on stdin.
    fn nested(&mut self, remote: &str, fed: Option<Payload>, depth: usize) {
        if remote.trim().is_empty() {
            return;
        }
        let outer_fed = std::mem::replace(&mut self.fed, fed);
        let outer_remote = std::mem::replace(&mut self.remote, true);
        self.text(remote, depth + 1);
        self.fed = outer_fed;
        self.remote = outer_remote;
    }

    /// The script on this stage's stdin: its own redirection or producer,
    /// else whatever an enclosing hop feeds it.
    fn input_payload(&self, stage: &Simple, producer: Option<&Simple>) -> Option<Payload> {
        if stage.stdin.is_none() && producer.is_none() {
            return self.fed.clone();
        }
        self.stdin_payload(stage, producer)
    }

    /// A redirected stdin as a script: a file is read, a heredoc / here-string
    /// is checked as text, and a body built at run time is undeterminable.
    fn stdin_script(&self, stdin: &Stdin) -> Option<Payload> {
        match stdin {
            Stdin::File(w) => self.file_payload(w),
            Stdin::Heredoc { body, expands } => {
                let local = Word {
                    text: body.clone(),
                    dynamic: *expands,
                };
                if *expands {
                    script_payload(body, &[&local], "the heredoc script")
                } else {
                    text_payload(body, "the heredoc script")
                }
            }
            Stdin::HereString(w) => script_payload(&w.text, &[w], "the here-string script"),
        }
    }

    /// Classify the script a remote shell would read from stdin. `None` when
    /// it is read-only (or there is no stdin to speak of).
    fn stdin_payload(&self, stage: &Simple, producer: Option<&Simple>) -> Option<Payload> {
        if let Some(stdin) = &stage.stdin {
            return self.stdin_script(stdin);
        }
        let p = producer?;
        let ci = command_index(&p.words)?;
        let args = &p.words[ci + 1..];
        match base(&p.words[ci].text) {
            "cat" => {
                let files: Vec<&Word> = args
                    .iter()
                    .filter(|w| !w.text.starts_with('-') || w.text == "-")
                    .collect();
                if files.is_empty() || files.iter().any(|w| w.text == "-") {
                    return match &p.stdin {
                        Some(stdin) => self.stdin_script(stdin),
                        None => Some(Payload::Unresolvable("stdin of `cat`".into())),
                    };
                }
                files.into_iter().find_map(|w| self.file_payload(w))
            }
            name @ ("echo" | "printf") => {
                let joined: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
                let source = "the piped-in script";
                let local: Vec<&Word> = args.iter().collect();
                script_payload(&joined.join(" "), &local, source).or_else(|| {
                    // `printf FMT "$CMD"`: each argument lands in the format
                    // on its own, so check each expanded one on its own too.
                    let skip = usize::from(name == "printf");
                    args.iter()
                        .skip(skip)
                        .filter(|w| w.dynamic)
                        .find_map(|w| script_payload(&w.text, &[w], source))
                })
            }
            other => Some(Payload::Unresolvable(format!("the output of `{other}`"))),
        }
    }

    fn file_payload(&self, w: &Word) -> Option<Payload> {
        if self.remote {
            let why = "a file on an intermediate host, unreadable here";
            return Some(Payload::Unresolvable(format!("`{}` ({why})", w.text)));
        }
        if w.dynamic {
            return Some(Payload::Unresolvable(format!("`{}` (a variable path)", w.text)));
        }
        let path = self.resolve(&w.text);
        let readable = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file() && m.len() <= MAX_SCRIPT_BYTES)
            .and_then(|_| std::fs::read_to_string(&path).ok());
        match readable {
            Some(body) => text_payload(&body, &format!("`{}`", w.text)),
            None => Some(Payload::Unresolvable(format!("`{}` (not readable)", w.text))),
        }
    }

    fn resolve(&self, p: &str) -> PathBuf {
        if let (Some(rest), Some(home)) = (p.strip_prefix("~/"), self.ctx.home.as_ref()) {
            return home.join(rest);
        }
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.ctx.cwd.join(path)
        }
    }

    /// `scp` / `rsync`: a remote destination is a push onto that host.
    fn copy(&mut self, transport: &'static str, args: &[Word], takes: &[&str]) {
        let mut positional = Vec::new();
        let mut i = 0;
        while let Some(a) = args.get(i) {
            let t = a.text.as_str();
            i += 1;
            if t.starts_with("--") {
                if !t.contains('=') && takes.contains(&t) {
                    i += 1;
                }
            } else if t.len() > 1 && t.starts_with('-') {
                let last = format!("-{}", t.chars().last().unwrap_or('-'));
                if takes.contains(&last.as_str()) {
                    i += 1;
                }
            } else {
                positional.push(a);
            }
        }
        if positional.len() < 2 {
            return;
        }
        if let Some(dest) = positional.last().and_then(|w| remote_spec(w)) {
            self.push(transport, dest, Payload::Push);
        }
    }

    fn aws(&mut self, args: &[Word], depth: usize) {
        let texts: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
        let Some(ssm) = texts.iter().position(|t| *t == "ssm") else {
            return;
        };
        let Some(&sub) = texts.get(ssm + 1) else {
            return;
        };
        if !matches!(sub, "send-command" | "start-session") {
            return;
        }
        let rest = &args[ssm + 2..];
        let mut instances = Vec::new();
        let mut tagged = false;
        let mut params: Vec<Word> = Vec::new();
        let mut i = 0;
        while let Some(w) = rest.get(i) {
            i += 1;
            let (flag, values) = match w.text.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (
                    f.to_string(),
                    vec![Word {
                        text: v.to_string(),
                        dynamic: w.dynamic,
                    }],
                ),
                _ => {
                    let start = i;
                    while rest.get(i).is_some_and(|v| !v.text.starts_with("--")) {
                        i += 1;
                    }
                    (w.text.clone(), rest[start..i].to_vec())
                }
            };
            match flag.as_str() {
                "--instance-ids" | "--target" => {
                    instances.extend(values.into_iter().map(|v| v.text));
                }
                "--targets" => tagged = true,
                "--parameters" | "--cli-input-json" => params.extend(values),
                _ => {}
            }
        }
        let target = if tagged {
            Target::TagTargets
        } else {
            Target::Instances(instances)
        };
        let transport = if sub == "send-command" {
            "aws ssm send-command"
        } else {
            "aws ssm start-session"
        };
        for p in &params {
            if !p.text.starts_with("file://") {
                for command in ssm_commands(&p.text) {
                    self.nested(&command, None, depth);
                }
            }
        }
        for p in params {
            let payload = if let Some(file) = p.text.strip_prefix("file://") {
                self.file_payload(&Word {
                    text: file.to_string(),
                    dynamic: p.dynamic,
                })
            } else {
                text_payload(&p.text, "the --parameters commands")
            };
            if let Some(payload) = payload {
                self.push(transport, target, payload);
                return;
            }
        }
    }
}

const RSYNC_ARG_FLAGS: &[&str] = &[
    "-e",
    "-f",
    "-T",
    "-B",
    "-M",
    "--rsh",
    "--rsync-path",
    "--exclude",
    "--include",
    "--filter",
    "--files-from",
    "--exclude-from",
    "--include-from",
    "--temp-dir",
    "--log-file",
    "--partial-dir",
    "--backup-dir",
    "--suffix",
    "--chmod",
    "--chown",
    "--port",
    "--password-file",
    "--compare-dest",
    "--copy-dest",
    "--link-dest",
    "--timeout",
    "--bwlimit",
    "--max-size",
    "--min-size",
    "--out-format",
    "--info",
    "--debug",
    "--iconv",
    "--remote-option",
    "--usermap",
    "--groupmap",
];

/// Skip ssh options from `args[i]`: the index of the first non-option word,
/// and whether a `--` ended the options.
fn skip_ssh_options(args: &[Word], mut i: usize) -> (usize, bool) {
    const TAKES: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    while let Some(a) = args.get(i) {
        let t = a.text.as_str();
        if t == "--" {
            return (i + 1, true);
        }
        if t.len() < 2 || !t.starts_with('-') {
            break;
        }
        i += 1;
        for (k, ch) in t[1..].char_indices() {
            if TAKES.contains(ch) {
                if k + ch.len_utf8() == t.len() - 1 {
                    i += 1; // argument is the next word
                }
                break;
            }
        }
    }
    (i, false)
}

/// The shell commands in an SSM `--parameters` value: `commands=["a","b"]`
/// (or a bare `commands=a`), each scanned as a command line.
fn ssm_commands(value: &str) -> Vec<String> {
    let v = value.trim();
    let v = v.strip_prefix("commands=").unwrap_or(v);
    let v = v.trim().trim_start_matches('[').trim_end_matches(']');
    v.split("\",\"")
        .map(|c| c.trim().trim_matches('"').replace("\\\"", "\""))
        .filter(|c| !c.trim().is_empty())
        .collect()
}

fn text_payload(text: &str, source: &str) -> Option<Payload> {
    destructive_pattern(text).map(|pattern| Payload::Destructive {
        pattern,
        source: source.to_string(),
    })
}

/// [`text_payload`], then: a script built at run time is undeterminable.
fn script_payload(text: &str, local: &[&Word], source: &str) -> Option<Payload> {
    text_payload(text, source).or_else(|| {
        dynamic::undeterminable(text, local)
            .map(|why| Payload::Unresolvable(format!("{source} ({why})")))
    })
}

/// The host part of an ssh destination (`user@host`, `ssh://user@host:22`).
fn host_target(w: &Word, ssh_dest: bool) -> Target {
    let mut t = w.text.as_str();
    if let Some(rest) = t.strip_prefix("ssh://") {
        t = rest.split('/').next().unwrap_or(rest);
    }
    let host = t.rsplit_once('@').map_or(t, |(_, h)| h);
    let host = if ssh_dest && !host.starts_with('[') && host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host)
    } else {
        host
    };
    if w.dynamic {
        Target::Dynamic(w.text.clone())
    } else {
        Target::Host(host.to_string())
    }
}

/// A `scp`/`rsync` remote operand: `[user@]host:path`, `host::module`,
/// `scp://` / `rsync://`.
fn remote_spec(w: &Word) -> Option<Target> {
    let t = w.text.as_str();
    for scheme in ["scp://", "rsync://"] {
        if let Some(rest) = t.strip_prefix(scheme) {
            let authority = rest.split('/').next().unwrap_or(rest);
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            let host = host.split(':').next().unwrap_or(host);
            return Some(if w.dynamic {
                Target::Dynamic(t.into())
            } else {
                Target::Host(host.into())
            });
        }
    }
    let (hostpart, _) = t.split_once(':')?;
    if hostpart.is_empty() || hostpart.contains('/') {
        return None;
    }
    let host = hostpart.rsplit_once('@').map_or(hostpart, |(_, h)| h);
    Some(if w.dynamic && hostpart.contains('$') {
        Target::Dynamic(t.into())
    } else {
        Target::Host(host.into())
    })
}

// ===========================================================================
// The verb
// ===========================================================================

fn on_fleet(target: &Target, roster: &dyn Roster) -> bool {
    match target {
        Target::Host(h) => roster.is_fleet_host(h),
        Target::Dynamic(_) | Target::TagTargets => true,
        Target::Instances(ids) => ids.iter().any(|i| roster.is_fleet_instance(i)),
    }
}

fn describe_target(t: &Target) -> String {
    match t {
        Target::Host(h) => format!("fleet host `{h}`"),
        Target::Dynamic(d) => {
            format!("`{d}` (a host this guard cannot resolve, so treated as a fleet host)")
        }
        Target::Instances(ids) => format!("fleet instance(s) {}", ids.join(", ")),
        Target::TagTargets => "tag-selected instances (`--targets`, treated as fleet hosts)".into(),
    }
}

fn slug(t: &Target) -> String {
    let raw = match t {
        Target::Host(h) => h.clone(),
        Target::Instances(ids) => ids.first().cloned().unwrap_or_default(),
        _ => String::new(),
    };
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "fleet-host".into()
    } else {
        s
    }
}

/// The ask text: what, where, the approval rule, and the break-glass record.
#[must_use]
pub fn ask_reason(f: &Finding, fleet_repo: Option<&str>, date: &str) -> String {
    let what = match &f.payload {
        Payload::Destructive { pattern, source } => format!("runs `{pattern}` (in {source})"),
        Payload::Unresolvable(src) => {
            format!("runs a remote shell script whose contents cannot be checked: {src}")
        }
        Payload::Push => "copies files onto it".into(),
    };
    let store = fleet_repo.map_or_else(|| "the fleet store repo".to_string(), |r| format!("`{r}`"));
    format!(
        "{ASK_PREFIX}: this {} command {what} on {}. A destructive or outward change to a \
         fleet host needs the operator's explicit yes to THIS action — \"continue\", \"go on\" \
         or silence is not approval; if you have not had that yes, deny this and re-ask once \
         with the action spelled out. If approved, the change must leave a break-glass record \
         (FLEET.md §\"Changing the fleet\"): write `break-glass/{date}-{}.md` in {store} saying \
         what ran, on which hosts, why, and who approved it. Opt-out: guards.remoteFleetExec=false \
         or LOOM_GUARD_REMOTE_FLEET_EXEC=0 (#11568).",
        f.transport,
        describe_target(&f.target),
        slug(&f.target),
    )
}

/// The whole verb for one command: `Some(reason)` to ask.
#[must_use]
pub fn check(
    command: &str,
    ctx: &Context<'_>,
    roster: &dyn Roster,
    toggle: impl FnOnce() -> bool,
    fleet_repo: impl FnOnce() -> Option<String>,
    date: &str,
) -> Option<String> {
    let found = findings(command, ctx)
        .into_iter()
        .find(|f| on_fleet(&f.target, roster))?;
    toggle().then(|| ask_reason(&found, fleet_repo().as_deref(), date))
}

/// [`check`] against the process's own environment, config and roster.
#[must_use]
pub fn check_process(command: &str, cwd: &Path) -> Option<String> {
    let ctx = Context {
        cwd,
        home: std::env::var_os("HOME").map(PathBuf::from),
    };
    let roster = roster::ProcessRoster::new(cwd);
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    check(
        command,
        &ctx,
        &roster,
        || {
            let env = std::env::var(TOGGLE_ENV).ok();
            let config = crate::config_resolver::resolve_effective_config(cwd);
            toggle_enabled(env.as_deref(), crate::config_resolver::get_path(&config, TOGGLE_KEY))
        },
        || crate::fleet_sync::probe_status().map(|s| s.repo),
        &date,
    )
}

#[cfg(test)]
mod tests;
