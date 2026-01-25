/**
 * Activity Playback Modal
 *
 * Displays a timeline visualization of agent activity for a specific issue or PR.
 * Shows chronological sequence of prompts, outcomes, and workflow state changes.
 *
 * Features:
 * - Timeline visualization with expandable nodes
 * - Filter by issue number, PR number, date range, or agent role
 * - Color-coded outcomes (success/failure/pending)
 * - Summary statistics (total prompts, duration, cost)
 * - Export to markdown/JSON
 * - Compare two timelines side-by-side
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1112
 */

import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import { showToast } from "./toast";

const logger = Logger.forComponent("activity-playback");

// ============================================================================
// Types
// ============================================================================

/**
 * Timeline entry representing a single agent activity
 */
export interface TimelineEntry {
  id: number;
  timestamp: string;
  role: string;
  action: string;
  duration_ms: number | null;
  outcome: "success" | "failure" | "pending" | "in_progress";
  issue_number: number | null;
  pr_number: number | null;
  prompt_preview: string | null;
  output_preview: string | null;
  tokens: number | null;
  cost: number | null;
  event_type: string | null;
  label_before: string | null;
  label_after: string | null;
}

/**
 * Summary statistics for a timeline
 */
export interface TimelineSummary {
  total_prompts: number;
  total_duration_ms: number;
  total_tokens: number;
  total_cost: number;
  success_count: number;
  failure_count: number;
  pending_count: number;
  first_activity: string | null;
  last_activity: string | null;
  roles_involved: string[];
}

/**
 * Filter options for timeline queries
 */
export interface TimelineFilter {
  issue_number?: number;
  pr_number?: number;
  date_from?: string;
  date_to?: string;
  role?: string;
  limit?: number;
}

/**
 * Complete timeline data structure
 */
interface TimelineData {
  entries: TimelineEntry[];
  summary: TimelineSummary;
  filter: TimelineFilter;
}

// ============================================================================
// Modal State
// ============================================================================

let currentModal: ModalBuilder | null = null;
let currentFilter: TimelineFilter = {};
let currentData: TimelineData | null = null;
let comparisonData: TimelineData | null = null;
let isCompareMode = false;

// ============================================================================
// Public API
// ============================================================================

/**
 * Show the Activity Playback modal for a specific issue
 */
export async function showActivityPlaybackForIssue(issueNumber: number): Promise<void> {
  await showActivityPlaybackModal({ issue_number: issueNumber });
}

/**
 * Show the Activity Playback modal for a specific PR
 */
export async function showActivityPlaybackForPR(prNumber: number): Promise<void> {
  await showActivityPlaybackModal({ pr_number: prNumber });
}

/**
 * Show the Activity Playback modal with optional filters
 */
export async function showActivityPlaybackModal(filter?: TimelineFilter): Promise<void> {
  // Close existing modal if open
  if (currentModal?.isVisible()) {
    currentModal.close();
  }

  currentFilter = filter ?? {};
  isCompareMode = false;
  comparisonData = null;

  const modal = new ModalBuilder({
    title: "Activity Playback",
    width: "900px",
    maxHeight: "90vh",
    id: "activity-playback-modal",
    showHeader: false,
    customHeader: createCustomHeader(),
    onClose: () => {
      currentModal = null;
      currentData = null;
      comparisonData = null;
    },
  });

  currentModal = modal;

  // Show loading state
  modal.setContent(createLoadingContent());
  modal.show();

  // Set up event handlers
  setupEventHandlers(modal);

  // Load initial data
  await loadTimelineData(modal, currentFilter);
}

// ============================================================================
// Header and Controls
// ============================================================================

/**
 * Create custom header with filter controls and export buttons
 */
function createCustomHeader(): HTMLElement {
  const header = document.createElement("div");
  header.className = "border-b border-gray-200 dark:border-gray-700";

  header.innerHTML = `
    <div class="flex items-center justify-between p-4">
      <h2 id="modal-title" class="text-xl font-bold text-gray-900 dark:text-gray-100">
        Activity Playback
      </h2>
      <div class="flex gap-2">
        <button id="compare-btn" class="px-3 py-1 text-sm bg-purple-500 hover:bg-purple-600 text-white rounded transition-colors">
          Compare
        </button>
        <button id="export-md-btn" class="px-3 py-1 text-sm bg-blue-500 hover:bg-blue-600 text-white rounded transition-colors">
          Export MD
        </button>
        <button id="export-json-btn" class="px-3 py-1 text-sm bg-blue-500 hover:bg-blue-600 text-white rounded transition-colors">
          Export JSON
        </button>
        <button class="modal-close-btn text-gray-400 hover:text-gray-600 dark:hover:text-gray-300 font-bold text-2xl transition-colors" aria-label="Close modal">
          &times;
        </button>
      </div>
    </div>

    <!-- Filter Controls -->
    <div class="flex items-center gap-4 px-4 pb-4 flex-wrap">
      <div class="flex items-center gap-2">
        <label class="text-sm text-gray-600 dark:text-gray-400">Issue:</label>
        <input type="number" id="filter-issue" placeholder="#"
          class="w-20 px-2 py-1 text-sm border rounded dark:bg-gray-700 dark:border-gray-600 dark:text-gray-200"
        />
      </div>
      <div class="flex items-center gap-2">
        <label class="text-sm text-gray-600 dark:text-gray-400">PR:</label>
        <input type="number" id="filter-pr" placeholder="#"
          class="w-20 px-2 py-1 text-sm border rounded dark:bg-gray-700 dark:border-gray-600 dark:text-gray-200"
        />
      </div>
      <div class="flex items-center gap-2">
        <label class="text-sm text-gray-600 dark:text-gray-400">Role:</label>
        <select id="filter-role"
          class="px-2 py-1 text-sm border rounded dark:bg-gray-700 dark:border-gray-600 dark:text-gray-200">
          <option value="">All Roles</option>
          <option value="builder">Builder</option>
          <option value="judge">Judge</option>
          <option value="curator">Curator</option>
          <option value="doctor">Doctor</option>
          <option value="architect">Architect</option>
          <option value="hermit">Hermit</option>
          <option value="champion">Champion</option>
          <option value="guide">Guide</option>
        </select>
      </div>
      <div class="flex items-center gap-2">
        <label class="text-sm text-gray-600 dark:text-gray-400">From:</label>
        <input type="date" id="filter-date-from"
          class="px-2 py-1 text-sm border rounded dark:bg-gray-700 dark:border-gray-600 dark:text-gray-200"
        />
      </div>
      <div class="flex items-center gap-2">
        <label class="text-sm text-gray-600 dark:text-gray-400">To:</label>
        <input type="date" id="filter-date-to"
          class="px-2 py-1 text-sm border rounded dark:bg-gray-700 dark:border-gray-600 dark:text-gray-200"
        />
      </div>
      <button id="apply-filter-btn" class="px-3 py-1 text-sm bg-green-500 hover:bg-green-600 text-white rounded transition-colors">
        Apply
      </button>
      <button id="clear-filter-btn" class="px-3 py-1 text-sm bg-gray-300 hover:bg-gray-400 dark:bg-gray-600 dark:hover:bg-gray-500 rounded transition-colors">
        Clear
      </button>
    </div>
  `;

  return header;
}

// ============================================================================
// Event Handlers
// ============================================================================

/**
 * Set up all event handlers for the modal
 */
function setupEventHandlers(modal: ModalBuilder): void {
  // Close button
  const closeBtn = modal.querySelector(".modal-close-btn");
  closeBtn?.addEventListener("click", () => modal.close());

  // Filter controls
  modal.querySelector("#apply-filter-btn")?.addEventListener("click", async () => {
    const filter = getFilterFromInputs(modal);
    currentFilter = filter;
    await loadTimelineData(modal, filter);
  });

  modal.querySelector("#clear-filter-btn")?.addEventListener("click", async () => {
    clearFilterInputs(modal);
    currentFilter = {};
    await loadTimelineData(modal, {});
  });

  // Export buttons
  modal
    .querySelector("#export-md-btn")
    ?.addEventListener("click", () => exportTimeline("markdown"));
  modal.querySelector("#export-json-btn")?.addEventListener("click", () => exportTimeline("json"));

  // Compare button
  modal.querySelector("#compare-btn")?.addEventListener("click", () => {
    if (isCompareMode) {
      exitCompareMode(modal);
    } else {
      enterCompareMode(modal);
    }
  });
}

/**
 * Get filter values from input elements
 */
function getFilterFromInputs(modal: ModalBuilder): TimelineFilter {
  const filter: TimelineFilter = {};

  const issueInput = modal.querySelector<HTMLInputElement>("#filter-issue");
  if (issueInput?.value) {
    filter.issue_number = parseInt(issueInput.value, 10);
  }

  const prInput = modal.querySelector<HTMLInputElement>("#filter-pr");
  if (prInput?.value) {
    filter.pr_number = parseInt(prInput.value, 10);
  }

  const roleSelect = modal.querySelector<HTMLSelectElement>("#filter-role");
  if (roleSelect?.value) {
    filter.role = roleSelect.value;
  }

  const dateFromInput = modal.querySelector<HTMLInputElement>("#filter-date-from");
  if (dateFromInput?.value) {
    filter.date_from = dateFromInput.value;
  }

  const dateToInput = modal.querySelector<HTMLInputElement>("#filter-date-to");
  if (dateToInput?.value) {
    filter.date_to = dateToInput.value;
  }

  return filter;
}

/**
 * Clear all filter input values
 */
function clearFilterInputs(modal: ModalBuilder): void {
  const inputs = ["#filter-issue", "#filter-pr", "#filter-date-from", "#filter-date-to"];
  for (const selector of inputs) {
    const input = modal.querySelector<HTMLInputElement>(selector);
    if (input) input.value = "";
  }

  const roleSelect = modal.querySelector<HTMLSelectElement>("#filter-role");
  if (roleSelect) roleSelect.value = "";
}

/**
 * Populate filter inputs with current filter values
 */
function populateFilterInputs(modal: ModalBuilder, filter: TimelineFilter): void {
  const issueInput = modal.querySelector<HTMLInputElement>("#filter-issue");
  if (issueInput && filter.issue_number) {
    issueInput.value = String(filter.issue_number);
  }

  const prInput = modal.querySelector<HTMLInputElement>("#filter-pr");
  if (prInput && filter.pr_number) {
    prInput.value = String(filter.pr_number);
  }

  const roleSelect = modal.querySelector<HTMLSelectElement>("#filter-role");
  if (roleSelect && filter.role) {
    roleSelect.value = filter.role;
  }

  const dateFromInput = modal.querySelector<HTMLInputElement>("#filter-date-from");
  if (dateFromInput && filter.date_from) {
    dateFromInput.value = filter.date_from;
  }

  const dateToInput = modal.querySelector<HTMLInputElement>("#filter-date-to");
  if (dateToInput && filter.date_to) {
    dateToInput.value = filter.date_to;
  }
}

// ============================================================================
// Data Loading
// ============================================================================

/**
 * Load timeline data from backend
 */
async function loadTimelineData(modal: ModalBuilder, filter: TimelineFilter): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    // Query timeline entries
    const entries = await queryTimelineEntries(workspacePath, filter);

    // Calculate summary
    const summary = calculateSummary(entries);

    currentData = { entries, summary, filter };

    // Render content
    if (isCompareMode && comparisonData) {
      modal.setContent(createComparisonContent(currentData, comparisonData));
    } else {
      modal.setContent(createTimelineContent(currentData));
    }

    // Populate filter inputs
    populateFilterInputs(modal, filter);

    // Set up timeline node handlers
    setupTimelineHandlers(modal);
  } catch (error) {
    logger.error("Failed to load timeline data", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Query timeline entries from the database
 */
async function queryTimelineEntries(
  workspacePath: string,
  filter: TimelineFilter
): Promise<TimelineEntry[]> {
  try {
    // Use the get_activity_timeline Tauri command
    const entries = await invoke<TimelineEntry[]>("get_activity_timeline", {
      workspacePath,
      issueNumber: filter.issue_number ?? null,
      prNumber: filter.pr_number ?? null,
      role: filter.role ?? null,
      dateFrom: filter.date_from ?? null,
      dateTo: filter.date_to ?? null,
      limit: filter.limit ?? 100,
    });

    return entries;
  } catch (error) {
    // If the command doesn't exist yet, fall back to a mock or basic query
    logger.warn("get_activity_timeline not available, using fallback", { error: String(error) });
    return await queryTimelineEntriesFallback(workspacePath, filter);
  }
}

/**
 * Fallback query using existing commands
 */
async function queryTimelineEntriesFallback(
  workspacePath: string,
  filter: TimelineFilter
): Promise<TimelineEntry[]> {
  // Use existing read_recent_activity command and transform results
  const activities = await invoke<RawActivityEntry[]>("read_recent_activity", {
    workspacePath,
    limit: filter.limit ?? 100,
  });

  // Get prompt-GitHub correlations if filtering by issue/PR
  let githubEntries: RawPromptGitHubEntry[] = [];
  if (filter.issue_number) {
    try {
      githubEntries = await invoke<RawPromptGitHubEntry[]>("get_prompts_for_issue", {
        workspacePath,
        issueNumber: filter.issue_number,
      });
    } catch {
      // Command may not exist
    }
  } else if (filter.pr_number) {
    try {
      githubEntries = await invoke<RawPromptGitHubEntry[]>("get_prompts_for_pr", {
        workspacePath,
        prNumber: filter.pr_number,
      });
    } catch {
      // Command may not exist
    }
  }

  // Create a map of activity IDs from GitHub entries for filtering
  const activityIds = new Set(githubEntries.map((e) => e.activity_id));

  // Transform and filter activities
  let entries: TimelineEntry[] = activities.map((a) => ({
    id: 0, // Will be filled if available
    timestamp: a.timestamp,
    role: a.role,
    action: determineAction(a),
    duration_ms: a.duration_ms,
    outcome: determineOutcome(a),
    issue_number: a.issue_number,
    pr_number: null,
    prompt_preview: a.notes ?? null,
    output_preview: null,
    tokens: null,
    cost: null,
    event_type: null,
    label_before: null,
    label_after: null,
  }));

  // Apply filters
  if (filter.issue_number && activityIds.size > 0) {
    // Filter to only activities associated with this issue
    entries = entries.filter((e) => e.issue_number === filter.issue_number);
  }

  if (filter.role) {
    entries = entries.filter((e) => e.role.toLowerCase() === filter.role?.toLowerCase());
  }

  if (filter.date_from) {
    const fromDate = new Date(filter.date_from);
    entries = entries.filter((e) => new Date(e.timestamp) >= fromDate);
  }

  if (filter.date_to) {
    const toDate = new Date(filter.date_to);
    toDate.setHours(23, 59, 59, 999);
    entries = entries.filter((e) => new Date(e.timestamp) <= toDate);
  }

  return entries;
}

// Raw types from Rust backend
interface RawActivityEntry {
  timestamp: string;
  role: string;
  trigger: string;
  work_found: boolean;
  work_completed: boolean | null;
  issue_number: number | null;
  duration_ms: number | null;
  outcome: string;
  notes: string | null;
}

interface RawPromptGitHubEntry {
  id: number | null;
  activity_id: number;
  issue_number: number | null;
  pr_number: number | null;
  label_before: string | null;
  label_after: string | null;
  event_type: string;
  event_time: string;
}

/**
 * Determine action description from activity entry
 */
function determineAction(activity: RawActivityEntry): string {
  if (activity.outcome === "pr_created") return "Created PR";
  if (activity.outcome === "issue_claimed") return "Claimed issue";
  if (activity.outcome === "review_complete") return "Completed review";
  if (activity.outcome === "changes_requested") return "Requested changes";
  if (activity.outcome === "approved") return "Approved PR";
  if (activity.outcome === "merged") return "Merged PR";
  if (activity.outcome === "no_work") return "No work found";
  if (activity.trigger === "autonomous") return "Autonomous check";
  return activity.outcome || "Activity";
}

/**
 * Determine outcome status from activity entry
 */
function determineOutcome(
  activity: RawActivityEntry
): "success" | "failure" | "pending" | "in_progress" {
  if (activity.work_completed === true) return "success";
  if (activity.work_completed === false) return "failure";
  if (activity.outcome === "no_work") return "pending";
  return "in_progress";
}

/**
 * Calculate summary statistics for timeline entries
 */
function calculateSummary(entries: TimelineEntry[]): TimelineSummary {
  const rolesSet = new Set<string>();
  let totalDuration = 0;
  let totalTokens = 0;
  let totalCost = 0;
  let successCount = 0;
  let failureCount = 0;
  let pendingCount = 0;

  for (const entry of entries) {
    rolesSet.add(entry.role);
    if (entry.duration_ms) totalDuration += entry.duration_ms;
    if (entry.tokens) totalTokens += entry.tokens;
    if (entry.cost) totalCost += entry.cost;

    switch (entry.outcome) {
      case "success":
        successCount++;
        break;
      case "failure":
        failureCount++;
        break;
      case "pending":
        pendingCount++;
        break;
    }
  }

  return {
    total_prompts: entries.length,
    total_duration_ms: totalDuration,
    total_tokens: totalTokens,
    total_cost: totalCost,
    success_count: successCount,
    failure_count: failureCount,
    pending_count: pendingCount,
    first_activity: entries.length > 0 ? entries[entries.length - 1].timestamp : null,
    last_activity: entries.length > 0 ? entries[0].timestamp : null,
    roles_involved: Array.from(rolesSet),
  };
}

// ============================================================================
// Content Rendering
// ============================================================================

/**
 * Create loading content
 */
function createLoadingContent(): string {
  return `
    <div class="p-8 text-center text-gray-500 dark:text-gray-400">
      <div class="inline-block animate-spin rounded-full h-8 w-8 border-b-2 border-blue-500 mb-4"></div>
      <p>Loading timeline...</p>
    </div>
  `;
}

/**
 * Create error content
 */
function createErrorContent(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return `
    <div class="p-4 bg-red-50 dark:bg-red-900/20 text-red-700 dark:text-red-300 rounded m-4">
      <p class="font-semibold">Error loading timeline</p>
      <p class="text-sm mt-1">${escapeHtml(message)}</p>
    </div>
  `;
}

/**
 * Create timeline content
 */
function createTimelineContent(data: TimelineData): string {
  if (data.entries.length === 0) {
    return createEmptyContent(data.filter);
  }

  return `
    <div class="p-4">
      <!-- Summary Cards -->
      ${createSummaryCards(data.summary, data.filter)}

      <!-- Timeline -->
      <div class="mt-6">
        <h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100 mb-4">
          Activity Timeline
        </h3>
        <div class="relative">
          <!-- Timeline line -->
          <div class="absolute left-6 top-0 bottom-0 w-0.5 bg-gray-200 dark:bg-gray-700"></div>

          <!-- Timeline entries -->
          <div class="space-y-4">
            ${data.entries.map((entry, index) => createTimelineNode(entry, index)).join("")}
          </div>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create empty state content
 */
function createEmptyContent(filter: TimelineFilter): string {
  const hasFilter = Object.keys(filter).length > 0;

  return `
    <div class="p-8 text-center text-gray-500 dark:text-gray-400">
      <svg class="mx-auto h-12 w-12 text-gray-400 mb-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2"
          d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2" />
      </svg>
      <p class="text-lg font-medium">No activity found</p>
      <p class="mt-1 text-sm">
        ${hasFilter ? "Try adjusting your filters or selecting a different time range." : "Activity will appear here once agents start working."}
      </p>
    </div>
  `;
}

/**
 * Create summary cards
 */
function createSummaryCards(summary: TimelineSummary, filter: TimelineFilter): string {
  const durationStr = formatDuration(summary.total_duration_ms);
  const costStr = formatCurrency(summary.total_cost);
  const successRate =
    summary.total_prompts > 0
      ? Math.round((summary.success_count / summary.total_prompts) * 100)
      : 0;

  let titleStr = "All Activity";
  if (filter.issue_number) {
    titleStr = `Issue #${filter.issue_number}`;
  } else if (filter.pr_number) {
    titleStr = `PR #${filter.pr_number}`;
  }

  const dateRange =
    summary.first_activity && summary.last_activity
      ? `${formatDate(summary.first_activity)} - ${formatDate(summary.last_activity)}`
      : "No activity";

  return `
    <div class="mb-4">
      <div class="flex items-center justify-between mb-2">
        <h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100">${escapeHtml(titleStr)}</h3>
        <span class="text-sm text-gray-500 dark:text-gray-400">${escapeHtml(dateRange)}</span>
      </div>

      <div class="grid grid-cols-2 md:grid-cols-4 gap-4">
        <div class="bg-blue-50 dark:bg-blue-900/20 rounded-lg p-3">
          <div class="text-2xl font-bold text-blue-600 dark:text-blue-400">${summary.total_prompts}</div>
          <div class="text-sm text-blue-600 dark:text-blue-400">Total Prompts</div>
        </div>

        <div class="bg-green-50 dark:bg-green-900/20 rounded-lg p-3">
          <div class="text-2xl font-bold text-green-600 dark:text-green-400">${successRate}%</div>
          <div class="text-sm text-green-600 dark:text-green-400">Success Rate</div>
        </div>

        <div class="bg-purple-50 dark:bg-purple-900/20 rounded-lg p-3">
          <div class="text-2xl font-bold text-purple-600 dark:text-purple-400">${escapeHtml(durationStr)}</div>
          <div class="text-sm text-purple-600 dark:text-purple-400">Active Time</div>
        </div>

        <div class="bg-yellow-50 dark:bg-yellow-900/20 rounded-lg p-3">
          <div class="text-2xl font-bold text-yellow-600 dark:text-yellow-400">${escapeHtml(costStr)}</div>
          <div class="text-sm text-yellow-600 dark:text-yellow-400">Est. Cost</div>
        </div>
      </div>

      ${
        summary.roles_involved.length > 0
          ? `
        <div class="mt-3 flex items-center gap-2 flex-wrap">
          <span class="text-sm text-gray-500 dark:text-gray-400">Roles:</span>
          ${summary.roles_involved
            .map(
              (role) => `
            <span class="text-xs px-2 py-0.5 rounded ${getRoleBadgeColor(role)}">
              ${escapeHtml(capitalizeFirst(role))}
            </span>
          `
            )
            .join("")}
        </div>
      `
          : ""
      }
    </div>
  `;
}

/**
 * Create a single timeline node
 */
function createTimelineNode(entry: TimelineEntry, index: number): string {
  const time = formatTime(entry.timestamp);
  const date = formatDate(entry.timestamp);
  const durationStr = entry.duration_ms ? formatDuration(entry.duration_ms) : "--";
  const outcomeIcon = getOutcomeIcon(entry.outcome);
  const outcomeColor = getOutcomeColor(entry.outcome);
  const roleBadge = getRoleBadgeColor(entry.role);

  return `
    <div class="relative pl-12 timeline-entry" data-entry-index="${index}">
      <!-- Timeline dot -->
      <div class="absolute left-4 w-4 h-4 rounded-full ${outcomeColor} border-2 border-white dark:border-gray-800 transform -translate-x-1/2"></div>

      <!-- Entry content -->
      <div class="border border-gray-200 dark:border-gray-700 rounded-lg overflow-hidden hover:shadow-md transition-shadow">
        <!-- Summary row (always visible) -->
        <div class="p-3 bg-gray-50 dark:bg-gray-800/50 flex items-center gap-3 cursor-pointer timeline-toggle hover:bg-gray-100 dark:hover:bg-gray-800">
          <span class="toggle-icon text-gray-400 text-sm">▶</span>

          <div class="flex-1 min-w-0">
            <div class="flex items-center gap-2 flex-wrap">
              <span class="text-xs font-mono text-gray-500 dark:text-gray-400">${escapeHtml(time)}</span>
              <span class="text-xs text-gray-400 dark:text-gray-500">${escapeHtml(date)}</span>
              <span class="text-xs px-2 py-0.5 rounded ${roleBadge}">${escapeHtml(capitalizeFirst(entry.role))}</span>
              ${
                entry.issue_number
                  ? `<span class="text-xs font-mono text-blue-600 dark:text-blue-400">#${entry.issue_number}</span>`
                  : ""
              }
              ${
                entry.pr_number
                  ? `<span class="text-xs font-mono text-purple-600 dark:text-purple-400">PR #${entry.pr_number}</span>`
                  : ""
              }
            </div>
            <div class="mt-1 text-sm text-gray-700 dark:text-gray-300">${escapeHtml(entry.action)}</div>
          </div>

          <div class="flex items-center gap-3">
            <span class="text-xs text-gray-500 dark:text-gray-400">${escapeHtml(durationStr)}</span>
            <span class="text-xl">${outcomeIcon}</span>
          </div>
        </div>

        <!-- Expanded details (hidden by default) -->
        <div class="timeline-details hidden p-3 bg-white dark:bg-gray-800 border-t border-gray-200 dark:border-gray-700">
          ${
            entry.prompt_preview
              ? `
            <div class="mb-3">
              <h4 class="text-xs font-semibold text-gray-500 dark:text-gray-400 mb-1">PROMPT</h4>
              <pre class="text-xs bg-gray-100 dark:bg-gray-900 p-2 rounded overflow-x-auto whitespace-pre-wrap break-words max-h-32 overflow-y-auto"><code>${escapeHtml(entry.prompt_preview)}</code></pre>
            </div>
          `
              : ""
          }

          ${
            entry.output_preview
              ? `
            <div class="mb-3">
              <h4 class="text-xs font-semibold text-gray-500 dark:text-gray-400 mb-1">OUTPUT</h4>
              <pre class="text-xs bg-gray-100 dark:bg-gray-900 p-2 rounded overflow-x-auto whitespace-pre-wrap break-words max-h-32 overflow-y-auto"><code>${escapeHtml(entry.output_preview)}</code></pre>
            </div>
          `
              : ""
          }

          ${
            entry.label_before || entry.label_after
              ? `
            <div class="mb-3">
              <h4 class="text-xs font-semibold text-gray-500 dark:text-gray-400 mb-1">LABEL CHANGE</h4>
              <div class="flex items-center gap-2 text-sm">
                ${entry.label_before ? `<span class="px-2 py-0.5 bg-gray-200 dark:bg-gray-700 rounded">${escapeHtml(entry.label_before)}</span>` : "<span class='text-gray-400'>(none)</span>"}
                <span class="text-gray-400">→</span>
                ${entry.label_after ? `<span class="px-2 py-0.5 bg-blue-200 dark:bg-blue-800 rounded">${escapeHtml(entry.label_after)}</span>` : "<span class='text-gray-400'>(none)</span>"}
              </div>
            </div>
          `
              : ""
          }

          <div class="flex gap-4 text-xs text-gray-500 dark:text-gray-400">
            ${entry.tokens ? `<span>Tokens: ${entry.tokens.toLocaleString()}</span>` : ""}
            ${entry.cost ? `<span>Cost: ${formatCurrency(entry.cost)}</span>` : ""}
            ${entry.event_type ? `<span>Event: ${entry.event_type}</span>` : ""}
          </div>
        </div>
      </div>
    </div>
  `;
}

/**
 * Set up timeline node expand/collapse handlers
 */
function setupTimelineHandlers(modal: ModalBuilder): void {
  modal.querySelectorAll(".timeline-toggle").forEach((toggle) => {
    toggle.addEventListener("click", () => {
      const entry = toggle.closest(".timeline-entry");
      const details = entry?.querySelector(".timeline-details");
      const icon = toggle.querySelector(".toggle-icon");

      if (details && icon) {
        details.classList.toggle("hidden");
        icon.textContent = details.classList.contains("hidden") ? "▶" : "▼";
      }
    });
  });
}

// ============================================================================
// Comparison Mode
// ============================================================================

/**
 * Enter comparison mode
 */
function enterCompareMode(modal: ModalBuilder): void {
  isCompareMode = true;

  // Store current data as first timeline
  const firstData = currentData;

  // Prompt user to select second timeline
  modal.setContent(`
    <div class="p-4">
      <div class="mb-4 p-4 bg-purple-50 dark:bg-purple-900/20 rounded-lg">
        <h3 class="font-semibold text-purple-700 dark:text-purple-300">Compare Mode</h3>
        <p class="text-sm text-purple-600 dark:text-purple-400 mt-1">
          Select filters for the second timeline to compare.
        </p>
      </div>

      <div class="grid grid-cols-2 gap-4 mb-4">
        <div class="p-4 border rounded-lg dark:border-gray-700">
          <h4 class="font-semibold text-gray-700 dark:text-gray-300 mb-2">Timeline 1 (Current)</h4>
          <p class="text-sm text-gray-500 dark:text-gray-400">
            ${firstData ? `${firstData.summary.total_prompts} prompts` : "No data"}
          </p>
        </div>
        <div class="p-4 border-2 border-dashed border-purple-300 dark:border-purple-700 rounded-lg">
          <h4 class="font-semibold text-gray-700 dark:text-gray-300 mb-2">Timeline 2</h4>
          <p class="text-sm text-gray-500 dark:text-gray-400">
            Use filters above to select
          </p>
        </div>
      </div>

      <button id="load-comparison-btn" class="w-full py-2 bg-purple-500 hover:bg-purple-600 text-white rounded transition-colors">
        Load Comparison Timeline
      </button>
      <button id="cancel-compare-btn" class="w-full mt-2 py-2 bg-gray-300 hover:bg-gray-400 dark:bg-gray-600 dark:hover:bg-gray-500 rounded transition-colors">
        Cancel
      </button>
    </div>
  `);

  // Update compare button text
  const compareBtn = modal.querySelector("#compare-btn");
  if (compareBtn) {
    compareBtn.textContent = "Exit Compare";
  }

  // Set up handlers
  modal.querySelector("#load-comparison-btn")?.addEventListener("click", async () => {
    const filter = getFilterFromInputs(modal);
    const state = getAppState();
    const workspacePath = state.workspace.getWorkspace();

    if (!workspacePath) return;

    try {
      const entries = await queryTimelineEntries(workspacePath, filter);
      const summary = calculateSummary(entries);
      comparisonData = { entries, summary, filter };

      if (firstData && comparisonData) {
        modal.setContent(createComparisonContent(firstData, comparisonData));
        setupTimelineHandlers(modal);
      }
    } catch (error) {
      logger.error("Failed to load comparison data", error as Error);
      showToast("Failed to load comparison data", "error");
    }
  });

  modal.querySelector("#cancel-compare-btn")?.addEventListener("click", () => {
    exitCompareMode(modal);
  });
}

/**
 * Exit comparison mode
 */
function exitCompareMode(modal: ModalBuilder): void {
  isCompareMode = false;
  comparisonData = null;

  const compareBtn = modal.querySelector("#compare-btn");
  if (compareBtn) {
    compareBtn.textContent = "Compare";
  }

  if (currentData) {
    modal.setContent(createTimelineContent(currentData));
    setupTimelineHandlers(modal);
  }
}

/**
 * Create comparison view content
 */
function createComparisonContent(data1: TimelineData, data2: TimelineData): string {
  return `
    <div class="p-4">
      <!-- Comparison Summary -->
      <div class="grid grid-cols-2 gap-4 mb-6">
        <div class="p-4 border-2 border-blue-200 dark:border-blue-800 rounded-lg">
          <h3 class="font-semibold text-blue-700 dark:text-blue-300 mb-2">Timeline 1</h3>
          ${createComparisonSummary(data1)}
        </div>
        <div class="p-4 border-2 border-purple-200 dark:border-purple-800 rounded-lg">
          <h3 class="font-semibold text-purple-700 dark:text-purple-300 mb-2">Timeline 2</h3>
          ${createComparisonSummary(data2)}
        </div>
      </div>

      <!-- Comparison Metrics -->
      <div class="mb-6 p-4 bg-gray-50 dark:bg-gray-800/50 rounded-lg">
        <h3 class="font-semibold text-gray-700 dark:text-gray-300 mb-3">Comparison</h3>
        <div class="grid grid-cols-4 gap-4 text-center">
          ${createComparisonMetric("Prompts", data1.summary.total_prompts, data2.summary.total_prompts)}
          ${createComparisonMetric("Success Rate", Math.round((data1.summary.success_count / Math.max(data1.summary.total_prompts, 1)) * 100), Math.round((data2.summary.success_count / Math.max(data2.summary.total_prompts, 1)) * 100), "%")}
          ${createComparisonMetric("Duration", Math.round(data1.summary.total_duration_ms / 60000), Math.round(data2.summary.total_duration_ms / 60000), " min")}
          ${createComparisonMetric("Cost", data1.summary.total_cost, data2.summary.total_cost, "", true)}
        </div>
      </div>

      <!-- Side-by-side Timelines -->
      <div class="grid grid-cols-2 gap-4">
        <div>
          <h4 class="font-semibold text-gray-700 dark:text-gray-300 mb-2">Timeline 1</h4>
          <div class="max-h-96 overflow-y-auto">
            ${data1.entries
              .slice(0, 20)
              .map((e, i) => createMiniTimelineNode(e, i, "blue"))
              .join("")}
          </div>
        </div>
        <div>
          <h4 class="font-semibold text-gray-700 dark:text-gray-300 mb-2">Timeline 2</h4>
          <div class="max-h-96 overflow-y-auto">
            ${data2.entries
              .slice(0, 20)
              .map((e, i) => createMiniTimelineNode(e, i, "purple"))
              .join("")}
          </div>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create comparison summary for a timeline
 */
function createComparisonSummary(data: TimelineData): string {
  let filterDesc = "All activity";
  if (data.filter.issue_number) filterDesc = `Issue #${data.filter.issue_number}`;
  else if (data.filter.pr_number) filterDesc = `PR #${data.filter.pr_number}`;

  return `
    <p class="text-sm text-gray-500 dark:text-gray-400">${escapeHtml(filterDesc)}</p>
    <div class="mt-2 text-sm">
      <p>${data.summary.total_prompts} prompts</p>
      <p>${formatDuration(data.summary.total_duration_ms)}</p>
      <p>${formatCurrency(data.summary.total_cost)}</p>
    </div>
  `;
}

/**
 * Create a comparison metric display
 */
function createComparisonMetric(
  label: string,
  val1: number,
  val2: number,
  suffix: string = "",
  isCurrency: boolean = false
): string {
  const diff = val2 - val1;
  const diffPercent = val1 > 0 ? Math.round((diff / val1) * 100) : 0;
  const isPositive = diff > 0;
  const diffColor = isPositive ? "text-red-500" : "text-green-500";
  const diffIcon = isPositive ? "↑" : "↓";

  const format = (v: number) => (isCurrency ? formatCurrency(v) : `${v}${suffix}`);

  return `
    <div>
      <div class="text-xs text-gray-500 dark:text-gray-400">${escapeHtml(label)}</div>
      <div class="flex justify-center gap-2 text-sm">
        <span class="text-blue-600">${escapeHtml(format(val1))}</span>
        <span class="text-gray-400">vs</span>
        <span class="text-purple-600">${escapeHtml(format(val2))}</span>
      </div>
      ${diff !== 0 ? `<div class="text-xs ${diffColor}">${diffIcon} ${Math.abs(diffPercent)}%</div>` : ""}
    </div>
  `;
}

/**
 * Create a mini timeline node for comparison view
 */
function createMiniTimelineNode(
  entry: TimelineEntry,
  _index: number,
  color: "blue" | "purple"
): string {
  const time = formatTime(entry.timestamp);
  const outcomeIcon = getOutcomeIcon(entry.outcome);
  const borderColor = color === "blue" ? "border-blue-200" : "border-purple-200";

  return `
    <div class="p-2 mb-2 border ${borderColor} rounded text-xs">
      <div class="flex items-center justify-between">
        <span class="text-gray-500">${escapeHtml(time)}</span>
        <span>${outcomeIcon}</span>
      </div>
      <div class="font-medium">${escapeHtml(capitalizeFirst(entry.role))}: ${escapeHtml(entry.action)}</div>
    </div>
  `;
}

// ============================================================================
// Export Functions
// ============================================================================

/**
 * Export timeline data
 */
async function exportTimeline(format: "markdown" | "json"): Promise<void> {
  if (!currentData) {
    showToast("No data to export", "error");
    return;
  }

  try {
    let content: string;
    let extension: string;
    let filterName: string;

    if (format === "markdown") {
      content = generateMarkdownExport(currentData);
      extension = "md";
      filterName = "Markdown";
    } else {
      content = JSON.stringify(currentData, null, 2);
      extension = "json";
      filterName = "JSON";
    }

    // Generate filename
    let filename = "activity-timeline";
    if (currentData.filter.issue_number) {
      filename = `issue-${currentData.filter.issue_number}-timeline`;
    } else if (currentData.filter.pr_number) {
      filename = `pr-${currentData.filter.pr_number}-timeline`;
    }

    const filePath = await save({
      defaultPath: `${filename}.${extension}`,
      filters: [{ name: filterName, extensions: [extension] }],
    });

    if (filePath) {
      await writeTextFile(filePath, content);
      showToast(`Timeline exported to ${filePath}`, "success");
    }
  } catch (error) {
    logger.error("Export failed", error as Error);
    showToast(`Export failed: ${error}`, "error");
  }
}

/**
 * Generate markdown export content
 */
function generateMarkdownExport(data: TimelineData): string {
  const lines: string[] = [];

  // Title
  let title = "Activity Timeline";
  if (data.filter.issue_number) {
    title = `Activity Timeline - Issue #${data.filter.issue_number}`;
  } else if (data.filter.pr_number) {
    title = `Activity Timeline - PR #${data.filter.pr_number}`;
  }
  lines.push(`# ${title}`);
  lines.push("");

  // Summary
  lines.push("## Summary");
  lines.push("");
  lines.push(`- **Total Prompts**: ${data.summary.total_prompts}`);
  lines.push(`- **Active Time**: ${formatDuration(data.summary.total_duration_ms)}`);
  lines.push(`- **Estimated Cost**: ${formatCurrency(data.summary.total_cost)}`);
  lines.push(
    `- **Success Rate**: ${data.summary.total_prompts > 0 ? Math.round((data.summary.success_count / data.summary.total_prompts) * 100) : 0}%`
  );
  lines.push(`- **Roles Involved**: ${data.summary.roles_involved.join(", ")}`);

  if (data.summary.first_activity && data.summary.last_activity) {
    lines.push(
      `- **Duration**: ${formatDate(data.summary.first_activity)} to ${formatDate(data.summary.last_activity)}`
    );
  }
  lines.push("");

  // Timeline
  lines.push("## Timeline");
  lines.push("");

  for (const entry of data.entries) {
    const time = formatTime(entry.timestamp);
    const date = formatDate(entry.timestamp);
    const outcome = entry.outcome === "success" ? "✅" : entry.outcome === "failure" ? "❌" : "⏳";
    const duration = entry.duration_ms ? formatDuration(entry.duration_ms) : "--";

    lines.push(`### ${time} ${date} - ${capitalizeFirst(entry.role)}`);
    lines.push("");
    lines.push(`- **Action**: ${entry.action}`);
    lines.push(`- **Outcome**: ${outcome}`);
    lines.push(`- **Duration**: ${duration}`);

    if (entry.issue_number) lines.push(`- **Issue**: #${entry.issue_number}`);
    if (entry.pr_number) lines.push(`- **PR**: #${entry.pr_number}`);
    if (entry.tokens) lines.push(`- **Tokens**: ${entry.tokens.toLocaleString()}`);
    if (entry.cost) lines.push(`- **Cost**: ${formatCurrency(entry.cost)}`);

    if (entry.prompt_preview) {
      lines.push("");
      lines.push("**Prompt:**");
      lines.push("```");
      lines.push(entry.prompt_preview);
      lines.push("```");
    }

    lines.push("");
  }

  lines.push("---");
  lines.push(`*Exported from Loom Activity Playback on ${new Date().toISOString()}*`);

  return lines.join("\n");
}

// ============================================================================
// Utility Functions
// ============================================================================

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function capitalizeFirst(str: string): string {
  return str.charAt(0).toUpperCase() + str.slice(1).toLowerCase();
}

function formatTime(isoString: string): string {
  const date = new Date(isoString);
  return date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

function formatDate(isoString: string): string {
  const date = new Date(isoString);
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) return `${Math.round(ms / 1000)}s`;
  if (ms < 3600000) return `${Math.round(ms / 60000)}m`;
  return `${(ms / 3600000).toFixed(1)}h`;
}

function formatCurrency(amount: number): string {
  return `$${amount.toFixed(2)}`;
}

function getOutcomeIcon(outcome: TimelineEntry["outcome"]): string {
  switch (outcome) {
    case "success":
      return "✅";
    case "failure":
      return "❌";
    case "pending":
      return "⏳";
    case "in_progress":
      return "🔄";
    default:
      return "•";
  }
}

function getOutcomeColor(outcome: TimelineEntry["outcome"]): string {
  switch (outcome) {
    case "success":
      return "bg-green-500";
    case "failure":
      return "bg-red-500";
    case "pending":
      return "bg-yellow-500";
    case "in_progress":
      return "bg-blue-500";
    default:
      return "bg-gray-400";
  }
}

function getRoleBadgeColor(role: string): string {
  const colors: Record<string, string> = {
    builder: "bg-blue-100 dark:bg-blue-900 text-blue-700 dark:text-blue-300",
    judge: "bg-purple-100 dark:bg-purple-900 text-purple-700 dark:text-purple-300",
    curator: "bg-green-100 dark:bg-green-900 text-green-700 dark:text-green-300",
    doctor: "bg-red-100 dark:bg-red-900 text-red-700 dark:text-red-300",
    architect: "bg-indigo-100 dark:bg-indigo-900 text-indigo-700 dark:text-indigo-300",
    hermit: "bg-orange-100 dark:bg-orange-900 text-orange-700 dark:text-orange-300",
    champion: "bg-yellow-100 dark:bg-yellow-900 text-yellow-700 dark:text-yellow-300",
    guide: "bg-teal-100 dark:bg-teal-900 text-teal-700 dark:text-teal-300",
    shepherd: "bg-pink-100 dark:bg-pink-900 text-pink-700 dark:text-pink-300",
  };
  return (
    colors[role.toLowerCase()] || "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300"
  );
}
