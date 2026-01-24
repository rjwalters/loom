/**
 * Agent Metrics Dashboard Modal
 *
 * Displays agent effectiveness metrics including prompt counts, token usage,
 * costs, success rates, and GitHub activity broken down by role and time range.
 */

import {
  type AgentMetrics,
  formatCurrency,
  formatNumber,
  formatPercent,
  formatTokens,
  getAgentMetrics,
  getMetricsByRole,
  getRoleDisplayName,
  getSuccessRateColor,
  getTimeRangeLabel,
  type RoleMetrics,
  type TimeRange,
} from "./agent-metrics";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";

// Current time range selection
let currentTimeRange: TimeRange = "today";

/**
 * Show the agent metrics dashboard modal
 */
export async function showAgentMetricsModal(): Promise<void> {
  const modal = new ModalBuilder({
    title: "Agent Metrics",
    width: "700px",
    maxHeight: "85vh",
    id: "agent-metrics-modal",
  });

  // Show loading state initially
  modal.setContent(createLoadingContent());

  // Add footer button
  modal.addFooterButton("Close", () => modal.close(), "primary");

  modal.show();

  // Load and display metrics
  await refreshMetrics(modal);
}

/**
 * Refresh metrics data in the modal
 */
async function refreshMetrics(modal: ModalBuilder): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    const [summary, roleMetrics] = await Promise.all([
      getAgentMetrics(workspacePath, currentTimeRange),
      getMetricsByRole(workspacePath, currentTimeRange),
    ]);

    modal.setContent(createMetricsContent(summary, roleMetrics));
    setupTimeRangeHandlers(modal);
  } catch (error) {
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Create loading state content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-12">
      <div class="text-gray-500 dark:text-gray-400">Loading metrics...</div>
    </div>
  `;
}

/**
 * Create error state content
 */
function createErrorContent(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return `
    <div class="p-4 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
      <p class="text-red-700 dark:text-red-300">Failed to load metrics: ${escapeHtml(message)}</p>
    </div>
  `;
}

/**
 * Create main metrics content
 */
function createMetricsContent(summary: AgentMetrics, roleMetrics: RoleMetrics[]): string {
  return `
    <!-- Time Range Selector -->
    <div class="mb-6 flex gap-2">
      ${createTimeRangeButton("today")}
      ${createTimeRangeButton("week")}
      ${createTimeRangeButton("month")}
      ${createTimeRangeButton("all")}
    </div>

    <!-- Summary Cards -->
    <div class="grid grid-cols-2 md:grid-cols-3 gap-4 mb-6">
      ${createMetricCard("Prompts", formatNumber(summary.prompt_count), "Total agent interactions")}
      ${createMetricCard("Tokens", formatTokens(summary.total_tokens), "Total tokens consumed")}
      ${createMetricCard("Cost", formatCurrency(summary.total_cost), "Estimated API cost")}
      ${createMetricCard("Success Rate", formatPercent(summary.success_rate), "Work completion rate", getSuccessRateColor(summary.success_rate))}
      ${createMetricCard("PRs Created", formatNumber(summary.prs_created), "Pull requests opened")}
      ${createMetricCard("Issues Closed", formatNumber(summary.issues_closed), "Issues resolved")}
    </div>

    <!-- Role Breakdown -->
    <div class="mt-6">
      <h3 class="text-lg font-semibold mb-4 text-gray-800 dark:text-gray-200">By Role</h3>
      ${roleMetrics.length > 0 ? createRoleTable(roleMetrics) : createEmptyState()}
    </div>
  `;
}

/**
 * Create a time range button
 */
function createTimeRangeButton(range: TimeRange): string {
  const isActive = range === currentTimeRange;
  const activeClass = isActive
    ? "bg-blue-600 text-white"
    : "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-200 dark:hover:bg-gray-600";

  return `
    <button
      class="time-range-btn px-4 py-2 rounded-lg text-sm font-medium transition-colors ${activeClass}"
      data-range="${range}"
    >
      ${getTimeRangeLabel(range)}
    </button>
  `;
}

/**
 * Create a metric card
 */
function createMetricCard(
  label: string,
  value: string,
  description: string,
  valueColor?: string
): string {
  const colorClass = valueColor ?? "text-gray-900 dark:text-gray-100";

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-1">${label}</div>
      <div class="text-2xl font-bold ${colorClass}">${value}</div>
      <div class="text-xs text-gray-400 dark:text-gray-500 mt-1">${description}</div>
    </div>
  `;
}

/**
 * Create the role metrics table
 */
function createRoleTable(roleMetrics: RoleMetrics[]): string {
  return `
    <div class="overflow-x-auto">
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b border-gray-200 dark:border-gray-700">
            <th class="text-left py-2 font-medium text-gray-700 dark:text-gray-300">Role</th>
            <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Prompts</th>
            <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Tokens</th>
            <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Cost</th>
            <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Success</th>
          </tr>
        </thead>
        <tbody>
          ${roleMetrics.map(createRoleRow).join("")}
        </tbody>
      </table>
    </div>
  `;
}

/**
 * Create a role metrics row
 */
function createRoleRow(metrics: RoleMetrics): string {
  const successColor = getSuccessRateColor(metrics.success_rate);

  return `
    <tr class="border-b border-gray-100 dark:border-gray-800">
      <td class="py-3">
        <span class="font-medium text-gray-800 dark:text-gray-200">${getRoleDisplayName(metrics.role)}</span>
      </td>
      <td class="text-right py-3 text-gray-600 dark:text-gray-400">${formatNumber(metrics.prompt_count)}</td>
      <td class="text-right py-3 text-gray-600 dark:text-gray-400">${formatTokens(metrics.total_tokens)}</td>
      <td class="text-right py-3 text-gray-600 dark:text-gray-400">${formatCurrency(metrics.total_cost)}</td>
      <td class="text-right py-3 ${successColor}">${formatPercent(metrics.success_rate)}</td>
    </tr>
  `;
}

/**
 * Create empty state when no data is available
 */
function createEmptyState(): string {
  return `
    <div class="text-center py-8 text-gray-500 dark:text-gray-400">
      <p class="mb-2">No agent activity recorded for this time range.</p>
      <p class="text-sm">Activity will appear here as agents work on issues and PRs.</p>
    </div>
  `;
}

/**
 * Set up time range button handlers
 */
function setupTimeRangeHandlers(modal: ModalBuilder): void {
  const buttons = modal.querySelectorAll<HTMLButtonElement>(".time-range-btn");

  buttons.forEach((button) => {
    button.addEventListener("click", async () => {
      const range = button.dataset.range as TimeRange;
      if (range && range !== currentTimeRange) {
        currentTimeRange = range;
        modal.setContent(createLoadingContent());
        await refreshMetrics(modal);
      }
    });
  });
}

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
