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
        cmd.args(&["new-session", "-d", "-s", &tmux_session]);

        if let Some(dir) = &working_dir {
            cmd.args(&["-c", dir]);
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
            .args(&["kill-session", "-t", &info.tmux_session])
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
                    .args(&["send-keys", "-t", &info.tmux_session, "Enter"])
                    .spawn()?;
            }
            "\u{0003}" => {
                Command::new("tmux")
                    .args(&["send-keys", "-t", &info.tmux_session, "C-c"])
                    .spawn()?;
            }
            _ => {
                Command::new("tmux")
                    .args(&["send-keys", "-t", &info.tmux_session, "-l", data])
                    .spawn()?;
            }
        }

        Ok(())
    }

    pub fn restore_from_tmux(&mut self) -> Result<()> {
        let output = Command::new("tmux")
            .args(&["list-sessions", "-F", "#{session_name}"])
            .output()?;

        let sessions = String::from_utf8_lossy(&output.stdout);

        for session in sessions.lines() {
            if let Some(uuid_part) = session.strip_prefix("loom-") {
                let id = uuid_part.to_string();

                if !self.terminals.contains_key(&id) {
                    let info = TerminalInfo {
                        id: id.clone(),
                        name: format!("Restored: {}", session),
                        tmux_session: session.to_string(),
                        working_dir: None,
                        created_at: chrono::Utc::now().timestamp(),
                    };

                    self.terminals.insert(id, info);
                }
            }
        }

        Ok(())
    }
}
