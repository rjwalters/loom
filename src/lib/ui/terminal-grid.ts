import { type Terminal, TerminalStatus } from "../state";
import { getTarotCardPath } from "../tarot-cards";
import { getTheme, getThemeStyles, isDarkMode } from "../themes";
import { escapeHtml, formatDuration, getStatusColor } from "./helpers";

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
          class="terminal-card group w-40 h-40 bg-gray-100 dark:bg-gray-800 hover:bg-gray-200 dark:hover:bg-white/5 rounded-lg cursor-grab transition-all relative overflow-hidden"
          style="border: ${borderWidth}px solid ${borderColor}"
          data-terminal-id="${terminal.id}"
          draggable="true"
          role="tab"
          aria-selected="${terminal.isPrimary}"
          aria-label="${escapeHtml(terminal.name)} terminal, ${terminal.status}"
          tabindex="${terminal.isPrimary ? 0 : -1}"
        >
          <!-- Regular terminal card content -->
          <div class="terminal-card-content">
            <div class="p-2 border-b border-gray-200 dark:border-gray-700 bg-gray-100 dark:bg-gray-700 group-hover:bg-gray-200 dark:group-hover:bg-gray-600 flex items-center justify-between transition-colors rounded-t-lg" style="background-color: ${styles.backgroundColor}">
              <div class="flex items-center gap-2 flex-1 min-w-0">
                <div class="w-2 h-2 rounded-full flex-shrink-0 ${getStatusColor(terminal.status)}"></div>
                <span class="terminal-name text-xs font-medium truncate" data-tooltip="Double-click to rename, drag to reorder" data-tooltip-position="top">${escapeHtml(terminal.name)}</span>
              </div>
              <div class="flex items-center gap-0.5 flex-shrink-0">
                <button
                  class="show-activity-btn p-0.5 text-gray-400 hover:text-blue-500 dark:hover:text-blue-400 transition-colors"
                  data-terminal-id="${terminal.id}"
                  data-tooltip="View activity"
                  data-tooltip-position="top"
                  title="View activity"
                  aria-label="View activity for ${escapeHtml(terminal.name)}"
                >
                  <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z"></path>
                  </svg>
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

export function renderMiniTerminals(
  terminals: Terminal[],
  hasWorkspace: boolean,
  terminalHealthMap?: Map<string, { lastActivity: number | null; isStale: boolean }>
): void {
  const container = document.getElementById("mini-terminal-row");
  if (!container) return;

  // Save the current scroll position before re-rendering
  const scrollContainer = container.querySelector(".overflow-x-auto");
  const savedScrollLeft = scrollContainer?.scrollLeft ?? 0;

  const terminalCards = terminals
    .map((t, index) => {
      const health = terminalHealthMap?.get(t.id);
      return createMiniTerminalHTML(t, index, health);
    })
    .join("");

  const addButtonClasses = hasWorkspace
    ? "bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 cursor-pointer"
    : "bg-gray-100 dark:bg-gray-800 cursor-not-allowed opacity-50";

  const addButtonDisabled = hasWorkspace ? "" : "disabled";
  const addButtonTitle = hasWorkspace ? "Add terminal" : "Select a workspace first";

  container.innerHTML = `
    <div
      class="h-full flex items-center gap-2 px-4 py-2 overflow-x-auto overflow-y-visible"
      role="tablist"
      aria-label="Terminal tabs"
    >
      ${terminalCards}
      <button
        id="add-terminal-btn"
        data-tooltip="${addButtonTitle}"
        data-tooltip-position="auto"
        class="flex-shrink-0 w-40 h-40 flex items-center justify-center ${addButtonClasses} rounded-lg border-2 border-dashed border-gray-300 dark:border-gray-600 transition-colors"
        title="${addButtonTitle}"
        aria-label="Add new terminal"
        ${addButtonDisabled}
      >
        <span class="text-3xl text-gray-400" aria-hidden="true">+</span>
      </button>
    </div>
  `;

  // Restore the scroll position after re-rendering (without animation)
  const newScrollContainer = container.querySelector(".overflow-x-auto") as HTMLElement;
  if (newScrollContainer && savedScrollLeft > 0) {
    // Use scrollTo with behavior: 'instant' to skip animation
    newScrollContainer.scrollTo({
      left: savedScrollLeft,
      behavior: "instant" as ScrollBehavior,
    });
  }
}
