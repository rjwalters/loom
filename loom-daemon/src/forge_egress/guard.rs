//! `loom-daemon forge egress guard --for-command` (#9989 slice 1, C6 of epic
//! #9983) — the classifier behind the `loom:forge-egress` `PreToolUse` rule in
//! `defaults/hooks/guard-loom-workflow.sh`.
//!
//! Routing by construction (C2–C5) covers code Loom ships; it cannot cover what
//! an agent *types*. Under a resolved policy whose `enforcement.api` is not
//! `observe` (the same fail-closed reading as [`policy::is_observe_only`]), this
//! denies the typed bypass shapes — a raw HTTP client or interpreter aimed at
//! the canonical API host, `gh api https://…`, `GH_HOST=` / `GH_CONFIG_DIR=`,
//! `gh … --hostname`, `gh config set api_host`, `gh auth
//! login|setup-git|refresh`, `env -i … gh`, a path-qualified `gh` other than
//! the managed launcher, and GitHub SDK installs/imports — and names the
//! managed alternative (plain `gh …`).
//!
//! The logic lives here rather than in the hook (shell-language policy): the
//! hook only builds the masked command text (its existing `gh pr merge`
//! masking pipeline, so a mention inside a commit message or `--body` is not an
//! invocation) and calls this verb. Wrapper spellings the merge-redirect rule
//! sees through — `sh -c "…"`, `bash -lc '…'`, `eval "…"`, `echo … | sh` — are
//! unwrapped by [`classify`].
//!
//! Inert unless a policy resolves: no policy, an unreadable one (dispatch and
//! spawn already refuse that), or an `observe` one ⇒ allow. A daemon too old to
//! have this verb exits 2 on the unknown subcommand, which the hook treats as
//! allow. The deny text never quotes the command, the environment, or any
//! policy value beyond its origin.

use regex::Regex;
use serde_json::Value;
use std::path::Path;
use std::sync::LazyLock;

use super::policy::{self, dig_str, Origin, Resolution};

/// The finding code every denial names.
pub const FINDING_CODE: &str = "routing.denied-by-guard";
/// The first bytes of every denial; the hook denies only on this prefix.
pub const DENY_PREFIX: &str = "BLOCKED [routing.denied-by-guard]";
/// The category toggle (`guards.forgeEgress`, default on).
pub const TOGGLE_KEY: &str = "guards.forgeEgress";
/// The toggle's env override (Claude Code settings `env`).
pub const TOGGLE_ENV: &str = "LOOM_GUARD_FORGE_EGRESS";

/// One typed-bypass class. Slice 2 (`role_tool_policy`) reuses this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bypass {
    /// curl/wget/httpie/xh/… or an interpreter, plus `api.github.com` /
    /// `uploads.github.com` anywhere in the command.
    CanonicalApiClient,
    /// `gh api https://…` — an absolute URL ignores the configured origin.
    GhApiAbsoluteUrl,
    /// A `GH_HOST=` assignment.
    GhHostEnv,
    /// A `GH_CONFIG_DIR=` assignment.
    GhConfigDirEnv,
    /// `gh … --hostname`.
    GhHostnameFlag,
    /// `gh config set … api_host`.
    GhConfigApiHost,
    /// `gh auth login|setup-git|refresh`.
    GhAuthMutation,
    /// `env -i … gh` (or `env -` / `--ignore-environment`).
    EnvClearedGh,
    /// A path-qualified `gh` that is not the policy's launcher.
    PathQualifiedGh,
    /// Installing or importing PyGithub / octokit / octocrab / go-github.
    Sdk,
}

impl Bypass {
    pub const ALL: [Bypass; 10] = [
        Bypass::CanonicalApiClient,
        Bypass::GhApiAbsoluteUrl,
        Bypass::GhHostEnv,
        Bypass::GhConfigDirEnv,
        Bypass::GhHostnameFlag,
        Bypass::GhConfigApiHost,
        Bypass::GhAuthMutation,
        Bypass::EnvClearedGh,
        Bypass::PathQualifiedGh,
        Bypass::Sdk,
    ];

    /// What the command does, for the denial text. Never quotes the command.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::CanonicalApiClient => {
                "a raw HTTP client or interpreter addressing the canonical API host (api.github.com / uploads.github.com)"
            }
            Self::GhApiAbsoluteUrl => "`gh api` with an absolute URL, which ignores the managed API origin",
            Self::GhHostEnv => "a GH_HOST= override",
            Self::GhConfigDirEnv => "a GH_CONFIG_DIR= override, which switches gh to an unmanaged profile",
            Self::GhHostnameFlag => "`gh … --hostname`, which targets a host other than the managed one",
            Self::GhConfigApiHost => "`gh config set api_host`, which rewrites the managed routing",
            Self::GhAuthMutation => {
                "`gh auth login|setup-git|refresh`, which mints or installs an unmanaged credential"
            }
            Self::EnvClearedGh => "`env -i … gh`, which strips the managed environment",
            Self::PathQualifiedGh => "a path-qualified gh binary other than the managed launcher",
            Self::Sdk => "a GitHub SDK install or import (PyGithub, octokit, octocrab, go-github)",
        }
    }
}

/// A resolved policy that turns the guard on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enforced {
    pub origin: Origin,
    /// `toolchain.launcherPath`, only from an origin trusted to choose an
    /// executable ([`Origin::may_choose_executable`]); a repo-local policy
    /// cannot exempt a binary of its choosing.
    pub launcher: Option<String>,
}

/// Whether `resolution` enforces the guard: a loaded policy that is not
/// `observe`-only. Unconfigured and unreadable are inert here.
#[must_use]
pub fn enforced(resolution: &Resolution) -> Option<Enforced> {
    let Resolution::Loaded(doc) = resolution else {
        return None;
    };
    if policy::is_observe_only(&doc.data) {
        return None;
    }
    let launcher = Some(dig_str(&doc.data, &["toolchain", "launcherPath"]))
        .filter(|p| doc.origin.may_choose_executable() && p.starts_with('/'))
        .map(str::to_string);
    Some(Enforced {
        origin: doc.origin,
        launcher,
    })
}

/// The category toggle: env (`0|false|no` / `1|true|yes`) beats config
/// (`guards.forgeEgress`: `false` or `"false"` disables); default on.
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

/// The denial text: finding code, class, managed alternative, policy origin.
#[must_use]
pub fn deny_reason(class: Bypass, enforced: &Enforced) -> String {
    format!(
        "{DENY_PREFIX}: this command uses {}. This host's forge egress policy (origin: {}) \
         sets enforcement.api=required, so GitHub API traffic must go through the managed gh \
         launcher. Use plain `gh …` instead — e.g. `gh api repos/OWNER/REPO`, `gh api graphql`, \
         `./.loom/scripts/merge-pr.sh`, `./.loom/scripts/create-issue.sh`. A repo may opt this \
         guard out (guards.forgeEgress=false, or LOOM_GUARD_FORGE_EGRESS=0 in Claude Code's \
         settings env); the policy is still enforced at runtime. See \
         .loom/docs/forge-egress.md (#9989).",
        class.description(),
        enforced.origin.as_str(),
    )
}

/// The whole verb for one command: `Some(reason)` to deny.
#[must_use]
pub fn check(
    command: &str,
    resolution: &Resolution,
    toggle: impl FnOnce() -> bool,
) -> Option<String> {
    let enforced = enforced(resolution)?;
    let class = classify(command, enforced.launcher.as_deref())?;
    toggle().then(|| deny_reason(class, &enforced))
}

/// [`check`] against the process's own policy and config for `workspace`.
#[must_use]
pub fn check_process(command: &str, workspace: &Path) -> Option<String> {
    let resolution = policy::resolve(&policy::PolicySources::from_process(Some(workspace)));
    check(command, &resolution, || {
        let env = std::env::var(TOGGLE_ENV).ok();
        let config = crate::config_resolver::resolve_effective_config(workspace);
        toggle_enabled(env.as_deref(), crate::config_resolver::get_path(&config, TOGGLE_KEY))
    })
}

static GH_HOST_ENV: LazyLock<Regex> = LazyLock::new(|| re(r"(?:^|[^A-Za-z0-9_])GH_HOST="));
static GH_CONFIG_DIR_ENV: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:^|[^A-Za-z0-9_])GH_CONFIG_DIR="));
static CANONICAL_HOST: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(?:^|[^a-z0-9.-])(?:api|uploads)\.github\.com(?:[^a-z0-9.-]|$)"));
static SDK_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r#"(?m)(?:^|[\s;'"(])(?:from\s+github\s+import\b|import\s+github\b)"#,
        r#"|require\(\s*['"]@octokit/|from\s+['"]@octokit/|\bnew\s+Octokit\b"#,
        r"|\buse\s+octocrab\b|\boctocrab::|github\.com/google/go-github",
    ))
});

#[allow(clippy::expect_used)]
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static forge-egress guard regex")
}

const HTTP_CLIENTS: &[&str] = &[
    "curl",
    "wget",
    "http",
    "https",
    "xh",
    "xhs",
    "httpie",
    "aria2c",
    "lwp-request",
    "nc",
    "ncat",
    "socat",
    "openssl",
    "telnet",
];
const INTERPRETERS: &[&str] = &[
    "node",
    "nodejs",
    "ruby",
    "perl",
    "deno",
    "bun",
    "php",
    "pwsh",
    "powershell",
];
const INSTALLERS: &[&str] = &[
    "pip", "pip3", "uv", "pipx", "poetry", "pdm", "conda", "mamba", "npm", "pnpm", "yarn", "npx",
    "pnpx", "bunx", "uvx", "cargo", "go", "gem",
];
const SDK_NAMES: &[&str] = &["pygithub", "octokit", "octocrab", "go-github"];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
const KEYWORDS: &[&str] = &[
    "{", "}", "!", "if", "then", "else", "elif", "do", "while", "until", "time",
];
/// Prefix commands whose own flags are skipped before the real command.
const WRAPPERS: &[&str] = &[
    "exec",
    "command",
    "builtin",
    "nohup",
    "setsid",
    "stdbuf",
    "xargs",
    "unbuffer",
    "ionice",
    "chrt",
    "caffeinate",
    "sudo",
    "doas",
    "nice",
    "timeout",
    "watch",
];

/// Classify `command` (the hook's masked text). First matching class wins.
#[must_use]
pub fn classify(command: &str, launcher: Option<&str>) -> Option<Bypass> {
    if GH_HOST_ENV.is_match(command) {
        return Some(Bypass::GhHostEnv);
    }
    if GH_CONFIG_DIR_ENV.is_match(command) {
        return Some(Bypass::GhConfigDirEnv);
    }
    if SDK_IMPORT.is_match(command) {
        return Some(Bypass::Sdk);
    }
    let canonical = CANONICAL_HOST.is_match(command);
    classify_commands(command, launcher, canonical, 0)
}

fn classify_commands(
    text: &str,
    launcher: Option<&str>,
    canonical: bool,
    depth: u8,
) -> Option<Bypass> {
    let segments = segments(text);
    let parsed: Vec<Cmd> = segments.iter().filter_map(|s| parse(s)).collect();
    // `… | sh`, `bash <<EOF`, `bash -s`: a shell reading its script from stdin
    // makes an `echo`/`printf` argument live code.
    let stdin_shell = parsed.iter().any(|c| c.stdin_shell);
    for cmd in &parsed {
        let base = basename(cmd.word);
        if let Some(class) = classify_gh(cmd, base, launcher) {
            return Some(class);
        }
        let lower: Vec<String> = cmd.args.iter().map(|a| a.to_ascii_lowercase()).collect();
        let is_installer = INSTALLERS.contains(&base);
        let is_interp = is_interpreter(base);
        if (is_installer || is_interp)
            && lower
                .iter()
                .any(|a| SDK_NAMES.iter().any(|n| a.contains(n)))
        {
            return Some(Bypass::Sdk);
        }
        if canonical && (HTTP_CLIENTS.contains(&base) || is_interp) {
            return Some(Bypass::CanonicalApiClient);
        }
        if stdin_shell && depth < 3 && (base == "echo" || base == "printf") {
            // Skip flags, and printf's format string.
            let inner = cmd
                .args
                .iter()
                .skip_while(|a| a.starts_with('-') || (base == "printf" && a.contains('%')))
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(class) = classify_commands(&inner, launcher, canonical, depth + 1) {
                return Some(class);
            }
        }
    }
    None
}

fn classify_gh(cmd: &Cmd, base: &str, launcher: Option<&str>) -> Option<Bypass> {
    if base != "gh" {
        return None;
    }
    let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
    if cmd.env_cleared {
        return Some(Bypass::EnvClearedGh);
    }
    if cmd.word.contains('/') && Some(cmd.word) != launcher {
        return Some(Bypass::PathQualifiedGh);
    }
    match args.as_slice() {
        ["auth", "login" | "setup-git" | "refresh", ..] => return Some(Bypass::GhAuthMutation),
        ["config", "set", rest @ ..] if rest.contains(&"api_host") => {
            return Some(Bypass::GhConfigApiHost)
        }
        _ => {}
    }
    if args.first() == Some(&"api") {
        let absolute = args.iter().skip(1).any(|a| {
            let a = a.to_ascii_lowercase();
            a.starts_with("https://") || a.starts_with("http://")
        });
        if absolute {
            return Some(Bypass::GhApiAbsoluteUrl);
        }
    }
    if args
        .iter()
        .any(|a| *a == "--hostname" || a.starts_with("--hostname="))
    {
        return Some(Bypass::GhHostnameFlag);
    }
    None
}

fn is_interpreter(base: &str) -> bool {
    INTERPRETERS.contains(&base)
        || base
            .strip_prefix("python")
            .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Split into simple commands. Backslashes are dropped and quotes become token
/// breaks, so
/// `bash -c "curl …"` and `eval '…'` expose their payload as ordinary tokens;
/// command separators, subshell/substitution parens, backticks and newlines
/// (heredoc bodies fed to a shell) end a command.
fn segments(text: &str) -> Vec<Vec<String>> {
    let text = text.replace("\\\n", " ").replace('\\', "");
    text.split([';', '&', '|', '(', ')', '\n', '`'])
        .map(|s| {
            s.split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"'))
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .collect()
}

struct Cmd<'a> {
    word: &'a str,
    args: &'a [String],
    env_cleared: bool,
    stdin_shell: bool,
}

fn is_assignment(t: &str) -> bool {
    t.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Whether `wrapper`'s option `flag` consumes the next token.
fn wrapper_flag_takes_arg(wrapper: &str, flag: &str) -> bool {
    let takes: &[&str] = match wrapper {
        "sudo" | "doas" => &["-u", "-g", "-C", "-h", "-p"],
        "nice" | "ionice" => &["-n", "-c", "-p"],
        "timeout" => &["-s", "-k"],
        "xargs" => &["-I", "-n", "-P", "-L", "-d", "-E", "-a", "-s"],
        "stdbuf" => &["-i", "-o", "-e"],
        "exec" => &["-a"],
        "watch" => &["-n", "-d"],
        _ => &[],
    };
    takes.contains(&flag)
}

/// A shell with no script operand — only flags and redirections (a bare `<`,
/// `<<`, `<<-` operator's word is the redirection target, not a script) —
/// reads its script from stdin.
fn reads_stdin(args: &[String]) -> bool {
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        if matches!(a, "<" | "<<" | "<<-" | "<<<") {
            it.next();
        } else if !(a.starts_with('-') || a.starts_with('+') || a.starts_with('<')) {
            return false;
        }
    }
    true
}

/// A short-option cluster containing `ch` (`-lc` contains `c`).
fn short_cluster_has(t: &str, ch: char) -> bool {
    t.len() > 1
        && t.starts_with('-')
        && !t.starts_with("--")
        && t[1..].chars().all(|c| c.is_ascii_alphabetic())
        && t[1..].contains(ch)
}

/// Find the command word of one simple command, unwrapping assignments,
/// keywords, prefix wrappers (with their flags), `env`, `eval` and
/// `sh|bash … -c`.
fn parse(tokens: &[String]) -> Option<Cmd<'_>> {
    let mut i = 0;
    let mut env_cleared = false;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if is_assignment(t) || KEYWORDS.contains(&t) || t == "eval" {
            i += 1;
            continue;
        }
        if t == "env" {
            i += 1;
            while let Some(a) = tokens.get(i).map(String::as_str) {
                if a == "-" || a == "--ignore-environment" || short_cluster_has(a, 'i') {
                    env_cleared = true;
                    i += 1;
                } else if matches!(a, "-u" | "-C" | "-S" | "--unset" | "--chdir") {
                    i += 2;
                } else if a.starts_with('-') || is_assignment(a) {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        if WRAPPERS.contains(&t) {
            if t == "command" && tokens.get(i + 1).is_some_and(|a| a == "-v" || a == "-V") {
                return None;
            }
            i += 1;
            while let Some(a) = tokens.get(i).map(String::as_str) {
                if wrapper_flag_takes_arg(t, a) {
                    i += 2;
                } else if a.starts_with('-') {
                    i += 1;
                } else {
                    break;
                }
            }
            if t == "timeout" && tokens.get(i).is_some() {
                i += 1; // the duration
            }
            continue;
        }
        if SHELLS.contains(&basename(t)) {
            let mut j = i + 1;
            let mut dash_c = false;
            while let Some(a) = tokens.get(j).map(String::as_str) {
                if short_cluster_has(a, 'c') {
                    dash_c = true;
                    j += 1;
                    break;
                } else if a == "-o" || a == "+o" {
                    j += 2;
                } else if a.starts_with('-') || a.starts_with('+') {
                    j += 1;
                } else {
                    break;
                }
            }
            if dash_c {
                i = j;
                continue;
            }
            return Some(Cmd {
                word: t,
                args: &tokens[i + 1..],
                env_cleared,
                stdin_shell: reads_stdin(&tokens[i + 1..]),
            });
        }
        return Some(Cmd {
            word: t,
            args: &tokens[i + 1..],
            env_cleared,
            stdin_shell: false,
        });
    }
    None
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
