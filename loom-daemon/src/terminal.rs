use crate::types::{TerminalId, TerminalInfo};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build the pipe-pane command string that strips ANSI escape sequences from output.
///
/// The sed command removes:
/// - Standard ANSI escape sequences: ESC[...letter (colors, cursor, modes)
/// - Terminal mode queries: ESC[?...h/l (like ?2026h/l)
/// - OSC sequences: ESC]...BEL (title setting, etc.)
/// - Carriage returns (\r) from TUI line rewriting
/// - Backspaces (\x08) from cursor corrections
/// - Bare escape sequences (ESC not followed by [ or ]) from raw cursor movement
fn pipe_pane_cmd(output_file: &str) -> String {
    format!("sed -E 's/\\x1b\\[[?0-9;]*[a-zA-Z]//g; s/\\x1b\\][^\\x07]*\\x07//g; s/\\r//g; s/\\x08//g; s/\\x1b[^][]//g' >> {output_file}")
}

pub struct TerminalManager {
    terminals: HashMap<TerminalId, TerminalInfo>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: HashMap::new(),
        }
    }

    /// Validate terminal ID to prevent command injection
    /// Only allows alphanumeric characters, hyphens, and underscores
    fn validate_terminal_id(id: &str) -> Result<()> {
        if id.is_empty() {
            return Err(anyhow!("Terminal ID cannot be empty"));
        }

        if !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(anyhow!(
                "Invalid terminal ID: '{id}'. Only alphanumeric characters, hyphens, and underscores are allowed"
            ));
        }

        Ok(())
    }

    /// Derive the repository root from a working directory or `LOOM_WORKSPACE` env var.
    fn find_repo_root(working_dir: Option<&str>) -> Option<PathBuf> {
        working_dir
            .map(Path::new)
            .and_then(|p| {
                p.ancestors()
                    .find(|ancestor| ancestor.join(".loom").is_dir())
            })
            .map(Path::to_path_buf)
            .or_else(|| std::env::var("LOOM_WORKSPACE").ok().map(PathBuf::from))
    }

    /// Set up per-agent `CLAUDE_CONFIG_DIR` isolation for a tmux session.
    ///
    /// Creates an isolated config directory and sets `CLAUDE_CONFIG_DIR` and `TMPDIR`
    /// environment variables on the tmux session.
    fn setup_config_dir_isolation(
        terminal_id: &str,
        working_dir: Option<&str>,
        tmux_session: &str,
    ) {
        let Some(repo_root) = Self::find_repo_root(working_dir) else {
            log::debug!(
                "No repo root found for terminal {terminal_id}; skipping CLAUDE_CONFIG_DIR isolation"
            );
            return;
        };

        let Some(config_dir) =
            crate::claude_config::setup_agent_config_dir(terminal_id, &repo_root)
        else {
            return;
        };

        let config_dir_str = config_dir.to_string_lossy();

        // CLAUDE_CONFIG_DIR, TMPDIR, and — because that TMPDIR would
        // otherwise move it — the host's forge-call sink (W5), all on the
        // tmux session: one list shared with the manual spawn path.
        for (key, value) in crate::agent_session::isolation::vars(&config_dir) {
            let _ = Command::new("tmux")
                .args(["-L", "loom"])
                .args(["set-environment", "-t", tmux_session, key, &value])
                .output();
        }

        log::info!("Set CLAUDE_CONFIG_DIR={config_dir_str} for session {tmux_session}");
    }

    /// Classify a tmux `list-sessions` stderr string encountered during
    /// `restore_from_tmux` as an *expected* "nothing to restore" condition.
    ///
    /// A dead tmux server (`no server running`) at restore time implies zero
    /// live sessions by definition — there is nothing restorable *from tmux*,
    /// which is the normal state on a host that has never run the Manual
    /// Orchestration Mode tmux pool (e.g. autonomous-only), and also the
    /// normal state immediately after a reboot. See #4378.
    fn is_expected_empty_restore(stderr: &str) -> bool {
        stderr.contains("no server running")
            || (stderr.contains("error connecting to")
                && stderr.contains("No such file or directory"))
    }

    /// Handle tmux command errors with consistent logging
    /// Returns true if the error indicates the tmux server is dead
    fn handle_tmux_error(stderr: &str, operation: &str) -> bool {
        if stderr.contains("no server running") {
            log::error!(
                "🚨 TMUX SERVER DEAD during {operation} - Socket should be at /private/tmp/tmux-$UID/loom"
            );
            true
        } else if stderr.contains("no sessions") || stderr.contains("no such session") {
            log::debug!("No tmux sessions found during {operation}: {stderr}");
            false
        } else {
            log::error!("tmux {operation} failed: {stderr}");
            false
        }
    }

    /// Kill the process tree rooted at a tmux session's pane processes.
    ///
    /// When tmux kill-session sends SIGHUP, it doesn't propagate across process group
    /// boundaries. The `claude` CLI is typically behind a wrapper/timeout chain that
    /// creates separate process groups, so it survives session destruction as an orphan.
    ///
    /// This method kills the entire process tree first (SIGTERM then SIGKILL escalation),
    /// then destroys the tmux session.
    fn kill_process_tree(session_name: &str, force: bool) {
        // Get pane PIDs for this session
        let pane_output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-panes", "-t", session_name, "-F", "#{pane_pid}"])
            .output();

        let pane_pids: Vec<String> = match pane_output {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(std::string::ToString::to_string)
                .collect(),
            _ => {
                log::debug!("Could not get pane PIDs for session {session_name}");
                Vec::new()
            }
        };

        if pane_pids.is_empty() {
            return;
        }

        // Collect all descendant PIDs (depth-first for bottom-up kill)
        let mut all_pids: Vec<String> = Vec::new();
        for pane_pid in &pane_pids {
            Self::collect_descendants(pane_pid, &mut all_pids);
            all_pids.push(pane_pid.clone());
        }

        if all_pids.is_empty() {
            return;
        }

        log::info!(
            "Killing process tree for session {session_name}: {} process(es)",
            all_pids.len()
        );

        if force {
            // Force mode: SIGKILL immediately
            for pid in &all_pids {
                let _ = Command::new("kill").args(["-9", pid]).output();
            }
        } else {
            // Graceful mode: SIGTERM first
            for pid in &all_pids {
                let _ = Command::new("kill").args(["-15", pid]).output();
            }

            // Brief wait for processes to terminate
            std::thread::sleep(std::time::Duration::from_secs(1));

            // Escalate to SIGKILL for any survivors
            for pid in &all_pids {
                // Check if process is still alive (kill -0)
                if Command::new("kill")
                    .args(["-0", pid])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    let _ = Command::new("kill").args(["-9", pid]).output();
                }
            }
        }
    }

    /// Recursively collect all descendant PIDs of a given PID (depth-first)
    fn collect_descendants(parent_pid: &str, pids: &mut Vec<String>) {
        let output = Command::new("pgrep").args(["-P", parent_pid]).output();

        if let Ok(output) = output {
            if output.status.success() {
                let children: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(std::string::ToString::to_string)
                    .collect();

                for child in &children {
                    // Recurse into grandchildren first (depth-first)
                    Self::collect_descendants(child, pids);
                    pids.push(child.clone());
                }
            }
        }
    }

    pub fn create_terminal(
        &mut self,
        config_id: &str,
        name: String,
        working_dir: Option<String>,
        role: Option<&String>,
        instance_number: Option<u32>,
    ) -> Result<TerminalId> {
        // Validate terminal ID to prevent command injection
        Self::validate_terminal_id(config_id)?;

        // Use config_id directly as the terminal ID
        let id = config_id.to_string();
        let role_part = role.map_or("default", String::as_str);
        let instance_part = instance_number.unwrap_or(0);
        let tmux_session = format!("loom-{id}-{role_part}-{instance_part}");

        log::info!("Creating tmux session: {tmux_session}, working_dir: {working_dir:?}");

        // First, verify tmux server is responsive
        let check_output = Command::new("tmux")
            .args(["-L", "loom", "list-sessions"])
            .output();

        match check_output {
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                // Special case: constructor only warns about server not running
                if stderr.contains("no server running") {
                    log::warn!("tmux server not running, will start on first session creation");
                } else {
                    Self::handle_tmux_error(&stderr, "new");
                }
            }
            Err(e) => {
                log::error!("Failed to check tmux server status: {e}");
            }
            _ => {}
        }

        let mut cmd = Command::new("tmux");
        cmd.args(["-L", "loom"]);
        cmd.args([
            "new-session",
            "-d",
            "-s",
            &tmux_session,
            "-x",
            "80", // Standard width: 80 columns
            "-y",
            "24", // Standard height: 24 rows
        ]);

        if let Some(dir) = &working_dir {
            cmd.args(["-c", dir]);
        }

        log::info!("About to spawn tmux command...");
        let result = cmd.spawn()?.wait()?;
        log::info!("Tmux command completed with status: {result}");

        if !result.success() {
            // Get more details about the failure
            let stderr_output = Command::new("tmux")
                .args(["-L", "loom", "list-sessions"])
                .output();

            if let Ok(out) = stderr_output {
                let stderr = String::from_utf8_lossy(&out.stderr);
                log::error!("tmux session creation failed. Server status: {stderr}");
            }

            return Err(anyhow!("Failed to create tmux session '{tmux_session}'"));
        }

        // Set up pipe-pane to capture output with ANSI stripping
        let output_file = format!("/tmp/loom-{id}.out");
        let pipe_cmd = pipe_pane_cmd(&output_file);

        log::info!("Setting up pipe-pane for session {tmux_session} to {output_file}");
        let result = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["pipe-pane", "-t", &tmux_session, "-o", &pipe_cmd])
            .output()?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            log::error!("pipe-pane failed for session {tmux_session}: {stderr}");

            // Check if session still exists
            let check = Command::new("tmux")
                .args(["-L", "loom", "has-session", "-t", &tmux_session])
                .output();

            if let Ok(out) = check {
                if out.status.success() {
                    log::error!("Session {tmux_session} exists but pipe-pane setup failed");
                } else {
                    log::error!("Session {tmux_session} disappeared during pipe-pane setup!");
                }
            }

            return Err(anyhow!("Failed to set up pipe-pane for {tmux_session}: {stderr}"));
        }
        log::info!("pipe-pane setup successful for session {tmux_session}");

        // Set up per-agent CLAUDE_CONFIG_DIR isolation
        Self::setup_config_dir_isolation(&id, working_dir.as_deref(), &tmux_session);

        let info = TerminalInfo {
            id: id.clone(),
            name,
            tmux_session,
            working_dir,
            created_at: chrono::Utc::now().timestamp(),
            role: role.cloned(),
            agent_pid: None,
            agent_status: crate::types::AgentStatus::default(),
            last_interval_run: None,
        };

        self.terminals.insert(id.clone(), info);
        Ok(id)
    }

    pub fn list_terminals(&mut self) -> Vec<TerminalInfo> {
        // If registry is empty but tmux sessions exist, restore from tmux
        // Skip restore when LOOM_NO_RESTORE=1 is set (used in tests to prevent
        // cross-test-binary contamination via shared tmux server). The
        // predicate is single-sourced in `terminal_restore` so the daemon's
        // startup restore path honors the same flag (issue #8463).
        let no_restore = crate::terminal_restore::no_restore_env();

        if self.terminals.is_empty() && !no_restore {
            log::debug!("Registry empty, attempting to restore from tmux");
            if let Err(e) = self.restore_from_tmux() {
                log::warn!("Failed to restore terminals from tmux: {e}");
            }
        }

        // Issue #4794: garbage-collect registry entries whose backing tmux
        // session no longer exists. Without this, a terminal that was
        // registered by a since-crashed daemon, or whose tmux session was
        // reaped externally (e.g. the whole `loom` tmux socket losing its
        // server), sticks around in `self.terminals` forever and is reported
        // to every `list_terminals` caller (including the MCP `list_terminals`
        // tool) as an "active" terminal indefinitely.
        if !no_restore {
            self.prune_dead_terminals();
        }

        self.terminals.values().cloned().collect()
    }

    /// Remove registry entries whose `tmux_session` no longer has a live tmux
    /// session backing it (Issue #4794).
    ///
    /// A single `tmux -L loom list-sessions` call is used to build the live-session
    /// set rather than one `has-session` call per terminal, so this stays cheap
    /// even with many registered terminals. Any failure to query tmux at all
    /// (including "no server running", the exact symptom reported in #4794)
    /// is treated as "zero live sessions" — conservatively correct, since a
    /// terminal cannot be alive without a session to back it.
    ///
    /// Skipped when the registry is empty (nothing to prune) so this never
    /// pays for a `tmux` invocation on the common empty-registry path.
    fn prune_dead_terminals(&mut self) {
        if self.terminals.is_empty() {
            return;
        }

        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();

        let live_sessions: std::collections::HashSet<String> = match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
            // Any other outcome — nonzero exit (e.g. "no server running", the
            // #4794 symptom), or the `tmux` command failing to spawn at all —
            // means there are no live tmux sessions right now.
            _ => std::collections::HashSet::new(),
        };

        let dead_ids: Vec<TerminalId> = self
            .terminals
            .iter()
            .filter(|(_, info)| !live_sessions.contains(&info.tmux_session))
            .map(|(id, _)| id.clone())
            .collect();

        for id in dead_ids {
            if let Some(info) = self.terminals.remove(&id) {
                log::info!(
                    "Pruning dead terminal registry entry '{id}': backing tmux session '{}' no longer exists",
                    info.tmux_session
                );
            }
        }
    }

    pub fn destroy_terminal(&mut self, id: &TerminalId) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Stop pipe-pane (passing no command closes the pipe)
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["pipe-pane", "-t", &info.tmux_session])
            .spawn();

        // Kill process tree before destroying tmux session
        // This prevents orphaned claude processes that survive SIGHUP
        Self::kill_process_tree(&info.tmux_session, false);

        // Kill the tmux session (may already be dead from kill_process_tree)
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["kill-session", "-t", &info.tmux_session])
            .spawn()
            .and_then(|mut c| c.wait());

        // Clean up the output file
        let output_file = format!("/tmp/loom-{id}.out");
        let _ = std::fs::remove_file(output_file);

        // Clean up per-agent CLAUDE_CONFIG_DIR
        if let Some(root) = Self::find_repo_root(info.working_dir.as_deref()) {
            if crate::claude_config::cleanup_agent_config_dir(id, &root) {
                log::info!("Cleaned up CLAUDE_CONFIG_DIR for terminal {id}");
            }
        }

        self.terminals.remove(id);
        Ok(())
    }

    pub fn send_input(&self, id: &TerminalId, data: &str) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        match data {
            "\r" => {
                Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["send-keys", "-t", &info.tmux_session, "Enter"])
                    .spawn()?;
            }
            "\u{0003}" => {
                Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["send-keys", "-t", &info.tmux_session, "C-c"])
                    .spawn()?;
            }
            _ => {
                Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["send-keys", "-t", &info.tmux_session, "-l", data])
                    .spawn()?;
            }
        }

        Ok(())
    }

    #[allow(clippy::cast_possible_truncation, clippy::unused_self)]
    pub fn get_terminal_output(
        &self,
        id: &TerminalId,
        start_byte: Option<usize>,
    ) -> Result<(Vec<u8>, usize)> {
        use std::fs;
        use std::io::{Read, Seek};

        // Use config_id directly for filename
        let output_file = format!("/tmp/loom-{id}.out");
        log::debug!("Reading terminal output from: {output_file}");

        let mut file = match fs::File::open(&output_file) {
            Ok(f) => f,
            Err(e) => {
                // File doesn't exist yet, return empty
                log::debug!("Output file doesn't exist yet: {e}");
                return Ok((Vec::new(), 0));
            }
        };

        // Get file size
        let metadata = file.metadata()?;
        let file_size = metadata.len() as usize;
        log::debug!("Output file size: {file_size} bytes");

        // If start_byte is specified, seek to that position and read from there
        let bytes_to_read = if let Some(start) = start_byte {
            if start >= file_size {
                // No new data
                log::debug!("No new data (start_byte={start} >= file_size={file_size})");
                return Ok((Vec::new(), file_size));
            }
            file.seek(std::io::SeekFrom::Start(start as u64))?;
            let bytes = file_size - start;
            log::debug!("Seeking to byte {start} and reading {bytes} bytes");
            file_size - start
        } else {
            // Read entire file
            log::debug!("Reading entire file ({file_size} bytes)");
            file_size
        };

        let mut buffer = vec![0u8; bytes_to_read];
        file.read_exact(&mut buffer)?;
        log::debug!("Read {len} bytes successfully", len = buffer.len());

        Ok((buffer, file_size))
    }

    /// Restore terminals from existing tmux sessions.
    ///
    /// By default (no filter), imports ALL `loom-*` sessions for backward compatibility.
    /// When a filter is provided via `restore_from_tmux_with_filter`, only sessions
    /// matching configured terminal IDs are restored. This prevents importing stale
    /// sessions from crashed daemons or other daemon instances.
    pub fn restore_from_tmux(&mut self) -> Result<()> {
        self.restore_from_tmux_with_filter(None)
    }

    /// Restore terminals from existing tmux sessions, optionally filtering by configured IDs.
    ///
    /// # Arguments
    /// * `configured_ids` - If Some, only restore sessions whose extracted terminal ID
    ///   matches one of the configured IDs. If None, restore all loom-* sessions.
    ///
    /// # Session Ownership (Issue #1952)
    /// Without filtering, the daemon imports ANY `loom-*` session, which causes:
    /// - Test interference between different test binaries
    /// - Stale session accumulation from crashed daemons
    /// - No ownership verification between daemon instances
    ///
    /// With filtering (recommended), only sessions matching the workspace's config.json
    /// terminal definitions are restored, providing configuration-based ownership.
    pub fn restore_from_tmux_with_filter(
        &mut self,
        configured_ids: Option<&std::collections::HashSet<String>>,
    ) -> Result<()> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        // Enhanced logging: Check for tmux server failure
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Issue #4378: log the expected "nothing to restore" case at
            // debug instead of routing it through `handle_tmux_error`'s
            // ERROR siren, mirroring the existing `clean_stale_sessions`
            // precedent for the same stderr pattern.
            if Self::is_expected_empty_restore(&stderr) {
                log::debug!("No tmux server during restore_from_tmux — nothing to restore");
                return Ok(());
            }

            Self::handle_tmux_error(&stderr, "restore_from_tmux");

            // Return early with empty list if server is dead
            return Ok(());
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let session_count = sessions.lines().count();
        log::info!("📊 tmux server status: {session_count} total sessions");

        if let Some(ids) = configured_ids {
            log::info!(
                "🔒 Configuration-based restore: filtering to {} configured terminal(s)",
                ids.len()
            );
        } else {
            log::debug!("📦 Legacy restore: importing all loom-* sessions (no filter)");
        }

        let mut restored_count = 0;
        let mut skipped_count = 0;

        for session in sessions.lines() {
            if let Some(remainder) = session.strip_prefix("loom-") {
                // Session format: loom-{config_id}-{role}-{instance}
                // Extract config_id by checking for the "terminal-{number}" pattern
                //
                // Example session: loom-terminal-1-claude-code-worker-64
                // After strip_prefix: terminal-1-claude-code-worker-64
                // Split by '-': ["terminal", "1", "claude", "code", "worker", "64"]
                //
                // Strategy: If format is "terminal-{number}-...", extract "terminal-{number}"
                // Otherwise use first part for backwards compatibility

                let parts: Vec<&str> = remainder.split('-').collect();
                if parts.is_empty() {
                    log::warn!("Skipping malformed session name: {session}");
                    continue;
                }

                // Check if this matches the "terminal-{number}" pattern
                let id = if parts.len() >= 2
                    && parts[0] == "terminal"
                    && parts[1].chars().all(|c| c.is_ascii_digit())
                {
                    // Format: terminal-{number}-{role}-{instance}
                    // Extract "terminal-{number}" as the ID
                    format!("{}-{}", parts[0], parts[1])
                } else {
                    // For backwards compatibility with old format (no hyphens in ID),
                    // use first part as the terminal ID
                    parts[0].to_string()
                };

                // Validate terminal ID to prevent command injection
                if let Err(e) = Self::validate_terminal_id(&id) {
                    log::warn!("Skipping invalid terminal ID from tmux session {session}: {e}");
                    continue;
                }

                // If filter is provided, skip sessions not in the configured set
                if let Some(ids) = configured_ids {
                    if !ids.contains(&id) {
                        log::debug!(
                            "Skipping unconfigured session {session} (terminal ID '{id}' not in config)"
                        );
                        skipped_count += 1;
                        continue;
                    }
                }

                // Clear any existing pipe-pane for this session to avoid duplicates
                log::debug!("Clearing existing pipe-pane for session {session}");
                let _ = Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["pipe-pane", "-t", session])
                    .spawn();

                // Set up fresh pipe-pane to capture output with ANSI stripping
                let output_file = format!("/tmp/loom-{id}.out");
                let pipe_cmd = pipe_pane_cmd(&output_file);

                log::info!("Setting up pipe-pane for session {session} to {output_file}");
                let result = Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["pipe-pane", "-t", session, "-o", &pipe_cmd])
                    .output()?;

                if result.status.success() {
                    log::info!("pipe-pane setup successful for {session}");
                } else {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    log::warn!("pipe-pane setup failed for {session}: {stderr}");
                    // Continue anyway - terminal is still usable
                }

                self.terminals
                    .entry(id.clone())
                    .or_insert_with(|| TerminalInfo {
                        id: id.clone(),
                        name: format!("Restored: {session}"),
                        tmux_session: session.to_string(),
                        working_dir: None,
                        created_at: chrono::Utc::now().timestamp(),
                        role: None,
                        agent_pid: None,
                        agent_status: crate::types::AgentStatus::default(),
                        last_interval_run: None,
                    });

                restored_count += 1;
            }
        }

        if configured_ids.is_some() {
            log::info!(
                "📊 Restore complete: {restored_count} restored, {skipped_count} skipped (unconfigured)"
            );
        }

        Ok(())
    }

    /// Clean up stale tmux sessions from previous daemon runs.
    /// Lists all loom-* sessions and kills any that weren't restored
    /// into the terminal registry by `restore_from_tmux()`.
    pub fn clean_stale_sessions(&self) -> Result<usize> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no server running") || stderr.contains("no sessions") {
                return Ok(0);
            }
            log::warn!("Failed to list tmux sessions for cleanup: {stderr}");
            return Ok(0);
        }

        let sessions = String::from_utf8_lossy(&output.stdout);

        // Build a set of tmux session names that are tracked in the registry
        let tracked_sessions: std::collections::HashSet<&str> = self
            .terminals
            .values()
            .map(|t| t.tmux_session.as_str())
            .collect();

        let mut cleaned = 0;
        for session in sessions.lines() {
            if !session.starts_with("loom-") {
                continue;
            }

            if tracked_sessions.contains(session) {
                continue;
            }

            log::info!("Cleaning stale tmux session: {session}");

            // Kill process tree before destroying the stale session
            Self::kill_process_tree(session, true);

            // Force kill the session (may already be dead)
            let result = Command::new("tmux")
                .args(["-L", "loom"])
                .args(["kill-session", "-t", session])
                .output();

            match result {
                Ok(out) if out.status.success() => {
                    cleaned += 1;
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    // Session may already be dead from kill_process_tree
                    if !stderr.contains("no such session") {
                        log::warn!("Failed to kill stale session {session}: {stderr}");
                    }
                    cleaned += 1;
                }
                Err(e) => {
                    log::warn!("Failed to kill stale session {session}: {e}");
                }
            }
        }

        Ok(cleaned)
    }

    /// Check if a tmux session exists for the given terminal ID
    pub fn has_tmux_session(&self, id: &TerminalId) -> Result<bool> {
        log::info!("🔍 has_tmux_session called for terminal id: '{id}'");
        log::info!(
            "📋 Registry has {} terminals: {:?}",
            self.terminals.len(),
            self.terminals.keys().collect::<Vec<_>>()
        );

        // First check if we have this terminal registered
        if let Some(info) = self.terminals.get(id) {
            // Terminal is registered - check its specific tmux session
            log::info!(
                "✅ Terminal '{}' found in registry, checking session: '{}'",
                id,
                info.tmux_session
            );
            let output = Command::new("tmux")
                .args(["-L", "loom"])
                .args(["has-session", "-t", &info.tmux_session])
                .output()?;

            let result = output.status.success();
            log::info!("📊 tmux has-session result for '{}': {}", info.tmux_session, result);
            return Ok(result);
        }

        // Terminal not registered yet - check if ANY loom session with this ID exists
        // This handles the race condition where frontend creates state before daemon registers
        log::warn!("⚠️  Terminal '{id}' NOT found in registry, checking tmux sessions directly");

        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::handle_tmux_error(&stderr, "has_tmux_session");

            return Ok(false);
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let prefix = format!("loom-{id}-");

        // Check if any session matches our terminal ID prefix
        let has_session = sessions.lines().any(|s| s.starts_with(&prefix));

        log::debug!(
            "Terminal {id} tmux session check (unregistered): {}",
            if has_session { "found" } else { "not found" }
        );

        Ok(has_session)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ===== pipe_pane_cmd tests =====

    #[test]
    fn test_pipe_pane_cmd_contains_output_file() {
        let cmd = pipe_pane_cmd("/tmp/loom-test.out");
        assert!(cmd.contains("/tmp/loom-test.out"));
    }

    #[test]
    fn test_pipe_pane_cmd_uses_sed() {
        let cmd = pipe_pane_cmd("/tmp/output.out");
        assert!(cmd.starts_with("sed "));
    }

    #[test]
    fn test_pipe_pane_cmd_strips_ansi_escapes() {
        let cmd = pipe_pane_cmd("/tmp/output.out");
        // Should contain the ANSI escape stripping pattern
        assert!(cmd.contains("\\x1b"));
    }

    #[test]
    fn test_pipe_pane_cmd_appends_to_file() {
        let cmd = pipe_pane_cmd("/tmp/output.out");
        assert!(cmd.contains(">> /tmp/output.out"));
    }

    // ===== validate_terminal_id tests =====

    #[test]
    fn test_validate_terminal_id_empty() {
        let result = TerminalManager::validate_terminal_id("");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_validate_terminal_id_valid_alphanumeric() {
        assert!(TerminalManager::validate_terminal_id("terminal1").is_ok());
    }

    #[test]
    fn test_validate_terminal_id_valid_with_hyphens() {
        assert!(TerminalManager::validate_terminal_id("terminal-1").is_ok());
    }

    #[test]
    fn test_validate_terminal_id_valid_with_underscores() {
        assert!(TerminalManager::validate_terminal_id("terminal_1").is_ok());
    }

    #[test]
    fn test_validate_terminal_id_rejects_spaces() {
        let result = TerminalManager::validate_terminal_id("terminal 1");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid terminal ID"));
    }

    #[test]
    fn test_validate_terminal_id_rejects_special_chars() {
        for id in &[
            "term;rm -rf /",
            "term$(cmd)",
            "term`cmd`",
            "term|pipe",
            "a/b",
        ] {
            assert!(
                TerminalManager::validate_terminal_id(id).is_err(),
                "Expected rejection for: {id}"
            );
        }
    }

    #[test]
    fn test_validate_terminal_id_rejects_dots() {
        assert!(TerminalManager::validate_terminal_id("terminal.1").is_err());
    }

    // ===== is_expected_empty_restore tests (#4378) =====

    #[test]
    fn test_is_expected_empty_restore_no_server_running() {
        assert!(TerminalManager::is_expected_empty_restore(
            "no server running on /private/tmp/tmux-501/loom"
        ));
    }

    #[test]
    fn test_is_expected_empty_restore_no_sessions_is_not_expected() {
        // "no sessions" is a distinct condition already handled separately by
        // `handle_tmux_error` (debug, not error) — it must not be reclassified here.
        assert!(!TerminalManager::is_expected_empty_restore("no sessions"));
    }

    #[test]
    fn test_is_expected_empty_restore_socket_missing() {
        assert!(TerminalManager::is_expected_empty_restore(
            "error connecting to /tmp/tmux-1000/loom (No such file or directory)"
        ));
        assert!(!TerminalManager::is_expected_empty_restore(
            "error connecting to /tmp/tmux-1000/loom (Permission denied)"
        ));
    }

    #[test]
    fn test_is_expected_empty_restore_other_error_is_not_expected() {
        assert!(!TerminalManager::is_expected_empty_restore("some other tmux error"));
    }

    // ===== handle_tmux_error tests =====

    #[test]
    fn test_handle_tmux_error_no_server_returns_true() {
        let result = TerminalManager::handle_tmux_error("no server running on /tmp/tmux", "test");
        assert!(result, "Should return true when server is dead");
    }

    #[test]
    fn test_handle_tmux_error_no_sessions_returns_false() {
        let result = TerminalManager::handle_tmux_error("no sessions", "test");
        assert!(!result);
    }

    #[test]
    fn test_handle_tmux_error_no_such_session_returns_false() {
        let result = TerminalManager::handle_tmux_error("no such session: loom-test", "test");
        assert!(!result);
    }

    #[test]
    fn test_handle_tmux_error_other_error_returns_false() {
        let result = TerminalManager::handle_tmux_error("some other tmux error", "test");
        assert!(!result);
    }

    // ===== TerminalManager::new tests =====

    #[test]
    fn test_terminal_manager_new_is_empty() {
        let tm = TerminalManager::new();
        assert!(tm.terminals.is_empty());
    }

    // ===== prune_dead_terminals tests (Issue #4794) =====
    //
    // A registry entry whose `tmux_session` has no live backing session (the
    // session died, was reaped externally, or the whole `loom` tmux socket
    // has no server at all) must not be reported forever by `list_terminals()`
    // as "active" — see #4794's reported symptom (`ID: test / Restored: ...`
    // surviving as the sole "active" terminal on a host with no `loom` tmux
    // server running at all).

    fn fake_terminal(id: &str, tmux_session: &str) -> TerminalInfo {
        TerminalInfo {
            id: id.to_string(),
            name: format!("fake-{id}"),
            tmux_session: tmux_session.to_string(),
            working_dir: None,
            created_at: chrono::Utc::now().timestamp(),
            role: None,
            agent_pid: None,
            agent_status: crate::types::AgentStatus::default(),
            last_interval_run: None,
        }
    }

    #[test]
    fn test_list_terminals_prunes_entries_with_no_live_tmux_session() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LOOM_NO_RESTORE");

        let mut tm = TerminalManager::new();
        // A session name that cannot possibly be a real, live tmux session.
        let ghost_session = format!("loom-ghost-{}", uuid::Uuid::new_v4());
        tm.terminals
            .insert("ghost".to_string(), fake_terminal("ghost", &ghost_session));

        let terminals = tm.list_terminals();

        assert!(
            terminals.iter().all(|t| t.id != "ghost"),
            "a terminal whose tmux session does not exist must be pruned from list_terminals()"
        );
        assert!(
            !tm.terminals.contains_key("ghost"),
            "the dead entry must also be removed from the underlying registry, not just filtered on read"
        );
    }

    #[test]
    fn test_list_terminals_skips_pruning_under_loom_no_restore() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("LOOM_NO_RESTORE", "1");

        let mut tm = TerminalManager::new();
        let ghost_session = format!("loom-ghost-{}", uuid::Uuid::new_v4());
        tm.terminals
            .insert("ghost".to_string(), fake_terminal("ghost", &ghost_session));

        let terminals = tm.list_terminals();

        std::env::remove_var("LOOM_NO_RESTORE");

        assert!(
            terminals.iter().any(|t| t.id == "ghost"),
            "LOOM_NO_RESTORE=1 must also suppress pruning, matching its existing \
             'skip all tmux server interaction' contract used by tests"
        );
    }

    // Serializes tests that mutate process-global env (LOOM_NO_RESTORE).
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
}
