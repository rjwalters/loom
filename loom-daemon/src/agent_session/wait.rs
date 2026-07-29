//! Native `agent-wait` (issue #4415).
//!
//! Replaces `loom_tools/agent_wait.py`: block until a tmux Claude agent
//! finishes, detecting completion via session destruction, an explicit
//! `/exit` in the log, shell/claude process exit, or an idle prompt.
//!
//! Exit codes (unchanged from `agent-wait.sh`, relied on by `loom-start.sh`
//! and `agent-destroy.sh`):
//!
//! | code | meaning |
//! |------|---------|
//! | 0    | agent completed |
//! | 1    | timeout reached |
//! | 2    | session not found, or error |

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{
    capture_pane, kill_session, log_info, log_success, log_warning, pane_pid, send_keys,
    session_age, session_exists, session_name, AgentEnv, PROCESSING_INDICATORS,
};

/// Default wall-clock budget, matching `agent-wait.sh`.
pub const DEFAULT_TIMEOUT: u64 = 3600;

/// Default seconds between polls.
pub const DEFAULT_POLL_INTERVAL: u64 = 5;

/// A session younger than this never trips idle-prompt detection — a freshly
/// created/restarted session renders an empty prompt before Claude Code has
/// painted anything (issue #1792).
pub const DEFAULT_MIN_SESSION_AGE: u64 = 10;

/// Consecutive idle observations required before declaring completion.
pub const IDLE_PROMPT_CONFIRM_COUNT: u32 = 2;

/// Number of trailing log lines scanned for an explicit `/exit`.
const EXIT_SCAN_LINES: usize = 100;

/// Configuration for one wait.
#[derive(Debug, Clone)]
pub struct WaitOptions {
    pub name: String,
    pub timeout: u64,
    pub poll_interval: u64,
    pub min_session_age: u64,
    pub json_output: bool,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            name: String::new(),
            timeout: DEFAULT_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            min_session_age: DEFAULT_MIN_SESSION_AGE,
            json_output: false,
        }
    }
}

/// Outcome of a wait. Serialized by `--json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitResult {
    /// `completed`, `timeout`, `not_found`, or `error`.
    pub status: String,
    pub name: String,
    pub elapsed: u64,
    pub reason: String,
    pub error: String,
}

impl WaitResult {
    fn new(status: &str, name: &str) -> Self {
        Self {
            status: status.to_string(),
            name: name.to_string(),
            elapsed: 0,
            reason: String::new(),
            error: String::new(),
        }
    }

    fn completed(name: &str, reason: &str, elapsed: u64) -> Self {
        let mut r = Self::new("completed", name);
        r.reason = reason.to_string();
        r.elapsed = elapsed;
        r
    }

    /// JSON payload, byte-compatible with the Python `WaitResult.to_dict()`.
    /// `timeout` duplicates `elapsed` on the timeout path to match the shape
    /// the original bash implementation emitted.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("status".into(), self.status.clone().into());
        obj.insert("name".into(), self.name.clone().into());
        if self.elapsed != 0 {
            obj.insert("elapsed".into(), self.elapsed.into());
        }
        if !self.reason.is_empty() {
            obj.insert("reason".into(), self.reason.clone().into());
        }
        if !self.error.is_empty() {
            obj.insert("error".into(), self.error.clone().into());
        }
        if self.status == "timeout" {
            obj.insert("timeout".into(), self.elapsed.into());
        }
        serde_json::Value::Object(obj)
    }

    /// Process exit code for this outcome.
    pub fn exit_code(&self) -> i32 {
        match self.status.as_str() {
            "completed" => 0,
            "timeout" => 1,
            _ => 2,
        }
    }
}

/// Whether the tail of the agent's log holds an explicit `/exit`.
///
/// Anchored to end-of-line so a `/exit` mentioned mid-sentence (or a path like
/// `/exit-codes.md`) does not count.
pub fn check_exit_command(log_file: &Path) -> bool {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(re) = RE
        .get_or_init(|| Regex::new(r"(?m)(^|\s+|❯\s*|>\s*)/exit\s*$").ok())
        .as_ref()
    else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(log_file) else {
        return false;
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return false;
    }
    let start = lines.len().saturating_sub(EXIT_SCAN_LINES);
    re.is_match(&lines[start..].join("\n"))
}

/// Whether the captured pane shows Claude sitting at an idle prompt.
///
/// Pure so the (fiddly) heuristic is directly testable: a visible
/// [`PROCESSING_INDICATORS`] means "still working"; otherwise one of the last
/// five non-empty lines must be exactly the `❯` prompt character.
pub fn pane_is_idle_prompt(pane: &str) -> bool {
    if pane.is_empty() || pane.contains(PROCESSING_INDICATORS) {
        return false;
    }
    let non_empty: Vec<&str> = pane.lines().filter(|l| !l.trim().is_empty()).collect();
    if non_empty.is_empty() {
        return false;
    }
    let start = non_empty.len().saturating_sub(5);
    non_empty[start..].iter().any(|line| line.trim() == "❯")
}

fn check_idle_prompt(env: &dyn AgentEnv, session: &str) -> bool {
    pane_is_idle_prompt(&capture_pane(env, session, None))
}

/// Handle a detected `/exit`: echo `/exit` into the prompt as a backup, then
/// destroy the session.
fn handle_exit_detection(
    env: &dyn AgentEnv,
    session: &str,
    name: &str,
    elapsed: u64,
    json_output: bool,
) -> WaitResult {
    if !json_output {
        log_info(format!(
            "/exit detected in output - sending /exit to prompt and terminating '{session}'"
        ));
    }
    send_keys(env, session, &["/exit", "C-m"]);
    env.sleep(1);
    kill_session(env, session);
    if !json_output {
        log_success(format!("Agent '{name}' completed (explicit /exit after {elapsed}s)"));
    }
    WaitResult::completed(name, "explicit_exit", elapsed)
}

/// Block until the agent named `opts.name` completes, times out, or is found
/// to be missing.
pub fn wait_for_agent(env: &dyn AgentEnv, opts: &WaitOptions, repo_root: &Path) -> WaitResult {
    let session = session_name(&opts.name);
    let log_file = repo_root
        .join(".loom")
        .join("logs")
        .join(format!("{session}.log"));

    if !session_exists(env, &session) {
        if !opts.json_output {
            log_warning(format!("Session not found: {session}"));
        }
        let mut r = WaitResult::new("not_found", &opts.name);
        r.error = format!("session {session}");
        return r;
    }

    let Some(shell_pid) = pane_pid(env, &session) else {
        if !opts.json_output {
            log_warning(format!("Could not find shell PID for session: {session}"));
        }
        let mut r = WaitResult::new("error", &opts.name);
        r.error = "could not find shell PID".into();
        return r;
    };

    if !opts.json_output {
        log_info(format!(
            "Waiting for agent '{}' to complete (timeout: {}s, poll: {}s)",
            opts.name, opts.timeout, opts.poll_interval
        ));
        log_info(format!("Session: {session}, Shell PID: {shell_pid}"));
    }

    let start_time = env.now();
    let mut idle_prompt_count = 0u32;

    loop {
        let elapsed = env.now().saturating_sub(start_time);

        if !session_exists(env, &session) {
            if !opts.json_output {
                log_success(format!(
                    "Agent '{}' completed (session destroyed after {elapsed}s)",
                    opts.name
                ));
            }
            return WaitResult::completed(&opts.name, "session_destroyed", elapsed);
        }

        if check_exit_command(&log_file) {
            return handle_exit_detection(env, &session, &opts.name, elapsed, opts.json_output);
        }

        // Re-fetch the shell PID in case the pane was recreated.
        let Some(shell_pid) = pane_pid(env, &session) else {
            if !opts.json_output {
                log_success(format!(
                    "Agent '{}' completed (no shell process after {elapsed}s)",
                    opts.name
                ));
            }
            return WaitResult::completed(&opts.name, "no_shell", elapsed);
        };

        if !env.claude_running(&shell_pid) {
            if !opts.json_output {
                log_success(format!(
                    "Agent '{}' completed (claude exited after {elapsed}s)",
                    opts.name
                ));
            }
            return WaitResult::completed(&opts.name, "claude_exited", elapsed);
        }

        // Idle-prompt detection, guarded against false positives.
        if opts.timeout == 0 {
            // Non-blocking mode: one check, gated on the tmux session's own
            // age rather than on elapsed wait time (issue #1792).
            let age = session_age(env, &session);
            if age >= 0 && (age as u64) < opts.min_session_age {
                if !opts.json_output {
                    log_info(format!(
                        "Session '{session}' is only {age}s old (< {}s) - skipping idle check",
                        opts.min_session_age
                    ));
                }
            } else if check_idle_prompt(env, &session) {
                if !opts.json_output {
                    log_success(format!(
                        "Agent '{}' completed (idle at prompt after {elapsed}s)",
                        opts.name
                    ));
                }
                return WaitResult::completed(&opts.name, "idle_prompt", elapsed);
            }
        } else if elapsed >= opts.min_session_age {
            if check_idle_prompt(env, &session) {
                idle_prompt_count += 1;
                if idle_prompt_count >= IDLE_PROMPT_CONFIRM_COUNT {
                    if !opts.json_output {
                        log_success(format!(
                            "Agent '{}' completed (idle at prompt after {elapsed}s)",
                            opts.name
                        ));
                    }
                    return WaitResult::completed(&opts.name, "idle_prompt", elapsed);
                }
            } else {
                idle_prompt_count = 0;
            }
        }

        let elapsed = env.now().saturating_sub(start_time);
        if elapsed >= opts.timeout {
            if !opts.json_output {
                log_warning(format!("Timeout waiting for agent '{}' after {elapsed}s", opts.name));
            }
            let mut r = WaitResult::new("timeout", &opts.name);
            r.elapsed = elapsed;
            return r;
        }

        env.sleep(opts.poll_interval.max(1));
    }
}

/// CLI entry point. Returns the process exit code.
pub fn run(env: &dyn AgentEnv, opts: &WaitOptions, start_dir: &Path) -> i32 {
    let repo_root = match super::find_repo_root(start_dir) {
        Some(root) => root,
        None => {
            log_warning("Not in a git repository");
            return 2;
        }
    };
    let result = wait_for_agent(env, opts, &repo_root);
    if opts.json_output {
        println!("{}", result.to_json());
    }
    result.exit_code()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::testing::FakeEnv;
    use super::super::CmdOutput;
    use super::*;
    use std::path::PathBuf;

    fn repo(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".loom").join("logs")).unwrap();
        root
    }

    fn live_env() -> FakeEnv {
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::ok(""));
        env.set("list-panes", CmdOutput::ok("999\n"));
        env.set_claude_running(true);
        env.set("capture-pane", CmdOutput::ok("working... esc to interrupt"));
        env
    }

    // --- Constants match the bash contract -------------------------------

    #[test]
    fn test_defaults_match_bash() {
        assert_eq!(DEFAULT_TIMEOUT, 3600);
        assert_eq!(DEFAULT_POLL_INTERVAL, 5);
        assert_eq!(DEFAULT_MIN_SESSION_AGE, 10);
        assert_eq!(IDLE_PROMPT_CONFIRM_COUNT, 2);
    }

    #[test]
    fn test_wait_options_defaults() {
        let opts = WaitOptions::default();
        assert_eq!(opts.timeout, DEFAULT_TIMEOUT);
        assert_eq!(opts.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(opts.min_session_age, DEFAULT_MIN_SESSION_AGE);
        assert!(!opts.json_output);
    }

    // --- Result shape / exit codes ---------------------------------------

    #[test]
    fn test_to_json_completed() {
        let json = WaitResult::completed("builder-1", "claude_exited", 42).to_json();
        assert_eq!(json["status"], "completed");
        assert_eq!(json["name"], "builder-1");
        assert_eq!(json["elapsed"], 42);
        assert_eq!(json["reason"], "claude_exited");
        assert!(json.get("timeout").is_none());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_to_json_timeout_duplicates_elapsed() {
        let mut r = WaitResult::new("timeout", "b");
        r.elapsed = 3600;
        let json = r.to_json();
        assert_eq!(json["elapsed"], 3600);
        assert_eq!(json["timeout"], 3600);
    }

    #[test]
    fn test_to_json_not_found() {
        let mut r = WaitResult::new("not_found", "b");
        r.error = "session loom-b".into();
        let json = r.to_json();
        assert_eq!(json["status"], "not_found");
        assert_eq!(json["error"], "session loom-b");
        assert!(json.get("elapsed").is_none());
    }

    #[test]
    fn test_to_json_minimal() {
        let json = WaitResult::new("completed", "b").to_json();
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn test_exit_codes() {
        assert_eq!(WaitResult::new("completed", "b").exit_code(), 0);
        assert_eq!(WaitResult::new("timeout", "b").exit_code(), 1);
        assert_eq!(WaitResult::new("not_found", "b").exit_code(), 2);
        assert_eq!(WaitResult::new("error", "b").exit_code(), 2);
    }

    // --- /exit detection --------------------------------------------------

    #[test]
    fn test_exit_detected_bare() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "doing work\n/exit\n").unwrap();
        assert!(check_exit_command(&log));
    }

    #[test]
    fn test_exit_detected_after_prompt_char() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "❯ /exit\n").unwrap();
        assert!(check_exit_command(&log));
    }

    #[test]
    fn test_exit_not_detected_mid_line() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "see /exit-codes.md for details\n").unwrap();
        assert!(!check_exit_command(&log));
    }

    #[test]
    fn test_exit_no_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!check_exit_command(&tmp.path().join("missing.log")));
    }

    #[test]
    fn test_exit_only_scans_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        let mut content = String::from("/exit\n");
        for i in 0..200 {
            content.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&log, content).unwrap();
        assert!(!check_exit_command(&log));
    }

    // --- Idle prompt heuristic -------------------------------------------

    #[test]
    fn test_idle_at_prompt() {
        assert!(pane_is_idle_prompt("Done with the task.\n❯\n"));
    }

    #[test]
    fn test_idle_with_surrounding_spaces() {
        assert!(pane_is_idle_prompt("Done.\n   ❯   \n"));
    }

    #[test]
    fn test_not_idle_while_processing() {
        assert!(!pane_is_idle_prompt("Thinking… (esc to interrupt)\n❯\n"));
    }

    #[test]
    fn test_not_idle_without_prompt() {
        assert!(!pane_is_idle_prompt("Some output\nmore output\n"));
    }

    #[test]
    fn test_not_idle_on_empty_pane() {
        assert!(!pane_is_idle_prompt(""));
        assert!(!pane_is_idle_prompt("\n\n  \n"));
    }

    #[test]
    fn test_not_idle_when_prompt_is_far_back() {
        let pane = "❯\na\nb\nc\nd\ne\nf\n";
        assert!(!pane_is_idle_prompt(pane), "only the last five non-empty lines count");
    }

    #[test]
    fn test_not_idle_when_prompt_has_trailing_text() {
        assert!(!pane_is_idle_prompt("❯ still typing something\n"));
    }

    // --- wait_for_agent ---------------------------------------------------

    #[test]
    fn test_session_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::fail());
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.status, "not_found");
        assert_eq!(r.error, "session loom-b");
        assert_eq!(r.exit_code(), 2);
    }

    #[test]
    fn test_no_shell_pid_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = FakeEnv::new();
        env.set("has-session", CmdOutput::ok(""));
        env.set("list-panes", CmdOutput::ok(""));
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.status, "error");
        assert_eq!(r.exit_code(), 2);
    }

    #[test]
    fn test_claude_exited_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        env.set_claude_running(false);
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.status, "completed");
        assert_eq!(r.reason, "claude_exited");
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn test_session_destroyed_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        // Alive for the pre-flight check, then gone inside the loop.
        env.queue("has-session", CmdOutput::ok(""));
        env.queue("has-session", CmdOutput::fail());
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.reason, "session_destroyed");
    }

    #[test]
    fn test_no_shell_process_mid_loop_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        env.queue("list-panes", CmdOutput::ok("999\n"));
        env.queue("list-panes", CmdOutput::ok(""));
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.reason, "no_shell");
    }

    #[test]
    fn test_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        let opts = WaitOptions {
            name: "b".into(),
            timeout: 10,
            poll_interval: 5,
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.status, "timeout");
        assert_eq!(r.elapsed, 10);
        assert_eq!(r.exit_code(), 1);
        assert_eq!(env.sleeps.borrow().as_slice(), [5, 5]);
    }

    #[test]
    fn test_exit_command_detected_kills_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        std::fs::write(root.join(".loom").join("logs").join("loom-b.log"), "all done\n/exit\n")
            .unwrap();
        let env = live_env();
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.reason, "explicit_exit");
        assert!(env.saw("send-keys -t loom-b /exit C-m"));
        assert!(env.saw("kill-session -t loom-b"));
    }

    #[test]
    fn test_idle_prompt_requires_two_confirmations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        env.set("capture-pane", CmdOutput::ok("done\n❯\n"));
        let opts = WaitOptions {
            name: "b".into(),
            timeout: 100,
            poll_interval: 5,
            min_session_age: 0,
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.reason, "idle_prompt");
        // One poll between the first and the confirming observation.
        assert_eq!(env.sleeps.borrow().len(), 1);
    }

    #[test]
    fn test_idle_prompt_confirmation_resets_on_activity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        // `set` first to clear live_env()'s steady-state "busy" pane, then
        // queue the observation sequence: idle, busy, idle, idle.
        env.set("capture-pane", CmdOutput::ok("done\n❯\n"));
        env.queue("capture-pane", CmdOutput::ok("busy esc to interrupt"));
        env.queue("capture-pane", CmdOutput::ok("done\n❯\n"));
        env.queue("capture-pane", CmdOutput::ok("done\n❯\n"));
        let opts = WaitOptions {
            name: "b".into(),
            timeout: 100,
            poll_interval: 5,
            min_session_age: 0,
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.reason, "idle_prompt");
        assert_eq!(
            env.sleeps.borrow().len(),
            3,
            "the interruption must reset the confirmation counter"
        );
    }

    #[test]
    fn test_idle_prompt_suppressed_before_min_session_age() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        env.set("capture-pane", CmdOutput::ok("done\n❯\n"));
        let opts = WaitOptions {
            name: "b".into(),
            timeout: 6,
            poll_interval: 5,
            min_session_age: 100,
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(
            r.status, "timeout",
            "idle detection must stay off until min_session_age elapses"
        );
    }

    #[test]
    fn test_nonblocking_skips_idle_check_for_young_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        env.set("capture-pane", CmdOutput::ok("done\n❯\n"));
        // session_created 3 seconds ago -> younger than min_session_age.
        env.set("display-message", CmdOutput::ok(format!("{}", env.now() - 3)));
        let opts = WaitOptions {
            name: "b".into(),
            timeout: 0,
            min_session_age: 10,
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.status, "timeout");
        assert!(env.sleeps.borrow().is_empty(), "must not block");
    }

    #[test]
    fn test_nonblocking_completes_for_mature_idle_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(&tmp);
        let env = live_env();
        env.set("capture-pane", CmdOutput::ok("done\n❯\n"));
        env.set("display-message", CmdOutput::ok(format!("{}", env.now() - 600)));
        let opts = WaitOptions {
            name: "b".into(),
            timeout: 0,
            min_session_age: 10,
            ..Default::default()
        };
        let r = wait_for_agent(&env, &opts, &root);
        assert_eq!(r.status, "completed");
        assert_eq!(r.reason, "idle_prompt");
    }

    #[test]
    fn test_run_returns_two_outside_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let env = FakeEnv::new();
        let opts = WaitOptions {
            name: "b".into(),
            ..Default::default()
        };
        assert_eq!(run(&env, &opts, tmp.path()), 2);
    }
}
