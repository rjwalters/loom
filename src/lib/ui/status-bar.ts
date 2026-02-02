/**
 * Status Bar - Bottom bar renderer
 *
 * Renders session state indicator and key metrics into the #status-bar
 * container (#status-left and #status-right sub-containers).
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1898
 */

import { formatCurrency, formatNumber } from "../agent-metrics";
import { type AnalyticsData, collectAnalyticsData } from "../analytics/file-collector";
import { Logger } from "../logger";
import { escapeHtml } from "./helpers";

const logger = Logger.forComponent("status-bar");

let statusRefreshTimer: ReturnType<typeof setInterval> | null = null;
const STATUS_REFRESH_MS = 30000;

/**
 * Render the status bar content
 *
 * @param workspacePath - Current workspace path, or null if no workspace
 * @param sessionState - Current session state indicator
 */
export async function renderStatusBar(
  workspacePath: string | null,
  sessionState: "idle" | "working" | "error" | "stopped" = "stopped"
): Promise<void> {
  const leftContainer = document.getElementById("status-left");
  const rightContainer = document.getElementById("status-right");
  if (!leftContainer || !rightContainer) return;

  // Left side: session state indicator
  leftContainer.innerHTML = renderSessionIndicator(sessionState);

  if (!workspacePath) {
    rightContainer.innerHTML = "";
    stopStatusRefresh();
    return;
  }

  try {
    const data = await collectAnalyticsData(workspacePath);
    rightContainer.innerHTML = renderMetricsSummary(data);
  } catch (error) {
    logger.warn("Status bar metrics failed", { error: String(error) });
    rightContainer.innerHTML = "";
  }

  startStatusRefresh(workspacePath, sessionState);
}

/**
 * Start periodic status bar updates
 */
function startStatusRefresh(
  workspacePath: string,
  _sessionState: "idle" | "working" | "error" | "stopped"
): void {
  if (statusRefreshTimer) return;

  statusRefreshTimer = setInterval(async () => {
    const rightContainer = document.getElementById("status-right");
    if (!rightContainer) return;

    try {
      const data = await collectAnalyticsData(workspacePath);
      rightContainer.innerHTML = renderMetricsSummary(data);
    } catch {
      // Silent failure on refresh
    }
  }, STATUS_REFRESH_MS);
}

/**
 * Stop status bar refresh timer
 */
export function stopStatusRefresh(): void {
  if (statusRefreshTimer) {
    clearInterval(statusRefreshTimer);
    statusRefreshTimer = null;
  }
}

/**
 * Render session state indicator with colored dot
 */
function renderSessionIndicator(state: string): string {
  const stateConfig: Record<string, { color: string; label: string }> = {
    idle: { color: "bg-green-500", label: "Idle" },
    working: { color: "bg-blue-500 animate-pulse", label: "Working" },
    error: { color: "bg-red-500", label: "Error" },
    stopped: { color: "bg-gray-400", label: "Stopped" },
  };

  const config = stateConfig[state] ?? stateConfig.stopped;

  return `
    <div class="flex items-center gap-1.5">
      <span class="w-2 h-2 rounded-full ${config.color}" aria-label="Session ${config.label}"></span>
      <span>${escapeHtml(config.label)}</span>
    </div>`;
}

/**
 * Render compact metrics summary for the right side
 */
function renderMetricsSummary(data: AnalyticsData): string {
  const items: string[] = [];

  if (data.todayMetrics.prompt_count > 0) {
    items.push(`${formatNumber(data.todayMetrics.prompt_count)} prompts`);
  }

  if (data.todayMetrics.issues_closed > 0) {
    items.push(`${formatNumber(data.todayMetrics.issues_closed)} issues`);
  }

  if (data.todayMetrics.prs_created > 0) {
    items.push(`${formatNumber(data.todayMetrics.prs_created)} PRs`);
  }

  if (data.todayMetrics.total_cost > 0) {
    items.push(formatCurrency(data.todayMetrics.total_cost));
  }

  if (items.length === 0) {
    return `<span class="text-gray-400 dark:text-gray-500">No activity today</span>`;
  }

  return `<span>${items.map(escapeHtml).join(" &middot; ")}</span>`;
}
