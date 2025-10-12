use crate::types::{TerminalId, TerminalInfo};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
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

    pub fn list_terminals(&self) -> Vec<TerminalInfo> {
        self.terminals.values().cloned().collect()
    }

    pub fn destroy_terminal(&mut self, id: &TerminalId) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        Command::new("tmux")
            .args(["kill-session", "-t", &info.tmux_session])
            .spawn()?
            .wait()?;

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

    pub fn get_terminal_output(
        &self,
        id: &TerminalId,
        start_line: Option<i32>,
    ) -> Result<(String, i32)> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // First, get the total number of lines in the history
        let history_output = Command::new("tmux")
            .args([
                "display-message",
                "-t",
                &info.tmux_session,
                "-p",
                "#{history_size}",
            ])
            .output()?;

        let total_lines: i32 = String::from_utf8_lossy(&history_output.stdout)
            .trim()
            .parse()
            .unwrap_or(0);

        // Capture pane content from start_line to end
        let mut cmd = Command::new("tmux");
        cmd.args(["capture-pane", "-t", &info.tmux_session, "-p", "-e", "-J"]);

        // If start_line is specified, only capture from that line onwards
        if let Some(start) = start_line {
            if start >= 0 && start < total_lines {
                let lines_to_capture = total_lines - start;
                cmd.args(["-S", &format!("-{lines_to_capture}")]);
            }
        } else {
            // Capture entire scrollback history
            cmd.arg("-S").arg("-");
        }

        let output = cmd.output()?;

        let content = String::from_utf8_lossy(&output.stdout).to_string();

        Ok((content, total_lines))
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
    pub fn list_available_sessions(&self) -> Result<Vec<String>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();

        // If tmux list-sessions fails (no server running), return empty vec
        let output = match output {
            Ok(o) => o,
            Err(_) => return Ok(Vec::new()),
        };

        let sessions = String::from_utf8_lossy(&output.stdout);
        let loom_sessions: Vec<String> = sessions
            .lines()
            .filter(|s| s.starts_with("loom-"))
            .map(|s| s.to_string())
            .collect();

        Ok(loom_sessions)
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
            return Err(anyhow!("Tmux session '{}' does not exist", session_name));
        }

        // Update the terminal info to point to the new session
        info.tmux_session = session_name;

        Ok(())
    }
}
