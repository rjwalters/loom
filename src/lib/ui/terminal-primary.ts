import { Logger } from "../logger";
import { type Terminal, TerminalStatus } from "../state";
import { getTheme, getThemeStyles, isDarkMode } from "../themes";
import { escapeHtml, formatDuration, getRoleLabel, getStatusColor } from "./helpers";

const logger = Logger.forComponent("ui");

export interface HealthCheckTiming {
  lastCheckTime: number | null;
  nextCheckTime: number | null;
  checkIntervalMs: number;
}

/**
 * Create "Restart Terminal" button HTML
 */
function createRestartButtonHTML(terminal: Terminal, context: "primary" | "mini"): string {
  const buttonClasses =
    context === "primary"
      ? "p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors restart-terminal-btn"
      : "p-1 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors restart-terminal-btn";

  const iconClasses =
    context === "primary"
      ? "w-4 h-4 text-gray-600 dark:text-gray-300"
      : "w-3 h-3 text-gray-600 dark:text-gray-300";

  return `
    <button
      data-terminal-id="${terminal.id}"
      data-tooltip="Restart terminal"
      data-tooltip-position="bottom"
      class="${buttonClasses}"
      title="Restart terminal"
    >
      <svg class="${iconClasses}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15"></path>
      </svg>
    </button>
  `;
}

/**
 * Create "Run Now" button HTML for interval mode terminals
 */
function createRunNowButtonHTML(terminal: Terminal, context: "primary" | "mini"): string {
  const hasInterval =
    terminal.roleConfig?.targetInterval !== undefined &&
    (terminal.roleConfig.targetInterval as number) > 0;

  if (!hasInterval) {
    return "";
  }

  const buttonClasses =
    context === "primary"
      ? "p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors run-now-btn"
      : "p-1 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors run-now-btn";

  const iconClasses =
    context === "primary"
      ? "w-4 h-4 text-gray-600 dark:text-gray-300"
      : "w-3 h-3 text-gray-600 dark:text-gray-300";

  return `
    <button
      data-terminal-id="${terminal.id}"
      data-tooltip="Run interval prompt now"
      data-tooltip-position="bottom"
      class="${buttonClasses}"
      title="Run interval prompt now"
    >
      <svg class="${iconClasses}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M14.752 11.168l-3.197-2.132A1 1 0 0010 9.87v4.263a1 1 0 001.555.832l3.197-2.132a1 1 0 000-1.664z"></path>
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M21 12a9 9 0 11-18 0 9 9 0 0118 0z"></path>
      </svg>
    </button>
  `;
}

export function renderPrimaryTerminal(
  terminal: Terminal | null,
  hasWorkspace: boolean,
  displayedWorkspacePath: string
): void {
  const container = document.getElementById("terminal-view");
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
                data-tooltip="Enter path to git repository"
                data-tooltip-position="bottom"
                class="px-3 py-2 text-sm bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 w-96"
              />
              <button
                id="browse-workspace"
                data-tooltip="Browse for folder"
                data-tooltip-position="bottom"
                class="p-2 hover:bg-gray-100 dark:hover:bg-gray-700 rounded border border-gray-300 dark:border-gray-600"
                title="Browse for folder"
              >
                📁
              </button>
            </div>
            <div id="workspace-error" class="text-sm text-red-500 dark:text-red-400 min-h-[20px]"></div>
            <div class="flex items-center gap-2 mt-2">
              <div class="flex-1 h-px bg-gray-300 dark:bg-gray-600"></div>
              <span class="text-sm text-gray-500 dark:text-gray-400">or</span>
              <div class="flex-1 h-px bg-gray-300 dark:bg-gray-600"></div>
            </div>
            <button
              id="create-new-project-btn"
              data-tooltip="Create a new git repository with Loom configuration"
              data-tooltip-position="bottom"
              class="px-4 py-2 text-sm font-medium text-blue-600 dark:text-blue-400 hover:bg-blue-50 dark:hover:bg-blue-900/20 rounded transition-colors"
            >
              Create New Project
            </button>
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

  // Check if we need to initialize the persistent structure (first time only)
  const existingWrapper = document.getElementById("terminal-wrapper");
  if (!existingWrapper) {
    // First render - create persistent structure
    container.innerHTML = `
      <div class="h-full flex flex-col bg-gray-100 dark:bg-gray-800 rounded-lg border-l-4 border-r border-t border-b border-gray-200 dark:border-gray-700 overflow-hidden" style="border-left-color: ${styles.borderColor}" id="terminal-wrapper">
        <div class="flex items-center justify-between px-4 py-2 border-b border-gray-200 dark:border-gray-700 bg-gray-100 dark:bg-gray-700" style="background-color: ${styles.backgroundColor}" id="terminal-header">
          <!-- Header content will be updated below -->
        </div>
        <div id="persistent-xterm-containers" class="flex-1 overflow-auto relative">
          <!-- Persistent xterm containers created by terminal-manager, shown/hidden via display style -->

          <!-- Search panel (hidden by default) -->
          <div id="terminal-search-panel" class="hidden absolute top-4 right-4 z-50 bg-gray-100 dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded-lg shadow-lg p-3" style="backdrop-filter: blur(10px);">
            <div class="flex items-center gap-2 mb-2">
              <input
                id="terminal-search-input"
                type="text"
                placeholder="Search terminal output..."
                class="flex-1 px-2 py-1 text-sm bg-gray-50 dark:bg-gray-700 border border-gray-300 dark:border-gray-500 rounded focus:outline-none focus:ring-2 focus:ring-blue-500"
                aria-label="Search terminal output"
              />
              <button
                id="terminal-search-close"
                class="p-1 hover:bg-gray-100 dark:hover:bg-gray-600 rounded"
                aria-label="Close search"
                title="Close (Esc)"
              >
                <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"></path>
                </svg>
              </button>
            </div>
            <div class="flex items-center gap-2">
              <label class="flex items-center gap-1 text-xs text-gray-600 dark:text-gray-300">
                <input
                  id="terminal-search-case-sensitive"
                  type="checkbox"
                  class="rounded"
                  aria-label="Case sensitive search"
                />
                Aa
              </label>
              <label class="flex items-center gap-1 text-xs text-gray-600 dark:text-gray-300">
                <input
                  id="terminal-search-regex"
                  type="checkbox"
                  class="rounded"
                  aria-label="Regular expression search"
                />
                .*
              </label>
              <div class="flex-1"></div>
              <button
                id="terminal-search-prev"
                class="p-1 hover:bg-gray-100 dark:hover:bg-gray-600 rounded text-gray-600 dark:text-gray-300"
                aria-label="Previous match"
                title="Previous (Shift+Enter)"
              >
                <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 15l7-7 7 7"></path>
                </svg>
              </button>
              <button
                id="terminal-search-next"
                class="p-1 hover:bg-gray-100 dark:hover:bg-gray-600 rounded text-gray-600 dark:text-gray-300"
                aria-label="Next match"
                title="Next (Enter)"
              >
                <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"></path>
                </svg>
              </button>
            </div>
          </div>
        </div>
      </div>
    `;
  }

  // Update header content (safe - doesn't touch xterm containers)
  const headerContainer = document.getElementById("terminal-header");
  if (headerContainer) {
    headerContainer.innerHTML = `
      <div class="flex items-center gap-2">
        <div class="w-2 h-2 rounded-full ${getStatusColor(terminal.status)}"></div>
        <span class="terminal-name font-medium text-sm" data-terminal-id="${terminal.id}" data-tooltip="Double-click to rename" data-tooltip-position="bottom">${escapeHtml(terminal.name)}</span>
        <span class="text-xs text-gray-500 dark:text-gray-400">• ${roleLabel}</span>
      </div>
      <div class="flex items-center gap-1">
        ${createRestartButtonHTML(terminal, "primary")}
        ${createRunNowButtonHTML(terminal, "primary")}
        <button
          id="terminal-search-btn"
          data-terminal-id="${terminal.id}"
          data-tooltip="Search terminal output (⌘F)"
          data-tooltip-position="bottom"
          class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
          title="Search"
          aria-label="Search ${escapeHtml(terminal.name)} terminal output"
        >
          <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z"></path>
          </svg>
        </button>
        <button
          id="terminal-export-btn"
          data-terminal-id="${terminal.id}"
          data-tooltip="Export terminal output as text file"
          data-tooltip-position="bottom"
          class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
          title="Export"
          aria-label="Export ${escapeHtml(terminal.name)} terminal output"
        >
          <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4"></path>
          </svg>
        </button>
        <button
          id="terminal-clear-btn"
          data-terminal-id="${terminal.id}"
          data-tooltip="Clear terminal history"
          data-tooltip-position="bottom"
          class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
          title="Clear terminal"
          aria-label="Clear ${escapeHtml(terminal.name)} terminal history"
        >
          <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"></path>
          </svg>
        </button>
        <button
          id="terminal-settings-btn"
          data-terminal-id="${terminal.id}"
          data-tooltip="Configure terminal role and settings"
          data-tooltip-position="bottom"
          class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
          title="Terminal settings"
          aria-label="Configure ${escapeHtml(terminal.name)} terminal settings"
        >
          <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"></path>
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"></path>
          </svg>
        </button>
        <button
          id="terminal-close-btn"
          data-terminal-id="${terminal.id}"
          data-tooltip="Close terminal"
          data-tooltip-position="bottom"
          class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors text-gray-600 dark:text-gray-300 hover:text-red-500 dark:hover:text-red-400 font-bold text-lg"
          title="Close terminal"
          aria-label="Close ${escapeHtml(terminal.name)} terminal"
        >
          <span aria-hidden="true">×</span>
        </button>
      </div>
    `;
    headerContainer.style.backgroundColor = styles.backgroundColor;
  }

  // Update wrapper border color
  const wrapper = document.getElementById("terminal-wrapper");
  if (wrapper) {
    wrapper.style.borderLeftColor = styles.borderColor;
  }

  // If missing session, render error UI inside the content container after DOM update
  if (hasMissingSession) {
    logger.info("Terminal has missing session, rendering error overlay", {
      terminalId: terminal.id,
    });
    setTimeout(async () => {
      // Get health check timing from health monitor
      const { getHealthMonitor } = await import("../health-monitor");
      const healthMonitor = getHealthMonitor();
      const healthTiming = healthMonitor.getHealthCheckTiming();

      renderMissingSessionError(terminal.id, terminal.id, healthTiming);
    }, 0);
  } else {
    logger.info("Terminal session valid, showing xterm", {
      terminalId: terminal.id,
      missingSession: terminal.missingSession,
    });
  }
}

export function renderMissingSessionError(
  sessionId: string,
  configId: string,
  healthTiming?: HealthCheckTiming
): void {
  logger.info("Rendering missing session error overlay", {
    sessionId,
    configId,
  });
  const container = document.getElementById(`xterm-container-${sessionId}`);
  if (!container) {
    logger.warn("Container not found for session", {
      sessionId,
      containerId: `xterm-container-${sessionId}`,
    });
    return;
  }
  logger.info("Found container, replacing with error UI", { sessionId });

  // Calculate health check timing info
  let healthCheckInfo = "";
  if (healthTiming) {
    const now = Date.now();
    const lastCheckText = healthTiming.lastCheckTime
      ? formatDuration(now - healthTiming.lastCheckTime)
      : "never";
    const nextCheckText = healthTiming.nextCheckTime
      ? formatDuration(healthTiming.nextCheckTime - now)
      : "unknown";

    healthCheckInfo = `
      <div class="mt-4 p-4 bg-gray-100 dark:bg-gray-800 rounded-lg border border-gray-300 dark:border-gray-600">
        <h4 class="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-3">Health Check Diagnostics</h4>
        <div class="grid grid-cols-2 gap-3 text-xs">
          <div class="flex flex-col">
            <span class="text-gray-500 dark:text-gray-400">Last Check:</span>
            <span class="font-mono font-semibold text-gray-700 dark:text-gray-300">${lastCheckText} ago</span>
          </div>
          <div class="flex flex-col">
            <span class="text-gray-500 dark:text-gray-400">Next Check:</span>
            <span class="font-mono font-semibold text-gray-700 dark:text-gray-300">in ${nextCheckText}</span>
          </div>
          <div class="flex flex-col col-span-2">
            <span class="text-gray-500 dark:text-gray-400">Check Interval:</span>
            <span class="font-mono font-semibold text-gray-700 dark:text-gray-300">${formatDuration(healthTiming.checkIntervalMs)}</span>
          </div>
        </div>
        <button
          id="check-now-btn"
          data-terminal-id="${configId}"
          class="mt-3 w-full px-4 py-2 bg-purple-600 hover:bg-purple-700 text-white rounded-lg font-medium transition-colors flex items-center justify-center gap-2"
        >
          <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15"></path>
          </svg>
          Check Now
        </button>
      </div>
    `;
  }

  container.innerHTML = `
    <div class="h-full flex items-center justify-center bg-red-50 dark:bg-red-900/20">
      <div class="max-w-2xl mx-auto p-6 text-center">
        <div class="mb-4 flex justify-center">
          <svg class="w-16 h-16 text-red-500 dark:text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z"></path>
          </svg>
        </div>
        <h3 class="text-lg font-semibold text-red-700 dark:text-red-300 mb-2">Terminal Session Missing</h3>
        <p class="text-sm text-red-600 dark:text-red-400 mb-4">
          The tmux session for this terminal no longer exists. This can happen if the daemon was restarted or the session was killed.
        </p>
        <p class="text-sm text-gray-600 dark:text-gray-400 mb-6">
          The app will automatically recreate the session on next restart, or you can trigger a health check below.
        </p>
        ${healthCheckInfo}
      </div>
    </div>
  `;
}
