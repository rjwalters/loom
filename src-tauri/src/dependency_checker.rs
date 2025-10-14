use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct DependencyStatus {
    pub tmux_available: bool,
    pub git_available: bool,
    pub claude_code_available: bool,
    pub gh_available: bool,
    pub gh_copilot_available: bool,
    pub gemini_cli_available: bool,
    pub deepseek_cli_available: bool,
    pub grok_cli_available: bool,
}

pub fn check_dependencies() -> DependencyStatus {
    DependencyStatus {
        tmux_available: check_command("tmux"),
        git_available: check_command("git"),
        claude_code_available: check_command("claude"),
        gh_available: check_command("gh"),
        gh_copilot_available: check_gh_copilot(),
        gemini_cli_available: check_command("gemini"),
        deepseek_cli_available: check_command("deepseek"),
        grok_cli_available: check_command("grok"),
    }
}

fn check_gh_copilot() -> bool {
    // Check if gh copilot extension is installed by running `gh copilot --help`
    Command::new("gh")
        .args(["copilot", "--help"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn check_command(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
