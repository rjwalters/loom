//! Native tmux agent-session spawn/wait (issue #4415, epic #4081 Phase 3
//! family 4).
//!
//! This module is the native replacement for the deleted Python
//! `loom_tools.agent_spawn` / `loom_tools.agent_wait` modules (and the shared
//! `loom_tools.common.tmux_session` helper they were built on). It backs the
//! `loom-daemon agent-spawn` / `loom-daemon agent-wait` CLI subcommands, which
//! `defaults/scripts/agent-spawn.sh` / `agent-wait.sh` now delegate to.
//!
//! Design notes:
//!
//! * **All external effects go through [`AgentEnv`]** — tmux invocations,
//!   `pgrep` process probing, clock/sleep, OAuth token selection, and
//!   per-agent `CLAUDE_CONFIG_DIR` isolation. [`SystemEnv`] is the production
//!   implementation; the unit tests drive a scripted fake, which is how the
//!   ~115 pytest cases this port replaces are re-covered without touching a
//!   live tmux server.
//! * **`CLAUDE_CONFIG_DIR` isolation is *not* reimplemented here.**
//!   [`SystemEnv`] delegates to the `claude_config` module already living in
//!   `terminal.rs` (exposed via the `claude_config_*` wrappers and the
//!   `loom-daemon claude-config` CLI surface), so there is exactly one native
//!   implementation shared by the daemon's `create_terminal` path and the
//!   manual-mode spawn path.
//! * **OAuth token selection is the native pool selector.** [`SystemEnv`]
//!   calls `tokens_pool::select::select_token` in-process — the same code
//!   `loom-daemon tokens select --export` runs for `spawn-claude.sh` — rather
//!   than the removed Python `loom_tools.tokens.select`.

pub mod spawn;
pub mod wait;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Shared tmux socket. Must match `terminal.rs`, `agent-destroy.sh`, and
/// `loom-start.sh` (`tmux -L loom`).
pub const TMUX_SOCKET: &str = "loom";

/// Prefix applied to every loom-managed tmux session name.
pub const SESSION_PREFIX: &str = "loom-";

/// Claude Code renders this in the status bar while actively processing.
/// Its presence in a captured pane means the agent is *not* idle.
pub const PROCESSING_INDICATORS: &str = "esc to interrupt";

/// Fully-qualified tmux session name for an agent `name`.
pub fn session_name(name: &str) -> String {
    format!("{SESSION_PREFIX}{name}")
}

/// Result of one external command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    /// A successful run producing `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed run (exit 1, empty output).
    pub fn fail() -> Self {
        Self {
            code: 1,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// Environment variables inherited from the caller that the spawned agent's
/// command line must carry forward.
///
/// Captured once at CLI-parse time rather than read ad hoc deep in the spawn
/// path, so the spawn logic itself is a pure function of its inputs (and its
/// tests never mutate process env, which races under `cargo test`).
#[derive(Debug, Clone, Default)]
pub struct InheritedEnv {
    /// `CLAUDE_CODE_OAUTH_TOKEN` explicitly set by the caller. When present it
    /// wins over pool selection — an operator override is never overridden.
    pub oauth_token: Option<String>,
    /// `LOOM_SHEPHERD_TASK_ID`, propagated so `claude-wrapper.sh` can skip its
    /// auth pre-flight for subprocess sessions (issue #2524).
    pub shepherd_task_id: Option<String>,
    /// `LOOM_MAX_RETRIES`, propagated so a caller that already owns retry
    /// logic can stop the wrapper from double-retrying (issue #2516).
    pub max_retries: Option<String>,
    /// The caller's `PYTHONPATH`, prefixed with the worktree's
    /// `loom-tools/src` when that directory exists (issue #2358).
    pub pythonpath: Option<String>,
}

impl InheritedEnv {
    /// Snapshot the relevant variables from the current process environment.
    pub fn from_process() -> Self {
        fn non_empty(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.is_empty())
        }
        Self {
            oauth_token: non_empty("CLAUDE_CODE_OAUTH_TOKEN"),
            shepherd_task_id: non_empty("LOOM_SHEPHERD_TASK_ID"),
            // Deliberately *not* `non_empty`: the Python original propagated
            // `LOOM_MAX_RETRIES` whenever it was set at all, including to an
            // empty string.
            max_retries: std::env::var("LOOM_MAX_RETRIES").ok(),
            pythonpath: non_empty("PYTHONPATH"),
        }
    }
}

/// Every external effect the spawn/wait logic performs.
///
/// Implemented by [`SystemEnv`] in production and by a scripted fake in tests.
pub trait AgentEnv {
    /// Run `tmux -L <socket> <args...>`. Returns `None` when tmux could not be
    /// executed at all (not installed, or the call timed out) — callers treat
    /// that identically to the Python original's
    /// `except (TimeoutExpired, FileNotFoundError)` branches.
    fn tmux(&self, args: &[&str]) -> Option<CmdOutput>;

    /// Whether a `claude` process is running as a child or grandchild of
    /// `shell_pid` (the grandchild case is `shell -> claude-wrapper.sh ->
    /// claude`).
    fn claude_running(&self, shell_pid: &str) -> bool;

    /// Current time as whole unix seconds.
    fn now(&self) -> u64;

    /// Sleep for `seconds`. The fake advances its clock instead of blocking.
    fn sleep(&self, seconds: u64);

    /// Whether the `tmux` binary is on `PATH`.
    fn tmux_available(&self) -> bool;

    /// Whether the `claude` binary is on `PATH`.
    fn claude_cli_available(&self) -> bool;

    /// Whether `path` is inside a git repository.
    fn git_repo_ok(&self, path: &Path) -> bool;

    /// Select an OAuth token from the workspace pool, or `None` to fall back
    /// to the per-agent Keychain credential.
    fn select_oauth_token(&self, repo_root: &Path) -> Option<String>;

    /// Create (or refresh) the isolated `CLAUDE_CONFIG_DIR` for `agent_name`.
    fn setup_config_dir(&self, agent_name: &str, repo_root: &Path) -> Option<PathBuf>;

    /// Whether `agent_name`'s config dir is present and healthy.
    fn validate_config_dir(&self, agent_name: &str, repo_root: &Path) -> bool;

    /// Remove `agent_name`'s config dir. Returns whether it existed.
    fn cleanup_config_dir(&self, agent_name: &str, repo_root: &Path) -> bool;

    /// Pre-seed the folder-trust flag for `project_dir` (issue #4334).
    fn trust_project(&self, project_dir: &Path);
}

// ---------------------------------------------------------------------------
// tmux session helpers (the native replacement for
// `loom_tools/common/tmux_session.py`'s `TmuxSession`)
// ---------------------------------------------------------------------------

/// Whether a tmux session exists (`has-session`).
pub fn session_exists(env: &dyn AgentEnv, session: &str) -> bool {
    env.tmux(&["has-session", "-t", session])
        .is_some_and(|o| o.success())
}

/// Whether the session exists *and* has at least one window.
pub fn session_is_alive(env: &dyn AgentEnv, session: &str) -> bool {
    match env.tmux(&["list-windows", "-t", session]) {
        Some(o) if o.success() => o.stdout.lines().any(|l| !l.trim().is_empty()),
        _ => false,
    }
}

/// Shell PID of the session's first pane, if any.
pub fn pane_pid(env: &dyn AgentEnv, session: &str) -> Option<String> {
    let out = env.tmux(&["list-panes", "-t", session, "-F", "#{pane_pid}"])?;
    if !out.success() {
        return None;
    }
    out.stdout
        .trim()
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Capture pane content. `scrollback` requests `-S -<n>` history lines;
/// `None` captures only the visible pane.
pub fn capture_pane(env: &dyn AgentEnv, session: &str, scrollback: Option<u32>) -> String {
    let out = match scrollback {
        Some(n) => {
            let start = format!("-{n}");
            env.tmux(&["capture-pane", "-t", session, "-p", "-S", &start])
        }
        None => env.tmux(&["capture-pane", "-t", session, "-p"]),
    };
    match out {
        Some(o) if o.success() => o.stdout,
        _ => String::new(),
    }
}

/// Send keystrokes to a session.
pub fn send_keys(env: &dyn AgentEnv, session: &str, keys: &[&str]) {
    let mut args = vec!["send-keys", "-t", session];
    args.extend_from_slice(keys);
    let _ = env.tmux(&args);
}

/// Kill a session (best effort).
pub fn kill_session(env: &dyn AgentEnv, session: &str) {
    let _ = env.tmux(&["kill-session", "-t", session]);
}

/// Set a session-scoped environment variable.
pub fn set_session_env(env: &dyn AgentEnv, session: &str, key: &str, value: &str) {
    let _ = env.tmux(&["set-environment", "-t", session, key, value]);
}

/// Age of the session in seconds since creation, or `-1` when unknown.
pub fn session_age(env: &dyn AgentEnv, session: &str) -> i64 {
    let Some(out) = env.tmux(&["display-message", "-t", session, "-p", "#{session_created}"])
    else {
        return -1;
    };
    if !out.success() {
        return -1;
    }
    let created: i64 = match out.stdout.trim().parse() {
        Ok(v) => v,
        Err(_) => return -1,
    };
    if created == 0 {
        return -1;
    }
    env.now() as i64 - created
}

// ---------------------------------------------------------------------------
// Operator-facing logging
// ---------------------------------------------------------------------------
//
// These mirror `loom_tools.common.logging`'s `log_info`/`log_warning`/
// `log_error`/`log_success`: a UTC-timestamped `[LEVEL] message` line written
// to **stderr**. Keeping them on stderr is load-bearing — `--json` callers
// (`loom-start.sh`, `agent-destroy.sh`, the sweep phases) parse stdout, and a
// stray progress line there would corrupt the payload.

fn emit(label: &str, message: &str) {
    eprintln!("[{}] [{label}] {message}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
}

/// Informational progress line (stderr).
pub fn log_info(message: impl AsRef<str>) {
    emit("INFO", message.as_ref());
}

/// Warning line (stderr).
pub fn log_warning(message: impl AsRef<str>) {
    emit("WARN", message.as_ref());
}

/// Error line (stderr).
pub fn log_error(message: impl AsRef<str>) {
    emit("ERROR", message.as_ref());
}

/// Success line (stderr).
pub fn log_success(message: impl AsRef<str>) {
    emit("OK", message.as_ref());
}

// ---------------------------------------------------------------------------
// Shared pure helpers
// ---------------------------------------------------------------------------

/// Escape `s` for embedding inside a single-quoted shell word.
///
/// Mirrors the Python original's `value.replace("'", "'\"'\"'")` — the callers
/// always wrap the result in `'...'`.
pub fn sh_escape(s: &str) -> String {
    s.replace('\'', "'\"'\"'")
}

/// Walk up from `start` to the repository root, requiring a `.loom/`
/// directory alongside the git root.
///
/// Mirrors `loom_tools.common.repo.find_repo_root`, including the worktree
/// case where `.git` is a *file* holding a `gitdir:` pointer back into the
/// main repository's `.git/worktrees/<name>` (the resolved root is the main
/// checkout, not the worktree).
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let current = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for candidate in current.ancestors() {
        let git_path = candidate.join(".git");
        if !git_path.exists() {
            continue;
        }
        let root = resolve_git_root(candidate, &git_path);
        if root.join(".loom").is_dir() {
            return Some(root);
        }
    }
    None
}

fn resolve_git_root(candidate: &Path, git_path: &Path) -> PathBuf {
    if git_path.is_dir() {
        return candidate.to_path_buf();
    }
    let Ok(text) = std::fs::read_to_string(git_path) else {
        return candidate.to_path_buf();
    };
    let text = text.trim();
    let Some(gitdir) = text.strip_prefix("gitdir:") else {
        return candidate.to_path_buf();
    };
    let gitdir = gitdir.trim();
    let joined = candidate.join(gitdir);
    let resolved = joined.canonicalize().unwrap_or(joined);
    // /repo/.git/worktrees/issue-42 -> /repo/.git -> /repo
    let mut p = resolved.as_path();
    while p.file_name().is_some_and(|n| n != ".git") {
        match p.parent() {
            Some(parent) if parent != p => p = parent,
            _ => break,
        }
    }
    if p.file_name().is_some_and(|n| n == ".git") {
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }
    candidate.to_path_buf()
}

// ---------------------------------------------------------------------------
// Production environment
// ---------------------------------------------------------------------------

/// The production [`AgentEnv`]: real tmux, real `pgrep`, real clock, the
/// native token pool, and `terminal.rs`'s `claude_config` module.
pub struct SystemEnv;

impl SystemEnv {
    fn run(program: &str, args: &[&str]) -> Option<CmdOutput> {
        let out = Command::new(program).args(args).output().ok()?;
        Some(CmdOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }

    fn on_path(binary: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {binary} >/dev/null 2>&1"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl AgentEnv for SystemEnv {
    fn tmux(&self, args: &[&str]) -> Option<CmdOutput> {
        let mut full = vec!["-L", TMUX_SOCKET];
        full.extend_from_slice(args);
        Self::run("tmux", &full)
    }

    fn claude_running(&self, shell_pid: &str) -> bool {
        // Direct child: shell -> claude (or shell -> claude-wrapper.sh).
        if let Some(out) = Self::run("pgrep", &["-P", shell_pid, "-f", "claude"]) {
            if out.success() {
                return true;
            }
        } else {
            return false;
        }

        // Grandchild: shell -> claude-wrapper.sh -> claude.
        let Some(children) = Self::run("pgrep", &["-P", shell_pid]) else {
            return false;
        };
        if !children.success() {
            return false;
        }
        for child in children.stdout.lines() {
            let child = child.trim();
            if child.is_empty() {
                continue;
            }
            if let Some(out) = Self::run("pgrep", &["-P", child, "-f", "claude"]) {
                if out.success() {
                    return true;
                }
            }
        }
        false
    }

    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn sleep(&self, seconds: u64) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }

    fn tmux_available(&self) -> bool {
        Self::on_path("tmux")
    }

    fn claude_cli_available(&self) -> bool {
        Self::on_path("claude")
    }

    fn git_repo_ok(&self, path: &Path) -> bool {
        Self::run("git", &["-C", &path.to_string_lossy(), "rev-parse", "--git-dir"])
            .is_some_and(|o| o.success())
    }

    fn select_oauth_token(&self, repo_root: &Path) -> Option<String> {
        // The native selector — the same code path `loom-daemon tokens select
        // --export` runs for `spawn-claude.sh` (issue #4228). An empty /
        // missing / fully-blocked pool is *not* an error here: the caller
        // falls back to the per-agent Keychain credential under
        // `CLAUDE_CONFIG_DIR`, exactly as the Python original did.
        match crate::tokens_pool::select::select_token(repo_root, None) {
            Ok(sel) if !sel.key.is_empty() => Some(sel.key),
            Ok(_) => None,
            Err(e) => {
                log::debug!("Token selection unavailable ({e}); falling back to Keychain auth");
                None
            }
        }
    }

    fn setup_config_dir(&self, agent_name: &str, repo_root: &Path) -> Option<PathBuf> {
        crate::terminal::claude_config_setup(agent_name, repo_root)
    }

    fn validate_config_dir(&self, agent_name: &str, repo_root: &Path) -> bool {
        crate::terminal::claude_config_validate(agent_name, repo_root)
    }

    fn cleanup_config_dir(&self, agent_name: &str, repo_root: &Path) -> bool {
        crate::terminal::claude_config_cleanup(agent_name, repo_root)
    }

    fn trust_project(&self, project_dir: &Path) {
        crate::terminal::claude_config_trust(project_dir);
    }
}

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::collections::VecDeque;

    /// Scripted [`AgentEnv`] used by the spawn/wait unit tests.
    ///
    /// tmux responses are queued per tmux subcommand (`has-session`,
    /// `capture-pane`, …). A queue with more than one entry pops; a queue with
    /// exactly one entry is sticky, so a test can set a steady-state answer
    /// with one call. Unqueued subcommands return [`CmdOutput::ok("")`].
    pub struct FakeEnv {
        pub calls: RefCell<Vec<Vec<String>>>,
        responses: RefCell<HashMap<String, VecDeque<CmdOutput>>>,
        claude_running: RefCell<VecDeque<bool>>,
        pub clock: Cell<u64>,
        pub sleeps: RefCell<Vec<u64>>,
        pub tmux_available: Cell<bool>,
        pub claude_cli_available: Cell<bool>,
        pub git_repo_ok: Cell<bool>,
        pub tmux_missing: Cell<bool>,
        pub oauth_token: RefCell<Option<String>>,
        pub config_dir: RefCell<Option<PathBuf>>,
        pub config_valid: Cell<bool>,
        pub cleanup_calls: RefCell<Vec<String>>,
        pub trusted: RefCell<Vec<PathBuf>>,
    }

    impl Default for FakeEnv {
        fn default() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(HashMap::new()),
                claude_running: RefCell::new(VecDeque::from(vec![true])),
                // Seeded from the real clock so tests that compare against
                // on-disk mtimes (stuck detection) see sane deltas.
                clock: Cell::new(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(1_000_000),
                ),
                sleeps: RefCell::new(Vec::new()),
                tmux_available: Cell::new(true),
                claude_cli_available: Cell::new(true),
                git_repo_ok: Cell::new(true),
                tmux_missing: Cell::new(false),
                oauth_token: RefCell::new(None),
                config_dir: RefCell::new(None),
                config_valid: Cell::new(true),
                cleanup_calls: RefCell::new(Vec::new()),
                trusted: RefCell::new(Vec::new()),
            }
        }
    }

    impl FakeEnv {
        pub fn new() -> Self {
            Self::default()
        }

        /// Append a response for tmux subcommand `sub`.
        pub fn queue(&self, sub: &str, out: CmdOutput) -> &Self {
            self.responses
                .borrow_mut()
                .entry(sub.to_string())
                .or_default()
                .push_back(out);
            self
        }

        /// Replace all queued responses for `sub` with a single sticky one.
        pub fn set(&self, sub: &str, out: CmdOutput) -> &Self {
            self.responses
                .borrow_mut()
                .insert(sub.to_string(), VecDeque::from(vec![out]));
            self
        }

        /// Set the (sticky) claude-running answer.
        pub fn set_claude_running(&self, running: bool) -> &Self {
            *self.claude_running.borrow_mut() = VecDeque::from(vec![running]);
            self
        }

        /// Queue a sequence of claude-running answers (last one sticks).
        pub fn queue_claude_running(&self, values: &[bool]) -> &Self {
            *self.claude_running.borrow_mut() = values.iter().copied().collect();
            self
        }

        /// All tmux invocations recorded so far, joined with spaces.
        pub fn call_strings(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|c| c.join(" ")).collect()
        }

        /// Whether any recorded tmux call contains `needle`.
        pub fn saw(&self, needle: &str) -> bool {
            self.call_strings().iter().any(|c| c.contains(needle))
        }

        /// The recorded `set-environment` value for `key`, if any.
        pub fn session_env(&self, key: &str) -> Option<String> {
            self.calls.borrow().iter().find_map(|c| {
                if c.first().map(String::as_str) == Some("set-environment") && c.get(3)? == key {
                    Some(c.get(4).cloned().unwrap_or_default())
                } else {
                    None
                }
            })
        }

        /// The command string sent via `send-keys ... C-m`, if any.
        pub fn sent_command(&self) -> Option<String> {
            self.calls.borrow().iter().find_map(|c| {
                if c.first().map(String::as_str) == Some("send-keys")
                    && c.last().map(String::as_str) == Some("C-m")
                {
                    c.get(3).cloned()
                } else {
                    None
                }
            })
        }
    }

    impl AgentEnv for FakeEnv {
        fn tmux(&self, args: &[&str]) -> Option<CmdOutput> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if self.tmux_missing.get() {
                return None;
            }
            let sub = args.first().copied().unwrap_or_default().to_string();
            let mut responses = self.responses.borrow_mut();
            match responses.get_mut(&sub) {
                Some(queue) if queue.len() > 1 => queue.pop_front(),
                Some(queue) => queue.front().cloned(),
                None => Some(CmdOutput::ok("")),
            }
        }

        fn claude_running(&self, _shell_pid: &str) -> bool {
            let mut q = self.claude_running.borrow_mut();
            if q.len() > 1 {
                q.pop_front().unwrap_or(false)
            } else {
                q.front().copied().unwrap_or(false)
            }
        }

        fn now(&self) -> u64 {
            self.clock.get()
        }

        fn sleep(&self, seconds: u64) {
            self.sleeps.borrow_mut().push(seconds);
            self.clock.set(self.clock.get() + seconds);
        }

        fn tmux_available(&self) -> bool {
            self.tmux_available.get()
        }

        fn claude_cli_available(&self) -> bool {
            self.claude_cli_available.get()
        }

        fn git_repo_ok(&self, _path: &Path) -> bool {
            self.git_repo_ok.get()
        }

        fn select_oauth_token(&self, _repo_root: &Path) -> Option<String> {
            self.oauth_token.borrow().clone()
        }

        fn setup_config_dir(&self, agent_name: &str, repo_root: &Path) -> Option<PathBuf> {
            Some(self.config_dir.borrow().clone().unwrap_or_else(|| {
                repo_root
                    .join(".loom")
                    .join("claude-config")
                    .join(agent_name)
            }))
        }

        fn validate_config_dir(&self, _agent_name: &str, _repo_root: &Path) -> bool {
            self.config_valid.get()
        }

        fn cleanup_config_dir(&self, agent_name: &str, _repo_root: &Path) -> bool {
            self.cleanup_calls.borrow_mut().push(agent_name.to_string());
            true
        }

        fn trust_project(&self, project_dir: &Path) {
            self.trusted.borrow_mut().push(project_dir.to_path_buf());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::testing::FakeEnv;
    use super::*;

    #[test]
    fn test_session_prefix_and_socket_match_bash() {
        assert_eq!(SESSION_PREFIX, "loom-");
        assert_eq!(TMUX_SOCKET, "loom");
        assert_eq!(session_name("builder-1"), "loom-builder-1");
    }

    #[test]
    fn test_processing_indicator_matches_python() {
        assert_eq!(PROCESSING_INDICATORS, "esc to interrupt");
    }

    #[test]
    fn test_session_exists_true() {
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::ok(""));
        assert!(session_exists(&env, "loom-a"));
    }

    #[test]
    fn test_session_exists_false() {
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::fail());
        assert!(!session_exists(&env, "loom-a"));
    }

    #[test]
    fn test_session_exists_false_when_tmux_missing() {
        let env = FakeEnv::new();
        env.tmux_missing.set(true);
        assert!(!session_exists(&env, "loom-a"));
    }

    #[test]
    fn test_session_is_alive_with_windows() {
        let env = FakeEnv::new();
        env.set("list-windows", CmdOutput::ok("0: bash* (1 panes)\n"));
        assert!(session_is_alive(&env, "loom-a"));
    }

    #[test]
    fn test_session_is_alive_false_on_error() {
        let env = FakeEnv::new();
        env.set("list-windows", CmdOutput::fail());
        assert!(!session_is_alive(&env, "loom-a"));
    }

    #[test]
    fn test_session_is_alive_false_on_empty_output() {
        let env = FakeEnv::new();
        env.set("list-windows", CmdOutput::ok("   \n\n"));
        assert!(!session_is_alive(&env, "loom-a"));
    }

    #[test]
    fn test_pane_pid_returns_first_line() {
        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("12345\n67890\n"));
        assert_eq!(pane_pid(&env, "loom-a").as_deref(), Some("12345"));
    }

    #[test]
    fn test_pane_pid_none_when_empty() {
        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("\n"));
        assert!(pane_pid(&env, "loom-a").is_none());
    }

    #[test]
    fn test_pane_pid_none_on_failure() {
        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::fail());
        assert!(pane_pid(&env, "loom-a").is_none());
    }

    #[test]
    fn test_capture_pane_visible_and_scrollback() {
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::ok("hello"));
        assert_eq!(capture_pane(&env, "loom-a", None), "hello");
        assert_eq!(capture_pane(&env, "loom-a", Some(200)), "hello");
        assert!(env.saw("capture-pane -t loom-a -p -S -200"));
    }

    #[test]
    fn test_capture_pane_empty_on_failure() {
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::fail());
        assert_eq!(capture_pane(&env, "loom-a", None), "");
    }

    #[test]
    fn test_session_age_valid() {
        let env = FakeEnv::new();
        env.clock.set(1_000_100);
        env.set("display-message", CmdOutput::ok("1000000\n"));
        assert_eq!(session_age(&env, "loom-a"), 100);
    }

    #[test]
    fn test_session_age_zero_timestamp_is_unknown() {
        let env = FakeEnv::new();
        env.set("display-message", CmdOutput::ok("0\n"));
        assert_eq!(session_age(&env, "loom-a"), -1);
    }

    #[test]
    fn test_session_age_missing_session_is_unknown() {
        let env = FakeEnv::new();
        env.set("display-message", CmdOutput::fail());
        assert_eq!(session_age(&env, "loom-a"), -1);
    }

    #[test]
    fn test_session_age_unparseable_is_unknown() {
        let env = FakeEnv::new();
        env.set("display-message", CmdOutput::ok("not-a-number"));
        assert_eq!(session_age(&env, "loom-a"), -1);
    }

    #[test]
    fn test_sh_escape_single_quotes() {
        assert_eq!(sh_escape("plain"), "plain");
        assert_eq!(sh_escape("it's"), "it'\"'\"'s");
    }

    #[test]
    fn test_kill_and_send_keys_record_calls() {
        let env = FakeEnv::new();
        send_keys(&env, "loom-a", &["Down", "Enter"]);
        kill_session(&env, "loom-a");
        set_session_env(&env, "loom-a", "FOO", "bar");
        assert!(env.saw("send-keys -t loom-a Down Enter"));
        assert!(env.saw("kill-session -t loom-a"));
        assert_eq!(env.session_env("FOO").as_deref(), Some("bar"));
    }

    #[test]
    fn test_find_repo_root_plain_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_repo_root(&nested).unwrap();
        assert_eq!(
            found.canonicalize().unwrap(),
            root.canonicalize().unwrap(),
            "should walk up to the repo root"
        );
    }

    #[test]
    fn test_find_repo_root_none_without_loom_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        assert!(find_repo_root(tmp.path()).is_none());
    }

    #[test]
    fn test_find_repo_root_from_worktree_resolves_main_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("issue-42")).unwrap();
        std::fs::create_dir_all(root.join(".loom")).unwrap();

        let worktree = root.join(".loom").join("worktrees").join("issue-42");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", root.join(".git/worktrees/issue-42").display()),
        )
        .unwrap();

        let found = find_repo_root(&worktree).unwrap();
        assert_eq!(found, root);
    }

    #[test]
    fn test_inherited_env_default_is_all_none() {
        let inherited = InheritedEnv::default();
        assert!(inherited.oauth_token.is_none());
        assert!(inherited.shepherd_task_id.is_none());
        assert!(inherited.max_retries.is_none());
        assert!(inherited.pythonpath.is_none());
    }
}
