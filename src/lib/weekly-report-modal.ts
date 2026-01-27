/**
 * Weekly Report Modal
 *
 * UI component for viewing and interacting with weekly intelligence reports.
 * Provides a rich display of report data with sections for summary, patterns,
 * anomalies, recommendations, and insights.
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1111
 */

import {
  formatCurrency,
  formatCycleTime,
  formatNumber,
  formatPercent,
  formatTokens,
  getRoleDisplayName,
  getSuccessRateColor,
  getTrendColor,
  getTrendIcon,
} from "./agent-metrics";
import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import {
  exportReportAsMarkdown,
  formatHour,
  formatWeekRange,
  generateWeeklyReport,
  getDayName,
  getLatestReport,
  getReport,
  getReportHistory,
  getReportSchedule,
  markReportViewed,
  type ReportSchedule,
  saveReportSchedule,
  type WeeklyReport,
} from "./weekly-report";

const logger = Logger.forComponent("weekly-report-modal");

// Modal state
let currentModal: ModalBuilder | null = null;
let currentReport: WeeklyReport | null = null;

// ============================================================================
// Main Modal Functions
// ============================================================================

/**
 * Show the weekly report modal
 *
 * @param reportId - Optional specific report ID to show (defaults to latest)
 */
export async function showWeeklyReportModal(reportId?: string): Promise<void> {
  // Close existing modal if open
  if (currentModal?.isVisible()) {
    currentModal.close();
  }

  const modal = new ModalBuilder({
    title: "Weekly Intelligence Report",
    width: "900px",
    maxHeight: "90vh",
    id: "weekly-report-modal",
    onClose: () => {
      currentModal = null;
      currentReport = null;
    },
  });

  currentModal = modal;

  // Show loading state
  modal.setContent(createLoadingContent());
  modal.addFooterButton("Close", () => modal.close(), "primary");
  modal.show();

  // Load report
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    let report: WeeklyReport | null = null;

    if (reportId) {
      report = await getReport(workspacePath, reportId);
    } else {
      report = await getLatestReport(workspacePath);
    }

    if (!report) {
      modal.setContent(createNoReportContent(workspacePath, modal));
      return;
    }

    currentReport = report;

    // Mark as viewed
    await markReportViewed(workspacePath, report.id);

    // Display report
    modal.setContent(createReportContent(report, workspacePath));

    // Add footer buttons
    modal.clearFooterButtons();
    modal.addFooterButton("Export Markdown", () => handleExport(report), "secondary");
    modal.addFooterButton("View History", () => showHistoryView(workspacePath, modal), "secondary");
    modal.addFooterButton("Settings", () => showSettingsView(workspacePath, modal), "secondary");
    modal.addFooterButton("Close", () => modal.close(), "primary");

    // Set up interactive elements
    setupInteractiveElements(modal, report);
  } catch (error) {
    logger.error("Failed to load weekly report", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Show notification that a new report is ready
 *
 * @param report - The newly generated report
 */
export function showReportReadyNotification(report: WeeklyReport): void {
  // Create a toast notification
  const toast = document.createElement("div");
  toast.className =
    "fixed bottom-4 right-4 bg-blue-600 text-white px-4 py-3 rounded-lg shadow-lg flex items-center gap-3 z-50 animate-slide-up cursor-pointer";
  toast.innerHTML = `
    <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
    </svg>
    <div>
      <div class="font-medium">Weekly Report Ready</div>
      <div class="text-sm opacity-90">${formatWeekRange(report.week_start, report.week_end)}</div>
    </div>
    <button class="ml-2 p-1 hover:bg-blue-700 rounded">
      <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
      </svg>
    </button>
  `;

  // Click to view report
  toast.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest("button")) {
      toast.remove();
    } else {
      toast.remove();
      showWeeklyReportModal(report.id);
    }
  });

  document.body.appendChild(toast);

  // Auto-remove after 10 seconds
  setTimeout(() => {
    if (toast.parentElement) {
      toast.classList.add("animate-slide-down");
      setTimeout(() => toast.remove(), 300);
    }
  }, 10000);
}

// ============================================================================
// Content Generation
// ============================================================================

/**
 * Create loading content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-16">
      <div class="flex flex-col items-center gap-4">
        <div class="animate-spin h-8 w-8 border-4 border-blue-500 border-t-transparent rounded-full"></div>
        <span class="text-gray-500 dark:text-gray-400">Loading report...</span>
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
          <h3 class="text-lg font-semibold text-red-700 dark:text-red-300">Failed to load report</h3>
          <p class="text-red-600 dark:text-red-400 mt-1">${escapeHtml(message)}</p>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create content when no report exists
 */
function createNoReportContent(workspacePath: string, modal: ModalBuilder): string {
  // Add generate button handler after content is set
  setTimeout(() => {
    const generateBtn = modal.querySelector("#generate-first-report-btn");
    if (generateBtn) {
      generateBtn.addEventListener("click", async () => {
        modal.setContent(createLoadingContent());
        try {
          const report = await generateWeeklyReport(workspacePath);
          currentReport = report;
          modal.setContent(createReportContent(report, workspacePath));
          setupInteractiveElements(modal, report);
        } catch (error) {
          modal.setContent(createErrorContent(error));
        }
      });
    }
  }, 0);

  return `
    <div class="text-center py-12">
      <div class="text-6xl mb-4">!</div>
      <h3 class="text-xl font-semibold text-gray-700 dark:text-gray-300 mb-2">No Reports Yet</h3>
      <p class="text-gray-500 dark:text-gray-400 mb-6 max-w-md mx-auto">
        Generate your first weekly intelligence report to see insights about your development patterns.
      </p>
      <button id="generate-first-report-btn" class="px-6 py-3 bg-blue-600 hover:bg-blue-700 text-white rounded-lg font-medium transition-colors">
        Generate Report Now
      </button>
    </div>
  `;
}

/**
 * Create the main report content
 */
function createReportContent(report: WeeklyReport, _workspacePath: string): string {
  return `
    <div class="space-y-6">
      <!-- Header -->
      <div class="flex items-center justify-between border-b border-gray-200 dark:border-gray-700 pb-4">
        <div>
          <h2 class="text-xl font-bold text-gray-800 dark:text-gray-200">
            ${formatWeekRange(report.week_start, report.week_end)}
          </h2>
          <p class="text-sm text-gray-500 dark:text-gray-400">
            Generated ${new Date(report.generated_at).toLocaleString()}
          </p>
        </div>
        <button id="regenerate-report-btn" class="px-3 py-1.5 text-sm bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 rounded-lg transition-colors">
          Regenerate
        </button>
      </div>

      <!-- Summary Cards -->
      ${createSummarySection(report)}

      <!-- Anomalies/Alerts -->
      ${createAnomaliesSection(report)}

      <!-- Success Patterns -->
      ${createPatternsSection(report)}

      <!-- Recommendations -->
      ${createRecommendationsSection(report)}

      <!-- Did You Know -->
      ${createDidYouKnowSection(report)}

      <!-- Role Performance -->
      ${createRolePerformanceSection(report)}
    </div>
  `;
}

/**
 * Create summary section with trend cards
 */
function createSummarySection(report: WeeklyReport): string {
  const s = report.summary;

  return `
    <div class="grid grid-cols-2 md:grid-cols-5 gap-3">
      ${createTrendCard("Features", s.features_completed, s.prev_features_completed, s.features_trend, "bg-green-50 dark:bg-green-900/20")}
      ${createTrendCard("PRs Merged", s.prs_merged, s.prev_prs_merged, s.prs_trend, "bg-blue-50 dark:bg-blue-900/20")}
      ${createTrendCard("Cost", formatCurrency(s.total_cost), formatCurrency(s.prev_total_cost), s.cost_trend, "bg-yellow-50 dark:bg-yellow-900/20", true)}
      ${createTrendCard("Success", formatPercent(s.success_rate), formatPercent(s.prev_success_rate), s.success_trend, "bg-purple-50 dark:bg-purple-900/20")}
      ${createTrendCard("Cycle Time", formatCycleTime(s.avg_cycle_time_hours), formatCycleTime(s.prev_avg_cycle_time_hours), s.cycle_time_trend, "bg-indigo-50 dark:bg-indigo-900/20", true)}
    </div>
  `;
}

/**
 * Create a trend card
 */
function createTrendCard(
  label: string,
  current: number | string,
  previous: number | string,
  trend: string,
  bgClass: string,
  lowerIsBetter = false
): string {
  const trendIcon = getTrendIcon(trend as import("./agent-metrics").TrendDirection);
  const trendColor = getTrendColor(
    trend as import("./agent-metrics").TrendDirection,
    lowerIsBetter
  );

  return `
    <div class="p-3 ${bgClass} rounded-lg">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide">${label}</div>
      <div class="text-xl font-bold text-gray-900 dark:text-gray-100 mt-1">${current}</div>
      <div class="flex items-center gap-1 mt-1">
        <span class="${trendColor}">${trendIcon}</span>
        <span class="text-xs text-gray-400 dark:text-gray-500">vs ${previous}</span>
      </div>
    </div>
  `;
}

/**
 * Create anomalies/alerts section
 */
function createAnomaliesSection(report: WeeklyReport): string {
  if (report.anomalies.length === 0) return "";

  const items = report.anomalies
    .map((anomaly) => {
      const severityClass =
        anomaly.severity === "critical"
          ? "bg-red-100 dark:bg-red-900/30 border-red-300 dark:border-red-700"
          : "bg-yellow-100 dark:bg-yellow-900/30 border-yellow-300 dark:border-yellow-700";
      const iconClass = anomaly.severity === "critical" ? "text-red-600" : "text-yellow-600";

      return `
        <div class="p-3 ${severityClass} border rounded-lg">
          <div class="flex items-start gap-2">
            <span class="${iconClass} font-bold">!</span>
            <div>
              <div class="font-medium text-gray-800 dark:text-gray-200">${escapeHtml(anomaly.message)}</div>
              <div class="text-sm text-gray-600 dark:text-gray-400 mt-1">${escapeHtml(anomaly.details)}</div>
            </div>
          </div>
        </div>
      `;
    })
    .join("");

  return `
    <div>
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Alerts</h3>
      <div class="space-y-2">${items}</div>
    </div>
  `;
}

/**
 * Create patterns section
 */
function createPatternsSection(report: WeeklyReport): string {
  if (report.success_patterns.length === 0 && report.improvement_areas.length === 0) return "";

  let content = `<h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Patterns</h3>`;
  content += `<div class="grid grid-cols-1 md:grid-cols-2 gap-4">`;

  // Success patterns
  if (report.success_patterns.length > 0) {
    const successItems = report.success_patterns
      .map(
        (p) => `
        <div class="flex items-start gap-2">
          <span class="text-green-500">+</span>
          <div>
            <div class="text-sm text-gray-800 dark:text-gray-200">${escapeHtml(p.description)}</div>
            <div class="text-xs text-gray-500 dark:text-gray-400">${escapeHtml(p.impact)}</div>
          </div>
        </div>
      `
      )
      .join("");

    content += `
      <div class="p-4 bg-green-50 dark:bg-green-900/20 rounded-lg">
        <div class="text-xs font-semibold text-green-700 dark:text-green-400 uppercase mb-3">What Worked Well</div>
        <div class="space-y-3">${successItems}</div>
      </div>
    `;
  }

  // Improvement areas
  if (report.improvement_areas.length > 0) {
    const improvementItems = report.improvement_areas
      .map(
        (p) => `
        <div class="flex items-start gap-2">
          <span class="text-orange-500">!</span>
          <div>
            <div class="text-sm text-gray-800 dark:text-gray-200">${escapeHtml(p.description)}</div>
            <div class="text-xs text-gray-500 dark:text-gray-400">${escapeHtml(p.impact)}</div>
          </div>
        </div>
      `
      )
      .join("");

    content += `
      <div class="p-4 bg-orange-50 dark:bg-orange-900/20 rounded-lg">
        <div class="text-xs font-semibold text-orange-700 dark:text-orange-400 uppercase mb-3">Areas for Improvement</div>
        <div class="space-y-3">${improvementItems}</div>
      </div>
    `;
  }

  content += `</div>`;
  return content;
}

/**
 * Create recommendations section
 */
function createRecommendationsSection(report: WeeklyReport): string {
  if (report.recommendations.length === 0) return "";

  const items = report.recommendations
    .map((rec) => {
      const priorityClass =
        rec.priority === "high"
          ? "bg-red-100 dark:bg-red-900/30 text-red-700 dark:text-red-400"
          : rec.priority === "medium"
            ? "bg-yellow-100 dark:bg-yellow-900/30 text-yellow-700 dark:text-yellow-400"
            : "bg-gray-100 dark:bg-gray-800 text-gray-600 dark:text-gray-400";

      return `
        <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg">
          <div class="flex items-start justify-between">
            <div class="flex-1">
              <div class="flex items-center gap-2">
                <span class="text-xs px-2 py-0.5 rounded ${priorityClass} font-medium uppercase">${rec.priority}</span>
                <span class="text-xs text-gray-400">${escapeHtml(rec.category)}</span>
              </div>
              <h4 class="font-medium text-gray-800 dark:text-gray-200 mt-2">${escapeHtml(rec.title)}</h4>
              <p class="text-sm text-gray-600 dark:text-gray-400 mt-1">${escapeHtml(rec.description)}</p>
              <p class="text-sm text-blue-600 dark:text-blue-400 mt-2">Action: ${escapeHtml(rec.action)}</p>
            </div>
          </div>
        </div>
      `;
    })
    .join("");

  return `
    <div>
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Recommendations</h3>
      <div class="space-y-3">${items}</div>
    </div>
  `;
}

/**
 * Create "Did You Know" section
 */
function createDidYouKnowSection(report: WeeklyReport): string {
  if (report.did_you_know.length === 0) return "";

  const items = report.did_you_know
    .map(
      (insight) => `
      <div class="flex items-start gap-3 p-3 bg-blue-50 dark:bg-blue-900/20 rounded-lg">
        <span class="text-blue-500 text-xl">?</span>
        <div>
          <div class="text-sm font-medium text-gray-800 dark:text-gray-200">${escapeHtml(insight.fact)}</div>
          <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">${escapeHtml(insight.context)}</div>
        </div>
      </div>
    `
    )
    .join("");

  return `
    <div>
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Did You Know?</h3>
      <div class="grid grid-cols-1 md:grid-cols-3 gap-3">${items}</div>
    </div>
  `;
}

/**
 * Create role performance section
 */
function createRolePerformanceSection(report: WeeklyReport): string {
  if (report.role_metrics.length === 0) return "";

  const rows = report.role_metrics
    .map((role) => {
      const successColor = getSuccessRateColor(role.success_rate);
      const barWidth = Math.round(role.success_rate * 100);

      return `
        <tr class="border-b border-gray-100 dark:border-gray-800">
          <td class="py-2 font-medium text-gray-800 dark:text-gray-200">${getRoleDisplayName(role.role)}</td>
          <td class="py-2 text-gray-600 dark:text-gray-400 text-right">${formatNumber(role.prompt_count)}</td>
          <td class="py-2 text-gray-600 dark:text-gray-400 text-right">${formatTokens(role.total_tokens)}</td>
          <td class="py-2 text-gray-600 dark:text-gray-400 text-right">${formatCurrency(role.total_cost)}</td>
          <td class="py-2 text-right">
            <div class="flex items-center justify-end gap-2">
              <div class="w-16 h-2 bg-gray-200 dark:bg-gray-700 rounded-full overflow-hidden">
                <div class="h-full ${role.success_rate >= 0.7 ? "bg-green-500" : role.success_rate >= 0.4 ? "bg-yellow-500" : "bg-red-500"}" style="width: ${barWidth}%"></div>
              </div>
              <span class="${successColor} text-sm">${formatPercent(role.success_rate)}</span>
            </div>
          </td>
        </tr>
      `;
    })
    .join("");

  return `
    <div>
      <h3 class="text-sm font-semibold text-gray-600 dark:text-gray-400 uppercase tracking-wide mb-3">Performance by Role</h3>
      <div class="bg-gray-50 dark:bg-gray-900 rounded-lg overflow-hidden">
        <table class="w-full text-sm">
          <thead>
            <tr class="text-left text-gray-500 dark:text-gray-400 border-b border-gray-200 dark:border-gray-700">
              <th class="py-2 px-3 font-medium">Role</th>
              <th class="py-2 px-3 font-medium text-right">Prompts</th>
              <th class="py-2 px-3 font-medium text-right">Tokens</th>
              <th class="py-2 px-3 font-medium text-right">Cost</th>
              <th class="py-2 px-3 font-medium text-right">Success</th>
            </tr>
          </thead>
          <tbody class="px-3">${rows}</tbody>
        </table>
      </div>
    </div>
  `;
}

// ============================================================================
// History View
// ============================================================================

/**
 * Show report history view
 */
async function showHistoryView(workspacePath: string, modal: ModalBuilder): Promise<void> {
  modal.setContent(createLoadingContent());

  try {
    const history = await getReportHistory(workspacePath, 8);

    if (history.length === 0) {
      modal.setContent(`
        <div class="text-center py-8">
          <p class="text-gray-500 dark:text-gray-400">No report history available.</p>
        </div>
      `);
      return;
    }

    const items = history
      .map(
        (entry) => `
        <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg hover:bg-gray-100 dark:hover:bg-gray-800 cursor-pointer transition-colors" data-report-id="${entry.id}">
          <div class="flex items-center justify-between">
            <div>
              <div class="font-medium text-gray-800 dark:text-gray-200">${formatWeekRange(entry.week_start, entry.week_end)}</div>
              <div class="text-sm text-gray-500 dark:text-gray-400 mt-1">
                ${formatNumber(entry.summary_features)} features | ${formatCurrency(entry.summary_cost)}
              </div>
            </div>
            <div class="text-sm text-gray-400">
              ${new Date(entry.generated_at).toLocaleDateString()}
            </div>
          </div>
        </div>
      `
      )
      .join("");

    modal.setContent(`
      <div class="space-y-4">
        <div class="flex items-center justify-between border-b border-gray-200 dark:border-gray-700 pb-4">
          <h2 class="text-xl font-bold text-gray-800 dark:text-gray-200">Report History</h2>
          <button id="back-to-report-btn" class="text-sm text-blue-600 hover:text-blue-700 dark:text-blue-400">
            Back to Current Report
          </button>
        </div>
        <div class="space-y-3">${items}</div>
      </div>
    `);

    // Set up click handlers
    const backBtn = modal.querySelector("#back-to-report-btn");
    if (backBtn) {
      backBtn.addEventListener("click", () => {
        if (currentReport) {
          modal.setContent(createReportContent(currentReport, workspacePath));
          setupInteractiveElements(modal, currentReport);
        }
      });
    }

    const reportItems = modal.querySelectorAll("[data-report-id]");
    reportItems.forEach((item) => {
      item.addEventListener("click", async () => {
        const reportId = item.getAttribute("data-report-id");
        if (reportId) {
          modal.setContent(createLoadingContent());
          const report = await getReport(workspacePath, reportId);
          if (report) {
            currentReport = report;
            modal.setContent(createReportContent(report, workspacePath));
            setupInteractiveElements(modal, report);
          }
        }
      });
    });
  } catch (error) {
    modal.setContent(createErrorContent(error));
  }
}

// ============================================================================
// Settings View
// ============================================================================

/**
 * Show settings view
 */
async function showSettingsView(workspacePath: string, modal: ModalBuilder): Promise<void> {
  modal.setContent(createLoadingContent());

  try {
    const schedule = await getReportSchedule(workspacePath);

    const dayOptions = [0, 1, 2, 3, 4, 5, 6]
      .map(
        (d) => `
        <option value="${d}" ${schedule.dayOfWeek === d ? "selected" : ""}>${getDayName(d)}</option>
      `
      )
      .join("");

    const hourOptions = Array.from({ length: 24 }, (_, h) => h)
      .map(
        (h) => `
        <option value="${h}" ${schedule.hourOfDay === h ? "selected" : ""}>${formatHour(h)}</option>
      `
      )
      .join("");

    modal.setContent(`
      <div class="space-y-6">
        <div class="flex items-center justify-between border-b border-gray-200 dark:border-gray-700 pb-4">
          <h2 class="text-xl font-bold text-gray-800 dark:text-gray-200">Report Settings</h2>
          <button id="back-to-report-btn" class="text-sm text-blue-600 hover:text-blue-700 dark:text-blue-400">
            Back to Report
          </button>
        </div>

        <div class="space-y-4">
          <div class="flex items-center justify-between">
            <div>
              <div class="font-medium text-gray-800 dark:text-gray-200">Automatic Reports</div>
              <div class="text-sm text-gray-500 dark:text-gray-400">Generate weekly reports automatically</div>
            </div>
            <label class="relative inline-flex items-center cursor-pointer">
              <input type="checkbox" id="schedule-enabled" class="sr-only peer" ${schedule.enabled ? "checked" : ""}>
              <div class="w-11 h-6 bg-gray-200 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-blue-300 dark:peer-focus:ring-blue-800 rounded-full peer dark:bg-gray-700 peer-checked:after:translate-x-full rtl:peer-checked:after:-translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:start-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all dark:border-gray-600 peer-checked:bg-blue-600"></div>
            </label>
          </div>

          <div class="grid grid-cols-2 gap-4">
            <div>
              <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">Day of Week</label>
              <select id="schedule-day" class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-800 text-gray-800 dark:text-gray-200">
                ${dayOptions}
              </select>
            </div>
            <div>
              <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">Time</label>
              <select id="schedule-hour" class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-800 text-gray-800 dark:text-gray-200">
                ${hourOptions}
              </select>
            </div>
          </div>

          <button id="save-settings-btn" class="w-full py-2 bg-blue-600 hover:bg-blue-700 text-white rounded-lg font-medium transition-colors">
            Save Settings
          </button>
        </div>

        <div class="pt-4 border-t border-gray-200 dark:border-gray-700">
          <button id="generate-now-btn" class="w-full py-2 bg-gray-100 dark:bg-gray-800 hover:bg-gray-200 dark:hover:bg-gray-700 text-gray-800 dark:text-gray-200 rounded-lg font-medium transition-colors">
            Generate Report Now
          </button>
        </div>
      </div>
    `);

    // Set up event handlers
    const backBtn = modal.querySelector("#back-to-report-btn");
    if (backBtn) {
      backBtn.addEventListener("click", () => {
        if (currentReport) {
          modal.setContent(createReportContent(currentReport, workspacePath));
          setupInteractiveElements(modal, currentReport);
        }
      });
    }

    const saveBtn = modal.querySelector("#save-settings-btn");
    if (saveBtn) {
      saveBtn.addEventListener("click", async () => {
        const enabled = (modal.querySelector("#schedule-enabled") as HTMLInputElement)?.checked;
        const dayOfWeek = parseInt(
          (modal.querySelector("#schedule-day") as HTMLSelectElement)?.value,
          10
        );
        const hourOfDay = parseInt(
          (modal.querySelector("#schedule-hour") as HTMLSelectElement)?.value,
          10
        );

        const newSchedule: ReportSchedule = {
          enabled: enabled ?? true,
          dayOfWeek: Number.isNaN(dayOfWeek) ? 1 : dayOfWeek,
          hourOfDay: Number.isNaN(hourOfDay) ? 9 : hourOfDay,
          timezoneOffset: new Date().getTimezoneOffset(),
        };

        try {
          await saveReportSchedule(workspacePath, newSchedule);
          // Show success feedback
          if (saveBtn instanceof HTMLButtonElement) {
            saveBtn.textContent = "Saved!";
            setTimeout(() => {
              saveBtn.textContent = "Save Settings";
            }, 2000);
          }
        } catch (error) {
          logger.error("Failed to save settings", error as Error);
        }
      });
    }

    const generateBtn = modal.querySelector("#generate-now-btn");
    if (generateBtn) {
      generateBtn.addEventListener("click", async () => {
        modal.setContent(createLoadingContent());
        try {
          const report = await generateWeeklyReport(workspacePath);
          currentReport = report;
          modal.setContent(createReportContent(report, workspacePath));
          setupInteractiveElements(modal, report);
        } catch (error) {
          modal.setContent(createErrorContent(error));
        }
      });
    }
  } catch (error) {
    modal.setContent(createErrorContent(error));
  }
}

// ============================================================================
// Interactive Elements
// ============================================================================

/**
 * Set up interactive elements in the report view
 */
function setupInteractiveElements(modal: ModalBuilder, _report: WeeklyReport): void {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  // Regenerate button
  const regenerateBtn = modal.querySelector("#regenerate-report-btn");
  if (regenerateBtn && workspacePath) {
    regenerateBtn.addEventListener("click", async () => {
      modal.setContent(createLoadingContent());
      try {
        const newReport = await generateWeeklyReport(workspacePath);
        currentReport = newReport;
        modal.setContent(createReportContent(newReport, workspacePath));
        setupInteractiveElements(modal, newReport);
      } catch (error) {
        modal.setContent(createErrorContent(error));
      }
    });
  }
}

/**
 * Handle export button click
 */
function handleExport(report: WeeklyReport): void {
  const markdown = exportReportAsMarkdown(report);

  // Create a blob and download
  const blob = new Blob([markdown], { type: "text/markdown" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `weekly-report-${report.week_start}.md`;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

// ============================================================================
// Utility Functions
// ============================================================================

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

/**
 * Close the modal if it's open
 */
export function closeWeeklyReportModal(): void {
  if (currentModal?.isVisible()) {
    currentModal.close();
  }
}

/**
 * Check if the modal is currently visible
 */
export function isModalVisible(): boolean {
  return currentModal?.isVisible() ?? false;
}
