/**
 * Intelligence Dashboard
 *
 * A full-page dashboard view that displays comprehensive agent performance metrics,
 * real-time activity status, and system health indicators.
 *
 * Features:
 * - Activity summary cards (prompts, features, cost, tokens)
 * - Agent performance breakdown by role with success rates
 * - Week-over-week trend comparison
 * - Real-time agent status indicators
 * - Auto-refresh (configurable, default 30s)
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1108
 */

import {
  type AgentMetrics,
  formatChangePercent,
  formatCurrency,
  formatCycleTime,
  formatNumber,
  formatPercent,
  formatTokens,
  getAgentMetrics,
  getMetricsByRole,
  getRoleDisplayName,
  getSuccessRateColor,
  getTrendColor,
  getTrendIcon,
  getVelocitySummary,
  type RoleMetrics,
  type TrendDirection,
  type VelocitySummary,
} from "./agent-metrics";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import { Logger } from "./logger";

const logger = Logger.forComponent("intelligence-dashboard");

// Dashboard state
let refreshIntervalId: ReturnType<typeof setInterval> | null = null;
let currentModal: ModalBuilder | null = null;
const DEFAULT_REFRESH_INTERVAL = 30000; // 30 seconds

/**
 * Dashboard data structure
 */
interface DashboardData {
  today: AgentMetrics;
  week: AgentMetrics;
  roleMetrics: RoleMetrics[];
  velocity: VelocitySummary;
  terminals: TerminalStatus[];
}

/**
 * Terminal status for the dashboard
 */
interface TerminalStatus {
  id: string;
  name: string;
  role: string;
  status: "active" | "idle" | "error";
  lastActivity: number | null;
}

/**
 * Show the Intelligence Dashboard modal
 *
 * @param refreshInterval - Auto-refresh interval in milliseconds (0 to disable)
 */
export async function showIntelligenceDashboard(
  refreshInterval: number = DEFAULT_REFRESH_INTERVAL
): Promise<void> {
  // Close existing dashboard if open
  if (currentModal?.isVisible()) {
    currentModal.close();
  }

  const modal = new ModalBuilder({
    title: "Intelligence Dashboard",
    width: "900px",
    maxHeight: "90vh",
    id: "intelligence-dashboard-modal",
    onClose: () => {
      // Clean up refresh interval when modal closes
      if (refreshIntervalId) {
        clearInterval(refreshIntervalId);
        refreshIntervalId = null;
      }
      currentModal = null;
    },
  });

  currentModal = modal;

  // Show loading state
  modal.setContent(createLoadingContent());
  modal.addFooterButton("Close", () => modal.close(), "primary");
  modal.show();

  // Load and display data
  await refreshDashboard(modal);

  // Set up auto-refresh if interval is specified
  if (refreshInterval > 0) {
    refreshIntervalId = setInterval(async () => {
      if (modal.isVisible()) {
        await refreshDashboard(modal, true);
      }
    }, refreshInterval);
  }
}

/**
 * Refresh dashboard data
 */
async function refreshDashboard(
  modal: ModalBuilder,
  isAutoRefresh: boolean = false
): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    // Show subtle loading indicator for auto-refresh
    if (isAutoRefresh) {
      const refreshIndicator = modal.querySelector("#refresh-indicator");
      if (refreshIndicator) {
        refreshIndicator.classList.remove("hidden");
      }
    }

    // Fetch all data in parallel
    const [today, week, roleMetrics, velocity] = await Promise.all([
      getAgentMetrics(workspacePath, "today"),
      getAgentMetrics(workspacePath, "week"),
      getMetricsByRole(workspacePath, "week"),
      getVelocitySummary(workspacePath),
    ]);

    // Get terminal status from app state
    const terminals = getTerminalStatuses(state);

    const data: DashboardData = {
      today,
      week,
      roleMetrics,
      velocity,
      terminals,
    };

    modal.setContent(createDashboardContent(data));

    // Hide refresh indicator
    if (isAutoRefresh) {
      const refreshIndicator = modal.querySelector("#refresh-indicator");
      if (refreshIndicator) {
        refreshIndicator.classList.add("hidden");
      }
    }

    // Set up manual refresh button
    setupRefreshButton(modal);
  } catch (error) {
    logger.error("Failed to load dashboard data", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Get terminal statuses from app state
 */
function getTerminalStatuses(state: ReturnType<typeof getAppState>): TerminalStatus[] {
  const terminals = state.terminals.getTerminals();

  return terminals.map((terminal) => {
    // Determine status based on terminal state
    let status: "active" | "idle" | "error" = "idle";

    if (terminal.status === "error" || terminal.missingSession) {
      status = "error";
    } else if (terminal.status === "busy") {
      status = "active";
    }

    // Extract role file from roleConfig
    const roleFile = terminal.roleConfig?.roleFile;
    const role = typeof roleFile === "string" ? roleFile.replace(".md", "") : "driver";

    return {
      id: terminal.id,
      name: terminal.name,
      role,
      status,
      lastActivity: null, // Would need health monitor integration
    };
  });
}

/**
 * Create loading content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-16">
      <div class="flex flex-col items-center gap-4">
        <div class="animate-spin h-8 w-8 border-4 border-blue-500 border-t-transparent rounded-full"></div>
        <span class="text-gray-500 dark:text-gray-400">Loading dashboard data...</span>
      </div>
    </div>
  `;
}

/**
 * Create error content
 */
function createErrorContent(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return `
    <div class="p-6 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
      <div class="flex items-center gap-3">
        <span class="text-2xl">!</span>
        <div>
          <h3 class="text-lg font-semibold text-red-700 dark:text-red-300">Failed to load dashboard</h3>
          <p class="text-red-600 dark:text-red-400 mt-1">${escapeHtml(message)}</p>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create the main dashboard content
 */
function createDashboardContent(data: DashboardData): string {
  return `
    <!-- Header with refresh indicator -->
    <div class="flex items-center justify-between mb-6">
      <div class="flex items-center gap-3">
        <h2 class="text-xl font-bold text-gray-800 dark:text-gray-200">System Overview</h2>
        <span id="refresh-indicator" class="hidden text-sm text-gray-400">
          <span class="animate-pulse">Refreshing...</span>
        </span>
      </div>
      <button id="refresh-dashboard-btn" class="flex items-center gap-2 px-3 py-1.5 text-sm bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 rounded-lg transition-colors">
        <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
        </svg>
        Refresh
      </button>
    </div>

    <!-- Agent Activity Status -->
    ${createAgentStatusSection(data.terminals)}

    <!-- Today's Summary Cards -->
    ${createSummarySection(data.today)}

    <!-- Week-over-Week Trends -->
    ${createTrendSection(data.velocity)}

    <!-- Performance by Role -->
    ${createRolePerformanceSection(data.roleMetrics)}

    <!-- Last updated timestamp -->
    <div class="mt-6 text-xs text-gray-400 dark:text-gray-500 text-center">
      Last updated: ${new Date().toLocaleTimeString()} | Auto-refresh every 30s
    </div>
  `;
}

/**
 * Create agent status section
 */
function createAgentStatusSection(terminals: TerminalStatus[]): string {
  if (terminals.length === 0) {
    return `
      <div class="mb-6 p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Agent Status</h3>
        <p class="text-gray-500 dark:text-gray-400 text-sm">No active terminals</p>
      </div>
    `;
  }

  const activeCount = terminals.filter((t) => t.status === "active").length;
  const idleCount = terminals.filter((t) => t.status === "idle").length;
  const errorCount = terminals.filter((t) => t.status === "error").length;

  const statusBadges = terminals
    .slice(0, 8) // Show max 8 agents
    .map((terminal) => {
      const statusColors = {
        active: "bg-green-100 dark:bg-green-900/30 text-green-700 dark:text-green-400 border-green-300 dark:border-green-700",
        idle: "bg-gray-100 dark:bg-gray-800 text-gray-600 dark:text-gray-400 border-gray-300 dark:border-gray-600",
        error: "bg-red-100 dark:bg-red-900/30 text-red-700 dark:text-red-400 border-red-300 dark:border-red-700",
      };

      const statusDots = {
        active: "bg-green-500",
        idle: "bg-gray-400",
        error: "bg-red-500",
      };

      return `
        <div class="flex items-center gap-2 px-3 py-2 rounded-lg border ${statusColors[terminal.status]}">
          <span class="w-2 h-2 rounded-full ${statusDots[terminal.status]}"></span>
          <span class="text-sm font-medium">${escapeHtml(terminal.name)}</span>
          <span class="text-xs opacity-75">${getRoleDisplayName(terminal.role)}</span>
        </div>
      `;
    })
    .join("");

  const moreCount = terminals.length - 8;
  const moreIndicator =
    moreCount > 0 ? `<span class="text-sm text-gray-400">+${moreCount} more</span>` : "";

  return `
    <div class="mb-6 p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="flex items-center justify-between mb-3">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide">Agent Status</h3>
        <div class="flex items-center gap-4 text-sm">
          <span class="flex items-center gap-1">
            <span class="w-2 h-2 rounded-full bg-green-500"></span>
            <span class="text-gray-600 dark:text-gray-400">${activeCount} active</span>
          </span>
          <span class="flex items-center gap-1">
            <span class="w-2 h-2 rounded-full bg-gray-400"></span>
            <span class="text-gray-600 dark:text-gray-400">${idleCount} idle</span>
          </span>
          ${errorCount > 0 ? `
            <span class="flex items-center gap-1">
              <span class="w-2 h-2 rounded-full bg-red-500"></span>
              <span class="text-red-600 dark:text-red-400">${errorCount} error</span>
            </span>
          ` : ""}
        </div>
      </div>
      <div class="flex flex-wrap gap-2">
        ${statusBadges}
        ${moreIndicator}
      </div>
    </div>
  `;
}

/**
 * Create summary cards section
 */
function createSummarySection(metrics: AgentMetrics): string {
  return `
    <div class="mb-6">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Today's Activity</h3>
      <div class="grid grid-cols-2 md:grid-cols-4 gap-4">
        ${createSummaryCard("Prompts", formatNumber(metrics.prompt_count), "Agent interactions today", "bg-blue-50 dark:bg-blue-900/20 border-blue-200 dark:border-blue-800")}
        ${createSummaryCard("Features", formatNumber(metrics.issues_closed), "Issues resolved today", "bg-green-50 dark:bg-green-900/20 border-green-200 dark:border-green-800")}
        ${createSummaryCard("Cost", formatCurrency(metrics.total_cost), "API spend today", "bg-yellow-50 dark:bg-yellow-900/20 border-yellow-200 dark:border-yellow-800")}
        ${createSummaryCard("Tokens", formatTokens(metrics.total_tokens), "Tokens consumed today", "bg-purple-50 dark:bg-purple-900/20 border-purple-200 dark:border-purple-800")}
      </div>
    </div>
  `;
}

/**
 * Create a summary card
 */
function createSummaryCard(
  label: string,
  value: string,
  description: string,
  colorClass: string
): string {
  return `
    <div class="p-4 rounded-lg border ${colorClass}">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-1">${label}</div>
      <div class="text-2xl font-bold text-gray-900 dark:text-gray-100">${value}</div>
      <div class="text-xs text-gray-400 dark:text-gray-500 mt-1">${description}</div>
    </div>
  `;
}

/**
 * Create trend section with week-over-week comparison
 */
function createTrendSection(velocity: VelocitySummary): string {
  return `
    <div class="mb-6">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Week-over-Week Trends</h3>
      <div class="grid grid-cols-3 gap-4">
        ${createTrendCard(
          "Issues Closed",
          velocity.issues_closed,
          velocity.prev_issues_closed,
          velocity.issues_trend
        )}
        ${createTrendCard(
          "PRs Merged",
          velocity.prs_merged,
          velocity.prev_prs_merged,
          velocity.prs_trend
        )}
        ${createCycleTimeCard(velocity)}
      </div>
    </div>
  `;
}

/**
 * Create a trend comparison card
 */
function createTrendCard(
  label: string,
  current: number,
  previous: number,
  trend: TrendDirection
): string {
  const trendIcon = getTrendIcon(trend);
  const trendColor = getTrendColor(trend);
  const changePercent =
    previous > 0 ? ((current - previous) / previous) * 100 : current > 0 ? 100 : 0;

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-2">${label}</div>
      <div class="flex items-end justify-between">
        <div>
          <span class="text-2xl font-bold text-gray-900 dark:text-gray-100">${current}</span>
          <span class="text-sm text-gray-400 dark:text-gray-500 ml-2">vs ${previous}</span>
        </div>
        <div class="flex items-center gap-1 ${trendColor}">
          <span class="text-lg">${trendIcon}</span>
          <span class="text-sm font-medium">${formatChangePercent(changePercent)}</span>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create cycle time trend card
 */
function createCycleTimeCard(velocity: VelocitySummary): string {
  const trendIcon = getTrendIcon(velocity.cycle_time_trend);
  // For cycle time, lower is better, so invert the color
  const trendColor = getTrendColor(velocity.cycle_time_trend, true);

  const currentTime = formatCycleTime(velocity.avg_cycle_time_hours);
  const prevTime = formatCycleTime(velocity.prev_avg_cycle_time_hours);

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-2">Avg Cycle Time</div>
      <div class="flex items-end justify-between">
        <div>
          <span class="text-2xl font-bold text-gray-900 dark:text-gray-100">${currentTime}</span>
          <span class="text-sm text-gray-400 dark:text-gray-500 ml-2">vs ${prevTime}</span>
        </div>
        <div class="flex items-center gap-1 ${trendColor}">
          <span class="text-lg">${trendIcon}</span>
          <span class="text-sm font-medium">
            ${velocity.cycle_time_trend === "improving" ? "Faster" : velocity.cycle_time_trend === "declining" ? "Slower" : "Same"}
          </span>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create role performance section
 */
function createRolePerformanceSection(roleMetrics: RoleMetrics[]): string {
  if (roleMetrics.length === 0) {
    return `
      <div class="mb-6">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Performance by Role</h3>
        <div class="p-8 text-center text-gray-500 dark:text-gray-400 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
          <p class="mb-2">No agent activity recorded this week.</p>
          <p class="text-sm">Activity will appear here as agents work on issues and PRs.</p>
        </div>
      </div>
    `;
  }

  const rows = roleMetrics.map((metrics) => createRoleRow(metrics)).join("");

  return `
    <div class="mb-6">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Performance by Role (This Week)</h3>
      <div class="bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700 overflow-hidden">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-gray-200 dark:border-gray-700 bg-gray-100 dark:bg-gray-800">
              <th class="text-left px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Role</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Prompts</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Tokens</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Cost</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Success Rate</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300"></th>
            </tr>
          </thead>
          <tbody>
            ${rows}
          </tbody>
        </table>
      </div>
    </div>
  `;
}

/**
 * Create a role performance table row
 */
function createRoleRow(metrics: RoleMetrics): string {
  const successColor = getSuccessRateColor(metrics.success_rate);
  const successBarWidth = Math.round(metrics.success_rate * 100);

  return `
    <tr class="border-b border-gray-100 dark:border-gray-800 hover:bg-gray-100 dark:hover:bg-gray-800/50">
      <td class="px-4 py-3">
        <span class="font-medium text-gray-800 dark:text-gray-200">${getRoleDisplayName(metrics.role)}</span>
      </td>
      <td class="text-right px-4 py-3 text-gray-600 dark:text-gray-400">${formatNumber(metrics.prompt_count)}</td>
      <td class="text-right px-4 py-3 text-gray-600 dark:text-gray-400">${formatTokens(metrics.total_tokens)}</td>
      <td class="text-right px-4 py-3 text-gray-600 dark:text-gray-400">${formatCurrency(metrics.total_cost)}</td>
      <td class="text-right px-4 py-3 ${successColor} font-medium">${formatPercent(metrics.success_rate)}</td>
      <td class="px-4 py-3 w-24">
        <div class="h-2 bg-gray-200 dark:bg-gray-700 rounded-full overflow-hidden">
          <div class="h-full ${metrics.success_rate >= 0.7 ? "bg-green-500" : metrics.success_rate >= 0.4 ? "bg-yellow-500" : "bg-red-500"} rounded-full transition-all" style="width: ${successBarWidth}%"></div>
        </div>
      </td>
    </tr>
  `;
}

/**
 * Set up refresh button handler
 */
function setupRefreshButton(modal: ModalBuilder): void {
  const refreshBtn = modal.querySelector("#refresh-dashboard-btn");
  if (refreshBtn) {
    refreshBtn.addEventListener("click", async () => {
      await refreshDashboard(modal, true);
    });
  }
}

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

/**
 * Close the dashboard if it's open
 */
export function closeIntelligenceDashboard(): void {
  if (currentModal?.isVisible()) {
    currentModal.close();
  }
}

/**
 * Check if the dashboard is currently visible
 */
export function isDashboardVisible(): boolean {
  return currentModal?.isVisible() ?? false;
}
