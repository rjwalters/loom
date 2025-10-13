import { type Terminal, TerminalStatus } from "./state";
import { getTheme, getThemeStyles, isDarkMode } from "./themes";

export function renderHeader(displayedWorkspacePath: string, hasWorkspace: boolean): void {
  const container = document.getElementById("workspace-name");
  if (!container) return;

  if (hasWorkspace) {
    // Show repo name in header (no "Loom" title)
    const repoName = extractRepoName(displayedWorkspacePath);
    container.innerHTML = `📂 ${escapeHtml(repoName)}`;
  } else {
    // Show "Loom" title when no workspace
    container.innerHTML = "Loom";
  }
}

function extractRepoName(path: string): string {
  if (!path) return "";
  // Get the last component of the path
  const parts = path.split("/").filter((p) => p.length > 0);
  return parts[parts.length - 1] || path;
}

export function renderPrimaryTerminal(
  terminal: Terminal | null,
  hasWorkspace: boolean,
  displayedWorkspacePath: string
): void {
  const container = document.getElementById("primary-terminal");
  if (!container) return;

  if (!terminal) {
    if (hasWorkspace) {
      // Workspace set but no terminals
      container.innerHTML = `
        <div class="h-full flex items-center justify-center text-gray-400">
          <p class="text-lg">No terminals. Click + to add a terminal.</p>
        </div>
      `;
    } else {
      // No workspace - show selector in center
      container.innerHTML = `
        <div class="h-full flex items-center justify-center">
          <div class="flex flex-col items-center gap-3">
            <p class="text-lg text-gray-400 mb-2">Open a git repository to begin</p>
            <div class="flex items-center gap-2">
              <input
                id="workspace-path"
                type="text"
                placeholder="Select or enter workspace path..."
                value="${escapeHtml(displayedWorkspacePath)}"
                class="px-3 py-2 text-sm bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 w-96"
              />
              <button
                id="browse-workspace"
                class="p-2 hover:bg-gray-100 dark:hover:bg-gray-700 rounded border border-gray-300 dark:border-gray-600"
                title="Browse for folder"
              >
                📁
              </button>
            </div>
            <div id="workspace-error" class="text-sm text-red-500 dark:text-red-400 min-h-[20px]"></div>
          </div>
        </div>
      `;
    }
    return;
  }

  // Check if this terminal has a missing session flag
  const hasMissingSession =
    terminal.status === TerminalStatus.Error && terminal.missingSession === true;

  const roleLabel = terminal.role ? getRoleLabel(terminal.role) : "Shell";

  // Get theme colors
  const theme = getTheme(terminal.theme, terminal.customTheme);
  const styles = getThemeStyles(theme, isDarkMode());

  const contentHTML = `<div class="flex-1 overflow-auto" id="terminal-content-${terminal.id}"></div>`;

  container.innerHTML = `
    <div class="h-full flex flex-col bg-white dark:bg-gray-800 rounded-lg border-l-4 border-r border-t border-b border-gray-200 dark:border-gray-700 overflow-hidden" style="border-left-color: ${styles.borderColor}">
      <div class="flex items-center justify-between px-4 py-2 border-b border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-700" style="background-color: ${styles.backgroundColor}">
        <div class="flex items-center gap-2">
          <div class="w-2 h-2 rounded-full ${getStatusColor(terminal.status)}"></div>
          <span class="terminal-name font-medium text-sm" data-terminal-id="${terminal.id}">${escapeHtml(terminal.name)}</span>
          <span class="text-xs text-gray-500 dark:text-gray-400">• ${roleLabel}</span>
        </div>
        <div class="flex items-center gap-1">
          <button
            id="terminal-clear-btn"
            data-terminal-id="${terminal.id}"
            class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
            title="Clear terminal"
          >
            <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"></path>
            </svg>
          </button>
          <button
            id="terminal-settings-btn"
            data-terminal-id="${terminal.id}"
            class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
            title="Terminal settings"
          >
            <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"></path>
              <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"></path>
            </svg>
          </button>
        </div>
      </div>
      ${contentHTML}
    </div>
  `;

  // If missing session, render error UI inside the content container after DOM update
  if (hasMissingSession) {
    setTimeout(() => {
      renderMissingSessionError(terminal.id);
    }, 0);
  }
}

export function renderMissingSessionError(terminalId: string): void {
  const container = document.getElementById(`terminal-content-${terminalId}`);
  if (!container) return;

  container.innerHTML = `
    <div class="h-full flex items-center justify-center bg-red-50 dark:bg-red-900/20">
      <div class="max-w-2xl mx-auto p-6 text-center">
        <div class="mb-4 flex justify-center">
          <svg class="w-16 h-16 text-red-500 dark:text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z"></path>
          </svg>
        </div>
        <h3 class="text-lg font-semibold text-red-700 dark:text-red-300 mb-2">Terminal Session Missing</h3>
        <p class="text-sm text-red-600 dark:text-red-400 mb-6">
          The tmux session for this terminal no longer exists. This can happen if the daemon was restarted or the session was killed.
        </p>
        <div class="flex flex-col gap-3 items-center">
          <button
            id="recover-new-session-btn"
            data-terminal-id="${terminalId}"
            class="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white rounded-lg font-medium transition-colors"
          >
            Create New Session
          </button>
          <button
            id="recover-attach-session-btn"
            data-terminal-id="${terminalId}"
            class="px-4 py-2 bg-gray-600 hover:bg-gray-700 text-white rounded-lg font-medium transition-colors"
          >
            Attach to Existing Session
          </button>
          <div id="available-sessions-${terminalId}" class="mt-4 w-full max-w-md"></div>
        </div>
      </div>
    </div>
  `;
}

export function renderAvailableSessionsList(terminalId: string, sessions: string[]): void {
  const container = document.getElementById(`available-sessions-${terminalId}`);
  if (!container) return;

  if (sessions.length === 0) {
    container.innerHTML = `
      <p class="text-sm text-gray-500 dark:text-gray-400">No available loom sessions found</p>
    `;
    return;
  }

  const sessionItems = sessions
    .map(
      (session) => `
      <button
        class="attach-session-item w-full px-4 py-2 text-left bg-white dark:bg-gray-700 hover:bg-gray-100 dark:hover:bg-gray-600 border border-gray-300 dark:border-gray-600 rounded transition-colors"
        data-terminal-id="${terminalId}"
        data-session-name="${escapeHtml(session)}"
      >
        <div class="flex items-center justify-between">
          <span class="text-sm font-mono">${escapeHtml(session)}</span>
          <span class="text-xs text-gray-500 dark:text-gray-400">Attach</span>
        </div>
      </button>
    `
    )
    .join("");

  container.innerHTML = `
    <div class="flex flex-col gap-2">
      <p class="text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">Available Sessions:</p>
      ${sessionItems}
    </div>
  `;
}

export function renderMiniTerminals(terminals: Terminal[], hasWorkspace: boolean): void {
  const container = document.getElementById("mini-terminal-row");
  if (!container) return;

  const terminalCards = terminals.map((t, index) => createMiniTerminalHTML(t, index)).join("");

  const addButtonClasses = hasWorkspace
    ? "bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 cursor-pointer"
    : "bg-gray-50 dark:bg-gray-800 cursor-not-allowed opacity-50";

  const addButtonDisabled = hasWorkspace ? "" : "disabled";
  const addButtonTitle = hasWorkspace ? "Add terminal" : "Select a workspace first";

  container.innerHTML = `
    <div class="h-full flex items-center gap-2 px-4 py-2 overflow-x-auto overflow-y-visible">
      ${terminalCards}
      <button
        id="add-terminal-btn"
        class="flex-shrink-0 w-32 h-32 flex items-center justify-center ${addButtonClasses} rounded-lg border-2 border-dashed border-gray-300 dark:border-gray-600 transition-colors"
        title="${addButtonTitle}"
        ${addButtonDisabled}
      >
        <span class="text-3xl text-gray-400">+</span>
      </button>
    </div>
  `;
}

function createMiniTerminalHTML(terminal: Terminal, index: number): string {
  // Get theme colors
  const theme = getTheme(terminal.theme, terminal.customTheme);
  const styles = getThemeStyles(theme, isDarkMode());

  const borderWidth = terminal.isPrimary ? "3" : "2";
  const borderColor = terminal.isPrimary ? styles.activeColor : styles.borderColor;

  return `
    <div class="p-1 flex-shrink-0">
      <div
        class="terminal-card group w-40 h-32 bg-white dark:bg-gray-800 hover:bg-gray-900/5 dark:hover:bg-white/5 rounded-lg cursor-grab transition-all"
        style="border: ${borderWidth}px solid ${borderColor}"
        data-terminal-id="${terminal.id}"
        draggable="true"
      >
      <div class="p-2 border-b border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-700 group-hover:bg-gray-100 dark:group-hover:bg-gray-600 flex items-center justify-between transition-colors rounded-t-lg" style="background-color: ${styles.backgroundColor}">
        <div class="flex items-center gap-2 flex-1 min-w-0">
          <div class="w-2 h-2 rounded-full flex-shrink-0 ${getStatusColor(terminal.status)}"></div>
          <span class="terminal-name text-xs font-medium truncate">${escapeHtml(terminal.name)}</span>
        </div>
        <button
          class="close-terminal-btn flex-shrink-0 text-gray-400 hover:text-red-500 dark:hover:text-red-400 font-bold transition-colors"
          data-terminal-id="${terminal.id}"
          title="Close terminal"
        >
          ×
        </button>
      </div>
      <div class="p-2 text-xs text-gray-500 dark:text-gray-400 flex items-center justify-between">
        <span>${terminal.status}</span>
        <span class="font-mono font-bold text-blue-600 dark:text-blue-400">#${index}</span>
      </div>
      </div>
    </div>
  `;
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

function getRoleLabel(role: string): string {
  const labels: Record<string, string> = {
    "claude-code-worker": "Claude Code Worker",
    "codex-worker": "Codex Worker",
  };
  return labels[role] || role;
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
