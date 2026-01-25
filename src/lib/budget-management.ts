/**
 * Budget Management Module
 *
 * Provides budget tracking, alerts, and cost visualization for the Loom workspace.
 * Allows users to set daily, weekly, and monthly budget limits with configurable
 * alert thresholds.
 *
 * Features:
 * - Budget limits (daily, weekly, monthly)
 * - Configurable alert thresholds (50%, 75%, 90%, 100%)
 * - Cost breakdown by role (pie chart visualization)
 * - Cost breakdown by issue (table view)
 * - Runway projection based on burn rate
 * - Toast notifications for budget alerts
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1110
 */

import { invoke } from "@tauri-apps/api/core";
import {
  formatCurrency,
  formatNumber,
  formatPercent,
  formatTokens,
  getAgentMetrics,
  getMetricsByRole,
  getRoleDisplayName,
  type RoleMetrics,
  type TimeRange,
} from "./agent-metrics";
import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import { showToast } from "./toast";

const logger = Logger.forComponent("budget-management");

// ============================================================================
// Types
// ============================================================================

/**
 * Budget period type
 */
export type BudgetPeriod = "daily" | "weekly" | "monthly";

/**
 * Budget configuration stored in workspace
 */
export interface BudgetConfig {
  /** Daily budget limit in USD */
  dailyLimit: number | null;
  /** Weekly budget limit in USD */
  weeklyLimit: number | null;
  /** Monthly budget limit in USD */
  monthlyLimit: number | null;
  /** Alert thresholds as percentages (e.g., [50, 75, 90, 100]) */
  alertThresholds: number[];
  /** Whether budget alerts are enabled */
  alertsEnabled: boolean;
  /** Timestamps of last alerts sent (to prevent spam) */
  lastAlerts: {
    daily: { [threshold: number]: number };
    weekly: { [threshold: number]: number };
    monthly: { [threshold: number]: number };
  };
}

/**
 * Budget status for a specific period
 */
export interface BudgetStatus {
  period: BudgetPeriod;
  limit: number | null;
  spent: number;
  remaining: number | null;
  percentUsed: number | null;
  isOverBudget: boolean;
  triggeredThresholds: number[];
}

/**
 * Cost breakdown by issue
 */
export interface IssueCost {
  issueNumber: number;
  issueTitle: string;
  totalCost: number;
  totalTokens: number;
  promptCount: number;
  lastActivity: string;
}

/**
 * Runway projection
 */
export interface RunwayProjection {
  dailyBurnRate: number;
  weeklyBurnRate: number;
  daysRemaining: number | null;
  projectedMonthlySpend: number;
  runwayDate: string | null;
}

// ============================================================================
// Default Configuration
// ============================================================================

const DEFAULT_BUDGET_CONFIG: BudgetConfig = {
  dailyLimit: null,
  weeklyLimit: null,
  monthlyLimit: null,
  alertThresholds: [50, 75, 90, 100],
  alertsEnabled: true,
  lastAlerts: {
    daily: {},
    weekly: {},
    monthly: {},
  },
};

// ============================================================================
// Budget State
// ============================================================================

let currentModal: ModalBuilder | null = null;
// Note: cachedBudgetConfig intentionally unused - reserved for future optimization

// ============================================================================
// API Functions
// ============================================================================

/**
 * Get the current budget configuration
 */
export async function getBudgetConfig(workspacePath: string): Promise<BudgetConfig> {
  try {
    const config = await invoke<BudgetConfig | null>("get_budget_config", {
      workspacePath,
    });
    return config ?? { ...DEFAULT_BUDGET_CONFIG };
  } catch (error) {
    logger.warn("Failed to load budget config, using defaults", { error });
    return { ...DEFAULT_BUDGET_CONFIG };
  }
}

/**
 * Save the budget configuration
 */
export async function saveBudgetConfig(
  workspacePath: string,
  config: BudgetConfig
): Promise<void> {
  try {
    await invoke("save_budget_config", {
      workspacePath,
      config,
    });
    logger.info("Budget config saved", { config });
  } catch (error) {
    logger.error("Failed to save budget config", error as Error);
    throw error;
  }
}

/**
 * Get budget status for all periods
 */
export async function getBudgetStatus(workspacePath: string): Promise<BudgetStatus[]> {
  const config = await getBudgetConfig(workspacePath);
  const statuses: BudgetStatus[] = [];

  // Get metrics for each period
  const periods: Array<{ period: BudgetPeriod; timeRange: TimeRange; limit: number | null }> = [
    { period: "daily", timeRange: "today", limit: config.dailyLimit },
    { period: "weekly", timeRange: "week", limit: config.weeklyLimit },
    { period: "monthly", timeRange: "month", limit: config.monthlyLimit },
  ];

  for (const { period, timeRange, limit } of periods) {
    const metrics = await getAgentMetrics(workspacePath, timeRange);
    const spent = metrics.total_cost;
    const remaining = limit !== null ? limit - spent : null;
    const percentUsed = limit !== null && limit > 0 ? (spent / limit) * 100 : null;
    const isOverBudget = limit !== null && spent > limit;

    // Determine triggered thresholds
    const triggeredThresholds: number[] = [];
    if (percentUsed !== null) {
      for (const threshold of config.alertThresholds) {
        if (percentUsed >= threshold) {
          triggeredThresholds.push(threshold);
        }
      }
    }

    statuses.push({
      period,
      limit,
      spent,
      remaining,
      percentUsed,
      isOverBudget,
      triggeredThresholds,
    });
  }

  return statuses;
}

/**
 * Get cost breakdown by issue
 */
export async function getCostsByIssue(
  workspacePath: string,
  timeRange: TimeRange = "week"
): Promise<IssueCost[]> {
  try {
    return await invoke<IssueCost[]>("get_costs_by_issue", {
      workspacePath,
      timeRange,
    });
  } catch (error) {
    logger.error("Failed to get costs by issue", error as Error);
    return [];
  }
}

/**
 * Get runway projection based on current burn rate
 */
export async function getRunwayProjection(workspacePath: string): Promise<RunwayProjection> {
  try {
    const [_todayMetrics, weekMetrics, monthMetrics] = await Promise.all([
      getAgentMetrics(workspacePath, "today"),
      getAgentMetrics(workspacePath, "week"),
      getAgentMetrics(workspacePath, "month"),
    ]);
    // _todayMetrics reserved for future real-time burn rate calculations

    const config = await getBudgetConfig(workspacePath);

    // Calculate burn rates
    const dailyBurnRate = weekMetrics.total_cost / 7; // Average daily from last week
    const weeklyBurnRate = weekMetrics.total_cost;

    // Calculate days remaining until monthly budget exhausted
    let daysRemaining: number | null = null;
    let runwayDate: string | null = null;

    if (config.monthlyLimit !== null && dailyBurnRate > 0) {
      const remainingBudget = config.monthlyLimit - monthMetrics.total_cost;
      if (remainingBudget > 0) {
        daysRemaining = Math.floor(remainingBudget / dailyBurnRate);
        const runwayDateObj = new Date();
        runwayDateObj.setDate(runwayDateObj.getDate() + daysRemaining);
        runwayDate = runwayDateObj.toISOString().split("T")[0];
      } else {
        daysRemaining = 0;
        runwayDate = new Date().toISOString().split("T")[0];
      }
    }

    // Project monthly spend
    const projectedMonthlySpend = dailyBurnRate * 30;

    return {
      dailyBurnRate,
      weeklyBurnRate,
      daysRemaining,
      projectedMonthlySpend,
      runwayDate,
    };
  } catch (error) {
    logger.error("Failed to calculate runway projection", error as Error);
    return {
      dailyBurnRate: 0,
      weeklyBurnRate: 0,
      daysRemaining: null,
      projectedMonthlySpend: 0,
      runwayDate: null,
    };
  }
}

/**
 * Check budget thresholds and send alerts if needed
 */
export async function checkBudgetAlerts(workspacePath: string): Promise<void> {
  const config = await getBudgetConfig(workspacePath);

  if (!config.alertsEnabled) {
    return;
  }

  const statuses = await getBudgetStatus(workspacePath);
  const now = Date.now();
  const alertCooldown = 60 * 60 * 1000; // 1 hour cooldown between same alerts

  for (const status of statuses) {
    if (status.percentUsed === null) continue;

    for (const threshold of status.triggeredThresholds) {
      const lastAlert = config.lastAlerts[status.period][threshold] || 0;

      if (now - lastAlert > alertCooldown) {
        // Send alert
        const message =
          status.isOverBudget && threshold === 100
            ? `Budget exceeded! ${status.period} spend: ${formatCurrency(status.spent)} / ${formatCurrency(status.limit!)}`
            : `${status.period.charAt(0).toUpperCase() + status.period.slice(1)} budget ${threshold}% used: ${formatCurrency(status.spent)} / ${formatCurrency(status.limit!)}`;

        const toastType = status.isOverBudget ? "error" : threshold >= 90 ? "error" : "info";
        showToast(message, toastType, 5000);

        // Update last alert time
        config.lastAlerts[status.period][threshold] = now;
      }
    }
  }

  // Save updated alert times
  await saveBudgetConfig(workspacePath, config);
}

// ============================================================================
// Modal Functions
// ============================================================================

/**
 * Show the Budget Management modal
 */
export async function showBudgetManagementModal(): Promise<void> {
  // Close existing modal if open
  if (currentModal?.isVisible()) {
    currentModal.close();
  }

  const modal = new ModalBuilder({
    title: "Budget Management",
    width: "800px",
    maxHeight: "90vh",
    id: "budget-management-modal",
    onClose: () => {
      currentModal = null;
    },
  });

  currentModal = modal;

  // Show loading state
  modal.setContent(createLoadingContent());
  modal.addFooterButton("Close", () => modal.close(), "primary");
  modal.show();

  // Load and display data
  await refreshBudgetModal(modal);
}

/**
 * Refresh the budget modal content
 */
async function refreshBudgetModal(modal: ModalBuilder): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    const [config, statuses, roleMetrics, issueCosts, runway] = await Promise.all([
      getBudgetConfig(workspacePath),
      getBudgetStatus(workspacePath),
      getMetricsByRole(workspacePath, "week"),
      getCostsByIssue(workspacePath, "week"),
      getRunwayProjection(workspacePath),
    ]);

    modal.setContent(
      createBudgetContent(config, statuses, roleMetrics, issueCosts, runway, workspacePath)
    );

    // Set up event handlers
    setupBudgetEventHandlers(modal, workspacePath);
  } catch (error) {
    logger.error("Failed to load budget data", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Create loading content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-16">
      <div class="flex flex-col items-center gap-4">
        <div class="animate-spin h-8 w-8 border-4 border-blue-500 border-t-transparent rounded-full"></div>
        <span class="text-gray-500 dark:text-gray-400">Loading budget data...</span>
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
          <h3 class="text-lg font-semibold text-red-700 dark:text-red-300">Failed to load budget data</h3>
          <p class="text-red-600 dark:text-red-400 mt-1">${escapeHtml(message)}</p>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create the main budget content
 */
function createBudgetContent(
  config: BudgetConfig,
  statuses: BudgetStatus[],
  roleMetrics: RoleMetrics[],
  issueCosts: IssueCost[],
  runway: RunwayProjection,
  _workspacePath: string // Reserved for future features like issue links
): string {
  return `
    <!-- Budget Settings Section -->
    ${createBudgetSettingsSection(config)}

    <!-- Budget Status Cards -->
    ${createBudgetStatusSection(statuses)}

    <!-- Runway Projection -->
    ${createRunwaySection(runway)}

    <!-- Cost Breakdown by Role -->
    ${createRoleCostSection(roleMetrics)}

    <!-- Cost Breakdown by Issue -->
    ${createIssueCostSection(issueCosts)}
  `;
}

/**
 * Create budget settings section
 */
function createBudgetSettingsSection(config: BudgetConfig): string {
  return `
    <div class="mb-6 p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide">Budget Limits</h3>
        <label class="flex items-center gap-2 text-sm">
          <input type="checkbox" id="alerts-enabled" class="rounded" ${config.alertsEnabled ? "checked" : ""}>
          <span class="text-gray-600 dark:text-gray-400">Enable alerts</span>
        </label>
      </div>

      <div class="grid grid-cols-3 gap-4">
        <!-- Daily Limit -->
        <div>
          <label class="block text-xs text-gray-500 dark:text-gray-400 mb-1">Daily Limit (USD)</label>
          <div class="flex items-center gap-2">
            <span class="text-gray-400">$</span>
            <input
              type="number"
              id="daily-limit"
              value="${config.dailyLimit ?? ""}"
              placeholder="No limit"
              min="0"
              step="0.01"
              class="flex-1 px-3 py-2 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded-lg text-sm focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
            >
          </div>
        </div>

        <!-- Weekly Limit -->
        <div>
          <label class="block text-xs text-gray-500 dark:text-gray-400 mb-1">Weekly Limit (USD)</label>
          <div class="flex items-center gap-2">
            <span class="text-gray-400">$</span>
            <input
              type="number"
              id="weekly-limit"
              value="${config.weeklyLimit ?? ""}"
              placeholder="No limit"
              min="0"
              step="0.01"
              class="flex-1 px-3 py-2 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded-lg text-sm focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
            >
          </div>
        </div>

        <!-- Monthly Limit -->
        <div>
          <label class="block text-xs text-gray-500 dark:text-gray-400 mb-1">Monthly Limit (USD)</label>
          <div class="flex items-center gap-2">
            <span class="text-gray-400">$</span>
            <input
              type="number"
              id="monthly-limit"
              value="${config.monthlyLimit ?? ""}"
              placeholder="No limit"
              min="0"
              step="0.01"
              class="flex-1 px-3 py-2 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded-lg text-sm focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
            >
          </div>
        </div>
      </div>

      <div class="mt-4 flex justify-end">
        <button
          id="save-budget-btn"
          class="px-4 py-2 bg-blue-600 hover:bg-blue-500 text-white text-sm font-medium rounded-lg transition-colors"
        >
          Save Budget Settings
        </button>
      </div>
    </div>
  `;
}

/**
 * Create budget status section with progress bars
 */
function createBudgetStatusSection(statuses: BudgetStatus[]): string {
  const cards = statuses
    .map((status) => {
      const hasLimit = status.limit !== null;
      const percentUsed = status.percentUsed ?? 0;
      const progressColor = getProgressColor(percentUsed);

      return `
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <div class="flex items-center justify-between mb-2">
          <span class="text-sm font-medium text-gray-700 dark:text-gray-300 capitalize">${status.period}</span>
          ${
            status.isOverBudget
              ? '<span class="px-2 py-0.5 text-xs font-medium bg-red-100 dark:bg-red-900/30 text-red-700 dark:text-red-400 rounded">Over Budget</span>'
              : ""
          }
        </div>

        <div class="text-2xl font-bold text-gray-900 dark:text-gray-100 mb-1">
          ${formatCurrency(status.spent)}
        </div>

        ${
          hasLimit
            ? `
          <div class="text-sm text-gray-500 dark:text-gray-400 mb-2">
            of ${formatCurrency(status.limit!)} (${status.remaining !== null && status.remaining >= 0 ? formatCurrency(status.remaining) + " remaining" : "exceeded"})
          </div>

          <div class="h-2 bg-gray-200 dark:bg-gray-700 rounded-full overflow-hidden">
            <div class="h-full ${progressColor} rounded-full transition-all" style="width: ${Math.min(percentUsed, 100)}%"></div>
          </div>

          <div class="mt-1 text-xs text-gray-400 dark:text-gray-500 text-right">
            ${percentUsed.toFixed(1)}% used
          </div>
        `
            : `
          <div class="text-sm text-gray-400 dark:text-gray-500">
            No limit set
          </div>
        `
        }
      </div>
    `;
    })
    .join("");

  return `
    <div class="mb-6">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Current Spend</h3>
      <div class="grid grid-cols-3 gap-4">
        ${cards}
      </div>
    </div>
  `;
}

/**
 * Create runway projection section
 */
function createRunwaySection(runway: RunwayProjection): string {
  return `
    <div class="mb-6 p-4 bg-blue-50 dark:bg-blue-900/20 rounded-lg border border-blue-200 dark:border-blue-800">
      <h3 class="text-sm font-semibold text-blue-700 dark:text-blue-400 uppercase tracking-wide mb-3">Runway Projection</h3>

      <div class="grid grid-cols-4 gap-4">
        <div>
          <div class="text-xs text-blue-600 dark:text-blue-400 mb-1">Daily Burn Rate</div>
          <div class="text-lg font-bold text-blue-900 dark:text-blue-200">${formatCurrency(runway.dailyBurnRate)}/day</div>
        </div>

        <div>
          <div class="text-xs text-blue-600 dark:text-blue-400 mb-1">Weekly Burn Rate</div>
          <div class="text-lg font-bold text-blue-900 dark:text-blue-200">${formatCurrency(runway.weeklyBurnRate)}/week</div>
        </div>

        <div>
          <div class="text-xs text-blue-600 dark:text-blue-400 mb-1">Projected Monthly</div>
          <div class="text-lg font-bold text-blue-900 dark:text-blue-200">${formatCurrency(runway.projectedMonthlySpend)}</div>
        </div>

        <div>
          <div class="text-xs text-blue-600 dark:text-blue-400 mb-1">Budget Runway</div>
          <div class="text-lg font-bold ${runway.daysRemaining !== null && runway.daysRemaining < 7 ? "text-red-600 dark:text-red-400" : "text-blue-900 dark:text-blue-200"}">
            ${runway.daysRemaining !== null ? `${runway.daysRemaining} days` : "No limit set"}
          </div>
          ${runway.runwayDate ? `<div class="text-xs text-blue-500 dark:text-blue-400">Until ${runway.runwayDate}</div>` : ""}
        </div>
      </div>
    </div>
  `;
}

/**
 * Create cost breakdown by role section with visual chart
 */
function createRoleCostSection(roleMetrics: RoleMetrics[]): string {
  if (roleMetrics.length === 0) {
    return `
      <div class="mb-6">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Cost by Role (This Week)</h3>
        <div class="p-8 text-center text-gray-500 dark:text-gray-400 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
          <p>No cost data available for this period.</p>
        </div>
      </div>
    `;
  }

  const totalCost = roleMetrics.reduce((sum, m) => sum + m.total_cost, 0);
  const colors = [
    "bg-blue-500",
    "bg-green-500",
    "bg-yellow-500",
    "bg-purple-500",
    "bg-pink-500",
    "bg-indigo-500",
    "bg-red-500",
    "bg-orange-500",
  ];

  // Create bar chart
  const bars = roleMetrics
    .map((metrics, index) => {
      const percentage = totalCost > 0 ? (metrics.total_cost / totalCost) * 100 : 0;
      const color = colors[index % colors.length];

      return `
      <div class="flex items-center gap-3 mb-2">
        <div class="w-24 text-sm text-gray-700 dark:text-gray-300 truncate" title="${getRoleDisplayName(metrics.role)}">
          ${getRoleDisplayName(metrics.role)}
        </div>
        <div class="flex-1 h-6 bg-gray-200 dark:bg-gray-700 rounded overflow-hidden">
          <div class="h-full ${color} flex items-center justify-end px-2 text-xs text-white font-medium" style="width: ${Math.max(percentage, 5)}%">
            ${percentage > 10 ? formatCurrency(metrics.total_cost) : ""}
          </div>
        </div>
        <div class="w-20 text-right text-sm text-gray-600 dark:text-gray-400">
          ${formatPercent(percentage / 100)}
        </div>
      </div>
    `;
    })
    .join("");

  return `
    <div class="mb-6">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Cost by Role (This Week)</h3>
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        ${bars}
        <div class="mt-3 pt-3 border-t border-gray-200 dark:border-gray-700 flex justify-between text-sm">
          <span class="text-gray-600 dark:text-gray-400">Total</span>
          <span class="font-bold text-gray-900 dark:text-gray-100">${formatCurrency(totalCost)}</span>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create cost breakdown by issue section
 */
function createIssueCostSection(issueCosts: IssueCost[]): string {
  if (issueCosts.length === 0) {
    return `
      <div class="mb-6">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Cost by Issue (This Week)</h3>
        <div class="p-8 text-center text-gray-500 dark:text-gray-400 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
          <p>No issue-level cost data available.</p>
        </div>
      </div>
    `;
  }

  const rows = issueCosts
    .slice(0, 10) // Show top 10
    .map(
      (issue) => `
      <tr class="border-b border-gray-100 dark:border-gray-800 hover:bg-gray-100 dark:hover:bg-gray-800/50">
        <td class="px-4 py-3">
          <a href="#" class="text-blue-600 dark:text-blue-400 hover:underline font-medium">#${issue.issueNumber}</a>
        </td>
        <td class="px-4 py-3 text-gray-700 dark:text-gray-300 max-w-xs truncate" title="${escapeHtml(issue.issueTitle)}">
          ${escapeHtml(issue.issueTitle)}
        </td>
        <td class="px-4 py-3 text-right text-gray-600 dark:text-gray-400">${formatNumber(issue.promptCount)}</td>
        <td class="px-4 py-3 text-right text-gray-600 dark:text-gray-400">${formatTokens(issue.totalTokens)}</td>
        <td class="px-4 py-3 text-right font-medium text-gray-900 dark:text-gray-100">${formatCurrency(issue.totalCost)}</td>
      </tr>
    `
    )
    .join("");

  return `
    <div class="mb-6">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Cost by Issue (This Week)</h3>
      <div class="bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700 overflow-hidden">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-gray-200 dark:border-gray-700 bg-gray-100 dark:bg-gray-800">
              <th class="text-left px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Issue</th>
              <th class="text-left px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Title</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Prompts</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Tokens</th>
              <th class="text-right px-4 py-3 font-medium text-gray-700 dark:text-gray-300">Cost</th>
            </tr>
          </thead>
          <tbody>
            ${rows}
          </tbody>
        </table>
        ${issueCosts.length > 10 ? `<div class="px-4 py-2 text-sm text-gray-500 dark:text-gray-400 text-center border-t border-gray-200 dark:border-gray-700">Showing top 10 of ${issueCosts.length} issues</div>` : ""}
      </div>
    </div>
  `;
}

/**
 * Set up event handlers for budget modal
 */
function setupBudgetEventHandlers(modal: ModalBuilder, workspacePath: string): void {
  // Save button
  const saveBtn = modal.querySelector<HTMLButtonElement>("#save-budget-btn");
  if (saveBtn) {
    saveBtn.addEventListener("click", async () => {
      const dailyInput = modal.querySelector<HTMLInputElement>("#daily-limit");
      const weeklyInput = modal.querySelector<HTMLInputElement>("#weekly-limit");
      const monthlyInput = modal.querySelector<HTMLInputElement>("#monthly-limit");
      const alertsCheckbox = modal.querySelector<HTMLInputElement>("#alerts-enabled");

      const currentConfig = await getBudgetConfig(workspacePath);

      const newConfig: BudgetConfig = {
        ...currentConfig,
        dailyLimit: dailyInput?.value ? parseFloat(dailyInput.value) : null,
        weeklyLimit: weeklyInput?.value ? parseFloat(weeklyInput.value) : null,
        monthlyLimit: monthlyInput?.value ? parseFloat(monthlyInput.value) : null,
        alertsEnabled: alertsCheckbox?.checked ?? true,
      };

      try {
        await saveBudgetConfig(workspacePath, newConfig);
        showToast("Budget settings saved", "success");
        await refreshBudgetModal(modal);
      } catch (error) {
        showToast("Failed to save budget settings", "error");
      }
    });
  }
}

/**
 * Get progress bar color based on percentage
 */
function getProgressColor(percent: number): string {
  if (percent >= 100) return "bg-red-500";
  if (percent >= 90) return "bg-red-400";
  if (percent >= 75) return "bg-yellow-500";
  if (percent >= 50) return "bg-yellow-400";
  return "bg-green-500";
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
 * Close the budget management modal if it's open
 */
export function closeBudgetManagementModal(): void {
  if (currentModal?.isVisible()) {
    currentModal.close();
  }
}

/**
 * Check if the budget management modal is currently visible
 */
export function isBudgetModalVisible(): boolean {
  return currentModal?.isVisible() ?? false;
}
