/**
 * Comparative Analysis Modal
 *
 * Displays correlation analysis results, success factor breakdowns,
 * and prediction model insights. Helps users understand what factors
 * drive success in their development workflow.
 *
 * Part of Phase 3 (Intelligence & Learning) - Issue #2262
 */

import {
  analyzeDayOfWeekSuccess,
  analyzeRoleSuccessCorrelation,
  analyzeTimeOfDaySuccess,
  type CorrelationSummary,
  getSuccessRateColorClass,
  runCorrelationAnalysis,
  type SuccessRateByFactor,
} from "./correlation-analysis";
import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import {
  formatAccuracy,
  getPredictionModelStats,
  type ModelStats,
  trainPredictionModel,
} from "./prediction-model";
import { getOptimizationStats, type OptimizationStats } from "./prompt-optimization";
import { getAppState } from "./state";

const logger = Logger.forComponent("comparative-analysis-modal");

let currentModal: ModalBuilder | null = null;

interface AnalysisData {
  summary: CorrelationSummary;
  roleBreakdown: SuccessRateByFactor[];
  timeOfDay: SuccessRateByFactor[];
  dayOfWeek: SuccessRateByFactor[];
  modelStats: ModelStats;
  optimizationStats: OptimizationStats;
}

/**
 * Show the Comparative Analysis modal
 */
export async function showComparativeAnalysisModal(): Promise<void> {
  if (currentModal?.isVisible()) {
    currentModal.close();
  }

  const modal = new ModalBuilder({
    title: "Comparative Analysis",
    width: "900px",
    maxHeight: "90vh",
    id: "comparative-analysis-modal",
    onClose: () => {
      currentModal = null;
    },
  });

  currentModal = modal;

  modal.setContent(createLoadingContent());
  modal.addFooterButton("Close", () => modal.close(), "primary");
  modal.show();

  await refreshAnalysis(modal);
}

/**
 * Refresh analysis data
 */
async function refreshAnalysis(modal: ModalBuilder): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    const [summary, roleBreakdown, timeOfDay, dayOfWeek, modelStats, optimizationStats] =
      await Promise.all([
        runCorrelationAnalysis(workspacePath),
        analyzeRoleSuccessCorrelation(workspacePath),
        analyzeTimeOfDaySuccess(workspacePath),
        analyzeDayOfWeekSuccess(workspacePath),
        getPredictionModelStats(workspacePath),
        getOptimizationStats(workspacePath),
      ]);

    const data: AnalysisData = {
      summary,
      roleBreakdown,
      timeOfDay,
      dayOfWeek,
      modelStats,
      optimizationStats,
    };

    modal.setContent(createAnalysisContent(data));
    setupEventHandlers(modal, workspacePath);
  } catch (error) {
    logger.error("Failed to load comparative analysis", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Set up event handlers for interactive elements
 */
function setupEventHandlers(modal: ModalBuilder, workspacePath: string): void {
  const refreshBtn = modal.querySelector("#refresh-analysis-btn");
  if (refreshBtn) {
    refreshBtn.addEventListener("click", async () => {
      await refreshAnalysis(modal);
    });
  }

  const trainBtn = modal.querySelector("#train-model-btn");
  if (trainBtn) {
    trainBtn.addEventListener("click", async () => {
      trainBtn.textContent = "Training...";
      (trainBtn as HTMLButtonElement).disabled = true;
      try {
        const result = await trainPredictionModel(workspacePath);
        const statusEl = modal.querySelector("#model-train-status");
        if (statusEl) {
          statusEl.innerHTML = `
            <span class="text-green-600 dark:text-green-400 text-sm">
              Trained on ${result.samples_used} samples - Accuracy: ${formatAccuracy(result.accuracy)}
            </span>
          `;
        }
      } catch {
        const statusEl = modal.querySelector("#model-train-status");
        if (statusEl) {
          statusEl.innerHTML = `<span class="text-red-500 text-sm">Training failed</span>`;
        }
      }
      trainBtn.textContent = "Retrain Model";
      (trainBtn as HTMLButtonElement).disabled = false;
    });
  }
}

/**
 * Create the main analysis content
 */
function createAnalysisContent(data: AnalysisData): string {
  return `
    <div class="space-y-6">
      <!-- Header with refresh -->
      <div class="flex items-center justify-between">
        <h2 class="text-xl font-bold text-gray-800 dark:text-gray-200">Success Factor Analysis</h2>
        <button id="refresh-analysis-btn" class="flex items-center gap-2 px-3 py-1.5 text-sm bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 rounded-lg transition-colors">
          <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
          </svg>
          Refresh
        </button>
      </div>

      <!-- Summary overview -->
      ${createSummarySection(data.summary)}

      <!-- Key insights -->
      ${createInsightsSection(data.summary)}

      <!-- Role breakdown -->
      ${createRoleBreakdownSection(data.roleBreakdown)}

      <!-- Time analysis -->
      ${createTimeAnalysisSection(data.timeOfDay, data.dayOfWeek)}

      <!-- Prediction model -->
      ${createModelSection(data.modelStats)}

      <!-- Optimization stats -->
      ${createOptimizationSection(data.optimizationStats)}
    </div>
  `;
}

/**
 * Summary overview cards
 */
function createSummarySection(summary: CorrelationSummary): string {
  return `
    <div class="grid grid-cols-3 gap-4">
      <div class="p-4 bg-blue-50 dark:bg-blue-900/20 rounded-lg border border-blue-200 dark:border-blue-800">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide">Total Samples</div>
        <div class="text-2xl font-bold text-gray-900 dark:text-gray-100 mt-1">${summary.total_samples}</div>
      </div>
      <div class="p-4 bg-green-50 dark:bg-green-900/20 rounded-lg border border-green-200 dark:border-green-800">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide">Overall Success Rate</div>
        <div class="text-2xl font-bold text-gray-900 dark:text-gray-100 mt-1">${Math.round(summary.success_rate * 100)}%</div>
      </div>
      <div class="p-4 bg-purple-50 dark:bg-purple-900/20 rounded-lg border border-purple-200 dark:border-purple-800">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide">Significant Correlations</div>
        <div class="text-2xl font-bold text-gray-900 dark:text-gray-100 mt-1">${summary.significant_correlations}</div>
      </div>
    </div>
  `;
}

/**
 * Key insights from correlation analysis
 */
function createInsightsSection(summary: CorrelationSummary): string {
  if (summary.top_insights.length === 0) {
    return `
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-2">Key Insights</h3>
        <p class="text-gray-500 dark:text-gray-400 text-sm">Not enough data to generate insights yet. Insights appear as agents accumulate more activity.</p>
      </div>
    `;
  }

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Key Insights</h3>
      <div class="space-y-3">
        ${summary.top_insights
          .map(
            (insight) => `
          <div class="p-3 bg-white dark:bg-gray-800 rounded-lg border border-gray-200 dark:border-gray-700">
            <div class="flex items-center gap-2 mb-1">
              <span class="text-xs px-2 py-0.5 rounded-full bg-blue-100 dark:bg-blue-900/30 text-blue-700 dark:text-blue-300">${escapeHtml(insight.factor)}</span>
              <span class="text-xs text-gray-400">${escapeHtml(insight.correlation_strength)}</span>
            </div>
            <p class="text-sm text-gray-800 dark:text-gray-200">${escapeHtml(insight.insight)}</p>
            <p class="text-xs text-gray-500 dark:text-gray-400 mt-1">${escapeHtml(insight.recommendation)}</p>
          </div>
        `
          )
          .join("")}
      </div>
    </div>
  `;
}

/**
 * Role-based success breakdown
 */
function createRoleBreakdownSection(roles: SuccessRateByFactor[]): string {
  if (roles.length === 0) {
    return `
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-2">Success by Role</h3>
        <p class="text-gray-500 dark:text-gray-400 text-sm">No role data available yet.</p>
      </div>
    `;
  }

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Success by Role</h3>
      <div class="space-y-2">
        ${roles.map((role) => createFactorBar(role)).join("")}
      </div>
    </div>
  `;
}

/**
 * Time-based analysis section
 */
function createTimeAnalysisSection(
  timeOfDay: SuccessRateByFactor[],
  dayOfWeek: SuccessRateByFactor[]
): string {
  return `
    <div class="grid grid-cols-2 gap-4">
      <!-- Time of Day -->
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Success by Time of Day</h3>
        ${
          timeOfDay.length > 0
            ? `<div class="space-y-2">${timeOfDay.map((t) => createFactorBar(t)).join("")}</div>`
            : `<p class="text-gray-500 dark:text-gray-400 text-sm">No data yet.</p>`
        }
      </div>

      <!-- Day of Week -->
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Success by Day of Week</h3>
        ${
          dayOfWeek.length > 0
            ? `<div class="space-y-2">${dayOfWeek.map((d) => createFactorBar(d)).join("")}</div>`
            : `<p class="text-gray-500 dark:text-gray-400 text-sm">No data yet.</p>`
        }
      </div>
    </div>
  `;
}

/**
 * Prediction model section
 */
function createModelSection(stats: ModelStats): string {
  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="flex items-center justify-between mb-3">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide">Prediction Model</h3>
        <button id="train-model-btn" class="text-xs px-3 py-1.5 bg-blue-600 hover:bg-blue-500 text-white rounded-lg transition-colors">
          ${stats.is_trained ? "Retrain Model" : "Train Model"}
        </button>
      </div>
      <div id="model-train-status"></div>
      <div class="grid grid-cols-3 gap-3">
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Status</div>
          <div class="text-sm font-medium mt-1 ${stats.is_trained ? "text-green-600 dark:text-green-400" : "text-gray-500"}">${stats.is_trained ? "Trained" : "Untrained"}</div>
        </div>
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Samples</div>
          <div class="text-sm font-medium mt-1 text-gray-800 dark:text-gray-200">${stats.samples_count}</div>
        </div>
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Accuracy</div>
          <div class="text-sm font-medium mt-1 text-gray-800 dark:text-gray-200">${stats.accuracy != null ? formatAccuracy(stats.accuracy) : "N/A"}</div>
        </div>
      </div>
      ${
        stats.last_trained
          ? `<div class="text-xs text-gray-400 dark:text-gray-500 mt-2 text-center">Last trained: ${formatDate(stats.last_trained)}</div>`
          : ""
      }
    </div>
  `;
}

/**
 * Optimization stats section
 */
function createOptimizationSection(stats: OptimizationStats): string {
  if (stats.total_suggestions === 0) {
    return `
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-2">Prompt Optimization</h3>
        <p class="text-gray-500 dark:text-gray-400 text-sm">No optimization suggestions generated yet.</p>
      </div>
    `;
  }

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Prompt Optimization</h3>
      <div class="grid grid-cols-4 gap-3">
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Total</div>
          <div class="text-lg font-bold text-gray-800 dark:text-gray-200">${stats.total_suggestions}</div>
        </div>
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Accepted</div>
          <div class="text-lg font-bold text-green-600 dark:text-green-400">${stats.accepted_suggestions}</div>
        </div>
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Acceptance Rate</div>
          <div class="text-lg font-bold text-gray-800 dark:text-gray-200">${Math.round(stats.acceptance_rate * 100)}%</div>
        </div>
        <div class="text-center">
          <div class="text-xs text-gray-500 dark:text-gray-400">Avg Improvement</div>
          <div class="text-lg font-bold text-blue-600 dark:text-blue-400">+${Math.round(stats.avg_improvement_when_accepted * 100)}%</div>
        </div>
      </div>
      ${
        stats.suggestions_by_type.length > 0
          ? `
        <div class="mt-3 border-t border-gray-200 dark:border-gray-700 pt-3">
          <div class="text-xs text-gray-500 dark:text-gray-400 mb-2">By Type</div>
          <div class="space-y-1">
            ${stats.suggestions_by_type
              .map(
                (t) => `
              <div class="flex items-center justify-between text-xs">
                <span class="text-gray-600 dark:text-gray-400">${escapeHtml(t.optimization_type)}</span>
                <span class="text-gray-800 dark:text-gray-200">${t.count} suggestions (${Math.round(t.acceptance_rate * 100)}% accepted)</span>
              </div>
            `
              )
              .join("")}
          </div>
        </div>
      `
          : ""
      }
    </div>
  `;
}

/**
 * Create a horizontal bar for a success factor
 */
function createFactorBar(factor: SuccessRateByFactor): string {
  const barWidth = Math.round(factor.success_rate * 100);
  const barColor =
    factor.success_rate >= 0.7
      ? "bg-green-500"
      : factor.success_rate >= 0.4
        ? "bg-yellow-500"
        : "bg-red-500";

  return `
    <div class="flex items-center gap-3">
      <span class="text-xs text-gray-600 dark:text-gray-400 w-24 text-right flex-shrink-0 truncate" title="${escapeHtml(factor.factor_value)}">${escapeHtml(factor.factor_value)}</span>
      <div class="flex-1 h-4 bg-gray-200 dark:bg-gray-700 rounded-full overflow-hidden">
        <div class="h-full ${barColor} rounded-full transition-all" style="width: ${barWidth}%"></div>
      </div>
      <span class="text-xs font-medium text-gray-800 dark:text-gray-200 w-12 text-right ${getSuccessRateColorClass(factor.success_rate)}">${Math.round(factor.success_rate * 100)}%</span>
      <span class="text-xs text-gray-400 w-10 text-right">${factor.total_count}</span>
    </div>
  `;
}

function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-16">
      <div class="flex flex-col items-center gap-4">
        <div class="animate-spin h-8 w-8 border-4 border-blue-500 border-t-transparent rounded-full"></div>
        <span class="text-gray-500 dark:text-gray-400">Running analysis...</span>
      </div>
    </div>
  `;
}

function createErrorContent(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return `
    <div class="p-6 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
      <h3 class="text-lg font-semibold text-red-700 dark:text-red-300">Analysis Failed</h3>
      <p class="text-red-600 dark:text-red-400 mt-1">${escapeHtml(message)}</p>
    </div>
  `;
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function formatDate(isoDate: string): string {
  try {
    return new Date(isoDate).toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric",
    });
  } catch {
    return isoDate;
  }
}
