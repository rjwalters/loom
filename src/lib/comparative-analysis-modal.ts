/**
 * Comparative Analysis Modal
 *
 * Displays A/B experiment results with statistical analysis, side-by-side
 * comparisons, and winner recommendations. Part of Phase 4 (Advanced Analytics).
 *
 * Features:
 * - List all A/B experiments with status
 * - View experiment results with metrics per variant
 * - Statistical significance indicators (p-value, confidence)
 * - Side-by-side comparison tables
 * - Bar charts comparing key metrics
 * - Winner recommendations with explanations
 * - Create new experiments from UI
 * - Export results as CSV/JSON
 *
 * @see Issue #1113 - Add Comparative Analysis UI for experiments
 * @see Issue #1071 - A/B testing framework
 */

import {
  addVariant,
  analyzeExperiment,
  cancelExperiment,
  concludeExperiment,
  createExperiment,
  type Experiment,
  type ExperimentAnalysis,
  type ExperimentStatus,
  type ExperimentsSummary,
  formatConfidence,
  formatEffectSize,
  formatMetricValue,
  formatPValue,
  formatStatus,
  formatSuccessRate,
  getDirectionIndicator,
  getExperiments,
  getExperimentsSummary,
  getMetricLabel,
  getPValueColor,
  getStatusColor,
  getVariants,
  isSignificant,
  startExperiment,
  type TargetDirection,
  type TargetMetric,
  type Variant,
  type VariantStats,
} from "./ab-testing";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import { showToast } from "./toast";

// ============================================================================
// Types
// ============================================================================

interface ViewState {
  currentView: "list" | "detail" | "create";
  selectedExperimentId: number | null;
  statusFilter: ExperimentStatus | "all";
}

// ============================================================================
// State
// ============================================================================

let viewState: ViewState = {
  currentView: "list",
  selectedExperimentId: null,
  statusFilter: "all",
};

// ============================================================================
// Main Entry Point
// ============================================================================

/**
 * Show the Comparative Analysis modal
 */
export async function showComparativeAnalysisModal(): Promise<void> {
  // Reset view state
  viewState = {
    currentView: "list",
    selectedExperimentId: null,
    statusFilter: "all",
  };

  const modal = new ModalBuilder({
    title: "Comparative Analysis",
    width: "900px",
    maxHeight: "90vh",
    id: "comparative-analysis-modal",
  });

  // Show loading state initially
  modal.setContent(createLoadingContent());

  // Add footer buttons
  modal.addFooterButton("Close", () => modal.close(), "primary");

  modal.show();

  // Load and display experiments
  await refreshContent(modal);
}

// ============================================================================
// Content Rendering
// ============================================================================

/**
 * Refresh the modal content based on current view state
 */
async function refreshContent(modal: ModalBuilder): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    switch (viewState.currentView) {
      case "list":
        await renderExperimentList(modal, workspacePath);
        break;
      case "detail":
        if (viewState.selectedExperimentId) {
          await renderExperimentDetail(modal, workspacePath, viewState.selectedExperimentId);
        }
        break;
      case "create":
        renderCreateExperiment(modal, workspacePath);
        break;
    }
  } catch (error) {
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Render the experiment list view
 */
async function renderExperimentList(modal: ModalBuilder, workspacePath: string): Promise<void> {
  const [summary, experiments] = await Promise.all([
    getExperimentsSummary(workspacePath),
    getExperiments(
      workspacePath,
      viewState.statusFilter === "all" ? undefined : viewState.statusFilter
    ),
  ]);

  modal.setContent(createListContent(summary, experiments));
  setupListEventHandlers(modal, workspacePath);
}

/**
 * Render the experiment detail view
 */
async function renderExperimentDetail(
  modal: ModalBuilder,
  workspacePath: string,
  experimentId: number
): Promise<void> {
  const experiments = await getExperiments(workspacePath);
  const experiment = experiments.find((e) => e.id === experimentId);

  if (!experiment) {
    modal.setContent(createErrorContent("Experiment not found"));
    return;
  }

  const [analysis, variants] = await Promise.all([
    analyzeExperiment(workspacePath, experimentId),
    getVariants(workspacePath, experimentId),
  ]);

  modal.setContent(createDetailContent(experiment, analysis, variants));
  setupDetailEventHandlers(modal, workspacePath, experiment);
}

/**
 * Render the create experiment form
 */
function renderCreateExperiment(modal: ModalBuilder, workspacePath: string): void {
  modal.setContent(createNewExperimentForm());
  setupCreateEventHandlers(modal, workspacePath);
}

// ============================================================================
// List View Content
// ============================================================================

/**
 * Create the experiment list content
 */
function createListContent(summary: ExperimentsSummary, experiments: Experiment[]): string {
  return `
    <!-- Header with Summary -->
    <div class="mb-6">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-lg font-semibold text-gray-800 dark:text-gray-200">A/B Experiments</h3>
        <button
          id="create-experiment-btn"
          class="px-4 py-2 bg-green-600 hover:bg-green-500 text-white rounded-lg text-sm font-medium transition-colors"
        >
          + New Experiment
        </button>
      </div>

      <!-- Summary Cards -->
      <div class="grid grid-cols-4 gap-4 mb-4">
        ${createSummaryCard("Total", summary.total_experiments.toString(), "All experiments")}
        ${createSummaryCard("Active", summary.active_experiments.toString(), "Currently running", "text-blue-600 dark:text-blue-400")}
        ${createSummaryCard("Concluded", summary.concluded_experiments.toString(), "With results", "text-green-600 dark:text-green-400")}
        ${createSummaryCard("Results", summary.total_results.toString(), "Recorded outcomes")}
      </div>
    </div>

    <!-- Filters -->
    <div class="mb-4 flex gap-2">
      ${createStatusFilterButton("all", "All")}
      ${createStatusFilterButton("draft", "Draft")}
      ${createStatusFilterButton("active", "Active")}
      ${createStatusFilterButton("concluded", "Concluded")}
      ${createStatusFilterButton("cancelled", "Cancelled")}
    </div>

    <!-- Experiment List -->
    <div class="space-y-3">
      ${
        experiments.length > 0
          ? experiments.map(createExperimentRow).join("")
          : createEmptyState(
              "No experiments found",
              "Create your first A/B experiment to start comparing approaches."
            )
      }
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
  valueColor?: string
): string {
  const colorClass = valueColor ?? "text-gray-900 dark:text-gray-100";

  return `
    <div class="p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide">${label}</div>
      <div class="text-xl font-bold ${colorClass}">${value}</div>
      <div class="text-xs text-gray-400 dark:text-gray-500">${description}</div>
    </div>
  `;
}

/**
 * Create a status filter button
 */
function createStatusFilterButton(status: ExperimentStatus | "all", label: string): string {
  const isActive = viewState.statusFilter === status;
  const activeClass = isActive
    ? "bg-blue-600 text-white"
    : "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-200 dark:hover:bg-gray-600";

  return `
    <button
      class="status-filter-btn px-3 py-1.5 rounded-lg text-sm font-medium transition-colors ${activeClass}"
      data-status="${status}"
    >
      ${label}
    </button>
  `;
}

/**
 * Create an experiment row in the list
 */
function createExperimentRow(experiment: Experiment): string {
  const statusColor = getStatusColor(experiment.status as ExperimentStatus);
  const statusLabel = formatStatus(experiment.status as ExperimentStatus);
  const metricLabel = getMetricLabel(experiment.target_metric as TargetMetric);
  const directionIndicator = getDirectionIndicator(experiment.target_direction as TargetDirection);

  return `
    <div
      class="experiment-row p-4 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-lg hover:border-blue-400 dark:hover:border-blue-600 cursor-pointer transition-colors"
      data-experiment-id="${experiment.id}"
    >
      <div class="flex items-start justify-between">
        <div class="flex-1">
          <div class="flex items-center gap-2 mb-1">
            <span class="font-semibold text-gray-900 dark:text-gray-100">${escapeHtml(experiment.name)}</span>
            <span class="px-2 py-0.5 text-xs rounded-full ${statusColor} bg-opacity-10 dark:bg-opacity-20">
              ${statusLabel}
            </span>
          </div>
          ${experiment.description ? `<p class="text-sm text-gray-600 dark:text-gray-400 mb-2">${escapeHtml(experiment.description)}</p>` : ""}
          <div class="flex items-center gap-4 text-xs text-gray-500 dark:text-gray-400">
            <span>Target: ${metricLabel}</span>
            <span>${directionIndicator}</span>
            <span>Min samples: ${experiment.min_sample_size}</span>
          </div>
        </div>
        <div class="text-xs text-gray-400 dark:text-gray-500">
          ${formatDate(experiment.created_at)}
        </div>
      </div>
    </div>
  `;
}

// ============================================================================
// Detail View Content
// ============================================================================

/**
 * Create the experiment detail content
 */
function createDetailContent(
  experiment: Experiment,
  analysis: ExperimentAnalysis,
  variants: Variant[]
): string {
  const statusColor = getStatusColor(experiment.status as ExperimentStatus);
  const statusLabel = formatStatus(experiment.status as ExperimentStatus);

  return `
    <!-- Back button and header -->
    <div class="mb-6">
      <button
        id="back-to-list-btn"
        class="flex items-center gap-1 text-sm text-blue-600 dark:text-blue-400 hover:underline mb-4"
      >
        &larr; Back to experiments
      </button>

      <div class="flex items-start justify-between">
        <div>
          <h3 class="text-xl font-bold text-gray-900 dark:text-gray-100 mb-1">
            ${escapeHtml(experiment.name)}
          </h3>
          <span class="px-2 py-0.5 text-xs rounded-full ${statusColor} bg-opacity-10 dark:bg-opacity-20">
            ${statusLabel}
          </span>
        </div>
        <div class="flex gap-2">
          ${experiment.status === "active" ? createActionButtons() : ""}
          <button
            id="export-btn"
            class="px-3 py-1.5 text-sm bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 rounded-lg transition-colors"
            data-tooltip="Export results"
          >
            Export
          </button>
        </div>
      </div>
    </div>

    <!-- Experiment Info -->
    ${
      experiment.hypothesis
        ? `
    <div class="mb-6 p-4 bg-blue-50 dark:bg-blue-900/20 border border-blue-200 dark:border-blue-800 rounded-lg">
      <div class="text-xs font-medium text-blue-600 dark:text-blue-400 uppercase mb-1">Hypothesis</div>
      <p class="text-gray-700 dark:text-gray-300">${escapeHtml(experiment.hypothesis)}</p>
    </div>
    `
        : ""
    }

    <!-- Analysis Summary -->
    ${createAnalysisSummary(analysis, experiment)}

    <!-- Comparison Table -->
    ${createComparisonTable(analysis.stats_per_variant, experiment)}

    <!-- Bar Chart Visualization -->
    ${createBarChart(analysis.stats_per_variant, experiment)}

    <!-- Recommendation -->
    ${createRecommendation(analysis)}

    <!-- Variants Config -->
    ${createVariantsSection(variants)}
  `;
}

/**
 * Create action buttons for active experiments
 */
function createActionButtons(): string {
  return `
    <button
      id="conclude-btn"
      class="px-3 py-1.5 text-sm bg-green-600 hover:bg-green-500 text-white rounded-lg transition-colors"
    >
      Conclude
    </button>
    <button
      id="cancel-btn"
      class="px-3 py-1.5 text-sm bg-red-600 hover:bg-red-500 text-white rounded-lg transition-colors"
    >
      Cancel
    </button>
  `;
}

/**
 * Create the analysis summary section
 */
function createAnalysisSummary(analysis: ExperimentAnalysis, _experiment: Experiment): string {
  const pValueColor = getPValueColor(analysis.p_value);
  const significanceLabel = isSignificant(analysis.p_value)
    ? "Statistically Significant"
    : "Not Significant";
  const significanceClass = isSignificant(analysis.p_value)
    ? "text-green-600 dark:text-green-400"
    : "text-gray-500 dark:text-gray-400";

  return `
    <div class="mb-6 grid grid-cols-4 gap-4">
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase mb-1">Winner</div>
        <div class="text-lg font-bold ${analysis.winner ? "text-green-600 dark:text-green-400" : "text-gray-500 dark:text-gray-400"}">
          ${analysis.winner ?? "No winner yet"}
        </div>
      </div>

      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase mb-1">Confidence</div>
        <div class="text-lg font-bold text-gray-900 dark:text-gray-100">
          ${formatConfidence(analysis.confidence)}
        </div>
      </div>

      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase mb-1">P-Value</div>
        <div class="text-lg font-bold ${pValueColor}">
          ${formatPValue(analysis.p_value)}
        </div>
        <div class="text-xs ${significanceClass}">${significanceLabel}</div>
      </div>

      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <div class="text-xs text-gray-500 dark:text-gray-400 uppercase mb-1">Effect Size</div>
        <div class="text-lg font-bold text-gray-900 dark:text-gray-100">
          ${formatEffectSize(analysis.effect_size)}
        </div>
      </div>
    </div>
  `;
}

/**
 * Create the comparison table
 */
function createComparisonTable(stats: VariantStats[], experiment: Experiment): string {
  if (stats.length === 0) {
    return createEmptyState("No data yet", "Assign issues to this experiment to collect data.");
  }

  const metricLabel = getMetricLabel(experiment.target_metric as TargetMetric);

  return `
    <div class="mb-6">
      <h4 class="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-3">Comparison Table</h4>
      <div class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-gray-200 dark:border-gray-700">
              <th class="text-left py-2 font-medium text-gray-700 dark:text-gray-300">Variant</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Sample Size</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">Success Rate</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">${metricLabel}</th>
              <th class="text-right py-2 font-medium text-gray-700 dark:text-gray-300">95% CI</th>
            </tr>
          </thead>
          <tbody>
            ${stats.map((s) => createVariantRow(s, experiment)).join("")}
          </tbody>
        </table>
      </div>
    </div>
  `;
}

/**
 * Create a variant row in the comparison table
 */
function createVariantRow(stats: VariantStats, experiment: Experiment): string {
  const ciText =
    stats.ci_lower !== null && stats.ci_upper !== null
      ? `${formatSuccessRate(stats.ci_lower)} - ${formatSuccessRate(stats.ci_upper)}`
      : "-";

  return `
    <tr class="border-b border-gray-100 dark:border-gray-800">
      <td class="py-3 font-medium text-gray-800 dark:text-gray-200">${escapeHtml(stats.variant_name)}</td>
      <td class="text-right py-3 text-gray-600 dark:text-gray-400">${stats.sample_size}</td>
      <td class="text-right py-3 text-gray-600 dark:text-gray-400">${formatSuccessRate(stats.success_rate)}</td>
      <td class="text-right py-3 text-gray-600 dark:text-gray-400">
        ${formatMetricValue(stats.avg_metric_value, experiment.target_metric as TargetMetric)}
      </td>
      <td class="text-right py-3 text-gray-500 dark:text-gray-400 text-xs">${ciText}</td>
    </tr>
  `;
}

/**
 * Create a simple bar chart visualization using CSS
 */
function createBarChart(stats: VariantStats[], _experiment: Experiment): string {
  if (stats.length === 0) return "";

  // Find the max value for scaling
  const maxRate = Math.max(...stats.map((s) => s.success_rate), 0.01);

  const colors = [
    "bg-blue-500",
    "bg-green-500",
    "bg-purple-500",
    "bg-orange-500",
    "bg-pink-500",
    "bg-cyan-500",
  ];

  return `
    <div class="mb-6">
      <h4 class="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-3">Success Rate Comparison</h4>
      <div class="space-y-3">
        ${stats
          .map((s, i) => {
            const width = Math.max((s.success_rate / maxRate) * 100, 2);
            const color = colors[i % colors.length];
            return `
              <div class="flex items-center gap-3">
                <div class="w-24 text-sm text-gray-700 dark:text-gray-300 truncate">${escapeHtml(s.variant_name)}</div>
                <div class="flex-1 h-6 bg-gray-100 dark:bg-gray-700 rounded overflow-hidden">
                  <div
                    class="${color} h-full rounded transition-all duration-500"
                    style="width: ${width}%"
                  ></div>
                </div>
                <div class="w-16 text-sm text-right text-gray-600 dark:text-gray-400">
                  ${formatSuccessRate(s.success_rate)}
                </div>
              </div>
            `;
          })
          .join("")}
      </div>
    </div>
  `;
}

/**
 * Create the recommendation section
 */
function createRecommendation(analysis: ExperimentAnalysis): string {
  const bgColor = analysis.should_conclude
    ? "bg-green-50 dark:bg-green-900/20 border-green-200 dark:border-green-800"
    : "bg-yellow-50 dark:bg-yellow-900/20 border-yellow-200 dark:border-yellow-800";

  const iconColor = analysis.should_conclude
    ? "text-green-600 dark:text-green-400"
    : "text-yellow-600 dark:text-yellow-400";

  return `
    <div class="mb-6 p-4 ${bgColor} border rounded-lg">
      <div class="flex items-start gap-3">
        <span class="${iconColor} text-xl">${analysis.should_conclude ? "✓" : "⚠"}</span>
        <div>
          <div class="font-semibold text-gray-800 dark:text-gray-200 mb-1">
            ${analysis.should_conclude ? "Ready to Conclude" : "Recommendation"}
          </div>
          <p class="text-sm text-gray-700 dark:text-gray-300">${escapeHtml(analysis.recommendation)}</p>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create the variants configuration section
 */
function createVariantsSection(variants: Variant[]): string {
  if (variants.length === 0) return "";

  return `
    <div class="mb-6">
      <h4 class="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-3">Variant Configurations</h4>
      <div class="space-y-2">
        ${variants
          .map(
            (v) => `
          <div class="p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
            <div class="flex items-center justify-between mb-1">
              <span class="font-medium text-gray-800 dark:text-gray-200">${escapeHtml(v.name)}</span>
              <span class="text-xs text-gray-500 dark:text-gray-400">Weight: ${v.weight}</span>
            </div>
            ${v.description ? `<p class="text-sm text-gray-600 dark:text-gray-400 mb-2">${escapeHtml(v.description)}</p>` : ""}
            ${
              v.config_json
                ? `<pre class="text-xs bg-gray-100 dark:bg-gray-800 p-2 rounded overflow-x-auto"><code>${escapeHtml(v.config_json)}</code></pre>`
                : ""
            }
          </div>
        `
          )
          .join("")}
      </div>
    </div>
  `;
}

// ============================================================================
// Create Experiment Form
// ============================================================================

/**
 * Create the new experiment form
 */
function createNewExperimentForm(): string {
  return `
    <!-- Back button -->
    <button
      id="back-to-list-btn"
      class="flex items-center gap-1 text-sm text-blue-600 dark:text-blue-400 hover:underline mb-4"
    >
      &larr; Back to experiments
    </button>

    <h3 class="text-lg font-bold text-gray-900 dark:text-gray-100 mb-4">Create New Experiment</h3>

    <form id="create-experiment-form" class="space-y-4">
      <!-- Name -->
      <div>
        <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Experiment Name <span class="text-red-500">*</span>
        </label>
        <input
          type="text"
          id="experiment-name"
          class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          placeholder="e.g., manual-vs-autonomous-builder"
          required
        />
      </div>

      <!-- Description -->
      <div>
        <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Description
        </label>
        <textarea
          id="experiment-description"
          class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          rows="2"
          placeholder="What are you testing?"
        ></textarea>
      </div>

      <!-- Hypothesis -->
      <div>
        <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Hypothesis
        </label>
        <textarea
          id="experiment-hypothesis"
          class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          rows="2"
          placeholder="What do you expect to find?"
        ></textarea>
      </div>

      <!-- Target Metric -->
      <div class="grid grid-cols-2 gap-4">
        <div>
          <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
            Target Metric <span class="text-red-500">*</span>
          </label>
          <select
            id="target-metric"
            class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          >
            <option value="success_rate">Success Rate</option>
            <option value="cycle_time">Cycle Time</option>
            <option value="cost">Cost</option>
          </select>
        </div>

        <div>
          <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
            Better When <span class="text-red-500">*</span>
          </label>
          <select
            id="target-direction"
            class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          >
            <option value="higher">Higher is better</option>
            <option value="lower">Lower is better</option>
          </select>
        </div>
      </div>

      <!-- Min Sample Size -->
      <div>
        <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Minimum Sample Size Per Variant
        </label>
        <input
          type="number"
          id="min-sample-size"
          class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          value="20"
          min="5"
          max="1000"
        />
        <p class="text-xs text-gray-500 dark:text-gray-400 mt-1">
          Minimum samples needed before statistical analysis is meaningful
        </p>
      </div>

      <!-- Variants Section -->
      <div class="border-t border-gray-200 dark:border-gray-700 pt-4 mt-4">
        <div class="flex items-center justify-between mb-3">
          <h4 class="text-sm font-semibold text-gray-700 dark:text-gray-300">Variants</h4>
          <button
            type="button"
            id="add-variant-btn"
            class="text-sm text-blue-600 dark:text-blue-400 hover:underline"
          >
            + Add Variant
          </button>
        </div>

        <div id="variants-container" class="space-y-3">
          ${createVariantInput(0, "Control", "The baseline configuration")}
          ${createVariantInput(1, "Treatment", "The experimental configuration")}
        </div>
      </div>

      <!-- Submit Buttons -->
      <div class="flex justify-end gap-3 pt-4 border-t border-gray-200 dark:border-gray-700">
        <button
          type="button"
          id="cancel-create-btn"
          class="px-4 py-2 text-gray-700 dark:text-gray-300 hover:bg-gray-100 dark:hover:bg-gray-700 rounded-lg transition-colors"
        >
          Cancel
        </button>
        <button
          type="submit"
          class="px-4 py-2 bg-blue-600 hover:bg-blue-500 text-white rounded-lg font-medium transition-colors"
        >
          Create Experiment
        </button>
      </div>
    </form>
  `;
}

/**
 * Create a variant input row
 */
function createVariantInput(index: number, name: string, description: string): string {
  return `
    <div class="variant-input p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700" data-variant-index="${index}">
      <div class="grid grid-cols-2 gap-3 mb-2">
        <input
          type="text"
          class="variant-name px-3 py-2 border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 text-sm"
          placeholder="Variant name"
          value="${escapeHtml(name)}"
          required
        />
        <input
          type="text"
          class="variant-description px-3 py-2 border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 text-sm"
          placeholder="Description"
          value="${escapeHtml(description)}"
        />
      </div>
      <textarea
        class="variant-config w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 text-sm font-mono"
        placeholder='{"mode": "manual"}'
        rows="2"
      ></textarea>
      ${index > 1 ? `<button type="button" class="remove-variant-btn mt-2 text-xs text-red-600 dark:text-red-400 hover:underline">Remove</button>` : ""}
    </div>
  `;
}

// ============================================================================
// Event Handlers
// ============================================================================

/**
 * Set up event handlers for the list view
 */
function setupListEventHandlers(modal: ModalBuilder, _workspacePath: string): void {
  // Create experiment button
  const createBtn = modal.querySelector<HTMLButtonElement>("#create-experiment-btn");
  createBtn?.addEventListener("click", () => {
    viewState.currentView = "create";
    refreshContent(modal);
  });

  // Status filter buttons
  const filterBtns = modal.querySelectorAll<HTMLButtonElement>(".status-filter-btn");
  filterBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      viewState.statusFilter = btn.dataset.status as ExperimentStatus | "all";
      modal.setContent(createLoadingContent());
      refreshContent(modal);
    });
  });

  // Experiment rows
  const experimentRows = modal.querySelectorAll<HTMLDivElement>(".experiment-row");
  experimentRows.forEach((row) => {
    row.addEventListener("click", () => {
      const experimentId = parseInt(row.dataset.experimentId ?? "0", 10);
      if (experimentId) {
        viewState.currentView = "detail";
        viewState.selectedExperimentId = experimentId;
        modal.setContent(createLoadingContent());
        refreshContent(modal);
      }
    });
  });
}

/**
 * Set up event handlers for the detail view
 */
function setupDetailEventHandlers(
  modal: ModalBuilder,
  workspacePath: string,
  experiment: Experiment
): void {
  // Back button
  const backBtn = modal.querySelector<HTMLButtonElement>("#back-to-list-btn");
  backBtn?.addEventListener("click", () => {
    viewState.currentView = "list";
    viewState.selectedExperimentId = null;
    modal.setContent(createLoadingContent());
    refreshContent(modal);
  });

  // Conclude button
  const concludeBtn = modal.querySelector<HTMLButtonElement>("#conclude-btn");
  concludeBtn?.addEventListener("click", async () => {
    if (experiment.id === null) return;

    try {
      await concludeExperiment(workspacePath, experiment.id);
      showToast("Experiment concluded successfully", "success");
      modal.setContent(createLoadingContent());
      await refreshContent(modal);
    } catch (error) {
      showToast(`Failed to conclude experiment: ${error}`, "error");
    }
  });

  // Cancel button
  const cancelBtn = modal.querySelector<HTMLButtonElement>("#cancel-btn");
  cancelBtn?.addEventListener("click", async () => {
    if (experiment.id === null) return;

    if (!confirm("Are you sure you want to cancel this experiment?")) return;

    try {
      await cancelExperiment(workspacePath, experiment.id);
      showToast("Experiment cancelled", "info");
      modal.setContent(createLoadingContent());
      await refreshContent(modal);
    } catch (error) {
      showToast(`Failed to cancel experiment: ${error}`, "error");
    }
  });

  // Export button
  const exportBtn = modal.querySelector<HTMLButtonElement>("#export-btn");
  exportBtn?.addEventListener("click", async () => {
    if (experiment.id === null) return;
    await exportExperimentResults(workspacePath, experiment.id, experiment.name);
  });
}

/**
 * Set up event handlers for the create view
 */
function setupCreateEventHandlers(modal: ModalBuilder, workspacePath: string): void {
  let variantCount = 2;

  // Back button
  const backBtn = modal.querySelector<HTMLButtonElement>("#back-to-list-btn");
  backBtn?.addEventListener("click", () => {
    viewState.currentView = "list";
    refreshContent(modal);
  });

  // Cancel button
  const cancelBtn = modal.querySelector<HTMLButtonElement>("#cancel-create-btn");
  cancelBtn?.addEventListener("click", () => {
    viewState.currentView = "list";
    refreshContent(modal);
  });

  // Add variant button
  const addVariantBtn = modal.querySelector<HTMLButtonElement>("#add-variant-btn");
  addVariantBtn?.addEventListener("click", () => {
    const container = modal.querySelector<HTMLDivElement>("#variants-container");
    if (container) {
      const newVariant = document.createElement("div");
      newVariant.innerHTML = createVariantInput(variantCount, `Variant ${variantCount + 1}`, "");
      const firstChild = newVariant.firstElementChild;
      if (firstChild) {
        container.appendChild(firstChild);
      }
      variantCount++;

      // Add remove handler for the new variant
      setupRemoveVariantHandler(modal);
    }
  });

  // Remove variant handlers
  setupRemoveVariantHandler(modal);

  // Form submission
  const form = modal.querySelector<HTMLFormElement>("#create-experiment-form");
  form?.addEventListener("submit", async (e) => {
    e.preventDefault();
    await handleCreateExperiment(modal, workspacePath);
  });
}

/**
 * Set up remove variant button handlers
 */
function setupRemoveVariantHandler(modal: ModalBuilder): void {
  const removeBtns = modal.querySelectorAll<HTMLButtonElement>(".remove-variant-btn");
  removeBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const variantDiv = btn.closest(".variant-input");
      variantDiv?.remove();
    });
  });
}

/**
 * Handle creating a new experiment
 */
async function handleCreateExperiment(modal: ModalBuilder, workspacePath: string): Promise<void> {
  const name = (modal.querySelector<HTMLInputElement>("#experiment-name")?.value ?? "").trim();
  const description =
    (modal.querySelector<HTMLTextAreaElement>("#experiment-description")?.value ?? "").trim() ||
    null;
  const hypothesis =
    (modal.querySelector<HTMLTextAreaElement>("#experiment-hypothesis")?.value ?? "").trim() ||
    null;
  const targetMetric = (modal.querySelector<HTMLSelectElement>("#target-metric")?.value ??
    "success_rate") as TargetMetric;
  const targetDirection = (modal.querySelector<HTMLSelectElement>("#target-direction")?.value ??
    "higher") as TargetDirection;
  const minSampleSize = parseInt(
    modal.querySelector<HTMLInputElement>("#min-sample-size")?.value ?? "20",
    10
  );

  // Collect variants
  const variantInputs = modal.querySelectorAll<HTMLDivElement>(".variant-input");
  const variants: Array<{ name: string; description: string | null; config: string | null }> = [];

  variantInputs.forEach((input) => {
    const variantName = input.querySelector<HTMLInputElement>(".variant-name")?.value.trim() ?? "";
    const variantDesc =
      input.querySelector<HTMLInputElement>(".variant-description")?.value.trim() || null;
    const variantConfig =
      input.querySelector<HTMLTextAreaElement>(".variant-config")?.value.trim() || null;

    if (variantName) {
      variants.push({ name: variantName, description: variantDesc, config: variantConfig });
    }
  });

  if (!name) {
    showToast("Experiment name is required", "error");
    return;
  }

  if (variants.length < 2) {
    showToast("At least 2 variants are required", "error");
    return;
  }

  try {
    // Create the experiment
    const experimentId = await createExperiment(workspacePath, {
      name,
      description,
      hypothesis,
      status: "draft",
      min_sample_size: minSampleSize,
      target_metric: targetMetric,
      target_direction: targetDirection,
    });

    // Add variants
    for (const variant of variants) {
      await addVariant(workspacePath, {
        experiment_id: experimentId,
        name: variant.name,
        description: variant.description,
        config_json: variant.config,
        weight: 1.0,
      });
    }

    // Start the experiment
    await startExperiment(workspacePath, experimentId);

    showToast("Experiment created and started", "success");

    // Navigate to the new experiment
    viewState.currentView = "detail";
    viewState.selectedExperimentId = experimentId;
    modal.setContent(createLoadingContent());
    await refreshContent(modal);
  } catch (error) {
    showToast(`Failed to create experiment: ${error}`, "error");
  }
}

// ============================================================================
// Export Functions
// ============================================================================

/**
 * Export experiment results as JSON
 */
async function exportExperimentResults(
  workspacePath: string,
  experimentId: number,
  experimentName: string
): Promise<void> {
  try {
    const [experiments, analysis, variants] = await Promise.all([
      getExperiments(workspacePath),
      analyzeExperiment(workspacePath, experimentId),
      getVariants(workspacePath, experimentId),
    ]);

    const experiment = experiments.find((e) => e.id === experimentId);

    const exportData = {
      experiment,
      analysis,
      variants,
      exported_at: new Date().toISOString(),
    };

    // Create and download file
    const blob = new Blob([JSON.stringify(exportData, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `experiment-${experimentName}-${new Date().toISOString().split("T")[0]}.json`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);

    showToast("Results exported successfully", "success");
  } catch (error) {
    showToast(`Failed to export results: ${error}`, "error");
  }
}

// ============================================================================
// Helper Functions
// ============================================================================

/**
 * Create loading content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-12">
      <div class="text-gray-500 dark:text-gray-400">Loading...</div>
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
      <p class="text-red-700 dark:text-red-300">Error: ${escapeHtml(message)}</p>
    </div>
  `;
}

/**
 * Create empty state content
 */
function createEmptyState(title: string, message: string): string {
  return `
    <div class="text-center py-8 text-gray-500 dark:text-gray-400">
      <p class="font-medium mb-1">${title}</p>
      <p class="text-sm">${message}</p>
    </div>
  `;
}

/**
 * Format a date string for display
 */
function formatDate(dateStr: string): string {
  try {
    const date = new Date(dateStr);
    return date.toLocaleDateString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
    });
  } catch {
    return dateStr;
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
