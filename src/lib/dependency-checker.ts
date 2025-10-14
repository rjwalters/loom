import { ask } from "@tauri-apps/api/dialog";
import { invoke } from "@tauri-apps/api/tauri";

interface DependencyStatus {
  tmux_available: boolean;
  git_available: boolean;
  claude_code_available: boolean;
}

export async function checkAndReportDependencies(): Promise<boolean> {
  const status = await invoke<DependencyStatus>("check_system_dependencies");

  const missing: string[] = [];
  if (!status.tmux_available) missing.push("tmux");
  if (!status.git_available) missing.push("git");
  if (!status.claude_code_available) missing.push("claude");

  if (missing.length === 0) {
    return true; // All dependencies available
  }

  // Build error message with installation instructions
  let message = "Loom requires the following tools to be installed:\n\n";

  if (!status.tmux_available) {
    message += "❌ tmux - Terminal multiplexer\n";
    message += "   Install: brew install tmux\n\n";
  }

  if (!status.git_available) {
    message += "❌ git - Version control system\n";
    message += "   Install: brew install git\n\n";
  }

  if (!status.claude_code_available) {
    message += "❌ claude - Claude Code CLI\n";
    message += "   Install: npm install -g @anthropic-ai/claude-code\n\n";
  }

  message += "Would you like to retry after installation?";

  const retry = await ask(message, {
    title: "Missing Dependencies",
    type: "warning",
  });

  if (retry) {
    // Recursive check after user installs
    return await checkAndReportDependencies();
  }

  return false;
}
