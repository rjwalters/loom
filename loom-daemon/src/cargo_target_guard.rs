//! The `loom:cargo-target-dir` `PreToolUse` rule (issue #11582): deny a cargo
//! build whose target dir lands somewhere nothing owns.
//!
//! `cargo-target-isolation.md` (#8370) tells every role to build into the
//! `CARGO_TARGET_DIR` Loom exported, or `<worktree>/target`, and never under
//! `/tmp`, `$TMPDIR`, `~`, `~/.cache` or `<repo>/.loom/target-*`. Agents kept
//! doing it anyway — multi-GB `/tmp/target-pr…` and `~/.cache/doctor-…-target`
//! dirs on fleet hosts, days after the rule shipped. A prompt rule broken that
//! often needs enforcement, so `guard-loom-workflow.sh` hands the (masked)
//! command to `loom-daemon cargo-target-dir guard`, which runs [`check`].
//!
//! # What is resolved
//!
//! For each `cargo build|test|check|clippy|run|nextest|…` (also behind `env`,
//! `nohup`, `timeout`, `bash -c`, and `make`), the effective target dir:
//! `--target-dir`, else a `CARGO_TARGET_DIR=` assignment in front of it (or an
//! `export` earlier in the same command), else the `CARGO_TARGET_DIR` the
//! session itself carries — the hook process's own environment, which is
//! Claude Code's. `make` is judged only on an assignment made in this command:
//! it may not run cargo at all, so an inherited value alone proves nothing.
//!
//! # The verdict
//!
//! - **Allow**: unset (cargo uses `<worktree>/target`); under
//!   `<repo>/.loom/targets/`; under the current worktree; the worktree's own
//!   per-worktree dir (`.loom-cargo-target-dir` marker, #8458, or
//!   `LOOM_WORKTREE_CARGO_TARGET_DIR`); anything else not listed below (an
//!   operator's deliberate shared volume, say).
//! - **Deny**: under `/tmp`, `$TMPDIR`, `~/.cache`, `~`, or any
//!   `.loom/target-*`. The message names the Loom-owned dir to use instead.
//!
//! Anything the evaluator cannot resolve — a variable set in an earlier Bash
//! call, an unknown `$( … )` — is an **allow**: like every Loom guard, this
//! resolves ambiguity toward allow, never toward a spurious deny.
//!
//! # Telemetry
//!
//! Every deny is a `guard.cargo_target_dir.denied` instant span carrying the
//! role, the forbidden root by name (`/tmp`, `$TMPDIR`, `~/.cache`, `~`,
//! `.loom/target-*`) and where the value came from — never the command or the
//! path itself. It goes through the execution's trace journal
//! (`LOOM_TRACE_CONTEXT_FILE`), which the daemon drains to its OTLP
//! exporters, the same path a `gh` invocation span takes from an agent
//! process. A session with no traced execution exports nothing; the hook's
//! `guards.decisionLog` line still records the deny locally.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

mod lex;
use lex::Word;
pub mod shared;

/// The category toggle (`guards.cargoTargetDir`, default on).
pub const TOGGLE_KEY: &str = "guards.cargoTargetDir";
/// The toggle's env override (Claude Code settings `env`).
pub const TOGGLE_ENV: &str = "LOOM_GUARD_CARGO_TARGET_DIR";
/// Every denial starts with this; the hook matches on it.
pub const DENY_PREFIX: &str = "BLOCKED [cargo-target-dir]";
/// Span attribute: which forbidden root ([`Root::label`]).
pub const ATTR_ROOT: &str = "loom.guard.root";
/// Span attribute: where the value came from ([`Source::label`]).
pub const ATTR_SOURCE: &str = "loom.guard.source";

const CTD: &str = "CARGO_TARGET_DIR";

/// cargo subcommands that build into the target dir.
const BUILD_SUBCOMMANDS: &[&str] = &[
    "build", "b", "check", "c", "clippy", "test", "t", "run", "r", "nextest", "bench", "doc",
    "rustc", "rustdoc", "fix", "llvm-cov", "miri",
];

/// A root nothing owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    Tmp,
    TmpDir,
    HomeCache,
    Home,
    LegacyLoomTarget,
}

impl Root {
    /// The root by name — the telemetry value, never a real path.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tmp => "/tmp",
            Self::TmpDir => "$TMPDIR",
            Self::HomeCache => "~/.cache",
            Self::Home => "~",
            Self::LegacyLoomTarget => ".loom/target-*",
        }
    }
}

/// Where the effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The session's own `CARGO_TARGET_DIR`.
    Inherited,
    /// `CARGO_TARGET_DIR=… cargo …`.
    Inline,
    /// `env CARGO_TARGET_DIR=… cargo …`.
    EnvWrapper,
    /// `export CARGO_TARGET_DIR=…` earlier in the same command.
    Export,
    /// `--target-dir …`.
    TargetDirFlag,
    /// `make CARGO_TARGET_DIR=…`.
    MakeArg,
}

impl Source {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Inherited => "inherited",
            Self::Inline => "inline",
            Self::EnvWrapper => "env",
            Self::Export => "export",
            Self::TargetDirFlag => "target-dir-flag",
            Self::MakeArg => "make-arg",
        }
    }

    const fn describe(self) -> &'static str {
        match self {
            Self::Inherited => "the CARGO_TARGET_DIR this session inherited",
            Self::Inline => "the inline CARGO_TARGET_DIR= assignment",
            Self::EnvWrapper => "the `env CARGO_TARGET_DIR=` assignment",
            Self::Export => "the CARGO_TARGET_DIR set earlier in this command",
            Self::TargetDirFlag => "--target-dir",
            Self::MakeArg => "the make CARGO_TARGET_DIR= argument",
        }
    }
}

/// What the evaluator knows about the machine and the session. Pure data, so
/// the tests need no environment.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub tmpdir: Option<PathBuf>,
    /// The session's `CARGO_TARGET_DIR`, when set and non-empty.
    pub inherited: Option<String>,
    /// The working tree `cwd` is in (`.git` dir or pointer file).
    pub worktree: Option<PathBuf>,
    /// The main checkout of that working tree.
    pub main_root: Option<PathBuf>,
    /// Loom-owned dirs outside both: the per-worktree target dir (#8458).
    pub owned: Vec<PathBuf>,
    /// cargo's configured `build.target-dir` for `cwd`, computed only in a
    /// Loom-managed worktree ([`shared`], #10646).
    pub configured: Option<shared::Configured>,
}

impl Context {
    /// Read the process environment and the filesystem around `cwd`.
    #[must_use]
    pub fn from_process(cwd: &Path) -> Self {
        let env_path = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let (worktree, main_root) = git_roots(cwd);
        let mut owned: Vec<PathBuf> = env_path("LOOM_WORKTREE_CARGO_TARGET_DIR")
            .filter(|p| p.is_absolute())
            .into_iter()
            .collect();
        if let Some(wt) = &worktree {
            owned.extend(crate::worktree_ops::cargo_target::per_worktree::marker_value(wt));
        }
        let home = env_path("HOME");
        Self {
            cwd: cwd.to_path_buf(),
            configured: shared::for_process(cwd, worktree.as_deref(), home.as_deref()),
            home,
            tmpdir: env_path("TMPDIR"),
            inherited: std::env::var(CTD).ok().filter(|v| !v.is_empty()),
            worktree,
            main_root,
            owned,
        }
    }
}

impl Context {
    /// This context re-rooted at `cwd`: the working tree, its main checkout
    /// and its owned dirs are recomputed; the machine facts carry over.
    fn at(&self, cwd: &Path) -> Self {
        let (worktree, main_root) = git_roots(cwd);
        let mut owned: Vec<PathBuf> = self
            .owned
            .iter()
            .filter(|_| worktree == self.worktree)
            .cloned()
            .collect();
        if worktree != self.worktree {
            if let Some(wt) = &worktree {
                owned.extend(crate::worktree_ops::cargo_target::per_worktree::marker_value(wt));
            }
        }
        Self {
            cwd: cwd.to_path_buf(),
            configured: shared::for_process(cwd, worktree.as_deref(), self.home.as_deref()),
            worktree,
            main_root,
            owned,
            ..self.clone()
        }
    }
}

/// The nearest ancestor of `start` with a `.git` entry, and the main checkout
/// it belongs to (a linked worktree's `gitdir:` pointer followed back). No
/// `git` subprocess: this runs on a hook's hot path.
fn git_roots(start: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut cur = normalize(start);
    loop {
        let dot_git = cur.join(".git");
        if dot_git.is_dir() {
            return (Some(cur.clone()), Some(cur));
        }
        if dot_git.is_file() {
            let main = std::fs::read_to_string(&dot_git).ok().and_then(|body| {
                let gitdir = PathBuf::from(body.trim().strip_prefix("gitdir:")?.trim());
                let gitdir = if gitdir.is_absolute() {
                    gitdir
                } else {
                    cur.join(gitdir)
                };
                // <main>/.git/worktrees/<name>
                let common = gitdir.parent()?.parent()?;
                (common.file_name()? == ".git").then(|| common.parent().map(Path::to_path_buf))?
            });
            return (Some(cur.clone()), main.or(Some(cur)));
        }
        if !cur.pop() {
            return (None, None);
        }
    }
}

/// One denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub target: PathBuf,
    pub root: Root,
    pub source: Source,
    alternative: String,
    /// The main checkout the target is under, when it is one (its `target/`
    /// is shared between every worktree, not unowned).
    shared_with: Option<PathBuf>,
}

/// The toggle: env wins (`0|false|no` off, `1|true|yes` on), then config
/// (`false` / `"false"` disables), default on.
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

/// The whole verb: `Some(denial)` when the command must be refused. The
/// toggle is read only once a deny is about to fire.
#[must_use]
pub fn check(command: &str, ctx: &Context, toggle: impl FnOnce() -> bool) -> Option<Denial> {
    let denial = evaluate(command, ctx)?;
    toggle().then_some(denial)
}

/// The hook's verdict on one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Deny(Denial),
    /// `loom:cargo-shared-target` (#10646): the build falls through to a
    /// shared `build.target-dir`.
    Ask(shared::SharedAsk),
}

/// The deny (when its toggle is on), else the shared-target ask (when its
/// toggle is on). Each toggle is read only once its verdict is about to fire.
#[must_use]
pub fn check_all(
    command: &str,
    ctx: &Context,
    toggle: impl Fn(&str, &str) -> bool,
) -> Option<Verdict> {
    let (denial, ask) = evaluate_all(command, ctx);
    if let Some(d) = denial.filter(|_| toggle(TOGGLE_KEY, TOGGLE_ENV)) {
        return Some(Verdict::Deny(d));
    }
    ask.filter(|_| toggle(shared::TOGGLE_KEY, shared::TOGGLE_ENV))
        .map(Verdict::Ask)
}

/// [`check_all`] against the process environment and config, for `cwd`.
#[must_use]
pub fn check_process(command: &str, cwd: &Path) -> Option<Verdict> {
    let ctx = Context::from_process(cwd);
    let config_root = ctx.main_root.clone().unwrap_or_else(|| cwd.to_path_buf());
    check_all(command, &ctx, |key, env_key| {
        let env = std::env::var(env_key).ok();
        let config = crate::config_resolver::resolve_effective_config(&config_root);
        toggle_enabled(env.as_deref(), crate::config_resolver::get_path(&config, key))
    })
}

/// The first build in `command` whose target dir is under a forbidden root.
#[must_use]
pub fn evaluate(command: &str, ctx: &Context) -> Option<Denial> {
    evaluate_all(command, ctx).0
}

/// [`evaluate`], plus the first build with no explicit target dir that would
/// fall through to a shared `build.target-dir` ([`shared::judge`]).
#[must_use]
pub fn evaluate_all(command: &str, ctx: &Context) -> (Option<Denial>, Option<shared::SharedAsk>) {
    let mut vars: HashMap<String, String> = HashMap::new();
    for (k, v) in [("HOME", &ctx.home), ("TMPDIR", &ctx.tmpdir)] {
        if let Some(v) = v {
            vars.insert(k.to_string(), v.to_string_lossy().into_owned());
        }
    }
    if let Some(v) = &ctx.inherited {
        vars.insert(CTD.to_string(), v.clone());
    }
    let mut st = State {
        vars,
        ctd: ctx.inherited.clone().map(|v| (Some(v), Source::Inherited)),
        cwd: Some(ctx.cwd.clone()),
        ask: None,
    };
    let denial = st.eval_text(command, ctx, 0);
    (denial, st.ask)
}

/// `Some(None)` = set to something unresolvable.
type Ctd = Option<(Option<String>, Source)>;

struct State {
    vars: HashMap<String, String>,
    ctd: Ctd,
    cwd: Option<PathBuf>,
    /// The first shared-target ask seen (#10646).
    ask: Option<shared::SharedAsk>,
}

const KEYWORDS: &[&str] = &[
    "if", "then", "do", "else", "elif", "while", "until", "!", "{", "}", "fi", "done", "time",
];

fn assignment(word: &Word) -> Option<(&str, &str)> {
    let (name, value) = word.text.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some((name, value))
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

impl State {
    fn eval_text(&mut self, text: &str, ctx: &Context, depth: u8) -> Option<Denial> {
        if depth > 4 {
            return None;
        }
        for words in lex::simple_commands(text) {
            if let Some(d) = self.eval_simple(&words, ctx, depth) {
                return Some(d);
            }
        }
        None
    }

    fn eval_simple(&mut self, words: &[Word], ctx: &Context, depth: u8) -> Option<Denial> {
        let mut i = 0;
        while i < words.len() && KEYWORDS.contains(&words[i].text.as_str()) {
            i += 1;
        }
        let mut prefix: Vec<(String, Option<String>)> = Vec::new();
        while let Some((name, value)) = words.get(i).and_then(assignment) {
            let expanded = self.expand(value, words[i].single_quoted_dollar);
            prefix.push((name.to_string(), expanded));
            i += 1;
        }
        if i == words.len() {
            // Bare assignments: shell variables, and a new value for an
            // already-exported CARGO_TARGET_DIR.
            for (name, value) in prefix {
                if name == CTD && self.ctd.is_some() {
                    self.ctd = Some((value.clone(), Source::Export));
                }
                self.set_var(&name, value);
            }
            return None;
        }
        let mut ctd = self.ctd.clone();
        if let Some((_, v)) = prefix.iter().rev().find(|(n, _)| n == CTD) {
            ctd = Some((v.clone(), Source::Inline));
        }
        self.exec(&words[i..], ctd, ctx, depth)
    }

    fn set_var(&mut self, name: &str, value: Option<String>) {
        match value {
            Some(v) => self.vars.insert(name.to_string(), v),
            None => self.vars.remove(name),
        };
    }

    fn exec(&mut self, words: &[Word], ctd: Ctd, ctx: &Context, depth: u8) -> Option<Denial> {
        let first = words.first()?;
        let args = &words[1..];
        match basename(&first.text) {
            "export" => {
                for w in args {
                    if let Some((name, value)) = assignment(w) {
                        let v = self.expand(value, w.single_quoted_dollar);
                        if name == CTD {
                            self.ctd = Some((v.clone(), Source::Export));
                        }
                        self.set_var(name, v);
                    } else if w.text == CTD {
                        self.ctd = Some((self.vars.get(CTD).cloned(), Source::Export));
                    }
                }
                None
            }
            "unset" => {
                if args.iter().any(|w| w.text == CTD) {
                    self.ctd = None;
                    self.vars.remove(CTD);
                }
                None
            }
            "cd" | "pushd" => {
                let target = match args.iter().find(|w| !w.text.starts_with('-')) {
                    Some(w) => self.expand(&w.text, w.single_quoted_dollar),
                    None => self.vars.get("HOME").cloned(),
                };
                self.cwd = target.and_then(|t| self.absolutize(&t));
                None
            }
            "env" => self.exec_env(args, ctd, ctx, depth),
            "nohup" | "command" | "exec" | "time" | "nice" | "timeout" | "stdbuf"
            | "caffeinate" => {
                let mut j = 0;
                while j < args.len() && args[j].text.starts_with('-') {
                    let takes_value = matches!(args[j].text.as_str(), "-n" | "-s" | "-k");
                    j += 1 + usize::from(takes_value);
                }
                if basename(&first.text) == "timeout" {
                    j += 1; // the duration
                }
                self.exec(args.get(j..)?, ctd, ctx, depth)
            }
            "bash" | "sh" | "zsh" | "dash" => {
                let at = args.iter().position(|w| {
                    w.text.starts_with('-') && !w.text.starts_with("--") && w.text.contains('c')
                })?;
                let script = args.get(at + 1)?.text.clone();
                let mut sub = State {
                    vars: self.vars.clone(),
                    ctd,
                    cwd: self.cwd.clone(),
                    ask: None,
                };
                let denial = sub.eval_text(&script, ctx, depth + 1);
                self.ask = self.ask.take().or(sub.ask);
                denial
            }
            "cargo" => self.cargo(args, ctd, ctx),
            "make" | "gmake" => {
                let from_arg = args.iter().rev().find_map(|w| {
                    let (name, value) = assignment(w)?;
                    (name == CTD)
                        .then(|| (self.expand(value, w.single_quoted_dollar), Source::MakeArg))
                });
                let effective = from_arg.or(ctd.filter(|(_, s)| *s != Source::Inherited));
                self.judge(effective, ctx)
            }
            _ => None,
        }
    }

    fn exec_env(
        &mut self,
        args: &[Word],
        mut ctd: Ctd,
        ctx: &Context,
        depth: u8,
    ) -> Option<Denial> {
        let mut j = 0;
        while let Some(w) = args.get(j) {
            let t = w.text.as_str();
            if t == "--" {
                j += 1;
                break;
            }
            if matches!(t, "-i" | "--ignore-environment" | "-") {
                ctd = None;
            } else if t == "-u" {
                if args.get(j + 1).is_some_and(|n| n.text == CTD) {
                    ctd = None;
                }
                j += 1;
            } else if t == "-uCARGO_TARGET_DIR" || t == "--unset=CARGO_TARGET_DIR" {
                ctd = None;
            } else if matches!(t, "-C" | "--chdir" | "-S" | "--split-string") {
                return None; // changes what runs or where: give up (allow)
            } else if t.starts_with('-') {
                // other flags take no value
            } else if let Some((name, value)) = assignment(w) {
                if name == CTD {
                    ctd = Some((self.expand(value, w.single_quoted_dollar), Source::EnvWrapper));
                }
            } else {
                break;
            }
            j += 1;
        }
        self.exec(args.get(j..)?, ctd, ctx, depth)
    }

    fn cargo(&mut self, args: &[Word], ctd: Ctd, ctx: &Context) -> Option<Denial> {
        let mut i = 0;
        while let Some(w) = args.get(i) {
            let t = w.text.as_str();
            if t.starts_with('+') {
                i += 1;
            } else if matches!(t, "--config" | "-Z" | "-C" | "--color") {
                i += 2;
            } else if t.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        }
        let sub = args.get(i)?.text.as_str();
        if !BUILD_SUBCOMMANDS.contains(&sub) {
            return None;
        }
        let mut flag: Option<Option<String>> = None;
        let mut k = i + 1;
        while let Some(w) = args.get(k) {
            if w.text == "--" {
                break;
            }
            if w.text == "--target-dir" {
                let v = args.get(k + 1)?;
                flag = Some(self.expand(&v.text, v.single_quoted_dollar));
                k += 1;
            } else if let Some(v) = w.text.strip_prefix("--target-dir=") {
                flag = Some(self.expand(v, w.single_quoted_dollar));
            }
            k += 1;
        }
        let effective = match flag {
            Some(v) => Some((v, Source::TargetDirFlag)),
            None => ctd,
        };
        if effective.is_none() && self.ask.is_none() {
            // No explicit target dir anywhere: cargo's config decides (#10646).
            // An unknown `cd` target leaves nothing to judge; a `cd` out of
            // this worktree is judged from where it lands.
            self.ask = self.cwd.as_ref().and_then(|c| {
                let moved = ctx.worktree.as_ref().is_none_or(|w| !under(&forms(c), w));
                let here = moved.then(|| ctx.at(c));
                shared::judge(here.as_ref().unwrap_or(ctx))
            });
        }
        self.judge(effective, ctx)
    }

    fn judge(&self, effective: Ctd, ctx: &Context) -> Option<Denial> {
        let (value, source) = effective?;
        let value = value.filter(|v| !v.is_empty())?;
        let target = self.absolutize(&value)?;
        // After a `cd` into another working tree, that tree's roots apply: a
        // sibling repo's own `target/` is not a leak. No `git` subprocess.
        let moved = self.cwd.as_ref().filter(|c| {
            **c != ctx.cwd && !ctx.worktree.as_ref().is_some_and(|w| under(&forms(c), w))
        });
        let rebased;
        let ctx = match moved {
            Some(c) => {
                rebased = ctx.at(c);
                &rebased
            }
            None => ctx,
        };
        let root = classify(&target, ctx)?;
        let shared_with = ctx
            .main_root
            .as_ref()
            .map(|m| m.join("target"))
            .filter(|t| under(&forms(&target), t));
        Some(Denial {
            alternative: alternative(ctx),
            target,
            root,
            source,
            shared_with,
        })
    }

    fn absolutize(&self, value: &str) -> Option<PathBuf> {
        let p = Path::new(value);
        if p.is_absolute() {
            Some(normalize(p))
        } else {
            Some(normalize(&self.cwd.as_ref()?.join(p)))
        }
    }

    /// Expand `~`, `$NAME`, `${NAME}`, `$(pwd)` and `` `pwd` ``. Anything else
    /// with a `$` or a backtick is unresolvable: `None`.
    fn expand(&self, text: &str, single_quoted_dollar: bool) -> Option<String> {
        let has_dollar = text.contains('$') || text.contains('`');
        if single_quoted_dollar && has_dollar {
            return None;
        }
        let mut text = text.to_string();
        if text == "~" || text.starts_with("~/") {
            text = format!("{}{}", self.vars.get("HOME")?, &text[1..]);
        }
        if !has_dollar {
            return Some(text);
        }
        let cwd = || self.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        let mut out = String::new();
        let mut rest = text.as_str();
        while let Some(pos) = rest.find(['$', '`']) {
            out.push_str(&rest[..pos]);
            rest = &rest[pos..];
            if let Some(r) = rest
                .strip_prefix("$(pwd)")
                .or_else(|| rest.strip_prefix("`pwd`"))
            {
                out.push_str(&cwd()?);
                rest = r;
                continue;
            }
            // `$(mktemp -d …)` with no explicit dir: a fresh dir under
            // $TMPDIR (or /tmp) — the most common way these leaks are made.
            if let Some(inner) = rest.strip_prefix("$(mktemp") {
                let end = inner.find(')')?;
                if inner[..end].contains(['/', '$', '`']) {
                    return None;
                }
                let base = self.vars.get("TMPDIR").map_or("/tmp", String::as_str);
                out.push_str(base.trim_end_matches('/'));
                out.push_str("/mktemp");
                rest = &inner[end + 1..];
                continue;
            }
            let after = rest.strip_prefix('$')?;
            let (name, r) = if let Some(braced) = after.strip_prefix('{') {
                let end = braced.find('}')?;
                (&braced[..end], &braced[end + 1..])
            } else {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], &after[end..])
            };
            if name.is_empty() {
                return None;
            }
            if name == "PWD" {
                out.push_str(&cwd()?);
            } else {
                out.push_str(self.vars.get(name)?);
            }
            rest = r;
        }
        out.push_str(rest);
        Some(out)
    }
}

/// Lexical normalization: `.` dropped, `..` popped. No filesystem access.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `p` with symlinks resolved on its longest existing prefix — so `/tmp/x`
/// and `/private/tmp/x` (macOS), or a symlinked home, compare equal.
fn physical(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name().map(std::ffi::OsStr::to_os_string), existing.pop()) {
            (Some(name), true) => rest.push(name),
            _ => return p.to_path_buf(),
        }
    }
}

fn forms(p: &Path) -> [PathBuf; 2] {
    [p.to_path_buf(), physical(p)]
}

fn under(path: &[PathBuf; 2], root: &Path) -> bool {
    let roots = forms(root);
    path.iter().any(|p| roots.iter().any(|r| p.starts_with(r)))
}

fn legacy_loom_target(p: &Path) -> bool {
    let parts: Vec<_> = p.components().collect();
    parts.windows(2).any(|w| {
        w[0].as_os_str() == ".loom"
            && w[1]
                .as_os_str()
                .to_str()
                .is_some_and(|s| s.starts_with("target-"))
    })
}

/// The forbidden root `target` is under, or `None` when it may be built into.
#[must_use]
pub fn classify(target: &Path, ctx: &Context) -> Option<Root> {
    let path = forms(target);
    if path.iter().any(|p| legacy_loom_target(p)) {
        return Some(Root::LegacyLoomTarget);
    }
    let allowed = ctx
        .main_root
        .iter()
        .map(|m| m.join(".loom/targets"))
        .chain(ctx.worktree.iter().cloned())
        .chain(ctx.owned.iter().cloned());
    for root in allowed {
        if under(&path, &root) {
            return None;
        }
    }
    // The most specific root wins: a home under /tmp (a CI runner) still
    // reports `~/.cache`, and a $TMPDIR under /tmp reports `$TMPDIR`.
    let mut forbidden: Vec<(Root, PathBuf)> = vec![
        (Root::Tmp, PathBuf::from("/tmp")),
        (Root::Tmp, PathBuf::from("/private/tmp")),
    ];
    if let Some(t) = &ctx.tmpdir {
        forbidden.push((Root::TmpDir, normalize(t)));
    }
    if let Some(h) = &ctx.home {
        forbidden.push((Root::Home, normalize(h)));
        forbidden.push((Root::HomeCache, normalize(&h.join(".cache"))));
    }
    forbidden
        .into_iter()
        .filter(|(_, root)| under(&path, root))
        .max_by_key(|(_, root)| root.components().count())
        .map(|(r, _)| r)
}

/// The Loom-owned dir the denial points at instead.
fn alternative(ctx: &Context) -> String {
    let exported = ctx.inherited.as_deref().map(Path::new).filter(|p| {
        p.is_absolute() && {
            let f = forms(p);
            ctx.main_root
                .iter()
                .any(|m| under(&f, &m.join(".loom/targets")))
                || ctx.owned.iter().any(|o| under(&f, o))
        }
    });
    if let Some(p) = exported {
        return format!(
            "Drop the override: this run already has a Loom-owned CARGO_TARGET_DIR ({}), which Loom reclaims for you",
            p.display()
        );
    }
    match &ctx.worktree {
        Some(wt) => format!(
            "Build into your worktree's own target dir instead: CARGO_TARGET_DIR=\"{}/target\" cargo … (or leave CARGO_TARGET_DIR unset). It goes away with the worktree",
            wt.display()
        ),
        None => "Build inside your worktree instead: CARGO_TARGET_DIR=\"$WORKTREE_ABS/target\" cargo … (or the CARGO_TARGET_DIR Loom exported, under <repo>/.loom/targets/)".to_string(),
    }
}

/// The denial text the hook passes through verbatim.
#[must_use]
pub fn deny_reason(d: &Denial) -> String {
    if let Some(main) = &d.shared_with {
        return format!(
            "{DENY_PREFIX}: this cargo command would build into '{}' (from {}), which is the \
             main checkout's target dir ({}). That dir is shared by every worktree of the repo, \
             so concurrent builds clobber each other and no one worktree can reclaim it (#8370, \
             #11582). {}. Do not move it to another unowned dir (/tmp, $TMPDIR, ~, ~/.cache, \
             <repo>/.loom/target-*); see /loom:cargo-target-isolation. To turn the rule off for an \
             attended session: LOOM_GUARD_CARGO_TARGET_DIR=0 in Claude Code's settings env, or \
             guards.cargoTargetDir=false.",
            d.target.display(),
            d.source.describe(),
            main.display(),
            d.alternative,
        );
    }
    format!(
        "{DENY_PREFIX}: this cargo command would build into '{}' (from {}), which is under {}. \
         Nothing owns that path, so an interrupted or abandoned run leaks the whole build \
         (several GB) until a host cleaner finds it (#8370, #11582). {}. Do not move it to \
         another unowned dir (/tmp, $TMPDIR, ~, ~/.cache, <repo>/.loom/target-*); see \
         /loom:cargo-target-isolation. To turn the rule off for an attended session: \
         LOOM_GUARD_CARGO_TARGET_DIR=0 in Claude Code's settings env, or guards.cargoTargetDir=false.",
        d.target.display(),
        d.source.describe(),
        d.root.label(),
        d.alternative,
    )
}

/// The `guard.cargo_target_dir.denied` span: a child of `parent` when there
/// is one, else its own root. Role, root and source only — never the path or
/// the command.
#[must_use]
pub fn denial_span(
    d: &Denial,
    role: Option<&str>,
    at: DateTime<Utc>,
    parent: Option<&TraceContext>,
) -> SpanRecord {
    let name = SpanName::GuardCargoTargetDirDenied;
    let started = crate::telemetry::trace::instant(at);
    let pid = std::process::id().to_string();
    let key = [name.as_str(), started.as_str(), pid.as_str()];
    let (context, parent_span_id) = match parent {
        Some(p) => (p.derived_child(&key), Some(p.span_id.clone())),
        None => (TraceContext::derived(name.as_str(), &key[1..]), None),
    };
    let mut attributes = TraceAttributes::new();
    attributes.insert(ATTR_ROOT.into(), d.root.label().into());
    attributes.insert(ATTR_SOURCE.into(), d.source.label().into());
    if let Some(role) = role.filter(|r| {
        !r.is_empty()
            && r.len() <= 64
            && r.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    }) {
        attributes.insert("loom.role".into(), role.to_string());
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id,
        name,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export the denial span through whatever path this process has: the
/// daemon's ops sink, else the ambient execution's trace journal. Never fails.
pub fn publish(d: &Denial) {
    let role = std::env::var("LOOM_ROLE").ok();
    if crate::observability::ops::spans_exported() {
        crate::observability::ops::emit_span(denial_span(d, role.as_deref(), Utc::now(), None));
        return;
    }
    use crate::telemetry::trace::store::{CONTEXT_FILE_ENV, TRACEPARENT_ENV};
    let (parent, file) = crate::gh_invocation::telemetry::parse_ambient(
        std::env::var(TRACEPARENT_ENV).ok().as_deref(),
        std::env::var_os(CONTEXT_FILE_ENV).map(PathBuf::from),
    );
    if let (Some(parent), Some(file)) = (parent, file) {
        let span = denial_span(d, role.as_deref(), Utc::now(), Some(&parent));
        if let Err(e) =
            crate::telemetry::trace::journal::Journal::for_context(&file).append_completed(span)
        {
            log::debug!("cargo_target_guard: span not journalled: {e}");
        }
    }
}

#[cfg(test)]
#[path = "cargo_target_guard/tests.rs"]
mod tests;
