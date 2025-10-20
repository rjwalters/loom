use crate::types::{TerminalId, TerminalInfo};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

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
            return Err(anyhow!("Failed to create tmux session"));
        }

        // Set up pipe-pane to capture all output to a file
        let output_file = format!("/tmp/loom-{id}.out");
        let pipe_cmd = format!("cat >> {output_file}");

        log::info!("Setting up pipe-pane for session {tmux_session} to {output_file}");
        let result = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["pipe-pane", "-t", &tmux_session, "-o", &pipe_cmd])
            .output()?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            log::error!("pipe-pane failed: {stderr}");
            return Err(anyhow!("Failed to set up pipe-pane: {stderr}"));
        }
        log::info!("pipe-pane setup successful");

        let info = TerminalInfo {
            id: id.clone(),
            name,
            tmux_session,
            working_dir,
            created_at: chrono::Utc::now().timestamp(),
            role: role.cloned(),
            worktree_path: None,
            agent_pid: None,
            agent_status: crate::types::AgentStatus::default(),
            last_interval_run: None,
        };

        self.terminals.insert(id.clone(), info);
        Ok(id)
    }

    pub fn list_terminals(&mut self) -> Vec<TerminalInfo> {
        // If registry is empty but tmux sessions exist, restore from tmux
        if self.terminals.is_empty() {
            log::debug!("Registry empty, attempting to restore from tmux");
            if let Err(e) = self.restore_from_tmux() {
                log::warn!("Failed to restore terminals from tmux: {e}");
            }
        }
        self.terminals.values().cloned().collect()
    }

    pub fn set_worktree_path(&mut self, id: &TerminalId, worktree_path: &str) -> Result<()> {
        let info = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        info.worktree_path = Some(worktree_path.to_string());
        log::info!("Set worktree path for terminal {id}: {worktree_path}");
        Ok(())
    }

    pub fn destroy_terminal(&mut self, id: &TerminalId) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Check if terminal has a worktree_path for reference counting
        if let Some(ref worktree_path) = info.worktree_path {
            let path = PathBuf::from(worktree_path);
            if path.to_string_lossy().contains(".loom/worktrees") {
                // Count how many OTHER terminals are using this worktree
                let other_users = self
                    .terminals
                    .values()
                    .filter(|t| t.id != *id && t.worktree_path.as_ref() == Some(worktree_path))
                    .count();

                if other_users == 0 {
                    // This is the last terminal - safe to remove
                    log::info!(
                        "Removing worktree at {} (no other terminals using it)",
                        path.display()
                    );

                    // First try to remove the worktree via git
                    let output = Command::new("git")
                        .args(["worktree", "remove", worktree_path])
                        .output();

                    if let Ok(output) = output {
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            log::warn!("git worktree remove failed: {stderr}");
                            log::info!("Attempting force removal...");

                            // Try force removal
                            let _ = Command::new("git")
                                .args(["worktree", "remove", "--force", worktree_path])
                                .output();
                        }
                    }

                    // Also try to remove directory manually as fallback
                    let _ = fs::remove_dir_all(&path);
                } else {
                    log::info!(
                        "Skipping worktree removal at {} ({} other terminal(s) still using it)",
                        path.display(),
                        other_users
                    );
                }
            }
        }

        // Stop pipe-pane (passing no command closes the pipe)
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["pipe-pane", "-t", &info.tmux_session])
            .spawn();

        // Kill the tmux session
        Command::new("tmux")
            .args(["-L", "loom"])
            .args(["kill-session", "-t", &info.tmux_session])
            .spawn()?
            .wait()?;

        // Clean up the output file
        let output_file = format!("/tmp/loom-{id}.out");
        let _ = std::fs::remove_file(output_file);

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

    pub fn resize_terminal(&self, id: &TerminalId, cols: u16, rows: u16) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Resize tmux window (which resizes the pane when there's only one pane)
        Command::new("tmux")
            .args(["-L", "loom"])
            .args([
                "resize-window",
                "-t",
                &info.tmux_session,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ])
            .spawn()?
            .wait()?;

        Ok(())
    }

    pub fn restore_from_tmux(&mut self) -> Result<()> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        // Enhanced logging: Check for tmux server failure
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Distinguish failure modes
            if stderr.contains("no server running") {
                log::error!(
                    "🚨 TMUX SERVER DEAD - Socket should be at /private/tmp/tmux-$UID/loom"
                );
            } else if stderr.contains("no such session") || stderr.contains("no sessions") {
                log::debug!("No tmux sessions found: {stderr}");
            } else {
                log::error!("tmux list-sessions failed: {stderr}");
            }

            // Return early with empty list if server is dead
            return Ok(());
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let session_count = sessions.lines().count();
        log::info!("📊 tmux server status: {session_count} total sessions");

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

                // Clear any existing pipe-pane for this session to avoid duplicates
                log::debug!("Clearing existing pipe-pane for session {session}");
                let _ = Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["pipe-pane", "-t", session])
                    .spawn();

                // Set up fresh pipe-pane to capture output
                let output_file = format!("/tmp/loom-{id}.out");
                let pipe_cmd = format!("cat >> {output_file}");

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
                        worktree_path: None,
                        agent_pid: None,
                        agent_status: crate::types::AgentStatus::default(),
                        last_interval_run: None,
                    });
            }
        }

        Ok(())
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

            // Enhanced logging: Distinguish failure modes
            if stderr.contains("no server running") {
                log::error!(
                    "🚨 TMUX SERVER DEAD during has_tmux_session check - Socket should be at /private/tmp/tmux-$UID/loom"
                );
            } else if stderr.contains("no sessions") {
                log::debug!("No tmux sessions exist: {stderr}");
            } else {
                log::error!("tmux list-sessions failed: {stderr}");
            }

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

    /// List all available loom tmux sessions
    #[allow(clippy::unused_self)]
    pub fn list_available_sessions(&self) -> Vec<String> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();

        // If tmux list-sessions fails (no server running), return empty vec
        let Ok(output) = output else {
            log::error!("Failed to execute tmux list-sessions command");
            return Vec::new();
        };

        // Enhanced logging: Check for tmux server failure
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if stderr.contains("no server running") {
                log::error!(
                    "🚨 TMUX SERVER DEAD during list_available_sessions - Socket should be at /private/tmp/tmux-$UID/loom"
                );
            } else if stderr.contains("no sessions") {
                log::debug!("No tmux sessions exist");
            } else {
                log::error!("tmux list-sessions failed: {stderr}");
            }

            return Vec::new();
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let loom_sessions: Vec<String> = sessions
            .lines()
            .filter(|s| s.starts_with("loom-"))
            .map(std::string::ToString::to_string)
            .collect();

        log::info!("📊 Found {} loom sessions", loom_sessions.len());
        loom_sessions
    }

    /// Attach an existing terminal record to a different tmux session
    pub fn attach_to_session(&mut self, id: &TerminalId, session_name: String) -> Result<()> {
        let info = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Verify the session exists
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["has-session", "-t", &session_name])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Enhanced logging: Distinguish failure modes
            if stderr.contains("no server running") {
                log::error!(
                    "🚨 TMUX SERVER DEAD during attach_to_session - Socket should be at /private/tmp/tmux-$UID/loom"
                );
                return Err(anyhow!("tmux server is not running"));
            }

            log::error!("tmux has-session failed for '{session_name}': {stderr}");
            return Err(anyhow!("Tmux session '{session_name}' does not exist"));
        }

        // Update the terminal info to point to the new session
        info.tmux_session = session_name;

        Ok(())
    }

    /// Kill a tmux session by name
    #[allow(clippy::unused_self)]
    pub fn kill_session(&self, session_name: &str) -> Result<()> {
        // Verify the session exists
        let check_output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["has-session", "-t", session_name])
            .output()?;

        if !check_output.status.success() {
            let stderr = String::from_utf8_lossy(&check_output.stderr);

            // Enhanced logging: Distinguish failure modes
            if stderr.contains("no server running") {
                log::error!(
                    "🚨 TMUX SERVER DEAD during kill_session - Socket should be at /private/tmp/tmux-$UID/loom"
                );
                return Err(anyhow!("tmux server is not running"));
            }

            log::error!("tmux has-session failed for '{session_name}': {stderr}");
            return Err(anyhow!("Tmux session '{session_name}' does not exist"));
        }

        // Kill the session
        Command::new("tmux")
            .args(["-L", "loom"])
            .args(["kill-session", "-t", session_name])
            .spawn()?
            .wait()?;

        log::info!("Killed tmux session: {session_name}");
        Ok(())
    }
}
