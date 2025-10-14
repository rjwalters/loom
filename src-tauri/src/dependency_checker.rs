use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Serialize, Deserialize)]
pub struct DependencyStatus {
    pub tmux_available: bool,
    pub git_available: bool,
    pub claude_code_available: bool,
}

pub fn check_dependencies() -> DependencyStatus {
    DependencyStatus {
        tmux_available: check_command("tmux"),
        git_available: check_command("git"),
        claude_code_available: check_command("claude"),
    }
}

fn check_command(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
