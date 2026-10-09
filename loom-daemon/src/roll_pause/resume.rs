//! Session handles for a roll resume (issue #10830; design §1, §3).
//!
//! The spawn scripts pin or capture each agent's runtime session id so that a
//! roll can resume it later, and they gain a resume launch mode. The logic for
//! both lives here; `spawn-claude.sh` / `spawn-codex.sh` only call it
//! (`loom-daemon agent-resume …`).
//!
//! * **Claude Code** sessions are pinned at launch: the daemon generates a
//!   uuid and passes it as [`CLAUDE_SESSION_ENV`]; [`claude_args`] turns it into
//!   `--session-id <uuid>`. A resume passes [`RESUME_SESSION_ENV`] and
//!   [`RESUME_PROMPT_ENV`] instead, which become `--resume <id> <prompt>`.
//!   Verified live (#10830): the transcript lives under the cwd's project dir
//!   of the same `HOME`, and a resume may run on a different pool account.
//! * **Codex** cannot be pinned: it prints `session id: <uuid>` on stderr when
//!   the session starts. [`capture_codex`] watches the stderr capture while the
//!   session runs and writes the handle file as soon as the line appears. The
//!   rollout lives under the account's `CODEX_HOME`, so a resume must pin the
//!   same account ([`codex_resume_prompt`] refuses without `LOOM_CODEX_HOME`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Pinned Claude session id for a fresh dispatch.
pub const CLAUDE_SESSION_ENV: &str = "LOOM_CLAUDE_SESSION_ID";
/// Session id to resume (both runtimes).
pub const RESUME_SESSION_ENV: &str = "LOOM_RESUME_SESSION_ID";
/// The prompt a resumed session is given (built by [`resume_prompt`]).
pub const RESUME_PROMPT_ENV: &str = "LOOM_RESUME_PROMPT";
/// Where a spawn script writes the live-captured handle (Codex).
pub const HANDLE_FILE_ENV: &str = "LOOM_RESUME_HANDLE_FILE";
/// The systemd scope unit name the daemon assigns to an agent.
pub const SCOPE_UNIT_ENV: &str = "LOOM_AGENT_SCOPE_UNIT";
/// Names the descriptor holding a private Codex account's lease. A private
/// dispatch passes it to `spawn-codex.sh` without close-on-exec
/// (`tokens_pool::private_workspace::dispatch`), so every child of the script
/// inherits it.
pub const PRIVATE_LEASE_FD_ENV: &str = "LOOM_PRIVATE_LEASE_FD";

/// Whether `id` looks like a runtime session id (a uuid: hex and dashes).
#[must_use]
pub fn valid_session_id(id: &str) -> bool {
    let hex = id.chars().filter(char::is_ascii_hexdigit).count();
    (32..=64).contains(&id.len())
        && hex >= 32
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn env_var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// The arguments `spawn-claude.sh` appends: `--resume <id> <prompt>` in resume
/// mode, `--session-id <id>` for a pinned fresh launch, nothing otherwise.
///
/// # Errors
/// A malformed session id, or resume mode without a prompt.
pub fn claude_args(
    resume: Option<&str>,
    prompt: Option<&str>,
    pinned: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(id) = resume {
        if !valid_session_id(id) {
            return Err(format!("{RESUME_SESSION_ENV}='{id}' is not a session id"));
        }
        let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) else {
            return Err(format!("{RESUME_SESSION_ENV} needs {RESUME_PROMPT_ENV}"));
        };
        return Ok(vec!["--resume".into(), id.into(), prompt.into()]);
    }
    match pinned {
        Some(id) if valid_session_id(id) => Ok(vec!["--session-id".into(), id.into()]),
        Some(id) => Err(format!("{CLAUDE_SESSION_ENV}='{id}' is not a session id")),
        None => Ok(Vec::new()),
    }
}

/// [`claude_args`] from the process environment, followed by the pause
/// hook's `--settings` wiring for a daemon-dispatched session
/// ([`super::wiring`], #11049): a consumer repo's own settings do not run
/// the hook, so the launch has to.
///
/// # Errors
/// As [`claude_args`].
pub fn claude_args_from_env() -> Result<Vec<String>, String> {
    let mut args = claude_args(
        env_var(RESUME_SESSION_ENV).as_deref(),
        env_var(RESUME_PROMPT_ENV).as_deref(),
        env_var(CLAUDE_SESSION_ENV).as_deref(),
    )?;
    args.extend(super::wiring::settings_args_from_env());
    Ok(args)
}

/// The prompt for a Codex resume, after checking the launch is resumable:
/// a well-formed session id, a prompt, and an account pinned through
/// `LOOM_CODEX_HOME` (the rollout lives in that account's `CODEX_HOME`).
///
/// # Errors
/// Any of the three missing or malformed.
pub fn codex_resume_prompt(
    resume: &str,
    prompt: Option<&str>,
    pinned_home: Option<&str>,
) -> Result<String, String> {
    if !valid_session_id(resume) {
        return Err(format!("{RESUME_SESSION_ENV}='{resume}' is not a session id"));
    }
    if pinned_home.is_none() {
        return Err(format!(
            "a Codex resume must pin the session's account: set LOOM_CODEX_HOME to the {}",
            "CODEX_HOME recorded in the resume handle"
        ));
    }
    prompt
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{RESUME_SESSION_ENV} needs {RESUME_PROMPT_ENV}"))
}

/// What the resume prompt says about the roll and the parked call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumePromptInput {
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub parked_tool: Option<String>,
    pub parked_summary: Option<String>,
}

/// The prompt a resumed session receives (design §2): it was paused for a roll
/// (`from -> to`), its parked tool call did not run, and its background
/// processes were stopped.
#[must_use]
pub fn resume_prompt(input: &ResumePromptInput) -> String {
    let roll = match (&input.from_version, &input.to_version) {
        (Some(f), Some(t)) => format!(" ({f} -> {t})"),
        (None, Some(t)) => format!(" (to {t})"),
        _ => String::new(),
    };
    let call = match (&input.parked_tool, &input.parked_summary) {
        (Some(tool), Some(s)) if !s.is_empty() => {
            format!("The tool call you were making when it was paused ({tool}: {s}) did NOT run.")
        }
        (Some(tool), _) => {
            format!("The {tool} tool call you were making when it was paused did NOT run.")
        }
        _ => "The tool call you were making when it was paused did NOT run.".to_string(),
    };
    format!(
        "Loom: this session was paused for a daemon roll{roll} and has now been resumed in \
the same working directory. {call} Background processes you had started (dev servers, \
watchers, background shells) were stopped. If that tool call is still needed, run it again \
once, restart any background process you still need, and then continue the task where you \
left off."
    )
}

/// The live-captured resume handle a spawn script writes for the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedHandle {
    pub runtime: String,
    pub session_id: String,
    #[serde(default)]
    pub session_store: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    /// The Codex sandbox mode of the original launch (#10831).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub captured_at: String,
}

/// Read a handle file written by [`capture_codex`].
#[must_use]
pub fn read_handle(path: &Path) -> Option<CapturedHandle> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The first `session id: <uuid>` in a Codex stderr capture.
#[must_use]
pub fn parse_codex_session_id(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.split("session id:").nth(1)?.trim();
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect();
        valid_session_id(&id).then_some(id)
    })
}

/// Inputs to [`capture_codex`].
#[derive(Debug, Clone)]
pub struct CaptureSpec {
    pub stderr_file: PathBuf,
    pub handle_file: PathBuf,
    /// Stop watching once this pid is gone (the spawn script).
    pub watch_pid: Option<u32>,
    pub poll: Duration,
    pub timeout: Duration,
    pub template: CapturedHandle,
}

/// How a capture ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureOutcome {
    Captured(String),
    WatchedExited,
    TimedOut,
}

fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence and permission.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Close the account lease descriptor a capture watcher inherited; `value` is
/// what [`PRIVATE_LEASE_FD_ENV`] holds. Returns whether a descriptor was closed.
///
/// The watcher is backgrounded and can outlive its spawn script by one poll.
/// The lease is a `flock` on an open file description that every inheritor
/// shares, so a watcher that kept its copy would keep the account busy after
/// the run had ended, and the next dispatch on that account would be refused.
/// The watcher never needs the lease: it only reads the stderr capture.
///
/// This covers the watcher process only. `spawn-codex.sh` must `exec` it, so
/// that no backgrounded subshell is left holding a second copy.
pub fn release_inherited_lease(value: Option<&str>) -> bool {
    release_lease_descriptor(value, crate::tokens_pool::private_workspace::dispatch::LEASE_FD)
}

/// [`release_inherited_lease`] against an explicit accepted descriptor.
///
/// Only `accepted` is ever closed (#10832, Judge finding on #10974): the
/// dispatcher hands the lease down on exactly one descriptor, and the same
/// rule guards the other reader of this variable
/// (`private_workspace::dispatch::inherited_lease`). A variable naming any
/// other number is not a lease this process was given, and closing it would
/// close something unrelated.
pub(crate) fn release_lease_descriptor(value: Option<&str>, accepted: i32) -> bool {
    let Some(fd) = value.and_then(|v| v.trim().parse::<i32>().ok()) else {
        return false;
    };
    // Never stdin, stdout or stderr, whatever the variable says.
    if fd <= 2 || fd != accepted {
        return false;
    }
    // SAFETY: closing a descriptor this process inherited and never wrapped
    // in an owning type; on a descriptor that is not open this is EBADF.
    unsafe { libc::close(fd) == 0 }
}

/// Watch a Codex stderr capture until the session id appears, then write the
/// handle file atomically. Ends when the id is captured, the watched pid is
/// gone, or the timeout runs out, so it never outlives its spawn script.
#[must_use]
pub fn capture_codex(spec: &CaptureSpec) -> CaptureOutcome {
    let deadline = Instant::now() + spec.timeout;
    loop {
        let text = std::fs::read(&spec.stderr_file)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if let Some(id) = parse_codex_session_id(&text) {
            let handle = CapturedHandle {
                runtime: "codex".to_string(),
                session_id: id.clone(),
                captured_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                ..spec.template.clone()
            };
            if let Ok(body) = serde_json::to_vec_pretty(&handle) {
                let _ = super::write_atomic(&spec.handle_file, &body);
            }
            return CaptureOutcome::Captured(id);
        }
        if spec.watch_pid.is_some_and(|p| !pid_alive(p)) {
            return CaptureOutcome::WatchedExited;
        }
        if Instant::now() >= deadline {
            return CaptureOutcome::TimedOut;
        }
        std::thread::sleep(spec.poll);
    }
}

/// Whether a rollout for `session_id` exists under a Codex `sessions/` tree
/// (`sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`). Bounded to that depth.
fn codex_rollout_exists(dir: &Path, session_id: &str, depth: u8) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|e| {
        let path = e.path();
        if path.is_dir() {
            depth > 0 && codex_rollout_exists(&path, session_id, depth - 1)
        } else {
            e.file_name().to_string_lossy().contains(session_id)
        }
    })
}

/// H5's "the session store is reachable" check (#10832; design §4, reason
/// `session-store-unavailable`): can a resume of `session_id` find the saved
/// session where the runtime will look for it?
///
/// * **Claude**: a transcript `projects/*/<id>.jsonl` under `session_store`,
///   else `$CLAUDE_CONFIG_DIR`, else `~/.claude`. This is the same glob
///   `claude-wrapper.sh` uses to decide between `--session-id` and `--resume`.
/// * **Codex**: `session_store` is the account's `CODEX_HOME`. It must be
///   recorded, exist, and hold a usable `auth.json` (what `spawn-codex.sh`
///   itself requires of a pinned profile). When the profile's `sessions/`
///   tree is visible on this host, it must hold the session's rollout.
///
/// # Errors
/// What is missing, for the requeue record.
pub fn session_store_reachable(
    runtime: &str,
    session_id: &str,
    session_store: Option<&str>,
) -> Result<(), String> {
    if !valid_session_id(session_id) {
        return Err(format!("`{session_id}` is not a session id"));
    }
    if runtime == "codex" {
        let Some(home) = session_store.filter(|s| !s.trim().is_empty()) else {
            return Err("no CODEX_HOME was recorded for the session".to_string());
        };
        let home = Path::new(home);
        if !home.is_dir() {
            return Err(format!("the session's CODEX_HOME {} is gone", home.display()));
        }
        let auth = home.join("auth.json");
        if !std::fs::metadata(&auth).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return Err(format!("{} has no usable auth.json", home.display()));
        }
        let sessions = home.join("sessions");
        if sessions.is_dir() && !codex_rollout_exists(&sessions, session_id, 4) {
            return Err(format!("no rollout for the session under {}", sessions.display()));
        }
        return Ok(());
    }
    let config = session_store
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| env_var("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")));
    let Some(config) = config else {
        return Err("no Claude config directory resolves".to_string());
    };
    let projects = config.join("projects");
    let transcript = format!("{session_id}.jsonl");
    let found = std::fs::read_dir(&projects).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|e| e.path().join(&transcript).is_file())
    });
    if found {
        Ok(())
    } else {
        Err(format!("no transcript {transcript} under {}", projects.display()))
    }
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
