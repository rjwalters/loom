//! Native `agent-spawn` (issue #4415).
//!
//! Replaces `loom_tools/agent_spawn.py` with full feature parity: tmux session
//! creation on the shared `loom` socket, log rotation + `pipe-pane` capture,
//! per-agent `CLAUDE_CONFIG_DIR` isolation (delegated to `terminal.rs`'s
//! `claude_config` module), folder-trust pre-seeding, worktree/`PYTHONPATH`
//! pinning, OAuth token injection from the native pool, spawn verification,
//! bypass-permissions modal auto-accept, and stuck-session detection/recovery.

use std::path::{Path, PathBuf};

use super::{
    capture_pane, find_repo_root, kill_session, log_error, log_info, log_success, log_warning,
    pane_pid, send_keys, session_exists, session_is_alive, session_name, set_session_env,
    sh_escape, AgentEnv, InheritedEnv, TMUX_SOCKET,
};

/// Seconds an idle session may go without log activity before it is treated as
/// stuck. Overridable via `LOOM_STUCK_SESSION_THRESHOLD`.
pub const DEFAULT_STUCK_THRESHOLD: u64 = 300;

/// Seconds to wait for the `claude` process to appear after `send-keys`.
/// Overridable via `LOOM_SPAWN_VERIFY_TIMEOUT`.
pub const DEFAULT_VERIFY_TIMEOUT: u64 = 10;

/// Total budget for the bypass-permissions modal poll loop.
pub const DEFAULT_BYPASS_POLL_TIMEOUT: u64 = 15;

/// Seconds between `capture-pane` attempts while polling for the modal.
pub const DEFAULT_BYPASS_POLL_INTERVAL: u64 = 1;

/// Markers that identify the "WARNING: Claude Code running in Bypass
/// Permissions mode" modal (issue #3348). Detection is intentionally
/// permissive — any marker counts as a hit, so a future rename of the warning
/// string stays auto-accepted until the list is updated.
pub const BYPASS_PROMPT_MARKERS: &[&str] =
    &["Bypass Permissions mode", "--dangerously-skip-permissions"];

/// Log substrings indicating a transient API error — the agent is waiting for
/// "try again" input rather than being stuck on a logic problem.
pub const API_ERROR_PATTERNS: &[&str] = &[
    "500 Internal Server Error",
    "Rate limit exceeded",
    "rate_limit",
    "overloaded",
    "temporarily unavailable",
    "503 Service",
    "502 Bad Gateway",
    "Connection refused",
    "ECONNREFUSED",
    "ETIMEDOUT",
    "ECONNRESET",
    "NetworkError",
    "network error",
    "socket hang up",
    "No messages returned",
];

/// Number of log lines from the tail inspected for API-error patterns.
const API_ERROR_TAIL_LINES: usize = 50;

/// Outcome of a spawn attempt. Serialized by `--json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnResult {
    pub status: String,
    pub name: String,
    pub session: String,
    pub on_demand: bool,
    pub log: String,
    pub error: String,
}

impl SpawnResult {
    fn new(status: &str, name: &str) -> Self {
        Self {
            status: status.to_string(),
            name: name.to_string(),
            session: String::new(),
            on_demand: false,
            log: String::new(),
            error: String::new(),
        }
    }

    fn error(name: &str, error: &str) -> Self {
        let mut r = Self::new("error", name);
        r.error = error.to_string();
        r
    }

    /// JSON payload, byte-compatible with the Python `SpawnResult.to_dict()`
    /// (empty fields are omitted; `on_demand` appears only for
    /// `spawned`/`exists`).
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("status".into(), self.status.clone().into());
        obj.insert("name".into(), self.name.clone().into());
        if !self.session.is_empty() {
            obj.insert("session".into(), self.session.clone().into());
        }
        if self.status == "spawned" || self.status == "exists" {
            obj.insert("on_demand".into(), self.on_demand.into());
        }
        if !self.log.is_empty() {
            obj.insert("log".into(), self.log.clone().into());
        }
        if !self.error.is_empty() {
            obj.insert("error".into(), self.error.clone().into());
        }
        serde_json::Value::Object(obj)
    }
}

/// Everything the spawn path needs, resolved up front from CLI flags and the
/// process environment.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub role: String,
    pub name: String,
    pub args: String,
    pub worktree: String,
    pub on_demand: bool,
    pub fresh: bool,
    pub do_wait: bool,
    pub wait_timeout: u64,
    pub json_output: bool,
    pub check_name: String,
    pub do_list: bool,
    pub stuck_threshold: u64,
    pub verify_timeout: u64,
    /// `LOOM_AUTO_ACCEPT_BYPASS != "0"`.
    pub auto_accept_bypass: bool,
    pub bypass_poll_timeout: u64,
    pub bypass_poll_interval: u64,
    pub inherited: InheritedEnv,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            role: String::new(),
            name: String::new(),
            args: String::new(),
            worktree: String::new(),
            on_demand: false,
            fresh: false,
            do_wait: false,
            wait_timeout: 3600,
            json_output: false,
            check_name: String::new(),
            do_list: false,
            stuck_threshold: DEFAULT_STUCK_THRESHOLD,
            verify_timeout: DEFAULT_VERIFY_TIMEOUT,
            auto_accept_bypass: true,
            bypass_poll_timeout: DEFAULT_BYPASS_POLL_TIMEOUT,
            bypass_poll_interval: DEFAULT_BYPASS_POLL_INTERVAL,
            inherited: InheritedEnv::default(),
        }
    }
}

impl SpawnOptions {
    /// Resolve the environment-tunable knobs (`LOOM_STUCK_SESSION_THRESHOLD`,
    /// `LOOM_SPAWN_VERIFY_TIMEOUT`, `LOOM_AUTO_ACCEPT_BYPASS`) plus the
    /// inherited spawn variables. Called once, at CLI-parse time.
    pub fn with_process_env(mut self) -> Self {
        fn env_u64(key: &str, default: u64) -> u64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
        }
        self.stuck_threshold = env_u64("LOOM_STUCK_SESSION_THRESHOLD", DEFAULT_STUCK_THRESHOLD);
        self.verify_timeout = env_u64("LOOM_SPAWN_VERIFY_TIMEOUT", DEFAULT_VERIFY_TIMEOUT);
        self.auto_accept_bypass = std::env::var("LOOM_AUTO_ACCEPT_BYPASS").as_deref() != Ok("0");
        self.inherited = InheritedEnv::from_process();
        self
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Whether a role definition exists under `.loom/roles/` or
/// `.claude/commands/loom/`. Symlinked role files count (Loom installs
/// `.loom/roles` as a symlink in this repo).
pub fn validate_role(role: &str, repo_root: &Path) -> bool {
    let loom_role = repo_root
        .join(".loom")
        .join("roles")
        .join(format!("{role}.md"));
    if loom_role.is_file() || loom_role.symlink_metadata().is_ok() {
        return true;
    }
    let command_role = repo_root
        .join(".claude")
        .join("commands")
        .join("loom")
        .join(format!("{role}.md"));
    if command_role.is_file() {
        return true;
    }

    log_error(format!("Role not found: {role}"));
    log_info(format!("Expected at: {}/.loom/roles/{role}.md", repo_root.display()));
    log_info(format!("         or: {}/.claude/commands/loom/{role}.md", repo_root.display()));
    log_info("");
    log_info("Available roles:");
    let roles_dir = repo_root.join(".loom").join("roles");
    if let Ok(entries) = std::fs::read_dir(&roles_dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    return None;
                }
                let stem = path.file_stem()?.to_string_lossy().to_string();
                if stem == "README" {
                    return None;
                }
                Some(stem)
            })
            .collect();
        names.sort();
        for name in names {
            log_info(format!("  - {name}"));
        }
    }
    false
}

/// Whether `worktree_path` exists and is a git repository.
pub fn validate_worktree(env: &dyn AgentEnv, worktree_path: &Path) -> bool {
    if !worktree_path.is_dir() {
        log_error(format!("Worktree path does not exist: {}", worktree_path.display()));
        return false;
    }
    if !env.git_repo_ok(worktree_path) {
        log_error(format!("Not a valid git repository: {}", worktree_path.display()));
        return false;
    }
    true
}

/// Whether a stop signal blocks spawning `name`.
pub fn check_stop_signals(name: &str, repo_root: &Path) -> bool {
    let loom = repo_root.join(".loom");
    if loom.join("stop-daemon").exists() {
        log_warning("Global stop signal exists (.loom/stop-daemon) - not spawning");
        return true;
    }
    if name.starts_with("shepherd-") && loom.join("stop-shepherds").exists() {
        log_warning("Shepherd stop signal exists (.loom/stop-shepherds) - not spawning");
        return true;
    }
    if loom.join("signals").join(format!("stop-{name}")).exists() {
        log_warning(format!("Agent stop signal exists (.loom/signals/stop-{name}) - not spawning"));
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Stuck detection
// ---------------------------------------------------------------------------

/// Scan the tail of `log_file` for a transient-API-error marker.
///
/// Returns the matched pattern, or `None`. Matching is case-insensitive and
/// restricted to the last [`API_ERROR_TAIL_LINES`] lines so historical errors
/// earlier in a long log never re-trigger.
pub fn check_log_for_api_errors(log_file: &Path, tail_lines: usize) -> Option<&'static str> {
    if !log_file.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(log_file).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(tail_lines);
    let tail = lines[start..].join("\n").to_lowercase();
    API_ERROR_PATTERNS
        .iter()
        .find(|p| tail.contains(&p.to_lowercase()))
        .copied()
}

/// Whether an existing session is stuck (no claude process, or an idle log
/// past `threshold` with no recent progress milestone).
pub fn session_is_stuck(env: &dyn AgentEnv, name: &str, repo_root: &Path, threshold: u64) -> bool {
    let session = session_name(name);
    let log_file = repo_root
        .join(".loom")
        .join("logs")
        .join(format!("{session}.log"));

    // Check 1: is claude actually running in this session?
    let Some(shell_pid) = pane_pid(env, &session) else {
        log_warning("Session has no shell PID - considered stuck");
        return true;
    };
    if !env.claude_running(&shell_pid) {
        log_warning("No claude process found in session - considered stuck");
        return true;
    }

    // Check 2: has the log been written to recently?
    let Some(idle_seconds) = file_age_seconds(env, &log_file) else {
        return false;
    };
    if idle_seconds < threshold {
        return false;
    }
    log_warning(format!("Session log idle for {idle_seconds}s (threshold: {threshold}s)"));

    // Check 3: a recent progress milestone means the session is still alive.
    let progress_dir = repo_root.join(".loom").join("progress");
    if let Ok(entries) = std::fs::read_dir(&progress_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name_matches = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("shepherd-") && n.ends_with(".json"));
            if !name_matches {
                continue;
            }
            if file_age_seconds(env, &path).is_some_and(|age| age < threshold) {
                log_info("Recent progress milestone found - session may still be active");
                return false;
            }
        }
    }

    // Check 4: surface a transient API error if the log shows one.
    if let Some(pattern) = check_log_for_api_errors(&log_file, API_ERROR_TAIL_LINES) {
        log_warning(format!(
            "API error pattern detected in log: {pattern} - session likely waiting \
             for 'try again' input"
        ));
    }
    true
}

/// Seconds since `path` was last modified, or `None` when it does not exist /
/// its mtime is unreadable.
fn file_age_seconds(env: &dyn AgentEnv, path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(env.now().saturating_sub(mtime))
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// Best-effort capture of a session's scrollback before it is killed.
///
/// Writes to `.loom/logs/<session>-killed-<timestamp>.log`. Every failure is
/// logged and swallowed — capture must never prevent the kill.
pub fn capture_session_output(env: &dyn AgentEnv, session: &str, repo_root: &Path) {
    let output = capture_pane(env, session, Some(200));
    if output.trim().is_empty() {
        log_info(format!("No scrollback content to capture for {session}"));
        return;
    }
    let log_dir = repo_root.join(".loom").join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        log_warning(format!("Failed to capture session output: {e}"));
        return;
    }
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let kill_log = log_dir.join(format!("{session}-killed-{timestamp}.log"));
    match std::fs::write(&kill_log, output) {
        Ok(()) => log_info(format!("Captured session output to {}", kill_log.display())),
        Err(e) => log_warning(format!("Failed to capture session output: {e}")),
    }
}

/// Kill a stuck session: capture scrollback, `C-c`, then `kill-session`.
pub fn kill_stuck_session(env: &dyn AgentEnv, name: &str, repo_root: &Path) {
    let session = session_name(name);
    log_warning(format!("Killing stuck session: {session}"));
    capture_session_output(env, &session, repo_root);
    send_keys(env, &session, &["C-c"]);
    env.sleep(1);
    kill_session(env, &session);
    log_success(format!("Stuck session killed: {session}"));
}

/// Kill a session whose windows are gone.
pub fn cleanup_dead_session(env: &dyn AgentEnv, name: &str) {
    let session = session_name(name);
    log_info(format!("Cleaning up dead session: {session}"));
    kill_session(env, &session);
}

/// Print all sessions on the loom socket.
pub fn list_sessions(env: &dyn AgentEnv) {
    match env.tmux(&["list-sessions"]) {
        Some(out) if out.success() && !out.stdout.trim().is_empty() => {
            println!("{}", out.stdout.trim());
        }
        Some(_) => log_info("No active loom-agent sessions"),
        None => log_info("No active loom-agent sessions (tmux not available)"),
    }
}

/// Whether a captured pane shows the bypass-permissions modal.
pub fn bypass_prompt_visible(pane: &str) -> bool {
    BYPASS_PROMPT_MARKERS.iter().any(|m| pane.contains(m))
}

/// Poll the pane for the bypass-permissions modal and accept it.
///
/// The modal's default selection is "1. No, exit", which would terminate the
/// session on an unattended Enter — so we send `Down Enter` to pick "2. Yes, I
/// accept". Returns whether the modal was seen and answered.
pub fn auto_accept_bypass_prompt(env: &dyn AgentEnv, session: &str, opts: &SpawnOptions) -> bool {
    if !opts.auto_accept_bypass {
        log_info("Bypass auto-accept disabled via LOOM_AUTO_ACCEPT_BYPASS=0");
        return false;
    }
    let interval = opts.bypass_poll_interval.max(1);
    let mut elapsed = 0;
    while elapsed < opts.bypass_poll_timeout {
        if bypass_prompt_visible(&capture_pane(env, session, Some(200))) {
            log_info(format!(
                "Bypass-permissions modal detected after {elapsed}s — sending Down+Enter to accept"
            ));
            send_keys(env, session, &["Down", "Enter"]);
            return true;
        }
        env.sleep(interval);
        elapsed += interval;
    }
    log_info(format!(
        "Bypass-permissions modal not detected within {}s (claude may have skipped \
         the prompt or already moved past it)",
        opts.bypass_poll_timeout
    ));
    false
}

// ---------------------------------------------------------------------------
// Command construction (pure)
// ---------------------------------------------------------------------------

/// Inputs to [`build_claude_command`].
pub struct ClaudeCommand<'a> {
    pub token: Option<&'a str>,
    /// Effective `PYTHONPATH` for the session, when the target has a
    /// `loom-tools/src` directory.
    pub pythonpath: Option<&'a str>,
    /// Set only when spawning into a worktree distinct from the repo root.
    pub worktree_path: Option<&'a Path>,
    pub max_retries: Option<&'a str>,
    pub shepherd_task_id: Option<&'a str>,
    pub name: &'a str,
    pub working_dir: &'a Path,
    pub config_dir: &'a Path,
    /// `claude-wrapper.sh`, when present and executable.
    pub wrapper: Option<&'a Path>,
    pub role_cmd: &'a str,
}

/// Build the shell command line sent to the tmux session.
///
/// Kept pure (and unit-tested) because this string is the entire contract
/// between Loom and a spawned agent: an env-prefix ordering slip silently
/// changes which token, config dir, or worktree the agent runs under.
pub fn build_claude_command(cmd: &ClaudeCommand<'_>) -> String {
    let mut prefix = String::new();
    if let Some(token) = cmd.token {
        prefix.push_str(&format!("CLAUDE_CODE_OAUTH_TOKEN='{}' ", sh_escape(token)));
    }
    if let Some(pythonpath) = cmd.pythonpath {
        prefix.push_str(&format!("PYTHONPATH='{pythonpath}' "));
    }
    if let Some(worktree) = cmd.worktree_path {
        prefix.push_str(&format!("LOOM_WORKTREE_PATH='{}' ", worktree.display()));
    }
    if let Some(max_retries) = cmd.max_retries {
        prefix.push_str(&format!("LOOM_MAX_RETRIES='{max_retries}' "));
    }
    if let Some(task_id) = cmd.shepherd_task_id {
        prefix.push_str(&format!("LOOM_SHEPHERD_TASK_ID='{task_id}' "));
    }

    match cmd.wrapper {
        Some(wrapper) => format!(
            "{prefix}LOOM_TERMINAL_ID='{name}' LOOM_WORKSPACE='{working_dir}' \
             CLAUDE_CONFIG_DIR='{config_dir}' TMPDIR='{tmpdir}' \
             '{wrapper}' --dangerously-skip-permissions \"{role_cmd}\"",
            name = cmd.name,
            working_dir = cmd.working_dir.display(),
            config_dir = cmd.config_dir.display(),
            tmpdir = cmd.config_dir.join("tmp").display(),
            wrapper = wrapper.display(),
            role_cmd = cmd.role_cmd,
        ),
        None => {
            // Only the token prefix survives the wrapper-less fallback — the
            // rest of the env is delivered via `tmux set-environment`.
            let token_prefix = match cmd.token {
                Some(token) => format!("CLAUDE_CODE_OAUTH_TOKEN='{}' ", sh_escape(token)),
                None => String::new(),
            };
            format!("{token_prefix}claude --dangerously-skip-permissions \"{}\"", cmd.role_cmd)
        }
    }
}

/// The `pipe-pane` filter command: the native `loom-daemon strip-ansi` filter,
/// falling back to GNU/BSD `sed` line-buffered ANSI stripping.
///
/// Issue #4275 repointed the first rung from `python3 -u -m
/// loom_tools.log_filter` (deleted with the rest of the script-helper family) to
/// the native subcommand that replaced it. The behavior of that rung is
/// unchanged — `strip-ansi` with no `--file` is the same real-time stdin filter,
/// including the `[repeated N more times]` dedup summary and the #2798
/// short-line safety rule.
///
/// The two `sed` rungs are retained verbatim: this string is evaluated inside
/// tmux's own shell long after the daemon returns, so if the binary is missing
/// or unreadable at that moment the pane must still get *some* ANSI stripping
/// rather than losing the log entirely. `$LOOM_DAEMON_BIN` is honored first for
/// the same reason every other resolver honors it (a non-PATH install).
pub fn pipe_filter_cmd(log_file: &Path) -> String {
    let log = log_file.display();
    format!(
        "\"${{LOOM_DAEMON_BIN:-loom-daemon}}\" strip-ansi >> '{log}' 2>/dev/null \
         || sed -l -E 's/\\x1b\\[[?0-9;]*[a-zA-Z]//g; s/\\x1b\\][^\\x07]*\\x07//g' >> '{log}' 2>/dev/null \
         || sed -u -E 's/\\x1b\\[[?0-9;]*[a-zA-Z]//g; s/\\x1b\\][^\\x07]*\\x07//g' >> '{log}'"
    )
}

/// The rotated name for an existing log file: `<stem>.<timestamp>.log`.
pub fn rotated_log_path(log_file: &Path, timestamp: &str) -> PathBuf {
    let stem = log_file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    log_file.with_file_name(format!("{stem}.{timestamp}.log"))
}

/// The role slash command for a spawn (`/loom:<role> [args]`).
///
/// The `loom:` namespace is required: role definitions live under
/// `.claude/commands/loom/<role>.md` since #3176 and Claude Code 2.1+ rejects
/// the bare `/<role>` form with "Unknown command" (issue #3345).
pub fn role_command(role: &str, args: &str) -> String {
    if args.is_empty() {
        format!("/loom:{role}")
    } else {
        format!("/loom:{role} {args}")
    }
}

/// Resolve the spawn target directory from `--worktree` (relative paths are
/// resolved against the repo root; an empty value means the repo root).
pub fn resolve_working_dir(worktree: &str, repo_root: &Path) -> PathBuf {
    if worktree.is_empty() {
        return repo_root.to_path_buf();
    }
    let p = Path::new(worktree);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo_root.join(p)
    }
}

// ---------------------------------------------------------------------------
// Core spawn
// ---------------------------------------------------------------------------

/// Create the tmux session and start the agent inside it.
pub fn spawn_agent(env: &dyn AgentEnv, opts: &SpawnOptions, repo_root: &Path) -> SpawnResult {
    let session = session_name(&opts.name);
    let log_dir = repo_root.join(".loom").join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        log_error(format!("Could not create log directory: {e}"));
        return SpawnResult::error(&opts.name, "log_dir_failed");
    }
    let log_file = log_dir.join(format!("{session}.log"));
    let working_dir = resolve_working_dir(&opts.worktree, repo_root);

    // Rotate the previous log so each spawn starts from a clean file.
    if log_file.is_file() {
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let rotated = rotated_log_path(&log_file, &timestamp);
        if std::fs::rename(&log_file, &rotated).is_ok() {
            log_info("Rotated previous log file");
        }
    }

    let started = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let header = format!(
        "# Loom Agent Log\n\
         # Session: {session}\n\
         # Role: {role}\n\
         # Args: {args}\n\
         # Working Directory: {working_dir}\n\
         # Started: {started}\n\
         # ---\n",
        role = opts.role,
        args = opts.args,
        working_dir = working_dir.display(),
    );
    if let Err(e) = std::fs::write(&log_file, header) {
        log_error(format!("Could not write log header: {e}"));
        return SpawnResult::error(&opts.name, "log_write_failed");
    }

    log_info(format!("Creating tmux session: {session}"));
    log_info(format!("Working directory: {}", working_dir.display()));
    log_info(format!("Log file: {}", log_file.display()));

    let created = env.tmux(&[
        "new-session",
        "-d",
        "-s",
        &session,
        "-c",
        &working_dir.to_string_lossy(),
    ]);
    if !created.is_some_and(|o| o.success()) {
        log_error(format!("Failed to create tmux session: {session}"));
        return SpawnResult::error(&opts.name, "session_create_failed");
    }

    // Output capture.
    let filter = pipe_filter_cmd(&log_file);
    let piped = env.tmux(&["pipe-pane", "-t", &session, &filter]);
    if !piped.is_some_and(|o| o.success()) {
        log_warning("Failed to set up output capture (continuing anyway)");
    }

    set_session_env(env, &session, "LOOM_TERMINAL_ID", &opts.name);
    set_session_env(env, &session, "LOOM_WORKSPACE", &working_dir.to_string_lossy());
    set_session_env(env, &session, "LOOM_ROLE", &opts.role);
    // Explicit empty value, never `-u`: tmux's `-u` removes the session-level
    // override, letting the tmux *server* environment's CLAUDECODE leak back
    // in and trip Claude Code's nested-session guard (issue #3208).
    set_session_env(env, &session, "CLAUDECODE", "");

    if let Some(task_id) = opts.inherited.shepherd_task_id.as_deref() {
        set_session_env(env, &session, "LOOM_SHEPHERD_TASK_ID", task_id);
    }

    // Per-agent CLAUDE_CONFIG_DIR isolation, delegated to terminal.rs's
    // `claude_config` module (the single native implementation). Setup is
    // idempotent and skips existing files, so a corrupted dir would otherwise
    // silently persist across every retry — validate first, and reinitialize
    // when it fails (issue #2909).
    if !env.validate_config_dir(&opts.name, repo_root)
        && env.cleanup_config_dir(&opts.name, repo_root)
    {
        log_warning(format!(
            "Agent config dir for '{}' failed validation — reinitializing before spawn",
            opts.name
        ));
    }
    let Some(config_dir) = env.setup_config_dir(&opts.name, repo_root) else {
        log_error(format!("Failed to set up CLAUDE_CONFIG_DIR for '{}'", opts.name));
        return SpawnResult::error(&opts.name, "config_dir_failed");
    };
    set_session_env(env, &session, "CLAUDE_CONFIG_DIR", &config_dir.to_string_lossy());
    set_session_env(env, &session, "TMPDIR", &config_dir.join("tmp").to_string_lossy());

    // Pre-seed folder trust for the *spawn target* (the worktree path when
    // spawning into one — trust is keyed per-path), so a freshly-created
    // worktree Claude Code has never opened does not stall the
    // non-interactive session on the trust modal (issue #4334).
    env.trust_project(&working_dir);

    // PYTHONPATH pinning so pytest inside a worktree resolves imports from the
    // worktree's own source rather than the main repo's editable install
    // (issue #2358).
    let worktree_src = working_dir.join("loom-tools").join("src");
    let mut pythonpath_value: Option<String> = None;
    if worktree_src.is_dir() {
        let value = match opts.inherited.pythonpath.as_deref() {
            Some(existing) => format!("{}:{existing}", worktree_src.display()),
            None => worktree_src.display().to_string(),
        };
        set_session_env(env, &session, "PYTHONPATH", &value);
        pythonpath_value = Some(value);
    }

    // Pin git operations to the worktree so absolute paths cannot resolve back
    // to the main repo (issue #2418), and export LOOM_WORKTREE_PATH so the
    // PreToolUse hook can confine Edit/Write (issue #2441).
    let in_worktree = !opts.worktree.is_empty() && working_dir != repo_root;
    if in_worktree {
        let git_file = working_dir.join(".git");
        if git_file.exists() {
            set_session_env(env, &session, "GIT_WORK_TREE", &working_dir.to_string_lossy());
            set_session_env(env, &session, "GIT_DIR", &git_file.to_string_lossy());
        }
        set_session_env(env, &session, "LOOM_WORKTREE_PATH", &working_dir.to_string_lossy());
    }

    let role_cmd = role_command(&opts.role, &opts.args);

    // OAuth token rotation (issue #3236). An explicit caller-supplied token
    // always wins; otherwise select from the native pool. When neither is
    // available we inject nothing and Claude Code falls back to the per-agent
    // Keychain credential under CLAUDE_CONFIG_DIR (backward compatible).
    let token: Option<String> = match opts.inherited.oauth_token.clone() {
        Some(inherited) => {
            log_info("Using CLAUDE_CODE_OAUTH_TOKEN from caller environment");
            Some(inherited)
        }
        None => match env.select_oauth_token(repo_root) {
            Some(selected) => {
                // Surface it on the session env too, so a later interactive
                // `claude` in the same session inherits the same account.
                set_session_env(env, &session, "CLAUDE_CODE_OAUTH_TOKEN", &selected);
                log_info("Injected CLAUDE_CODE_OAUTH_TOKEN from workspace token pool");
                Some(selected)
            }
            None => None,
        },
    };

    let wrapper_script = repo_root
        .join(".loom")
        .join("scripts")
        .join("claude-wrapper.sh");
    let wrapper = if is_executable(&wrapper_script) {
        Some(wrapper_script.as_path())
    } else {
        log_warning("claude-wrapper.sh not found, using claude directly (no retry logic)");
        None
    };

    let claude_cmd = build_claude_command(&ClaudeCommand {
        token: token.as_deref(),
        pythonpath: pythonpath_value.as_deref(),
        worktree_path: in_worktree.then_some(working_dir.as_path()),
        max_retries: opts.inherited.max_retries.as_deref(),
        shepherd_task_id: opts.inherited.shepherd_task_id.as_deref(),
        name: &opts.name,
        working_dir: &working_dir,
        config_dir: &config_dir,
        wrapper,
        role_cmd: &role_cmd,
    });

    log_info(format!("Starting Claude CLI with command: {role_cmd}"));
    send_keys(env, &session, &[&claude_cmd, "C-m"]);

    // Verify the spawn actually produced a claude process.
    log_info(format!("Verifying Claude process started (up to {}s)...", opts.verify_timeout));
    let mut elapsed = 0;
    let mut detected = false;
    while elapsed < opts.verify_timeout {
        if !session_exists(env, &session) {
            log_error(format!("tmux session disappeared: {session}"));
            return SpawnResult::error(&opts.name, "session_disappeared");
        }
        if pane_pid(env, &session).is_some_and(|pid| env.claude_running(&pid)) {
            log_info(format!("Claude process detected after {elapsed}s"));
            detected = true;
            break;
        }
        env.sleep(1);
        elapsed += 1;
    }

    if !detected {
        log_error(format!("Claude process not detected within {}s", opts.verify_timeout));
        log_error(format!("Session: {session}"));
        log_error("The tmux session exists but no claude process is running.");
        log_error(format!("Check: tmux -L {TMUX_SOCKET} attach -t {session}"));
        return SpawnResult::error(&opts.name, "process_not_detected");
    }

    log_success("Agent spawned successfully");

    auto_accept_bypass_prompt(env, &session, opts);

    log_info("");
    log_info(format!("Session: {session}"));
    log_info(format!("Attach:  tmux -L {TMUX_SOCKET} attach -t {session}"));
    log_info(format!("Logs:    tail -f {}", log_file.display()));
    log_info(format!("Stop:    ./.loom/scripts/signal.sh stop {}", opts.name));

    let mut result = SpawnResult::new("spawned", &opts.name);
    result.session = session;
    result.log = log_file.display().to_string();
    result
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

/// Execute the spawn command. Returns the process exit code
/// (`0` success, `1` error — matching `agent-spawn.sh`'s contract).
pub fn run(env: &dyn AgentEnv, opts: &SpawnOptions, start_dir: &Path) -> i32 {
    if opts.do_list {
        list_sessions(env);
        return 0;
    }

    if !opts.check_name.is_empty() {
        return if session_exists(env, &session_name(&opts.check_name)) {
            log_success(format!("Session exists: {}", session_name(&opts.check_name)));
            0
        } else {
            log_info(format!("Session does not exist: {}", session_name(&opts.check_name)));
            1
        };
    }

    if opts.role.is_empty() {
        log_error("Missing required parameter: --role");
        log_info("Run 'loom-daemon agent-spawn --help' for usage");
        return 1;
    }
    if opts.name.is_empty() {
        log_error("Missing required parameter: --name");
        log_info("Run 'loom-daemon agent-spawn --help' for usage");
        return 1;
    }

    let Some(repo_root) = find_repo_root(start_dir) else {
        log_error("Not in a git repository");
        return 1;
    };

    if !env.tmux_available() {
        log_error("tmux is not installed");
        log_info("Install with: brew install tmux (macOS) or apt-get install tmux (Linux)");
        return 1;
    }
    if !env.claude_cli_available() {
        log_error("Claude CLI not found in PATH");
        log_info("Install with: npm install -g @anthropic-ai/claude-code");
        return 1;
    }
    if !validate_role(&opts.role, &repo_root) {
        return 1;
    }
    if !opts.worktree.is_empty()
        && !validate_worktree(env, &resolve_working_dir(&opts.worktree, &repo_root))
    {
        return 1;
    }
    if check_stop_signals(&opts.name, &repo_root) {
        return 1;
    }

    // Idempotency: reuse a healthy session, recover a stuck one, reap a dead one.
    let session = session_name(&opts.name);
    if session_exists(env, &session) {
        if opts.fresh {
            log_info(format!("Fresh session requested - killing existing session: {session}"));
            kill_stuck_session(env, &opts.name, &repo_root);
        } else if session_is_alive(env, &session) {
            log_info(format!("Checking health of existing session: {session}"));
            if session_is_stuck(env, &opts.name, &repo_root, opts.stuck_threshold) {
                log_warning(format!(
                    "Session is stuck (idle > {}s with no progress)",
                    opts.stuck_threshold
                ));
                log_info("Recovering: killing stuck session and restarting fresh");
                kill_stuck_session(env, &opts.name, &repo_root);
            } else {
                log_success(format!("Session already exists and is healthy: {session}"));
                log_info(format!("Attach:  tmux -L {TMUX_SOCKET} attach -t {session}"));
                return 0;
            }
        } else {
            cleanup_dead_session(env, &opts.name);
        }
    }

    let mut result = spawn_agent(env, opts, &repo_root);

    if result.status == "error" {
        if opts.json_output {
            println!("{}", result.to_json());
        }
        return 1;
    }

    if opts.on_demand {
        result.on_demand = true;
        set_session_env(env, &result.session, "LOOM_ON_DEMAND", "true");
    }

    if opts.json_output && !opts.do_wait {
        println!("{}", result.to_json());
    }

    if opts.do_wait {
        let wait_opts = super::wait::WaitOptions {
            name: opts.name.clone(),
            timeout: opts.wait_timeout,
            json_output: opts.json_output,
            ..Default::default()
        };
        let wait_result = super::wait::wait_for_agent(env, &wait_opts, &repo_root);
        if opts.json_output {
            println!("{}", wait_result.to_json());
        }
        return wait_result.exit_code();
    }

    0
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::testing::FakeEnv;
    use super::super::CmdOutput;
    use super::*;

    fn repo(tmp: &tempfile::TempDir) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".loom").join("roles")).unwrap();
        std::fs::write(root.join(".loom").join("roles").join("builder.md"), "role").unwrap();
        let scripts = root.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let wrapper = scripts.join("claude-wrapper.sh");
        std::fs::write(&wrapper, "#!/bin/bash\nexec claude \"$@\"\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        root
    }

    fn spawn_opts(name: &str) -> SpawnOptions {
        SpawnOptions {
            role: "builder".into(),
            name: name.into(),
            verify_timeout: 3,
            // Keep the modal poll out of the way of assertions about tmux calls.
            auto_accept_bypass: false,
            ..Default::default()
        }
    }

    fn ready_env() -> FakeEnv {
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::ok(""));
        env.set("list-panes", CmdOutput::ok("4242\n"));
        env.set_claude_running(true);
        env
    }

    // --- SpawnResult JSON shape ------------------------------------------

    #[test]
    fn test_to_json_spawned() {
        let mut r = SpawnResult::new("spawned", "builder-1");
        r.session = "loom-builder-1".into();
        r.log = "/tmp/x.log".into();
        let json = r.to_json();
        assert_eq!(json["status"], "spawned");
        assert_eq!(json["name"], "builder-1");
        assert_eq!(json["session"], "loom-builder-1");
        assert_eq!(json["on_demand"], false);
        assert_eq!(json["log"], "/tmp/x.log");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_to_json_error_omits_on_demand() {
        let json = SpawnResult::error("builder-1", "session_create_failed").to_json();
        assert_eq!(json["status"], "error");
        assert_eq!(json["error"], "session_create_failed");
        assert!(json.get("on_demand").is_none());
        assert!(json.get("session").is_none());
        assert!(json.get("log").is_none());
    }

    #[test]
    fn test_to_json_on_demand_true() {
        let mut r = SpawnResult::new("spawned", "w");
        r.on_demand = true;
        assert_eq!(r.to_json()["on_demand"], true);
    }

    // --- Validation -------------------------------------------------------

    #[test]
    fn test_validate_role_in_loom_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        assert!(validate_role("builder", &root));
    }

    #[test]
    fn test_validate_role_in_claude_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let dir = root.join(".claude").join("commands").join("loom");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("judge.md"), "role").unwrap();
        assert!(validate_role("judge", &root));
    }

    #[test]
    fn test_validate_role_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        assert!(!validate_role("nope", &root));
    }

    #[test]
    fn test_validate_role_accepts_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let target = root.join("curator-source.md");
        std::fs::write(&target, "role").unwrap();
        std::os::unix::fs::symlink(&target, root.join(".loom/roles/curator.md")).unwrap();
        assert!(validate_role("curator", &root));
    }

    #[test]
    fn test_validate_worktree_missing_path() {
        let env = FakeEnv::new();
        let tmp = tempfile::tempdir().unwrap();
        assert!(!validate_worktree(&env, &tmp.path().join("nope")));
    }

    #[test]
    fn test_validate_worktree_ok() {
        let env = FakeEnv::new();
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate_worktree(&env, tmp.path()));
    }

    #[test]
    fn test_validate_worktree_not_a_git_repo() {
        let env = FakeEnv::new();
        env.git_repo_ok.set(false);
        let tmp = tempfile::tempdir().unwrap();
        assert!(!validate_worktree(&env, tmp.path()));
    }

    // --- Stop signals -----------------------------------------------------

    #[test]
    fn test_no_stop_signals() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        assert!(!check_stop_signals("builder-1", &root));
    }

    #[test]
    fn test_global_stop_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        std::fs::write(root.join(".loom").join("stop-daemon"), "").unwrap();
        assert!(check_stop_signals("builder-1", &root));
    }

    #[test]
    fn test_shepherd_stop_signal_only_applies_to_shepherds() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        std::fs::write(root.join(".loom").join("stop-shepherds"), "").unwrap();
        assert!(check_stop_signals("shepherd-1", &root));
        assert!(!check_stop_signals("builder-1", &root));
    }

    #[test]
    fn test_per_agent_stop_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let signals = root.join(".loom").join("signals");
        std::fs::create_dir_all(&signals).unwrap();
        std::fs::write(signals.join("stop-builder-1"), "").unwrap();
        assert!(check_stop_signals("builder-1", &root));
        assert!(!check_stop_signals("builder-2", &root));
    }

    // --- API error detection (replaces test_api_error_detection.py) -------

    #[test]
    fn test_api_errors_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(check_log_for_api_errors(&tmp.path().join("missing.log"), 50).is_none());
    }

    #[test]
    fn test_api_errors_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "").unwrap();
        assert!(check_log_for_api_errors(&log, 50).is_none());
    }

    #[test]
    fn test_api_errors_normal_log_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "Reading file\nRunning tests\nAll good\n").unwrap();
        assert!(check_log_for_api_errors(&log, 50).is_none());
    }

    #[test]
    fn test_api_errors_detects_each_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        for pattern in API_ERROR_PATTERNS {
            let log = tmp.path().join("a.log");
            std::fs::write(&log, format!("line one\nsomething {pattern} happened\n")).unwrap();
            assert_eq!(
                check_log_for_api_errors(&log, 50),
                Some(*pattern),
                "pattern {pattern} should be detected"
            );
        }
    }

    #[test]
    fn test_api_errors_only_checks_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        let mut content = String::from("overloaded\n");
        for i in 0..100 {
            content.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&log, content).unwrap();
        assert!(
            check_log_for_api_errors(&log, 50).is_none(),
            "an error 100 lines back must not re-trigger"
        );
    }

    #[test]
    fn test_api_errors_detects_error_in_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("line {i}\n"));
        }
        content.push_str("503 Service Unavailable\n");
        std::fs::write(&log, content).unwrap();
        assert_eq!(check_log_for_api_errors(&log, 50), Some("503 Service"));
    }

    #[test]
    fn test_api_errors_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "API is OVERLOADED right now\n").unwrap();
        assert_eq!(check_log_for_api_errors(&log, 50), Some("overloaded"));
    }

    // --- Stuck detection --------------------------------------------------

    #[test]
    fn test_stuck_when_no_shell_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok(""));
        assert!(session_is_stuck(&env, "builder-1", &root, 300));
    }

    #[test]
    fn test_stuck_when_no_claude_process() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("111\n"));
        env.set_claude_running(false);
        assert!(session_is_stuck(&env, "builder-1", &root, 300));
    }

    #[test]
    fn test_healthy_session_not_stuck_without_log() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("111\n"));
        env.set_claude_running(true);
        assert!(!session_is_stuck(&env, "builder-1", &root, 300));
    }

    #[test]
    fn test_fresh_log_is_not_stuck() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let logs = root.join(".loom").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("loom-builder-1.log"), "working\n").unwrap();

        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("111\n"));
        env.set_claude_running(true);
        assert!(!session_is_stuck(&env, "builder-1", &root, 300));
    }

    #[test]
    fn test_idle_log_past_threshold_is_stuck() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let logs = root.join(".loom").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("loom-builder-1.log"), "waiting\n").unwrap();

        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("111\n"));
        env.set_claude_running(true);
        // Jump the clock an hour past the file's mtime.
        env.clock.set(env.now() + 3600);
        assert!(session_is_stuck(&env, "builder-1", &root, 300));
    }

    #[test]
    fn test_recent_progress_milestone_rescues_idle_log() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let logs = root.join(".loom").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("loom-builder-1.log"), "waiting\n").unwrap();

        // Backdate the log an hour so it reads as idle, but leave the
        // progress milestone fresh.
        let log = std::fs::File::options()
            .write(true)
            .open(logs.join("loom-builder-1.log"))
            .unwrap();
        log.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();

        let progress = root.join(".loom").join("progress");
        std::fs::create_dir_all(&progress).unwrap();
        std::fs::write(progress.join("shepherd-42.json"), "{}").unwrap();

        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("111\n"));
        env.set_claude_running(true);
        assert!(!session_is_stuck(&env, "builder-1", &root, 300));
    }

    #[test]
    fn test_stale_progress_milestone_does_not_rescue_idle_log() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let logs = root.join(".loom").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("loom-builder-1.log"), "waiting\n").unwrap();
        let progress = root.join(".loom").join("progress");
        std::fs::create_dir_all(&progress).unwrap();
        std::fs::write(progress.join("shepherd-42.json"), "{}").unwrap();

        let env = FakeEnv::new();
        env.set("list-panes", CmdOutput::ok("111\n"));
        env.set_claude_running(true);
        // Everything is an hour stale from the clock's point of view.
        env.clock.set(env.now() + 3600);
        assert!(session_is_stuck(&env, "builder-1", &root, 300));
    }

    // --- Bypass modal -----------------------------------------------------

    #[test]
    fn test_bypass_prompt_visible_primary_marker() {
        assert!(bypass_prompt_visible("WARNING: Claude Code running in Bypass Permissions mode"));
    }

    #[test]
    fn test_bypass_prompt_visible_alternate_marker() {
        assert!(bypass_prompt_visible("running with --dangerously-skip-permissions"));
    }

    #[test]
    fn test_bypass_prompt_not_visible() {
        assert!(!bypass_prompt_visible("Welcome to Claude Code\n> "));
    }

    #[test]
    fn test_bypass_auto_accept_sends_down_enter() {
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::ok("WARNING: Bypass Permissions mode"));
        let opts = SpawnOptions {
            auto_accept_bypass: true,
            ..Default::default()
        };
        assert!(auto_accept_bypass_prompt(&env, "loom-a", &opts));
        assert!(env.saw("send-keys -t loom-a Down Enter"));
    }

    #[test]
    fn test_bypass_auto_accept_polls_until_modal_appears() {
        let env = FakeEnv::new();
        env.queue("capture-pane", CmdOutput::ok("starting"));
        env.queue("capture-pane", CmdOutput::ok("still starting"));
        env.queue("capture-pane", CmdOutput::ok("Bypass Permissions mode"));
        let opts = SpawnOptions {
            auto_accept_bypass: true,
            ..Default::default()
        };
        assert!(auto_accept_bypass_prompt(&env, "loom-a", &opts));
        assert_eq!(env.sleeps.borrow().len(), 2, "should have polled twice");
    }

    #[test]
    fn test_bypass_auto_accept_honours_timeout() {
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::ok("no modal here"));
        let opts = SpawnOptions {
            auto_accept_bypass: true,
            bypass_poll_timeout: 3,
            ..Default::default()
        };
        assert!(!auto_accept_bypass_prompt(&env, "loom-a", &opts));
        assert!(!env.saw("Down Enter"));
        assert_eq!(env.sleeps.borrow().len(), 3);
    }

    #[test]
    fn test_bypass_auto_accept_disabled() {
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::ok("Bypass Permissions mode"));
        let opts = SpawnOptions {
            auto_accept_bypass: false,
            ..Default::default()
        };
        assert!(!auto_accept_bypass_prompt(&env, "loom-a", &opts));
        assert!(env.calls.borrow().is_empty());
    }

    #[test]
    fn test_bypass_auto_accept_survives_capture_failure() {
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::fail());
        let opts = SpawnOptions {
            auto_accept_bypass: true,
            bypass_poll_timeout: 2,
            ..Default::default()
        };
        assert!(!auto_accept_bypass_prompt(&env, "loom-a", &opts));
    }

    // --- Command construction --------------------------------------------

    #[test]
    fn test_role_command_namespaced() {
        assert_eq!(role_command("builder", ""), "/loom:builder");
        assert_eq!(role_command("builder", "42"), "/loom:builder 42");
    }

    #[test]
    fn test_build_claude_command_with_wrapper() {
        let cmd = build_claude_command(&ClaudeCommand {
            token: None,
            pythonpath: None,
            worktree_path: None,
            max_retries: None,
            shepherd_task_id: None,
            name: "builder-1",
            working_dir: Path::new("/repo"),
            config_dir: Path::new("/repo/.loom/claude-config/builder-1"),
            wrapper: Some(Path::new("/repo/.loom/scripts/claude-wrapper.sh")),
            role_cmd: "/loom:builder 42",
        });
        assert!(cmd.contains("LOOM_TERMINAL_ID='builder-1'"));
        assert!(cmd.contains("LOOM_WORKSPACE='/repo'"));
        assert!(cmd.contains("CLAUDE_CONFIG_DIR='/repo/.loom/claude-config/builder-1'"));
        assert!(cmd.contains("TMPDIR='/repo/.loom/claude-config/builder-1/tmp'"));
        assert!(cmd.ends_with("--dangerously-skip-permissions \"/loom:builder 42\""));
        assert!(cmd.contains("'/repo/.loom/scripts/claude-wrapper.sh'"));
    }

    #[test]
    fn test_build_claude_command_without_wrapper() {
        let cmd = build_claude_command(&ClaudeCommand {
            token: Some("tok"),
            pythonpath: Some("/w/src"),
            worktree_path: Some(Path::new("/w")),
            max_retries: Some("1"),
            shepherd_task_id: Some("t1"),
            name: "builder-1",
            working_dir: Path::new("/w"),
            config_dir: Path::new("/c"),
            wrapper: None,
            role_cmd: "/loom:builder",
        });
        assert_eq!(
            cmd,
            "CLAUDE_CODE_OAUTH_TOKEN='tok' claude --dangerously-skip-permissions \"/loom:builder\""
        );
    }

    #[test]
    fn test_build_claude_command_prefix_order() {
        let cmd = build_claude_command(&ClaudeCommand {
            token: Some("tok"),
            pythonpath: Some("/w/src"),
            worktree_path: Some(Path::new("/w")),
            max_retries: Some("1"),
            shepherd_task_id: Some("t1"),
            name: "b",
            working_dir: Path::new("/w"),
            config_dir: Path::new("/c"),
            wrapper: Some(Path::new("/wrap.sh")),
            role_cmd: "/loom:builder",
        });
        assert!(cmd.starts_with(
            "CLAUDE_CODE_OAUTH_TOKEN='tok' PYTHONPATH='/w/src' LOOM_WORKTREE_PATH='/w' \
             LOOM_MAX_RETRIES='1' LOOM_SHEPHERD_TASK_ID='t1' LOOM_TERMINAL_ID='b' "
        ));
    }

    #[test]
    fn test_build_claude_command_escapes_token_quotes() {
        let cmd = build_claude_command(&ClaudeCommand {
            token: Some("a'b"),
            pythonpath: None,
            worktree_path: None,
            max_retries: None,
            shepherd_task_id: None,
            name: "b",
            working_dir: Path::new("/w"),
            config_dir: Path::new("/c"),
            wrapper: None,
            role_cmd: "/loom:builder",
        });
        assert!(cmd.starts_with("CLAUDE_CODE_OAUTH_TOKEN='a'\"'\"'b' "));
    }

    #[test]
    fn test_pipe_filter_prefers_native_strip_ansi_then_sed() {
        let cmd = pipe_filter_cmd(Path::new("/logs/loom-a.log"));
        assert!(
            cmd.starts_with("\"${LOOM_DAEMON_BIN:-loom-daemon}\" strip-ansi >> '/logs/loom-a.log'"),
            "got {cmd}"
        );
        assert!(
            !cmd.contains("loom_tools"),
            "the deleted Python filter must not be referenced: {cmd}"
        );
        assert!(cmd.contains("|| sed -l -E"));
        assert!(cmd.contains("|| sed -u -E"));
        assert!(cmd.contains("x1b"), "must strip ANSI CSI sequences");
        assert!(cmd.contains("x07"), "must strip OSC sequences");
    }

    #[test]
    fn test_rotated_log_path() {
        assert_eq!(
            rotated_log_path(Path::new("/l/loom-builder-1.log"), "20260729-120000"),
            Path::new("/l/loom-builder-1.20260729-120000.log")
        );
    }

    #[test]
    fn test_resolve_working_dir() {
        let root = Path::new("/repo");
        assert_eq!(resolve_working_dir("", root), Path::new("/repo"));
        assert_eq!(
            resolve_working_dir(".loom/worktrees/issue-42", root),
            Path::new("/repo/.loom/worktrees/issue-42")
        );
        assert_eq!(resolve_working_dir("/abs/wt", root), Path::new("/abs/wt"));
    }

    // --- spawn_agent end-to-end (scripted tmux) ---------------------------

    #[test]
    fn test_spawn_agent_clears_claudecode_env() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        let result = spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(result.status, "spawned");
        assert_eq!(env.session_env("CLAUDECODE").as_deref(), Some(""));
        assert!(
            !env.saw("set-environment -t loom-builder-1 -u CLAUDECODE"),
            "must never use tmux -u (server env would leak back in)"
        );
    }

    #[test]
    fn test_spawn_agent_sets_config_dir_and_tmpdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        let config_dir = env.session_env("CLAUDE_CONFIG_DIR").unwrap();
        assert!(config_dir.ends_with(".loom/claude-config/builder-1"));
        assert_eq!(env.session_env("TMPDIR").unwrap(), format!("{config_dir}/tmp"));
    }

    #[test]
    fn test_spawn_agent_reinitializes_corrupted_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.config_valid.set(false);
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(env.cleanup_calls.borrow().as_slice(), ["builder-1"]);
    }

    #[test]
    fn test_spawn_agent_does_not_clean_healthy_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert!(env.cleanup_calls.borrow().is_empty());
    }

    #[test]
    fn test_spawn_agent_seeds_trust_for_worktree_not_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let worktree = root.join(".loom").join("worktrees").join("issue-42");
        std::fs::create_dir_all(&worktree).unwrap();

        let env = ready_env();
        let mut opts = spawn_opts("builder-1");
        opts.worktree = ".loom/worktrees/issue-42".into();
        spawn_agent(&env, &opts, &root);

        assert_eq!(env.trusted.borrow().as_slice(), [worktree]);
    }

    #[test]
    fn test_spawn_agent_seeds_trust_for_repo_root_without_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(env.trusted.borrow().as_slice(), [root]);
    }

    #[test]
    fn test_spawn_agent_pins_git_env_for_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let worktree = root.join(".loom").join("worktrees").join("issue-42");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../../../.git").unwrap();

        let env = ready_env();
        let mut opts = spawn_opts("builder-1");
        opts.worktree = ".loom/worktrees/issue-42".into();
        spawn_agent(&env, &opts, &root);

        assert_eq!(
            env.session_env("GIT_WORK_TREE").as_deref(),
            Some(worktree.to_string_lossy().as_ref())
        );
        assert_eq!(
            env.session_env("GIT_DIR").as_deref(),
            Some(worktree.join(".git").to_string_lossy().as_ref())
        );
        assert_eq!(
            env.session_env("LOOM_WORKTREE_PATH").as_deref(),
            Some(worktree.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_spawn_agent_no_git_env_without_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert!(env.session_env("GIT_WORK_TREE").is_none());
        assert!(env.session_env("GIT_DIR").is_none());
        assert!(env.session_env("LOOM_WORKTREE_PATH").is_none());
    }

    #[test]
    fn test_spawn_agent_propagates_shepherd_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        let mut opts = spawn_opts("shepherd-1");
        opts.inherited.shepherd_task_id = Some("task-99".into());
        spawn_agent(&env, &opts, &root);

        assert_eq!(env.session_env("LOOM_SHEPHERD_TASK_ID").as_deref(), Some("task-99"));
        assert!(env
            .sent_command()
            .unwrap()
            .contains("LOOM_SHEPHERD_TASK_ID='task-99'"));
    }

    #[test]
    fn test_spawn_agent_no_shepherd_task_id_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert!(env.session_env("LOOM_SHEPHERD_TASK_ID").is_none());
        assert!(!env
            .sent_command()
            .unwrap()
            .contains("LOOM_SHEPHERD_TASK_ID"));
    }

    #[test]
    fn test_spawn_agent_uses_native_strip_ansi_in_pipe_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert!(env.saw("strip-ansi"));
        assert!(!env.saw("loom_tools"));
    }

    #[test]
    fn test_spawn_agent_writes_and_rotates_log() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let logs = root.join(".loom").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log_file = logs.join("loom-builder-1.log");
        std::fs::write(&log_file, "previous run\n").unwrap();

        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);

        let header = std::fs::read_to_string(&log_file).unwrap();
        assert!(header.starts_with("# Loom Agent Log\n"));
        assert!(header.contains("# Session: loom-builder-1"));
        assert!(header.contains("# Role: builder"));

        let rotated: Vec<_> = std::fs::read_dir(&logs)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "loom-builder-1.log")
            .collect();
        assert_eq!(rotated.len(), 1, "previous log should be rotated aside");
        assert!(rotated[0].starts_with("loom-builder-1."));
    }

    #[test]
    fn test_spawn_agent_injects_pool_token() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        *env.oauth_token.borrow_mut() = Some("pool-token".into());
        spawn_agent(&env, &spawn_opts("builder-1"), &root);

        assert_eq!(env.session_env("CLAUDE_CODE_OAUTH_TOKEN").as_deref(), Some("pool-token"));
        assert!(env
            .sent_command()
            .unwrap()
            .starts_with("CLAUDE_CODE_OAUTH_TOKEN='pool-token' "));
    }

    #[test]
    fn test_spawn_agent_uses_wrapper_when_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        let sent = env.sent_command().unwrap();
        assert!(sent.contains("claude-wrapper.sh' --dangerously-skip-permissions"));
        assert!(sent.contains("LOOM_TERMINAL_ID='builder-1'"));
    }

    #[test]
    fn test_spawn_agent_falls_back_to_bare_claude_without_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        std::fs::remove_file(root.join(".loom/scripts/claude-wrapper.sh")).unwrap();
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(
            env.sent_command().unwrap(),
            "claude --dangerously-skip-permissions \"/loom:builder\""
        );
    }

    #[test]
    fn test_spawn_agent_no_token_when_pool_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert!(env.session_env("CLAUDE_CODE_OAUTH_TOKEN").is_none());
        assert!(!env
            .sent_command()
            .unwrap()
            .contains("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn test_spawn_agent_inherited_token_wins_over_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        *env.oauth_token.borrow_mut() = Some("pool-token".into());
        let mut opts = spawn_opts("builder-1");
        opts.inherited.oauth_token = Some("caller-token".into());
        spawn_agent(&env, &opts, &root);

        assert!(env
            .sent_command()
            .unwrap()
            .starts_with("CLAUDE_CODE_OAUTH_TOKEN='caller-token' "));
        // The pool token must not be published on the session either.
        assert!(env.session_env("CLAUDE_CODE_OAUTH_TOKEN").is_none());
    }

    #[test]
    fn test_spawn_agent_error_when_session_create_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("new-session", CmdOutput::fail());
        let result = spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(result.status, "error");
        assert_eq!(result.error, "session_create_failed");
    }

    #[test]
    fn test_spawn_agent_error_when_session_disappears() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("has-session", CmdOutput::fail());
        let result = spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(result.error, "session_disappeared");
    }

    #[test]
    fn test_spawn_agent_error_when_claude_never_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set_claude_running(false);
        let result = spawn_agent(&env, &spawn_opts("builder-1"), &root);
        assert_eq!(result.error, "process_not_detected");
        assert_eq!(env.sleeps.borrow().len(), 3, "verify_timeout=3 -> 3 polls");
    }

    #[test]
    fn test_spawn_agent_pins_pythonpath_when_worktree_has_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let worktree = root.join(".loom").join("worktrees").join("issue-42");
        std::fs::create_dir_all(worktree.join("loom-tools").join("src")).unwrap();

        let env = ready_env();
        let mut opts = spawn_opts("builder-1");
        opts.worktree = ".loom/worktrees/issue-42".into();
        opts.inherited.pythonpath = Some("/existing".into());
        spawn_agent(&env, &opts, &root);

        let expected = format!("{}/loom-tools/src:/existing", worktree.display());
        assert_eq!(env.session_env("PYTHONPATH").as_deref(), Some(&*expected));
        assert!(env
            .sent_command()
            .unwrap()
            .contains(&format!("PYTHONPATH='{expected}'")));
    }

    // --- run() control flow ----------------------------------------------

    #[test]
    fn test_run_list_returns_zero() {
        let env = FakeEnv::new();
        let opts = SpawnOptions {
            do_list: true,
            ..Default::default()
        };
        assert_eq!(run(&env, &opts, Path::new("/")), 0);
    }

    #[test]
    fn test_run_check_exists_and_missing() {
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::ok(""));
        let opts = SpawnOptions {
            check_name: "builder-1".into(),
            ..Default::default()
        };
        assert_eq!(run(&env, &opts, Path::new("/")), 0);

        let env2 = FakeEnv::new();
        env2.set("has-session", CmdOutput::fail());
        assert_eq!(run(&env2, &opts, Path::new("/")), 1);
    }

    #[test]
    fn test_run_missing_role_and_name() {
        let env = FakeEnv::new();
        assert_eq!(run(&env, &SpawnOptions::default(), Path::new("/")), 1);
        let opts = SpawnOptions {
            role: "builder".into(),
            ..Default::default()
        };
        assert_eq!(run(&env, &opts, Path::new("/")), 1);
    }

    #[test]
    fn test_run_not_in_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let env = FakeEnv::new();
        assert_eq!(run(&env, &spawn_opts("builder-1"), tmp.path()), 1);
    }

    #[test]
    fn test_run_requires_tmux_and_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.tmux_available.set(false);
        assert_eq!(run(&env, &spawn_opts("builder-1"), &root), 1);

        let env2 = ready_env();
        env2.claude_cli_available.set(false);
        assert_eq!(run(&env2, &spawn_opts("builder-1"), &root), 1);
    }

    #[test]
    fn test_run_blocked_by_stop_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        std::fs::write(root.join(".loom").join("stop-daemon"), "").unwrap();
        let env = ready_env();
        assert_eq!(run(&env, &spawn_opts("builder-1"), &root), 1);
    }

    #[test]
    fn test_run_reuses_healthy_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("list-windows", CmdOutput::ok("0: bash* (1 panes)\n"));
        assert_eq!(run(&env, &spawn_opts("builder-1"), &root), 0);
        assert!(!env.saw("new-session"), "a healthy session must not be respawned");
    }

    #[test]
    fn test_run_fresh_kills_existing_session_then_spawns() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("list-windows", CmdOutput::ok("0: bash* (1 panes)\n"));
        let mut opts = spawn_opts("builder-1");
        opts.fresh = true;
        assert_eq!(run(&env, &opts, &root), 0);
        assert!(env.saw("kill-session -t loom-builder-1"));
        assert!(env.saw("new-session"));
    }

    #[test]
    fn test_run_cleans_up_dead_session_then_spawns() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("list-windows", CmdOutput::fail());
        assert_eq!(run(&env, &spawn_opts("builder-1"), &root), 0);
        assert!(env.saw("kill-session -t loom-builder-1"));
        assert!(env.saw("new-session"));
    }

    #[test]
    fn test_run_marks_on_demand_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("list-windows", CmdOutput::fail());
        let mut opts = spawn_opts("worker-1");
        opts.on_demand = true;
        assert_eq!(run(&env, &opts, &root), 0);
        assert_eq!(env.session_env("LOOM_ON_DEMAND").as_deref(), Some("true"));
    }

    /// Issue #6507: regression test pinning the `LOOM_ROLE` env-var contract
    /// (documented in `defaults/docs/daemon-reference.md` § "The `LOOM_ROLE`
    /// contract") for the tmux/MOM spawn path — `set_session_env(env,
    /// &session, "LOOM_ROLE", &opts.role)`, unconditional (no `if let Some`
    /// gate) since `opts.role.is_empty()` is already rejected earlier in
    /// `run()`.
    #[test]
    fn test_run_sets_loom_role_session_env() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = ready_env();
        env.set("list-windows", CmdOutput::fail());
        assert_eq!(run(&env, &spawn_opts("builder-1"), &root), 0);
        assert_eq!(
            env.session_env("LOOM_ROLE").as_deref(),
            Some("builder"),
            "every tmux-spawned agent session must carry LOOM_ROLE (issue #6507)"
        );
    }

    #[test]
    fn test_kill_stuck_session_captures_before_killing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::ok("scrollback content"));
        kill_stuck_session(&env, "builder-1", &root);

        let calls = env.call_strings();
        let capture_idx = calls.iter().position(|c| c.starts_with("capture-pane"));
        let kill_idx = calls.iter().position(|c| c.starts_with("kill-session"));
        assert!(capture_idx.unwrap() < kill_idx.unwrap());

        let logs: Vec<_> = std::fs::read_dir(root.join(".loom").join("logs"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].starts_with("loom-builder-1-killed-"));
    }

    #[test]
    fn test_kill_stuck_session_skips_empty_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::ok("   \n\n"));
        kill_stuck_session(&env, "builder-1", &root);
        assert!(!root.join(".loom").join("logs").exists());
        assert!(env.saw("kill-session -t loom-builder-1"));
    }

    #[test]
    fn test_kill_stuck_session_proceeds_when_capture_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("capture-pane", CmdOutput::fail());
        kill_stuck_session(&env, "builder-1", &root);
        assert!(env.saw("kill-session -t loom-builder-1"));
    }
}
