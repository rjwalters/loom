/**
 * Dashboard View - Analytics panel renderer
 *
 * Renders analytics data into the #analytics-view container (right panel).
 * Sections: session summary, tool usage, code changes, cost estimate.
 * Auto-refreshes every 30s when active.
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1898
 */

import {
  formatCurrency,
  formatNumber,
  formatPercent,
  formatTokens,
  getTrendColor,
  getTrendIcon,
  type TrendDirection,
} from "../agent-metrics";
import { type AnalyticsData, collectAnalyticsData } from "../analytics/file-collector";
import { Logger } from "../logger";
import { escapeHtml } from "./helpers";

const logger = Logger.forComponent("dashboard-view");

const REFRESH_INTERVAL_MS = 30000;
let refreshTimer: ReturnType<typeof setInterval> | null = null;

/**
 * Render the analytics dashboard into #analytics-view
 *
 * If workspacePath is null, renders the empty state.
 * Starts auto-refresh when first called with a workspace.
 */
export async function renderDashboardView(workspacePath: string | null): Promise<void> {
  const container = document.getElementById("analytics-view");
  if (!container) return;

  if (!workspacePath) {
    container.innerHTML = renderEmptyState();
    stopAutoRefresh();
    return;
  }

  try {
    const data = await collectAnalyticsData(workspacePath);
    container.innerHTML = renderDashboard(data);
  } catch (error) {
    logger.error("Failed to render dashboard", error as Error);
    container.innerHTML = renderErrorState();
  }

  startAutoRefresh(workspacePath);
}

/**
 * Start auto-refresh timer
 */
function startAutoRefresh(workspacePath: string): void {
  if (refreshTimer) return;

  refreshTimer = setInterval(async () => {
    try {
      const data = await collectAnalyticsData(workspacePath);
      const container = document.getElementById("analytics-view");
      if (container) {
        container.innerHTML = renderDashboard(data);
      }
    } catch (error) {
      logger.warn("Dashboard auto-refresh failed", { error: String(error) });
    }
  }, REFRESH_INTERVAL_MS);
}

/**
 * Stop auto-refresh timer
 */
export function stopAutoRefresh(): void {
  if (refreshTimer) {
    clearInterval(refreshTimer);
    refreshTimer = null;
  }
}

/**
 * Render empty state when no session is active
 */
function renderEmptyState(): string {
  return `
    <div class="h-full flex items-center justify-center">
      <div class="text-center space-y-3">
        <div class="text-4xl text-gray-300 dark:text-gray-600">
          <svg class="w-16 h-16 mx-auto" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M9 19v-6a2 2 0 00-2-2H5a2 2 0 00-2 2v6a2 2 0 002 2h2a2 2 0 002-2zm0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V5a2 2 0 012-2h2a2 2 0 012 2v14a2 2 0 01-2 2h-2a2 2 0 01-2-2z"/>
          </svg>
        </div>
        <p class="text-sm text-gray-500 dark:text-gray-400">Start a session to see analytics</p>
      </div>
    </div>`;
}

/**
 * Render error state
 */
function renderErrorState(): string {
  return `
    <div class="h-full flex items-center justify-center">
      <div class="text-center space-y-2">
        <p class="text-sm text-red-500 dark:text-red-400">Failed to load analytics</p>
        <p class="text-xs text-gray-400 dark:text-gray-500">Data will retry on next refresh</p>
      </div>
    </div>`;
}

/**
 * Render the full dashboard with all sections
 */
function renderDashboard(data: AnalyticsData): string {
  return `
    <div class="space-y-4 pb-4">
      <div class="flex items-center justify-between">
        <h2 class="text-sm font-semibold text-gray-700 dark:text-gray-300 uppercase tracking-wider">Analytics</h2>
        <span class="text-xs text-gray-400 dark:text-gray-500">${formatTimestamp(data.collectedAt)}</span>
      </div>
      ${renderSessionSummary(data)}
      ${renderToolUsage(data)}
      ${renderCodeChanges(data)}
      ${renderCostEstimate(data)}
    </div>`;
}

/**
 * Session summary section - key metrics cards
 */
function renderSessionSummary(data: AnalyticsData): string {
  const { todayMetrics, velocity } = data;

  return `
    <section aria-label="Session Summary">
      <h3 class="text-xs font-medium text-gray-500 dark:text-gray-400 mb-2">Session Summary</h3>
      <div class="grid grid-cols-2 gap-2">
        ${renderMetricCard("Prompts", formatNumber(todayMetrics.prompt_count), velocity.issues_trend, "Today")}
        ${renderMetricCard("Issues Closed", formatNumber(todayMetrics.issues_closed), velocity.issues_trend, "Today")}
        ${renderMetricCard("PRs Created", formatNumber(todayMetrics.prs_created), velocity.prs_trend, "Today")}
        ${renderMetricCard("Success Rate", formatPercent(todayMetrics.success_rate), null, "Today")}
      </div>
    </section>`;
}

/**
 * Tool usage section - input activity breakdown
 */
function renderToolUsage(data: AnalyticsData): string {
  const { inputStats, weekMetrics } = data;

  return `
    <section aria-label="Activity">
      <h3 class="text-xs font-medium text-gray-500 dark:text-gray-400 mb-2">Activity</h3>
      <div class="rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-900 overflow-hidden">
        <div class="divide-y divide-gray-100 dark:divide-gray-800">
          ${renderStatRow("Commands", formatNumber(inputStats.commands))}
          ${renderStatRow("Keystrokes", formatNumber(inputStats.keystrokes))}
          ${renderStatRow("Pastes", formatNumber(inputStats.pastes))}
          ${renderStatRow("Total Characters", formatNumber(inputStats.totalCharacters))}
        </div>
      </div>
      <div class="mt-2 grid grid-cols-2 gap-2">
        ${renderMetricCard("Tokens (Week)", formatTokens(weekMetrics.total_tokens), null, "This week")}
        ${renderMetricCard("Prompts (Week)", formatNumber(weekMetrics.prompt_count), null, "This week")}
      </div>
    </section>`;
}

/**
 * Code changes section - git and pipeline stats
 */
function renderCodeChanges(data: AnalyticsData): string {
  const { gitStats, velocity } = data;

  return `
    <section aria-label="Code Changes">
      <h3 class="text-xs font-medium text-gray-500 dark:text-gray-400 mb-2">Pipeline</h3>
      <div class="rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-900 overflow-hidden">
        <div class="divide-y divide-gray-100 dark:divide-gray-800">
          ${renderStatRow("PRs Merged", formatNumber(gitStats.commitsToday))}
          ${renderStatRow("In Progress", formatNumber(gitStats.activeBranches))}
          ${renderStatRow("In Pipeline", formatNumber(gitStats.filesChanged))}
          ${renderStatRow("Issues (Week)", formatNumber(velocity.issues_closed), velocity.issues_trend)}
          ${renderStatRow("PRs (Week)", formatNumber(velocity.prs_merged), velocity.prs_trend)}
        </div>
      </div>
    </section>`;
}

/**
 * Cost estimate section
 */
function renderCostEstimate(data: AnalyticsData): string {
  const { todayMetrics, weekMetrics, velocity } = data;

  return `
    <section aria-label="Cost Estimate">
      <h3 class="text-xs font-medium text-gray-500 dark:text-gray-400 mb-2">Cost</h3>
      <div class="grid grid-cols-2 gap-2">
        ${renderMetricCard("Today", formatCurrency(todayMetrics.total_cost), null, "")}
        ${renderMetricCard("This Week", formatCurrency(weekMetrics.total_cost), null, formatCurrency(velocity.total_cost_usd))}
      </div>
    </section>`;
}

/**
 * Render a metric card with optional trend indicator
 */
function renderMetricCard(
  label: string,
  value: string,
  trend: TrendDirection | null,
  subtitle: string
): string {
  const trendHtml = trend
    ? `<span class="${getTrendColor(trend)} text-xs ml-1">${escapeHtml(getTrendIcon(trend))}</span>`
    : "";

  return `
    <div class="p-3 rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-900">
      <div class="text-xs text-gray-500 dark:text-gray-400">${escapeHtml(label)}</div>
      <div class="text-lg font-semibold text-gray-900 dark:text-gray-100 mt-0.5">
        ${escapeHtml(value)}${trendHtml}
      </div>
      ${subtitle ? `<div class="text-xs text-gray-400 dark:text-gray-500 mt-0.5">${escapeHtml(subtitle)}</div>` : ""}
    </div>`;
}

/**
 * Render a single stat row in a table-like section
 */
function renderStatRow(label: string, value: string, trend?: TrendDirection): string {
  const trendHtml = trend
    ? `<span class="${getTrendColor(trend)} ml-1">${escapeHtml(getTrendIcon(trend))}</span>`
    : "";

  return `
    <div class="flex items-center justify-between px-3 py-2">
      <span class="text-xs text-gray-600 dark:text-gray-400">${escapeHtml(label)}</span>
      <span class="text-xs font-medium text-gray-900 dark:text-gray-100">${escapeHtml(value)}${trendHtml}</span>
    </div>`;
}

/**
 * Format an ISO timestamp for display
 */
function formatTimestamp(iso: string): string {
  try {
    const date = new Date(iso);
    return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  } catch {
    return "";
  }
}
