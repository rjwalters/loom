import { Logger } from "./logger";
import { type Terminal, TerminalStatus } from "./state";
import { getTarotCardPath } from "./tarot-cards";
import { getTheme, getThemeStyles, isDarkMode } from "./themes";

const logger = Logger.forComponent("ui");

/**
 * Format milliseconds into a human-readable duration
 */
function formatDuration(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);

  if (days > 0) return `${days}d ${hours % 24}h`;
  if (hours > 0) return `${hours}h ${minutes % 60}m`;
  if (minutes > 0) return `${minutes}m ${seconds % 60}s`;
  return `${seconds}s`;
}

export function renderHeader(
  displayedWorkspacePath: string,
  hasWorkspace: boolean,
  daemonConnected?: boolean,
  lastPing?: number | null
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

function extractRepoName(path: string): string {
  if (!path) return "";
  // Get the last component of the path
  const parts = path.split("/").filter((p) => p.length > 0);
  return parts[parts.length - 1] || path;
}

/**
 * Render loading state during factory reset
 */
export function renderLoadingState(message: string = "Resetting workspace..."): void {
  const container = document.getElementById("primary-terminal");
  if (!container) return;

  container.innerHTML = `
    <div class="h-full flex items-center justify-center bg-gray-50 dark:bg-gray-900">
      <div class="flex flex-col items-center gap-6">
        <!-- Loom weaving animation -->
        <div class="relative w-32 h-32">
          <!-- Vertical warp threads -->
          <div class="absolute inset-0 flex justify-around items-center">
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 0ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 200ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 400ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 600ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 800ms; animation-duration: 1.5s;"></div>
          </div>

          <!-- Horizontal weft shuttle -->
          <div class="absolute inset-0 flex items-center overflow-hidden">
            <div class="w-full h-1 bg-gradient-to-r from-transparent via-blue-500 dark:via-blue-400 to-transparent animate-pulse" style="animation-duration: 2s;"></div>
          </div>

          <!-- Weaving shuttle moving across and up -->
          <div class="absolute inset-0 flex items-center">
            <div class="h-2 w-6 bg-blue-600 dark:bg-blue-400 rounded-full shadow-lg" style="animation: shuttle 4s ease-in-out infinite;"></div>
          </div>
        </div>

        <!-- Animated message -->
        <div class="text-center">
          <p class="text-lg font-semibold text-gray-700 dark:text-gray-300 animate-pulse">${escapeHtml(message)}</p>
          <p class="text-sm text-gray-500 dark:text-gray-400 mt-2">This may take a few moments...</p>
        </div>
      </div>
    </div>

    <style>
      @keyframes shuttle {
        0% {
          transform: translateX(-200%) translateY(50%);
        }
        22% {
          transform: translateX(600%) translateY(50%);
        }
        25% {
          transform: translateX(600%) translateY(30%);
        }
        47% {
          transform: translateX(-200%) translateY(30%);
        }
        50% {
          transform: translateX(-200%) translateY(10%);
        }
        72% {
          transform: translateX(600%) translateY(10%);
        }
        75% {
          transform: translateX(600%) translateY(-10%);
        }
        97% {
          transform: translateX(-200%) translateY(-10%);
        }
        100% {
          transform: translateX(-200%) translateY(50%);
        }
      }
    </style>
  `;
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
      <div class="h-full flex flex-col bg-white dark:bg-gray-800 rounded-lg border-l-4 border-r border-t border-b border-gray-200 dark:border-gray-700 overflow-hidden" style="border-left-color: ${styles.borderColor}" id="terminal-wrapper">
        <div class="flex items-center justify-between px-4 py-2 border-b border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-700" style="background-color: ${styles.backgroundColor}" id="terminal-header">
          <!-- Header content will be updated below -->
        </div>
        <div id="persistent-xterm-containers" class="flex-1 overflow-auto relative">
          <!-- Persistent xterm containers created by terminal-manager, shown/hidden via display style -->
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
        ${createRunNowButtonHTML(terminal, "primary")}
        <button
          id="terminal-clear-btn"
          data-terminal-id="${terminal.id}"
          data-tooltip="Clear terminal history"
          data-tooltip-position="bottom"
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
          data-tooltip="Configure terminal role and settings"
          data-tooltip-position="bottom"
          class="p-1.5 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
          title="Terminal settings"
        >
          <svg class="w-4 h-4 text-gray-600 dark:text-gray-300" fill="none" stroke="currentColor" viewBox="0 0 24 24">
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
        >
          ×
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
      const { getHealthMonitor } = await import("./health-monitor");
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

export interface HealthCheckTiming {
  lastCheckTime: number | null;
  nextCheckTime: number | null;
  checkIntervalMs: number;
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

export function renderMiniTerminals(
  terminals: Terminal[],
  hasWorkspace: boolean,
  terminalHealthMap?: Map<string, { lastActivity: number | null; isStale: boolean }>
): void {
  const container = document.getElementById("mini-terminal-row");
  if (!container) return;

  const terminalCards = terminals
    .map((t, index) => {
      const health = terminalHealthMap?.get(t.id);
      return createMiniTerminalHTML(t, index, health);
    })
    .join("");

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
        data-tooltip="${addButtonTitle}"
        data-tooltip-position="auto"
        class="flex-shrink-0 w-40 h-40 flex items-center justify-center ${addButtonClasses} rounded-lg border-2 border-dashed border-gray-300 dark:border-gray-600 transition-colors"
        title="${addButtonTitle}"
        ${addButtonDisabled}
      >
        <span class="text-3xl text-gray-400">+</span>
      </button>
    </div>
  `;
}

/**
 * Create "Run Now" button HTML for interval mode terminals
 *
 * Only shown for terminals with autonomous mode enabled (targetInterval > 0)
 *
 * @param terminal - The terminal to create the button for
 * @param context - Whether this is for "primary" or "mini" view
 * @returns HTML string for the button, or empty string if not applicable
 */
function createRunNowButtonHTML(terminal: Terminal, context: "primary" | "mini"): string {
  // Check if terminal has interval mode enabled
  const hasInterval =
    terminal.roleConfig?.targetInterval !== undefined &&
    (terminal.roleConfig.targetInterval as number) > 0;

  if (!hasInterval) {
    return ""; // Don't show button for non-interval terminals
  }

  // Different styling for primary vs mini view
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

/**
 * Create timer display HTML for busy/idle time tracking
 */
function createTimerDisplayHTML(terminal: Terminal): string {
  // Initialize timers if not present
  const busyTime = terminal.busyTime || 0;
  const idleTime = terminal.idleTime || 0;
  const lastStateChange = terminal.lastStateChange || Date.now();
  const now = Date.now();

  // Calculate current delta based on current status
  let currentBusyTime = busyTime;
  let currentIdleTime = idleTime;

  if (terminal.lastStateChange) {
    const delta = now - lastStateChange;
    if (terminal.status === TerminalStatus.Busy) {
      currentBusyTime += delta;
    } else if (terminal.status === TerminalStatus.Idle) {
      currentIdleTime += delta;
    }
  }

  // Format the durations
  const busyDisplay = formatDuration(currentBusyTime);
  const idleDisplay = formatDuration(currentIdleTime);

  // Choose colors based on status
  const busyColor =
    terminal.status === TerminalStatus.Busy
      ? "text-blue-600 dark:text-blue-400 font-semibold"
      : "text-gray-500 dark:text-gray-400";
  const idleColor =
    terminal.status === TerminalStatus.Idle
      ? "text-gray-600 dark:text-gray-300 font-semibold"
      : "text-gray-500 dark:text-gray-400";

  return `
    <div class="flex flex-col gap-0.5 mt-1 border-t border-gray-200 dark:border-gray-700 pt-1">
      <div class="flex items-center justify-between ${busyColor}" data-tooltip="Time spent actively running commands" data-tooltip-position="top">
        <span>⏱️ Busy:</span>
        <span class="font-mono text-xs">${busyDisplay}</span>
      </div>
      <div class="flex items-center justify-between ${idleColor}" data-tooltip="Time spent waiting at prompt" data-tooltip-position="top">
        <span>💤 Idle:</span>
        <span class="font-mono text-xs">${idleDisplay}</span>
      </div>
    </div>
  `;
}

function createMiniTerminalHTML(
  terminal: Terminal,
  index: number,
  health?: { lastActivity: number | null; isStale: boolean }
): string {
  // Get theme colors
  const theme = getTheme(terminal.theme, terminal.customTheme);
  const styles = getThemeStyles(theme, isDarkMode());

  const borderWidth = terminal.isPrimary ? "3" : "2";
  const borderColor = terminal.isPrimary ? styles.activeColor : styles.borderColor;

  // Show notification badge when terminal needs input
  const needsInputBadge =
    terminal.status === TerminalStatus.NeedsInput
      ? `<div class="absolute -top-1 -right-1 w-3 h-3 bg-red-500 rounded-full border-2 border-white dark:border-gray-900 animate-pulse"></div>`
      : "";

  // Activity indicator
  let activityInfo = "";
  if (health) {
    if (health.lastActivity) {
      const timeSince = Date.now() - health.lastActivity;
      const activityText = formatDuration(timeSince);
      const activityColor = health.isStale
        ? "text-orange-500 dark:text-orange-400"
        : "text-green-600 dark:text-green-400";
      const activityIcon = health.isStale ? "⏸" : "⚡";
      activityInfo = `<span class="${activityColor} text-xs flex items-center gap-1" data-tooltip="Last activity: ${activityText} ago" data-tooltip-position="top">
        ${activityIcon} ${activityText}
      </span>`;
    } else {
      activityInfo = `<span class="text-gray-400 dark:text-gray-500 text-xs" data-tooltip="No activity recorded" data-tooltip-position="top">—</span>`;
    }
  }

  // Get tarot card path for this terminal's role
  const tarotCardPath = getTarotCardPath(terminal.role);

  return `
    <div class="p-1 flex-shrink-0">
      <div class="relative">
        ${needsInputBadge}
        <div
          class="terminal-card group w-40 h-40 bg-white dark:bg-gray-800 hover:bg-gray-900/5 dark:hover:bg-white/5 rounded-lg cursor-grab transition-all relative overflow-hidden"
          style="border: ${borderWidth}px solid ${borderColor}"
          data-terminal-id="${terminal.id}"
          draggable="true"
        >
          <!-- Regular terminal card content -->
          <div class="terminal-card-content">
            <div class="p-2 border-b border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-700 group-hover:bg-gray-100 dark:group-hover:bg-gray-600 flex items-center justify-between transition-colors rounded-t-lg" style="background-color: ${styles.backgroundColor}">
              <div class="flex items-center gap-2 flex-1 min-w-0">
                <div class="w-2 h-2 rounded-full flex-shrink-0 ${getStatusColor(terminal.status)}"></div>
                <span class="terminal-name text-xs font-medium truncate" data-tooltip="Double-click to rename, drag to reorder" data-tooltip-position="top">${escapeHtml(terminal.name)}</span>
              </div>
              <div class="flex items-center gap-0.5 flex-shrink-0">
                ${createRunNowButtonHTML(terminal, "mini")}
                <button
                  class="close-terminal-btn text-gray-400 hover:text-red-500 dark:hover:text-red-400 font-bold transition-colors"
                  data-terminal-id="${terminal.id}"
                  data-tooltip="Close terminal"
                  data-tooltip-position="top"
                  title="Close terminal"
                >
                  ×
                </button>
              </div>
            </div>
            <div class="p-2 text-xs text-gray-500 dark:text-gray-400 flex flex-col gap-1">
              <div class="flex items-center justify-between">
                <span>${terminal.status}</span>
                <span class="font-mono font-bold text-blue-600 dark:text-blue-400">#${index}</span>
              </div>
              ${activityInfo ? `<div class="flex items-center justify-between">${activityInfo}</div>` : ""}
              ${createTimerDisplayHTML(terminal)}
            </div>
          </div>

          <!-- Tarot card overlay (hidden by default, shown during drag) -->
          <div class="tarot-card-overlay absolute inset-0 pointer-events-none flex items-center justify-center bg-gray-900 dark:bg-black rounded-lg">
            <img src="${tarotCardPath}" alt="Tarot card" class="h-full w-full object-contain p-2" />
          </div>
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
