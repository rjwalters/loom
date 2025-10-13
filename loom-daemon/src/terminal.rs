use crate::types::{TerminalId, TerminalInfo};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub struct TerminalManager {
    terminals: HashMap<TerminalId, TerminalInfo>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: HashMap::new(),
        }
    }

    pub fn create_terminal(
        &mut self,
        name: String,
        working_dir: Option<String>,
    ) -> Result<TerminalId> {
        let id = Uuid::new_v4().to_string();
        let tmux_session = format!("loom-{}", &id[..8]);

        let mut cmd = Command::new("tmux");
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

        cmd.spawn()?.wait()?;

        // Set up pipe-pane to capture all output to a file
        let output_file = format!("/tmp/loom-{}.out", &id[..8]);
        let pipe_cmd = format!("cat >> {output_file}");

        log::info!("Setting up pipe-pane for session {tmux_session} to {output_file}");
        let result = Command::new("tmux")
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
        };

        self.terminals.insert(id.clone(), info);
        Ok(id)
    }

    /// Create a terminal with its own git worktree for isolation
    pub fn create_terminal_with_worktree(
        &mut self,
        name: String,
        workspace_path: &Path,
    ) -> Result<TerminalId> {
        let id = Uuid::new_v4().to_string();

        // Create worktree directory
        let worktrees_dir = workspace_path.join(".loom/worktrees");
        fs::create_dir_all(&worktrees_dir)?;

        let worktree_path = worktrees_dir.join(&id);

        log::info!("Creating git worktree at {}", worktree_path.display());

        // Create git worktree
        let worktree_path_str = worktree_path
            .to_str()
            .ok_or_else(|| anyhow!("Invalid worktree path: cannot convert to string"))?;
        let output = Command::new("git")
            .args(["worktree", "add", worktree_path_str, "HEAD"])
            .current_dir(workspace_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("Failed to create worktree: {stderr}");
            return Err(anyhow!("Failed to create git worktree: {stderr}"));
        }

        log::info!("Git worktree created successfully");

        // Create terminal with worktree as working directory
        self.create_terminal(name, Some(worktree_path.to_string_lossy().to_string()))
    }

    pub fn list_terminals(&self) -> Vec<TerminalInfo> {
        self.terminals.values().cloned().collect()
    }

    pub fn destroy_terminal(&mut self, id: &TerminalId) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // If terminal has a worktree working directory, clean it up
        if let Some(ref working_dir) = info.working_dir {
            let path = PathBuf::from(working_dir);
            // Check if this looks like a worktree path (.loom/worktrees/<id>)
            if path.to_string_lossy().contains(".loom/worktrees") {
                log::info!("Removing git worktree at {}", path.display());

                // First try to remove the worktree via git
                let output = Command::new("git")
                    .args(["worktree", "remove", working_dir])
                    .output();

                if let Ok(output) = output {
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        log::warn!("git worktree remove failed: {stderr}");
                        log::info!("Attempting force removal...");

                        // Try force removal
                        let _ = Command::new("git")
                            .args(["worktree", "remove", "--force", working_dir])
                            .output();
                    }
                }

                // Also try to remove directory manually as fallback
                let _ = fs::remove_dir_all(&path);
            }
        }

        // Stop pipe-pane (passing no command closes the pipe)
        let _ = Command::new("tmux")
            .args(["pipe-pane", "-t", &info.tmux_session])
            .spawn();

        // Kill the tmux session
        Command::new("tmux")
            .args(["kill-session", "-t", &info.tmux_session])
            .spawn()?
            .wait()?;

        // Clean up the output file
        let id_prefix = if id.len() >= 8 { &id[..8] } else { id };
        let output_file = format!("/tmp/loom-{id_prefix}.out");
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
                    .args(["send-keys", "-t", &info.tmux_session, "Enter"])
                    .spawn()?;
            }
            "\u{0003}" => {
                Command::new("tmux")
                    .args(["send-keys", "-t", &info.tmux_session, "C-c"])
                    .spawn()?;
            }
            _ => {
                Command::new("tmux")
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

        // Use first 8 chars of ID for filename, or entire ID if shorter
        let id_prefix = if id.len() >= 8 { &id[..8] } else { id };
        let output_file = format!("/tmp/loom-{id_prefix}.out");
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
        log::debug!("Read {} bytes successfully", buffer.len());

        Ok((buffer, file_size))
    }

    pub fn resize_terminal(&self, id: &TerminalId, cols: u16, rows: u16) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Resize tmux window (which resizes the pane when there's only one pane)
        Command::new("tmux")
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
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        let sessions = String::from_utf8_lossy(&output.stdout);

        for session in sessions.lines() {
            if let Some(uuid_part) = session.strip_prefix("loom-") {
                let id = uuid_part.to_string();

                self.terminals
                    .entry(id.clone())
                    .or_insert_with(|| TerminalInfo {
                        id: id.clone(),
                        name: format!("Restored: {session}"),
                        tmux_session: session.to_string(),
                        working_dir: None,
                        created_at: chrono::Utc::now().timestamp(),
                    });
            }
        }

        Ok(())
    }

    /// Clean up orphaned worktrees (worktrees without matching terminal IDs)
    pub fn cleanup_orphaned_worktrees(&self, workspace_path: &Path) -> Result<()> {
        let worktrees_dir = workspace_path.join(".loom/worktrees");

        // If worktrees directory doesn't exist, nothing to clean
        if !worktrees_dir.exists() {
            return Ok(());
        }

        log::info!("Scanning for orphaned worktrees in {}", worktrees_dir.display());

        // Get list of known terminal IDs
        let terminal_ids: std::collections::HashSet<String> =
            self.terminals.keys().cloned().collect();

        // Scan worktrees directory
        let entries = fs::read_dir(&worktrees_dir)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    // Check if this worktree ID matches any terminal
                    if !terminal_ids.contains(dir_name) {
                        log::info!("Found orphaned worktree: {}", path.display());
                        log::info!("Removing orphaned worktree...");

                        // Try to remove via git first
                        let path_str = path.to_string_lossy();
                        let output = Command::new("git")
                            .args(["worktree", "remove", "--force", &path_str])
                            .current_dir(workspace_path)
                            .output();

                        if let Ok(output) = output {
                            if !output.status.success() {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                log::warn!("git worktree remove failed: {stderr}");
                            }
                        }

                        // Remove directory as fallback
                        if path.exists() {
                            let _ = fs::remove_dir_all(&path);
                        }

                        log::info!("Orphaned worktree removed");
                    }
                }
            }
        }

        // Also prune stale worktree references in git
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(workspace_path)
            .output();

        Ok(())
    }

    /// Check if a tmux session exists for the given terminal ID
    pub fn has_tmux_session(&self, id: &TerminalId) -> Result<bool> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        let output = Command::new("tmux")
            .args(["has-session", "-t", &info.tmux_session])
            .output()?;

        Ok(output.status.success())
    }

    /// List all available loom tmux sessions
    #[allow(clippy::unused_self)]
    pub fn list_available_sessions(&self) -> Vec<String> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();

        // If tmux list-sessions fails (no server running), return empty vec
        let Ok(output) = output else {
            return Vec::new();
        };

        let sessions = String::from_utf8_lossy(&output.stdout);
        sessions
            .lines()
            .filter(|s| s.starts_with("loom-"))
            .map(std::string::ToString::to_string)
            .collect()
    }

    /// Attach an existing terminal record to a different tmux session
    pub fn attach_to_session(&mut self, id: &TerminalId, session_name: String) -> Result<()> {
        let info = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Verify the session exists
        let output = Command::new("tmux")
            .args(["has-session", "-t", &session_name])
            .output()?;

        if !output.status.success() {
            return Err(anyhow!("Tmux session '{session_name}' does not exist"));
        }

        // Update the terminal info to point to the new session
        info.tmux_session = session_name;

        Ok(())
    }
}
