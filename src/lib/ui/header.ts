import { escapeHtml, extractRepoName, formatDuration } from "./helpers";

export function renderHeader(
  displayedWorkspacePath: string,
  hasWorkspace: boolean,
  daemonConnected?: boolean,
  lastPing?: number | null,
  offlineMode?: boolean
): void {
  const container = document.getElementById("workspace-name");
  if (!container) return;

  let headerContent = "";
  if (hasWorkspace) {
    // Show repo name in header (no "Loom" title)
    const repoName = extractRepoName(displayedWorkspacePath);
    headerContent = `📂 ${escapeHtml(repoName)}`;
  } else {
    // Show "Loom" title when no workspace
    headerContent = "Loom";
  }

  // Add offline mode indicator if enabled
  if (offlineMode) {
    headerContent += ` <span class="inline-flex items-center gap-1 ml-2 px-2 py-0.5 rounded-md bg-yellow-100 dark:bg-yellow-900/30 text-yellow-800 dark:text-yellow-300 text-xs font-medium border border-yellow-300 dark:border-yellow-700" data-tooltip="Offline Mode: AI agents disabled, using status echoes" data-tooltip-position="bottom">
      <span class="text-xs">⚠️</span>
      <span>OFFLINE MODE</span>
    </span>`;
  }

  // Add daemon health indicator if we have that data
  if (daemonConnected !== undefined) {
    const statusColor = daemonConnected ? "bg-green-500" : "bg-red-500";
    const statusText = daemonConnected ? "Connected" : "Disconnected";
    const timeSincePing = lastPing ? Date.now() - lastPing : null;
    const pingInfo = timeSincePing !== null ? ` • ${formatDuration(timeSincePing)} ago` : "";

    headerContent += ` <span class="inline-flex items-center gap-1 ml-2 text-xs text-gray-500 dark:text-gray-400" data-tooltip="Daemon ${statusText}${pingInfo}" data-tooltip-position="bottom">
      <span class="w-2 h-2 rounded-full ${statusColor}"></span>
      <span class="text-xs">Daemon</span>
    </span>`;
  }

  container.innerHTML = headerContent;
}
