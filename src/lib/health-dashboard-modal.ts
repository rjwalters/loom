/**
 * Health Dashboard Modal
 *
 * Displays system health metrics including health score, alerts,
 * throughput trends, and queue depths. Provides proactive monitoring
 * for extended unattended autonomous operation.
 */

import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import { invoke } from "@tauri-apps/api/core";

// Types for health metrics
interface HealthMetrics {
  health_score: number;
  health_status: string;
  last_updated: string;
  metric_count: number;
  retention_hours: number;
  latest_metrics: LatestMetrics | null;
}

interface LatestMetrics {
  timestamp: string;
  throughput: {
    issues_per_hour: number;
    prs_per_hour: number;
  };
  latency: {
    avg_iteration_seconds: number;
  };
  queue_depths: {
    ready: number;
    building: number;
    review_requested: number;
    changes_requested: number;
    ready_to_merge: number;
  };
  error_rates: {
    consecutive_failures: number;
    success_rate: number;
    stuck_agents: number;
  };
  resource_usage: {
    active_shepherds: number;
    session_percent: number;
  };
}

interface Alert {
  id: string;
  type: string;
  severity: string;
  message: string;
  timestamp: string;
  acknowledged: boolean;
  context?: Record<string, unknown>;
}

interface AlertsResponse {
  active_count: number;
  total_count: number;
  alerts: Alert[];
}

interface HistoryMetric {
  timestamp: string;
  throughput?: { issues_per_hour: number; prs_per_hour: number };
  queue_depths?: { ready: number; building: number };
  error_rates?: { success_rate: number; stuck_agents: number };
}

interface HealthHistory {
  hours_requested: number;
  metric_count: number;
  metrics: HistoryMetric[];
  current_score: number;
  current_status: string;
}

// Track current view
let currentView: "overview" | "alerts" | "history" = "overview";

/**
 * Show the health dashboard modal
 */
export async function showHealthDashboardModal(): Promise<void> {
  const modal = new ModalBuilder({
    title: "System Health",
    width: "800px",
    maxHeight: "85vh",
    id: "health-dashboard-modal",
  });

  // Show loading state initially
  modal.setContent(createLoadingContent());

  // Add footer buttons
  modal.addFooterButton("Refresh", async () => {
    modal.setContent(createLoadingContent());
    await refreshContent(modal);
  });
  modal.addFooterButton("Close", () => modal.close(), "primary");

  modal.show();

  // Load and display health data
  await refreshContent(modal);
}

/**
 * Refresh the modal content based on current view
 */
async function refreshContent(modal: ModalBuilder): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    switch (currentView) {
      case "alerts":
        await showAlertsView(modal, workspacePath);
        break;
      case "history":
        await showHistoryView(modal, workspacePath);
        break;
      default:
        await showOverviewView(modal, workspacePath);
    }
    setupNavHandlers(modal);
  } catch (error) {
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Show the overview view with health score and current metrics
 */
async function showOverviewView(modal: ModalBuilder, workspacePath: string): Promise<void> {
  const [healthMetrics, alerts] = await Promise.all([
    getHealthMetrics(workspacePath),
    getActiveAlerts(workspacePath),
  ]);

  modal.setContent(createOverviewContent(healthMetrics, alerts));
}

/**
 * Show the alerts view
 */
async function showAlertsView(modal: ModalBuilder, workspacePath: string): Promise<void> {
  const alerts = await getActiveAlerts(workspacePath);
  modal.setContent(createAlertsContent(alerts));
  setupAlertHandlers(modal, workspacePath);
}

/**
 * Show the history view with trend data
 */
async function showHistoryView(modal: ModalBuilder, workspacePath: string): Promise<void> {
  const history = await getHealthHistory(workspacePath, 4);
  modal.setContent(createHistoryContent(history));
}

/**
 * Get health metrics from CLI
 */
async function getHealthMetrics(workspacePath: string): Promise<HealthMetrics> {
  try {
    const result = await invoke<string>("run_script", {
      workspacePath,
      scriptPath: ".loom/scripts/health-check.sh",
      args: ["--json"],
    });
    return JSON.parse(result);
  } catch {
    return {
      health_score: 0,
      health_status: "unknown",
      last_updated: "never",
      metric_count: 0,
      retention_hours: 24,
      latest_metrics: null,
    };
  }
}

/**
 * Get active alerts from CLI
 */
async function getActiveAlerts(workspacePath: string): Promise<AlertsResponse> {
  try {
    const result = await invoke<string>("run_script", {
      workspacePath,
      scriptPath: ".loom/scripts/health-check.sh",
      args: ["--alerts", "--json"],
    });
    return JSON.parse(result);
  } catch {
    return { active_count: 0, total_count: 0, alerts: [] };
  }
}

/**
 * Get health history from CLI
 */
async function getHealthHistory(workspacePath: string, hours: number): Promise<HealthHistory> {
  try {
    const result = await invoke<string>("run_script", {
      workspacePath,
      scriptPath: ".loom/scripts/health-check.sh",
      args: ["--history", String(hours), "--json"],
    });
    return JSON.parse(result);
  } catch {
    return {
      hours_requested: hours,
      metric_count: 0,
      metrics: [],
      current_score: 0,
      current_status: "unknown",
    };
  }
}

/**
 * Acknowledge an alert
 */
async function acknowledgeAlert(workspacePath: string, alertId: string): Promise<void> {
  await invoke<string>("run_script", {
    workspacePath,
    scriptPath: ".loom/scripts/health-check.sh",
    args: ["--acknowledge", alertId],
  });
}

/**
 * Create loading content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-12">
      <div class="text-gray-500 dark:text-gray-400">Loading health data...</div>
    </div>
  `;
}

/**
 * Create error content
 */
function createErrorContent(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return `
    <div class="p-4 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
      <p class="text-red-700 dark:text-red-300">Failed to load health data: ${escapeHtml(message)}</p>
    </div>
  `;
}

/**
 * Create navigation tabs
 */
function createNavTabs(): string {
  const tabs = [
    { id: "overview", label: "Overview" },
    { id: "alerts", label: "Alerts" },
    { id: "history", label: "History" },
  ];

  return `
    <div class="flex gap-2 mb-6 border-b border-gray-200 dark:border-gray-700">
      ${tabs
        .map(
          (tab) => `
        <button
          class="nav-tab px-4 py-2 text-sm font-medium transition-colors ${
            currentView === tab.id
              ? "text-blue-600 border-b-2 border-blue-600"
              : "text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300"
          }"
          data-view="${tab.id}"
        >
          ${tab.label}
        </button>
      `
        )
        .join("")}
    </div>
  `;
}

/**
 * Create overview content
 */
function createOverviewContent(metrics: HealthMetrics, alerts: AlertsResponse): string {
  const scoreColor = getScoreColor(metrics.health_score);
  const statusColor = getStatusColor(metrics.health_status);

  return `
    ${createNavTabs()}

    <!-- Health Score Card -->
    <div class="mb-6 p-6 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="flex items-center justify-between">
        <div>
          <div class="text-sm text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-1">Health Score</div>
          <div class="flex items-baseline gap-2">
            <span class="text-4xl font-bold ${scoreColor}">${metrics.health_score}</span>
            <span class="text-xl text-gray-400">/100</span>
          </div>
          <div class="mt-2">
            <span class="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${statusColor}">
              ${capitalizeFirst(metrics.health_status)}
            </span>
          </div>
        </div>
        <div class="text-right">
          <div class="text-sm text-gray-500 dark:text-gray-400">Last Updated</div>
          <div class="text-sm text-gray-700 dark:text-gray-300">${formatTimestamp(metrics.last_updated)}</div>
          <div class="text-xs text-gray-400 mt-1">${metrics.metric_count} samples stored</div>
        </div>
      </div>
      ${createHealthGauge(metrics.health_score)}
    </div>

    <!-- Alert Summary -->
    ${
      alerts.active_count > 0
        ? `
      <div class="mb-6 p-4 bg-yellow-50 dark:bg-yellow-900/20 border border-yellow-200 dark:border-yellow-800 rounded-lg">
        <div class="flex items-center gap-2">
          <span class="text-yellow-600 dark:text-yellow-400 text-lg">&#9888;</span>
          <span class="font-medium text-yellow-800 dark:text-yellow-200">${alerts.active_count} active alert(s)</span>
          <button class="nav-tab ml-auto text-sm text-yellow-600 dark:text-yellow-400 hover:underline" data-view="alerts">
            View Alerts
          </button>
        </div>
      </div>
    `
        : ""
    }

    <!-- Current Metrics Grid -->
    ${
      metrics.latest_metrics
        ? `
      <div class="grid grid-cols-2 md:grid-cols-3 gap-4">
        ${createMetricCard("Issues/Hour", String(metrics.latest_metrics.throughput.issues_per_hour), "Throughput")}
        ${createMetricCard("PRs/Hour", String(metrics.latest_metrics.throughput.prs_per_hour), "Throughput")}
        ${createMetricCard("Success Rate", `${metrics.latest_metrics.error_rates.success_rate}%`, "Reliability", getSuccessColor(metrics.latest_metrics.error_rates.success_rate))}
        ${createMetricCard("Ready Queue", String(metrics.latest_metrics.queue_depths.ready), "Queue Depth")}
        ${createMetricCard("Building", String(metrics.latest_metrics.queue_depths.building), "Queue Depth")}
        ${createMetricCard("Stuck Agents", String(metrics.latest_metrics.error_rates.stuck_agents), "Health", metrics.latest_metrics.error_rates.stuck_agents > 0 ? "text-red-600 dark:text-red-400" : "")}
        ${createMetricCard("Active Shepherds", String(metrics.latest_metrics.resource_usage.active_shepherds), "Resources")}
        ${createMetricCard("Session Used", `${metrics.latest_metrics.resource_usage.session_percent}%`, "Resources", getSessionColor(metrics.latest_metrics.resource_usage.session_percent))}
        ${createMetricCard("Avg Iteration", `${metrics.latest_metrics.latency.avg_iteration_seconds}s`, "Latency")}
      </div>
    `
        : `
      <div class="text-center py-8 text-gray-500 dark:text-gray-400">
        <p class="mb-2">No metrics collected yet.</p>
        <p class="text-sm">Run <code class="bg-gray-100 dark:bg-gray-700 px-1 rounded">health-check.sh --collect</code> to start collecting metrics.</p>
      </div>
    `
    }
  `;
}

/**
 * Create alerts content
 */
function createAlertsContent(alerts: AlertsResponse): string {
  return `
    ${createNavTabs()}

    <div class="mb-4 flex justify-between items-center">
      <h3 class="text-lg font-semibold text-gray-800 dark:text-gray-200">
        Active Alerts (${alerts.active_count})
      </h3>
      <span class="text-sm text-gray-500 dark:text-gray-400">
        ${alerts.total_count} total
      </span>
    </div>

    ${
      alerts.alerts.length > 0
        ? `
      <div class="space-y-3">
        ${alerts.alerts
          .filter((a) => !a.acknowledged)
          .map((alert) => createAlertCard(alert))
          .join("")}
      </div>
    `
        : `
      <div class="text-center py-8 text-gray-500 dark:text-gray-400">
        <p class="text-green-600 dark:text-green-400 text-lg mb-2">&#10003; No active alerts</p>
        <p class="text-sm">System is operating normally.</p>
      </div>
    `
    }
  `;
}

/**
 * Create an alert card
 */
function createAlertCard(alert: Alert): string {
  const severityColor = getSeverityColor(alert.severity);
  const typeIcon = getAlertTypeIcon(alert.type);

  return `
    <div class="p-4 rounded-lg border ${severityColor.border} ${severityColor.bg}">
      <div class="flex items-start justify-between">
        <div class="flex items-start gap-3">
          <span class="text-lg">${typeIcon}</span>
          <div>
            <div class="font-medium ${severityColor.text}">${escapeHtml(alert.message)}</div>
            <div class="text-sm text-gray-500 dark:text-gray-400 mt-1">
              ${formatTimestamp(alert.timestamp)} - ${alert.type}
            </div>
          </div>
        </div>
        <button
          class="ack-alert-btn px-3 py-1 text-sm bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 rounded transition-colors"
          data-alert-id="${alert.id}"
        >
          Acknowledge
        </button>
      </div>
    </div>
  `;
}

/**
 * Create history content
 */
function createHistoryContent(history: HealthHistory): string {
  return `
    ${createNavTabs()}

    <div class="mb-4">
      <h3 class="text-lg font-semibold text-gray-800 dark:text-gray-200">
        Metric History (Last ${history.hours_requested} hours)
      </h3>
      <p class="text-sm text-gray-500 dark:text-gray-400">
        ${history.metric_count} data points
      </p>
    </div>

    ${
      history.metrics.length > 0
        ? `
      <!-- Trend Sparklines -->
      <div class="mb-6 grid grid-cols-2 gap-4">
        ${createSparklineCard("Throughput", history.metrics.map((m) => m.throughput?.issues_per_hour ?? 0), "issues/hr")}
        ${createSparklineCard("Queue Depth", history.metrics.map((m) => m.queue_depths?.ready ?? 0), "ready")}
        ${createSparklineCard("Success Rate", history.metrics.map((m) => m.error_rates?.success_rate ?? 100), "%")}
        ${createSparklineCard("Stuck Agents", history.metrics.map((m) => m.error_rates?.stuck_agents ?? 0), "agents")}
      </div>

      <!-- History Table -->
      <div class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-gray-200 dark:border-gray-700">
              <th class="text-left py-2 font-medium text-gray-700 dark:text-gray-300">Time</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Ready</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Building</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Success</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Stuck</th>
            </tr>
          </thead>
          <tbody>
            ${history.metrics
              .slice(-10)
              .reverse()
              .map(
                (m) => `
              <tr class="border-b border-gray-100 dark:border-gray-800">
                <td class="py-2 text-gray-600 dark:text-gray-400">${formatTimestamp(m.timestamp)}</td>
                <td class="text-right py-2 text-gray-600 dark:text-gray-400">${m.queue_depths?.ready ?? "-"}</td>
                <td class="text-right py-2 text-gray-600 dark:text-gray-400">${m.queue_depths?.building ?? "-"}</td>
                <td class="text-right py-2 text-gray-600 dark:text-gray-400">${m.error_rates?.success_rate ?? "-"}%</td>
                <td class="text-right py-2 ${(m.error_rates?.stuck_agents ?? 0) > 0 ? "text-red-600 dark:text-red-400" : "text-gray-600 dark:text-gray-400"}">${m.error_rates?.stuck_agents ?? "-"}</td>
              </tr>
            `
              )
              .join("")}
          </tbody>
        </table>
      </div>
    `
        : `
      <div class="text-center py-8 text-gray-500 dark:text-gray-400">
        <p class="mb-2">No history available.</p>
        <p class="text-sm">Metrics will appear here as they are collected.</p>
      </div>
    `
    }
  `;
}

/**
 * Create a metric card
 */
function createMetricCard(
  label: string,
  value: string,
  category: string,
  valueColor?: string
): string {
  const colorClass = valueColor ?? "text-gray-900 dark:text-gray-100";

  return `
    <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-1">${label}</div>
      <div class="text-2xl font-bold ${colorClass}">${value}</div>
      <div class="text-xs text-gray-400 dark:text-gray-500 mt-1">${category}</div>
    </div>
  `;
}

/**
 * Create a visual health gauge
 */
function createHealthGauge(score: number): string {
  const percentage = Math.max(0, Math.min(100, score));

  return `
    <div class="mt-4">
      <div class="h-2 bg-gray-200 dark:bg-gray-700 rounded-full overflow-hidden">
        <div
          class="h-full ${getGaugeColor(score)} transition-all duration-500"
          style="width: ${percentage}%"
        ></div>
      </div>
      <div class="flex justify-between mt-1 text-xs text-gray-400">
        <span>Critical</span>
        <span>Warning</span>
        <span>Good</span>
        <span>Excellent</span>
      </div>
    </div>
  `;
}

/**
 * Create a sparkline card
 */
function createSparklineCard(label: string, values: number[], unit: string): string {
  if (values.length === 0) {
    return `
      <div class="p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <div class="text-xs text-gray-500 dark:text-gray-400 mb-1">${label}</div>
        <div class="text-sm text-gray-400">No data</div>
      </div>
    `;
  }

  const max = Math.max(...values, 1);
  const min = Math.min(...values);
  const latest = values[values.length - 1];
  const trend = values.length > 1 ? latest - values[values.length - 2] : 0;
  const trendIcon = trend > 0 ? "&#9650;" : trend < 0 ? "&#9660;" : "&#8212;";
  const trendColor = getTrendColor(label, trend);

  // Create SVG sparkline
  const width = 120;
  const height = 30;
  const points = values
    .map((v, i) => {
      const x = (i / Math.max(values.length - 1, 1)) * width;
      const y = height - ((v - min) / Math.max(max - min, 1)) * height;
      return `${x},${y}`;
    })
    .join(" ");

  return `
    <div class="p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="flex justify-between items-start mb-2">
        <div>
          <div class="text-xs text-gray-500 dark:text-gray-400">${label}</div>
          <div class="text-lg font-semibold text-gray-800 dark:text-gray-200">${latest} <span class="text-xs text-gray-400">${unit}</span></div>
        </div>
        <span class="text-sm ${trendColor}">${trendIcon}</span>
      </div>
      <svg width="${width}" height="${height}" class="w-full">
        <polyline
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          class="text-blue-500"
          points="${points}"
        />
      </svg>
    </div>
  `;
}

// Helper functions

function setupNavHandlers(modal: ModalBuilder): void {
  const tabs = modal.querySelectorAll<HTMLButtonElement>(".nav-tab");
  tabs.forEach((tab) => {
    tab.addEventListener("click", async () => {
      const view = tab.dataset.view as "overview" | "alerts" | "history";
      if (view && view !== currentView) {
        currentView = view;
        modal.setContent(createLoadingContent());
        await refreshContent(modal);
      }
    });
  });
}

function setupAlertHandlers(modal: ModalBuilder, workspacePath: string): void {
  const buttons = modal.querySelectorAll<HTMLButtonElement>(".ack-alert-btn");
  buttons.forEach((btn) => {
    btn.addEventListener("click", async () => {
      const alertId = btn.dataset.alertId;
      if (alertId) {
        btn.disabled = true;
        btn.textContent = "...";
        try {
          await acknowledgeAlert(workspacePath, alertId);
          modal.setContent(createLoadingContent());
          await refreshContent(modal);
        } catch {
          btn.disabled = false;
          btn.textContent = "Failed";
        }
      }
    });
  });
}

function getScoreColor(score: number): string {
  if (score >= 90) return "text-green-600 dark:text-green-400";
  if (score >= 70) return "text-green-500 dark:text-green-500";
  if (score >= 50) return "text-yellow-600 dark:text-yellow-400";
  if (score >= 30) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}

function getStatusColor(status: string): string {
  switch (status) {
    case "excellent":
      return "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400";
    case "good":
      return "bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-500";
    case "fair":
      return "bg-yellow-100 text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400";
    case "warning":
      return "bg-orange-100 text-orange-800 dark:bg-orange-900/30 dark:text-orange-400";
    case "critical":
      return "bg-red-100 text-red-800 dark:bg-red-900/30 dark:text-red-400";
    default:
      return "bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400";
  }
}

function getGaugeColor(score: number): string {
  if (score >= 90) return "bg-green-500";
  if (score >= 70) return "bg-green-400";
  if (score >= 50) return "bg-yellow-500";
  if (score >= 30) return "bg-orange-500";
  return "bg-red-500";
}

function getSuccessColor(rate: number): string {
  if (rate >= 90) return "text-green-600 dark:text-green-400";
  if (rate >= 70) return "text-yellow-600 dark:text-yellow-400";
  return "text-red-600 dark:text-red-400";
}

function getSessionColor(percent: number): string {
  if (percent >= 95) return "text-red-600 dark:text-red-400";
  if (percent >= 80) return "text-yellow-600 dark:text-yellow-400";
  return "";
}

function getSeverityColor(severity: string): {
  bg: string;
  border: string;
  text: string;
} {
  switch (severity) {
    case "critical":
      return {
        bg: "bg-red-50 dark:bg-red-900/20",
        border: "border-red-200 dark:border-red-800",
        text: "text-red-800 dark:text-red-200",
      };
    case "warning":
      return {
        bg: "bg-yellow-50 dark:bg-yellow-900/20",
        border: "border-yellow-200 dark:border-yellow-800",
        text: "text-yellow-800 dark:text-yellow-200",
      };
    default:
      return {
        bg: "bg-blue-50 dark:bg-blue-900/20",
        border: "border-blue-200 dark:border-blue-800",
        text: "text-blue-800 dark:text-blue-200",
      };
  }
}

function getAlertTypeIcon(type: string): string {
  switch (type) {
    case "stuck_agents":
      return "&#128721;"; // Stop sign
    case "high_error_rate":
      return "&#9888;"; // Warning
    case "resource_exhaustion":
      return "&#128200;"; // Chart
    case "queue_growth":
      return "&#128202;"; // Chart increasing
    case "throughput_decline":
      return "&#128201;"; // Chart decreasing
    default:
      return "&#9432;"; // Info
  }
}

function getTrendColor(label: string, trend: number): string {
  // For some metrics, down is good (stuck agents, queue depth)
  const downIsGood = ["Stuck Agents", "Queue Depth"].includes(label);

  if (trend === 0) return "text-gray-400";
  if ((trend > 0 && !downIsGood) || (trend < 0 && downIsGood)) {
    return "text-green-500";
  }
  return "text-red-500";
}

function formatTimestamp(timestamp: string): string {
  if (!timestamp || timestamp === "never") return "Never";
  try {
    const date = new Date(timestamp);
    return date.toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
    });
  } catch {
    return timestamp;
  }
}

function capitalizeFirst(str: string): string {
  return str.charAt(0).toUpperCase() + str.slice(1);
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
