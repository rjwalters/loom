import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";

interface DependencyStatus {
  tmux_available: boolean;
  git_available: boolean;
  claude_code_available: boolean;
  gh_available: boolean;
  gh_copilot_available: boolean;
  gemini_cli_available: boolean;
  deepseek_cli_available: boolean;
  grok_cli_available: boolean;
  amp_cli_available: boolean;
}

export async function checkAndReportDependencies(): Promise<boolean> {
  const status = await invoke<DependencyStatus>("check_system_dependencies");

  // Check critical dependencies (always required)
  const criticalMissing: string[] = [];
  if (!status.tmux_available) criticalMissing.push("tmux");
  if (!status.git_available) criticalMissing.push("git");

  // Check if at least one agent is available
  const hasAtLeastOneAgent =
    status.claude_code_available ||
    (status.gh_available && status.gh_copilot_available) ||
    status.gemini_cli_available ||
    status.deepseek_cli_available ||
    status.grok_cli_available ||
    status.amp_cli_available;

  // If we have critical deps and at least one agent, we're good
  if (criticalMissing.length === 0 && hasAtLeastOneAgent) {
    return true;
  }

  // Build error message with installation instructions
  let message = "Loom requires the following tools:\n\n";

  // Critical dependencies
  if (!status.tmux_available) {
    message += "❌ tmux - Terminal multiplexer (REQUIRED)\n";
    message += "   Install: brew install tmux\n\n";
  }

  if (!status.git_available) {
    message += "❌ git - Version control system (REQUIRED)\n";
    message += "   Install: brew install git\n\n";
  }

  // Agent availability
  if (!hasAtLeastOneAgent) {
    message += "⚠️  At least one AI coding agent is required:\n\n";

    if (!status.claude_code_available) {
      message += "   • claude - Claude Code CLI\n";
      message += "     Install: npm install -g @anthropic-ai/claude-code\n\n";
    }

    if (!status.gh_available) {
      message += "   • gh + gh copilot - GitHub Copilot CLI\n";
      message += "     Install: brew install gh\n";
      message += "     Then: gh extension install github/gh-copilot\n\n";
    } else if (!status.gh_copilot_available) {
      message += "   • gh copilot - GitHub Copilot CLI extension\n";
      message += "     Install: gh extension install github/gh-copilot\n\n";
    }

    if (!status.gemini_cli_available) {
      message += "   • gemini - Google Gemini CLI\n";
      message += "     Install: npm install -g @google/generative-ai-cli\n\n";
    }

    if (!status.deepseek_cli_available) {
      message += "   • deepseek - DeepSeek CLI\n";
      message += "     Install: npm install -g deepseek-cli\n\n";
    }

    if (!status.grok_cli_available) {
      message += "   • grok - xAI Grok CLI\n";
      message += "     Install: brew install xai/tap/grok-cli\n\n";
    }

    if (!status.amp_cli_available) {
      message += "   • amp - Sourcegraph Amp CLI\n";
      message += "     Install: See https://github.com/sourcegraph/amp\n\n";
    }
  }

  message += "Would you like to retry after installation?";

  const retry = await ask(message, {
    title: "Missing Dependencies",
    kind: "warning",
  });

  if (retry) {
    // Recursive check after user installs
    return await checkAndReportDependencies();
  }

  return false;
}

/**
 * Get list of available worker types based on installed dependencies
 * @returns Array of available worker type values and display names
 */
export async function getAvailableWorkerTypes(): Promise<Array<{ value: string; label: string }>> {
  const status = await invoke<DependencyStatus>("check_system_dependencies");

  const available: Array<{ value: string; label: string }> = [];

  if (status.claude_code_available) {
    available.push({ value: "claude", label: "Claude Code" });
  }

  // Note: Codex is not currently checked by dependency checker
  // If you want to add it, add codex_available to DependencyStatus
  // For now, we'll assume it might be available
  available.push({ value: "codex", label: "Codex" });

  if (status.gh_available && status.gh_copilot_available) {
    available.push({ value: "github-copilot", label: "GitHub Copilot" });
  }

  if (status.gemini_cli_available) {
    available.push({ value: "gemini", label: "Google Gemini" });
  }

  if (status.deepseek_cli_available) {
    available.push({ value: "deepseek", label: "DeepSeek Coder" });
  }

  if (status.grok_cli_available) {
    available.push({ value: "grok", label: "xAI Grok" });
  }

  if (status.amp_cli_available) {
    available.push({ value: "amp", label: "Sourcegraph Amp" });
  }

  return available;
}
