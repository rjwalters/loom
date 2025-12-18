import { TerminalStatus } from "../state";

/**
 * Format milliseconds into a human-readable duration
 */
export function formatDuration(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);

  if (days > 0) return `${days}d ${hours % 24}h`;
  if (hours > 0) return `${hours}h ${minutes % 60}m`;
  if (minutes > 0) return `${minutes}m ${seconds % 60}s`;
  return `${seconds}s`;
}

export function extractRepoName(path: string): string {
  if (!path) return "";
  // Get the last component of the path
  const parts = path.split("/").filter((p) => p.length > 0);
  return parts[parts.length - 1] || path;
}

export function getStatusColor(status: TerminalStatus): string {
  const colors = {
    [TerminalStatus.Idle]: "bg-green-500",
    [TerminalStatus.Busy]: "bg-blue-500",
    [TerminalStatus.NeedsInput]: "bg-yellow-500",
    [TerminalStatus.Error]: "bg-red-500",
    [TerminalStatus.Stopped]: "bg-gray-400",
  };
  return colors[status];
}

export function getRoleLabel(role: string): string {
  const labels: Record<string, string> = {
    "claude-code-worker": "Claude Code Worker",
    "codex-worker": "Codex Worker",
  };
  return labels[role] || role;
}

export function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
