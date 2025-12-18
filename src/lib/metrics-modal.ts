/**
 * Metrics Modal
 *
 * Displays telemetry data including performance metrics, error reports, and usage analytics.
 * All data shown is locally stored - no external services are used.
 */

import { ModalBuilder } from "./modal-builder";
import {
  deleteTelemetryData,
  type ErrorReport,
  exportTelemetryData,
  getErrorReports,
  getPerformanceStats,
  getTelemetrySettings,
  getUsageStats,
  type PerformanceStats,
  setTelemetrySettings,
  type UsageStats,
} from "./telemetry";
import { showToast } from "./toast";

/**
 * Show the metrics modal with telemetry data
 */
export async function showMetricsModal(): Promise<void> {
  const modal = new ModalBuilder({
    title: "Telemetry & Metrics",
    width: "800px",
    maxHeight: "85vh",
    id: "metrics-modal",
  });

  // Show loading state initially
  modal.setContent(createLoadingContent());

  // Add footer buttons
  modal.addFooterButton("Export Data", handleExportData);
  modal.addFooterButton("Delete All Data", handleDeleteData, "danger");
  modal.addFooterButton("Close", () => modal.close(), "primary");

  modal.show();

  // Load data and update content
  try {
    const [perfStats, errors, usageStats] = await Promise.all([
      getPerformanceStats(),
      getErrorReports(undefined, 20),
      getUsageStats(),
    ]);

    modal.setContent(createMetricsContent(perfStats, errors, usageStats));
    setupSettingsHandlers(modal);
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
function createMetricsContent(
  perfStats: PerformanceStats[],
  errors: ErrorReport[],
  usageStats: UsageStats[]
): string {
  const settings = getTelemetrySettings();

  return `
    <!-- Settings Section -->
    <div class="mb-6 p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">Telemetry Settings</h3>
      <p class="text-sm text-gray-600 dark:text-gray-400 mb-4">
        All telemetry data is stored locally on your machine. No data is sent to external services.
      </p>
      <div class="space-y-2">
        ${createSettingToggle("Performance Monitoring", "performance", settings.performanceEnabled, "Track operation timing to identify bottlenecks")}
        ${createSettingToggle("Error Tracking", "errorTracking", settings.errorTrackingEnabled, "Capture errors for debugging")}
        ${createSettingToggle("Usage Analytics", "usageAnalytics", settings.usageAnalyticsEnabled, "Track feature usage patterns")}
      </div>
    </div>

    <!-- Tabs -->
    <div class="border-b border-gray-200 dark:border-gray-700 mb-4">
      <nav class="flex space-x-4" role="tablist">
        <button role="tab" data-tab="performance" class="metrics-tab px-3 py-2 text-sm font-medium border-b-2 border-blue-500 text-blue-600 dark:text-blue-400">
          Performance
        </button>
        <button role="tab" data-tab="errors" class="metrics-tab px-3 py-2 text-sm font-medium border-b-2 border-transparent text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300">
          Errors (${errors.length})
        </button>
        <button role="tab" data-tab="usage" class="metrics-tab px-3 py-2 text-sm font-medium border-b-2 border-transparent text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300">
          Usage
        </button>
      </nav>
    </div>

    <!-- Tab Content -->
    <div id="metrics-tab-content">
      ${createPerformanceTab(perfStats)}
    </div>

    <!-- Hidden tabs for dynamic loading -->
    <template id="errors-tab-content">
      ${createErrorsTab(errors)}
    </template>
    <template id="usage-tab-content">
      ${createUsageTab(usageStats)}
    </template>
  `;
}

/**
 * Create a settings toggle
 */
function createSettingToggle(
  label: string,
  id: string,
  enabled: boolean,
  description: string
): string {
  return `
    <label class="flex items-start gap-3 cursor-pointer">
      <input
        type="checkbox"
        id="telemetry-${id}"
        class="telemetry-setting mt-1 h-4 w-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500"
        ${enabled ? "checked" : ""}
      />
      <div class="flex-1">
        <span class="text-gray-700 dark:text-gray-300">${label}</span>
        <p class="text-xs text-gray-500 dark:text-gray-400">${description}</p>
      </div>
    </label>
  `;
}

/**
 * Create performance tab content
 */
function createPerformanceTab(stats: PerformanceStats[]): string {
  if (stats.length === 0) {
    return `
      <div class="text-center py-8 text-gray-500 dark:text-gray-400">
        No performance data collected yet. Use the app to generate metrics.
      </div>
    `;
  }

  return `
    <div class="space-y-4">
      <h4 class="text-sm font-medium text-gray-500 dark:text-gray-400 uppercase">Performance by Category</h4>
      <div class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-gray-200 dark:border-gray-700">
              <th class="text-left py-2 font-medium text-gray-700 dark:text-gray-300">Category</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Count</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Avg (ms)</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Max (ms)</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Success Rate</th>
            </tr>
          </thead>
          <tbody>
            ${stats.map(createPerformanceRow).join("")}
          </tbody>
        </table>
      </div>
    </div>
  `;
}

/**
 * Create a performance stats row
 */
function createPerformanceRow(stat: PerformanceStats): string {
  const successRateClass =
    stat.success_rate >= 0.95
      ? "text-green-600 dark:text-green-400"
      : stat.success_rate >= 0.8
        ? "text-yellow-600 dark:text-yellow-400"
        : "text-red-600 dark:text-red-400";

  return `
    <tr class="border-b border-gray-100 dark:border-gray-800">
      <td class="py-2 text-gray-800 dark:text-gray-200">${escapeHtml(stat.category)}</td>
      <td class="text-right py-2 text-gray-600 dark:text-gray-400">${stat.count}</td>
      <td class="text-right py-2 text-gray-600 dark:text-gray-400">${stat.avg_duration_ms.toFixed(1)}</td>
      <td class="text-right py-2 ${stat.max_duration_ms > 1000 ? "text-orange-600 dark:text-orange-400" : "text-gray-600 dark:text-gray-400"}">${stat.max_duration_ms.toFixed(1)}</td>
      <td class="text-right py-2 ${successRateClass}">${(stat.success_rate * 100).toFixed(1)}%</td>
    </tr>
  `;
}

/**
 * Create errors tab content
 */
function createErrorsTab(errors: ErrorReport[]): string {
  if (errors.length === 0) {
    return `
      <div class="text-center py-8 text-gray-500 dark:text-gray-400">
        No errors recorded. That's good news!
      </div>
    `;
  }

  return `
    <div class="space-y-3">
      ${errors.map(createErrorRow).join("")}
    </div>
  `;
}

/**
 * Create an error report row
 */
function createErrorRow(error: ErrorReport): string {
  const timestamp = new Date(error.timestamp).toLocaleString();
  return `
    <div class="p-3 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
      <div class="flex justify-between items-start mb-1">
        <span class="text-sm font-medium text-red-800 dark:text-red-300">${escapeHtml(error.component)}</span>
        <span class="text-xs text-red-600 dark:text-red-400">${timestamp}</span>
      </div>
      <p class="text-sm text-red-700 dark:text-red-300">${escapeHtml(error.message)}</p>
      ${error.stack ? `<details class="mt-2"><summary class="text-xs text-red-600 dark:text-red-400 cursor-pointer">Stack trace</summary><pre class="mt-1 text-xs text-red-500 dark:text-red-400 overflow-x-auto">${escapeHtml(error.stack)}</pre></details>` : ""}
    </div>
  `;
}

/**
 * Create usage tab content
 */
function createUsageTab(stats: UsageStats[]): string {
  if (stats.length === 0) {
    return `
      <div class="text-center py-8 text-gray-500 dark:text-gray-400">
        No usage data collected yet. Use the app to generate analytics.
      </div>
    `;
  }

  return `
    <div class="space-y-4">
      <h4 class="text-sm font-medium text-gray-500 dark:text-gray-400 uppercase">Feature Usage</h4>
      <div class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-gray-200 dark:border-gray-700">
              <th class="text-left py-2 font-medium text-gray-700 dark:text-gray-300">Event</th>
              <th class="text-left py-2 font-medium text-gray-700 dark:text-gray-300">Category</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Count</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Last Used</th>
            </tr>
          </thead>
          <tbody>
            ${stats.map(createUsageRow).join("")}
          </tbody>
        </table>
      </div>
    </div>
  `;
}

/**
 * Create a usage stats row
 */
function createUsageRow(stat: UsageStats): string {
  const lastOccurrence = new Date(stat.last_occurrence).toLocaleString();
  const categoryBadgeClass =
    stat.category === "feature"
      ? "bg-blue-100 dark:bg-blue-900 text-blue-800 dark:text-blue-200"
      : stat.category === "workflow"
        ? "bg-green-100 dark:bg-green-900 text-green-800 dark:text-green-200"
        : "bg-gray-100 dark:bg-gray-700 text-gray-800 dark:text-gray-200";

  return `
    <tr class="border-b border-gray-100 dark:border-gray-800">
      <td class="py-2 text-gray-800 dark:text-gray-200">${escapeHtml(stat.event_name)}</td>
      <td class="py-2">
        <span class="px-2 py-0.5 rounded text-xs ${categoryBadgeClass}">${escapeHtml(stat.category)}</span>
      </td>
      <td class="text-right py-2 text-gray-600 dark:text-gray-400">${stat.count}</td>
      <td class="text-right py-2 text-gray-500 dark:text-gray-400 text-xs">${lastOccurrence}</td>
    </tr>
  `;
}

/**
 * Set up settings toggle handlers
 */
function setupSettingsHandlers(modal: ModalBuilder): void {
  // Tab switching
  const tabs = modal.querySelectorAll<HTMLButtonElement>(".metrics-tab");
  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      const tabId = tab.dataset.tab;
      if (!tabId) return;

      // Update tab styles
      tabs.forEach((t) => {
        t.classList.remove("border-blue-500", "text-blue-600", "dark:text-blue-400");
        t.classList.add("border-transparent", "text-gray-500", "dark:text-gray-400");
      });
      tab.classList.remove("border-transparent", "text-gray-500", "dark:text-gray-400");
      tab.classList.add("border-blue-500", "text-blue-600", "dark:text-blue-400");

      // Update content
      const contentContainer = modal.querySelector("#metrics-tab-content");
      const template = modal.querySelector<HTMLTemplateElement>(`#${tabId}-tab-content`);
      if (contentContainer && template) {
        contentContainer.innerHTML = template.innerHTML;
      }
    });
  });

  // Settings toggles
  const performanceToggle = modal.querySelector<HTMLInputElement>("#telemetry-performance");
  const errorToggle = modal.querySelector<HTMLInputElement>("#telemetry-errorTracking");
  const usageToggle = modal.querySelector<HTMLInputElement>("#telemetry-usageAnalytics");

  performanceToggle?.addEventListener("change", () => {
    setTelemetrySettings({ performanceEnabled: performanceToggle.checked });
  });

  errorToggle?.addEventListener("change", () => {
    setTelemetrySettings({ errorTrackingEnabled: errorToggle.checked });
  });

  usageToggle?.addEventListener("change", () => {
    setTelemetrySettings({ usageAnalyticsEnabled: usageToggle.checked });
  });
}

/**
 * Handle export data button click
 */
async function handleExportData(): Promise<void> {
  try {
    const data = await exportTelemetryData();
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `loom-telemetry-${new Date().toISOString().split("T")[0]}.json`;
    a.click();
    URL.revokeObjectURL(url);
    showToast("Telemetry data exported successfully", "success");
  } catch (error) {
    showToast(`Failed to export data: ${error}`, "error");
  }
}

/**
 * Handle delete data button click
 */
async function handleDeleteData(): Promise<void> {
  if (
    !confirm("Are you sure you want to delete all telemetry data? This action cannot be undone.")
  ) {
    return;
  }

  try {
    await deleteTelemetryData();
    showToast("Telemetry data deleted", "success");
    // Refresh the modal
    showMetricsModal();
  } catch (error) {
    showToast(`Failed to delete data: ${error}`, "error");
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
