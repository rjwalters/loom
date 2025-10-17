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
    // First try using `which` command
    if Command::new("which")
        .arg(command)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        return true;
    }

    // macOS GUI apps don't inherit full shell PATH, so check common installation paths
    // This fixes the issue where tmux is installed via Homebrew but not found by `which`
    let mut common_paths = vec![
        format!("/opt/homebrew/bin/{command}"), // Apple Silicon Homebrew
        format!("/usr/local/bin/{command}"),    // Intel Homebrew
        format!("/usr/bin/{command}"),          // System binaries
        format!("/bin/{command}"),              // Core utilities
    ];

    if let Some(home) = dirs::home_dir() {
        let mut push_home_path = |relative: &str| {
            let mut path = home.clone();
            path.push(relative);
            path.push(command);
            common_paths.push(path.to_string_lossy().to_string());
        };

        // Common locations for npm/pnpm/yarn global binaries
        push_home_path(".npm-global/bin");
        push_home_path(".yarn/bin");
        push_home_path(".local/bin");
        push_home_path("Library/pnpm");
        push_home_path("Library/pnpm/bin");
        push_home_path("Library/Application Support/fnm/node-versions/current/bin");
    }

    for path in common_paths {
        let path_ref = std::path::Path::new(&path);
        if path_ref.exists() && path_ref.is_file() {
            // On macOS GUI apps the PATH is trimmed, so simply finding an executable file
            // at a known location is sufficient to consider the dependency available.
            // Attempting to run `--version` or similar flags is unreliable (e.g. tmux).
            return true;
        }
    }

    false
}
