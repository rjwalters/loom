/**
 * Prompt Library Browser Modal
 *
 * Provides a UI for browsing, searching, and reusing successful prompt templates.
 * Users can filter by category, success rate, and search by keyword.
 * Templates can be copied for reuse or viewed in detail with metrics.
 */

import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import {
  calculateTemplateHealth,
  describeTemplate,
  formatSuccessRate,
  formatTemplate,
  getCategoryBadgeClass,
  getHealthColorClass,
  getHealthRating,
  getSuccessRateColorClass,
  getTemplateStats,
  getTemplates,
  type PromptTemplate,
  type TemplateStats,
} from "./template-generation";
import { showToast } from "./toast";

const logger = Logger.forComponent("prompt-library-modal");

// Filter state
interface FilterState {
  category: string | null;
  minSuccessRate: number;
  searchQuery: string;
  sortBy: "most_used" | "highest_success" | "most_recent";
  showRetired: boolean;
}

let filterState: FilterState = {
  category: null,
  minSuccessRate: 0,
  searchQuery: "",
  sortBy: "highest_success",
  showRetired: false,
};

// Pagination state
let currentPage = 0;
const PAGE_SIZE = 10;

// Cache for templates
let cachedTemplates: PromptTemplate[] = [];

/**
 * Show the prompt library browser modal
 */
export async function showPromptLibraryModal(): Promise<void> {
  // Reset state
  filterState = {
    category: null,
    minSuccessRate: 0,
    searchQuery: "",
    sortBy: "highest_success",
    showRetired: false,
  };
  currentPage = 0;
  cachedTemplates = [];

  const modal = new ModalBuilder({
    title: "Prompt Library",
    width: "900px",
    maxHeight: "85vh",
    id: "prompt-library-modal",
  });

  // Show loading state initially
  modal.setContent(createLoadingContent());

  // Add footer button
  modal.addFooterButton("Close", () => modal.close(), "primary");

  modal.show();

  // Load and display templates
  await refreshTemplates(modal);
}

/**
 * Refresh templates data in the modal
 */
async function refreshTemplates(modal: ModalBuilder): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    // Load templates and stats in parallel
    const [templates, stats] = await Promise.all([
      getTemplates(workspacePath, {
        category: filterState.category ?? undefined,
        activeOnly: !filterState.showRetired,
      }),
      getTemplateStats(workspacePath),
    ]);

    cachedTemplates = templates;

    // Apply client-side filtering and sorting
    const filteredTemplates = applyFilters(templates);

    modal.setContent(createLibraryContent(filteredTemplates, stats));
    setupEventHandlers(modal);
  } catch (error) {
    logger.error("Failed to load prompt templates", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Apply filters and sorting to templates
 */
function applyFilters(templates: PromptTemplate[]): PromptTemplate[] {
  let filtered = [...templates];

  // Filter by minimum success rate
  if (filterState.minSuccessRate > 0) {
    filtered = filtered.filter((t) => t.success_rate >= filterState.minSuccessRate / 100);
  }

  // Filter by search query
  if (filterState.searchQuery.trim()) {
    const query = filterState.searchQuery.toLowerCase();
    filtered = filtered.filter(
      (t) =>
        t.template_text.toLowerCase().includes(query) ||
        t.category.toLowerCase().includes(query) ||
        t.description?.toLowerCase().includes(query) ||
        t.placeholders.some((p) => p.toLowerCase().includes(query))
    );
  }

  // Sort
  switch (filterState.sortBy) {
    case "most_used":
      filtered.sort((a, b) => b.times_used - a.times_used);
      break;
    case "highest_success":
      filtered.sort((a, b) => b.success_rate - a.success_rate);
      break;
    case "most_recent":
      filtered.sort((a, b) => {
        const aDate = a.last_used_at ? new Date(a.last_used_at).getTime() : 0;
        const bDate = b.last_used_at ? new Date(b.last_used_at).getTime() : 0;
        return bDate - aDate;
      });
      break;
  }

  return filtered;
}

/**
 * Create loading state content
 */
function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-12">
      <div class="text-gray-500 dark:text-gray-400">Loading prompt templates...</div>
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
      <p class="text-red-700 dark:text-red-300">Failed to load templates: ${escapeHtml(message)}</p>
    </div>
  `;
}

/**
 * Create main library content
 */
function createLibraryContent(templates: PromptTemplate[], stats: TemplateStats): string {
  return `
    <!-- Stats Summary -->
    ${createStatsSummary(stats)}

    <!-- Filters -->
    ${createFiltersSection(stats)}

    <!-- Template List -->
    ${createTemplateList(templates)}
  `;
}

/**
 * Create stats summary cards
 */
function createStatsSummary(stats: TemplateStats): string {
  return `
    <div class="grid grid-cols-2 md:grid-cols-4 gap-3 mb-6">
      ${createStatCard("Total Templates", String(stats.total_templates), "All templates in library")}
      ${createStatCard("Active", String(stats.active_templates), "Templates available for use")}
      ${createStatCard("Retired", String(stats.retired_templates), "Underperforming templates")}
      ${createStatCard("Avg Success", formatSuccessRate(stats.avg_success_rate), "Average success rate", getSuccessRateColorClass(stats.avg_success_rate))}
    </div>
  `;
}

/**
 * Create a stat card
 */
function createStatCard(
  label: string,
  value: string,
  description: string,
  valueColor?: string
): string {
  const colorClass = valueColor ?? "text-gray-900 dark:text-gray-100";
  return `
    <div class="p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-1">${label}</div>
      <div class="text-xl font-bold ${colorClass}">${value}</div>
      <div class="text-xs text-gray-400 dark:text-gray-500 mt-1">${description}</div>
    </div>
  `;
}

/**
 * Create filters section
 */
function createFiltersSection(stats: TemplateStats): string {
  const categories = stats.templates_by_category.map((c) => c.category);

  return `
    <div class="mb-6 p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
      <div class="flex flex-wrap gap-4 items-end">
        <!-- Search -->
        <div class="flex-1 min-w-[200px]">
          <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">Search</label>
          <input
            type="text"
            id="template-search"
            placeholder="Search prompts..."
            value="${escapeHtml(filterState.searchQuery)}"
            class="w-full px-3 py-2 text-sm rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100 focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
          />
        </div>

        <!-- Category Filter -->
        <div class="w-40">
          <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">Category</label>
          <select
            id="template-category"
            class="w-full px-3 py-2 text-sm rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100"
          >
            <option value="">All Categories</option>
            ${categories.map((cat) => `<option value="${cat}" ${filterState.category === cat ? "selected" : ""}>${capitalizeFirst(cat)}</option>`).join("")}
          </select>
        </div>

        <!-- Success Rate Filter -->
        <div class="w-32">
          <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">Min Success</label>
          <select
            id="template-success-rate"
            class="w-full px-3 py-2 text-sm rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100"
          >
            <option value="0" ${filterState.minSuccessRate === 0 ? "selected" : ""}>Any</option>
            <option value="50" ${filterState.minSuccessRate === 50 ? "selected" : ""}>&gt; 50%</option>
            <option value="70" ${filterState.minSuccessRate === 70 ? "selected" : ""}>&gt; 70%</option>
            <option value="80" ${filterState.minSuccessRate === 80 ? "selected" : ""}>&gt; 80%</option>
            <option value="90" ${filterState.minSuccessRate === 90 ? "selected" : ""}>&gt; 90%</option>
          </select>
        </div>

        <!-- Sort By -->
        <div class="w-40">
          <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">Sort By</label>
          <select
            id="template-sort"
            class="w-full px-3 py-2 text-sm rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100"
          >
            <option value="highest_success" ${filterState.sortBy === "highest_success" ? "selected" : ""}>Highest Success</option>
            <option value="most_used" ${filterState.sortBy === "most_used" ? "selected" : ""}>Most Used</option>
            <option value="most_recent" ${filterState.sortBy === "most_recent" ? "selected" : ""}>Most Recent</option>
          </select>
        </div>

        <!-- Show Retired -->
        <div class="flex items-center">
          <label class="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-300 cursor-pointer">
            <input
              type="checkbox"
              id="template-show-retired"
              ${filterState.showRetired ? "checked" : ""}
              class="w-4 h-4 rounded border-gray-300 dark:border-gray-600 text-blue-600 focus:ring-blue-500"
            />
            Show Retired
          </label>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create template list
 */
function createTemplateList(templates: PromptTemplate[]): string {
  if (templates.length === 0) {
    return createEmptyState();
  }

  // Paginate
  const startIndex = currentPage * PAGE_SIZE;
  const endIndex = startIndex + PAGE_SIZE;
  const pageTemplates = templates.slice(startIndex, endIndex);
  const totalPages = Math.ceil(templates.length / PAGE_SIZE);

  return `
    <div class="space-y-3">
      ${pageTemplates.map(createTemplateCard).join("")}
    </div>

    <!-- Pagination -->
    ${totalPages > 1 ? createPagination(templates.length, totalPages) : ""}
  `;
}

/**
 * Create a template card
 */
function createTemplateCard(template: PromptTemplate): string {
  const health = calculateTemplateHealth(template);
  const healthRating = getHealthRating(health);
  const healthColor = getHealthColorClass(healthRating);

  return `
    <div class="template-card p-4 bg-white dark:bg-gray-800 rounded-lg border border-gray-200 dark:border-gray-700 hover:border-blue-300 dark:hover:border-blue-700 transition-colors cursor-pointer" data-template-id="${template.id}">
      <div class="flex items-start justify-between gap-4">
        <div class="flex-1 min-w-0">
          <!-- Category and Status -->
          <div class="flex items-center gap-2 mb-2">
            <span class="px-2 py-0.5 text-xs font-medium rounded ${getCategoryBadgeClass(template.category)}">
              ${capitalizeFirst(template.category)}
            </span>
            ${!template.active ? '<span class="px-2 py-0.5 text-xs font-medium rounded bg-gray-100 dark:bg-gray-700 text-gray-600 dark:text-gray-400">Retired</span>' : ""}
            <span class="text-xs ${healthColor}" title="Template health score">
              ${capitalizeFirst(healthRating)} (${health})
            </span>
          </div>

          <!-- Template Preview -->
          <p class="text-sm text-gray-700 dark:text-gray-300 line-clamp-2 mb-2" title="${escapeHtml(template.template_text)}">
            ${escapeHtml(truncateText(formatTemplate(template), 150))}
          </p>

          <!-- Description -->
          ${template.description ? `<p class="text-xs text-gray-500 dark:text-gray-400 mb-2">${escapeHtml(template.description)}</p>` : ""}

          <!-- Placeholders -->
          ${
            template.placeholders.length > 0
              ? `
            <div class="flex flex-wrap gap-1 mb-2">
              ${template.placeholders.map((p) => `<span class="px-1.5 py-0.5 text-xs bg-blue-50 dark:bg-blue-900/30 text-blue-700 dark:text-blue-300 rounded">{${p}}</span>`).join("")}
            </div>
          `
              : ""
          }

          <!-- Metrics -->
          <div class="flex items-center gap-4 text-xs text-gray-500 dark:text-gray-400">
            <span class="${getSuccessRateColorClass(template.success_rate)} font-medium">
              ${formatSuccessRate(template.success_rate)} success
            </span>
            <span>${template.times_used} uses</span>
            <span>${template.source_pattern_count} source patterns</span>
            ${template.last_used_at ? `<span>Last used: ${formatRelativeDate(template.last_used_at)}</span>` : ""}
          </div>
        </div>

        <!-- Actions -->
        <div class="flex flex-col gap-2">
          <button
            class="copy-template-btn px-3 py-1.5 text-xs font-medium bg-blue-600 hover:bg-blue-500 text-white rounded transition-colors"
            data-template-id="${template.id}"
            title="Copy template to clipboard"
          >
            Copy
          </button>
          <button
            class="view-template-btn px-3 py-1.5 text-xs font-medium bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 text-gray-700 dark:text-gray-300 rounded transition-colors"
            data-template-id="${template.id}"
            title="View full template details"
          >
            Details
          </button>
        </div>
      </div>
    </div>
  `;
}

/**
 * Create pagination controls
 */
function createPagination(totalItems: number, totalPages: number): string {
  const startItem = currentPage * PAGE_SIZE + 1;
  const endItem = Math.min((currentPage + 1) * PAGE_SIZE, totalItems);

  return `
    <div class="flex items-center justify-between mt-6 pt-4 border-t border-gray-200 dark:border-gray-700">
      <span class="text-sm text-gray-500 dark:text-gray-400">
        Showing ${startItem}-${endItem} of ${totalItems} templates
      </span>
      <div class="flex gap-2">
        <button
          id="prev-page-btn"
          class="px-3 py-1.5 text-sm font-medium rounded ${currentPage === 0 ? "bg-gray-100 dark:bg-gray-800 text-gray-400 cursor-not-allowed" : "bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 text-gray-700 dark:text-gray-300"}"
          ${currentPage === 0 ? "disabled" : ""}
        >
          Previous
        </button>
        <span class="px-3 py-1.5 text-sm text-gray-600 dark:text-gray-400">
          Page ${currentPage + 1} of ${totalPages}
        </span>
        <button
          id="next-page-btn"
          class="px-3 py-1.5 text-sm font-medium rounded ${currentPage >= totalPages - 1 ? "bg-gray-100 dark:bg-gray-800 text-gray-400 cursor-not-allowed" : "bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 text-gray-700 dark:text-gray-300"}"
          ${currentPage >= totalPages - 1 ? "disabled" : ""}
        >
          Next
        </button>
      </div>
    </div>
  `;
}

/**
 * Create empty state
 */
function createEmptyState(): string {
  return `
    <div class="text-center py-12">
      <div class="text-4xl mb-4">📚</div>
      <h3 class="text-lg font-medium text-gray-900 dark:text-gray-100 mb-2">No Templates Found</h3>
      <p class="text-sm text-gray-500 dark:text-gray-400 max-w-md mx-auto">
        ${
          filterState.searchQuery || filterState.category || filterState.minSuccessRate > 0
            ? "No templates match your current filters. Try adjusting your search criteria."
            : "No prompt templates have been generated yet. Templates are created automatically from successful prompt patterns as agents work."
        }
      </p>
    </div>
  `;
}

/**
 * Set up event handlers for interactive elements
 */
function setupEventHandlers(modal: ModalBuilder): void {
  // Search input
  const searchInput = modal.querySelector<HTMLInputElement>("#template-search");
  if (searchInput) {
    let debounceTimer: ReturnType<typeof setTimeout> | null = null;
    searchInput.addEventListener("input", () => {
      if (debounceTimer) clearTimeout(debounceTimer);
      debounceTimer = setTimeout(async () => {
        filterState.searchQuery = searchInput.value;
        currentPage = 0;
        await refreshTemplates(modal);
      }, 300);
    });
  }

  // Category filter
  const categorySelect = modal.querySelector<HTMLSelectElement>("#template-category");
  if (categorySelect) {
    categorySelect.addEventListener("change", async () => {
      filterState.category = categorySelect.value || null;
      currentPage = 0;
      await refreshTemplates(modal);
    });
  }

  // Success rate filter
  const successSelect = modal.querySelector<HTMLSelectElement>("#template-success-rate");
  if (successSelect) {
    successSelect.addEventListener("change", async () => {
      filterState.minSuccessRate = parseInt(successSelect.value, 10);
      currentPage = 0;
      await refreshTemplates(modal);
    });
  }

  // Sort filter
  const sortSelect = modal.querySelector<HTMLSelectElement>("#template-sort");
  if (sortSelect) {
    sortSelect.addEventListener("change", async () => {
      filterState.sortBy = sortSelect.value as FilterState["sortBy"];
      currentPage = 0;
      await refreshTemplates(modal);
    });
  }

  // Show retired checkbox
  const retiredCheckbox = modal.querySelector<HTMLInputElement>("#template-show-retired");
  if (retiredCheckbox) {
    retiredCheckbox.addEventListener("change", async () => {
      filterState.showRetired = retiredCheckbox.checked;
      currentPage = 0;
      await refreshTemplates(modal);
    });
  }

  // Pagination
  const prevBtn = modal.querySelector<HTMLButtonElement>("#prev-page-btn");
  if (prevBtn) {
    prevBtn.addEventListener("click", async () => {
      if (currentPage > 0) {
        currentPage--;
        await refreshTemplates(modal);
      }
    });
  }

  const nextBtn = modal.querySelector<HTMLButtonElement>("#next-page-btn");
  if (nextBtn) {
    nextBtn.addEventListener("click", async () => {
      const totalPages = Math.ceil(applyFilters(cachedTemplates).length / PAGE_SIZE);
      if (currentPage < totalPages - 1) {
        currentPage++;
        await refreshTemplates(modal);
      }
    });
  }

  // Copy buttons
  const copyButtons = modal.querySelectorAll<HTMLButtonElement>(".copy-template-btn");
  copyButtons.forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const templateId = parseInt(btn.dataset.templateId ?? "0", 10);
      const template = cachedTemplates.find((t) => t.id === templateId);
      if (template) {
        copyToClipboard(template.template_text);
        showToast("Template copied to clipboard!", "success");
      }
    });
  });

  // View details buttons
  const viewButtons = modal.querySelectorAll<HTMLButtonElement>(".view-template-btn");
  viewButtons.forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const templateId = parseInt(btn.dataset.templateId ?? "0", 10);
      const template = cachedTemplates.find((t) => t.id === templateId);
      if (template) {
        showTemplateDetailsModal(template);
      }
    });
  });

  // Card clicks (same as view details)
  const cards = modal.querySelectorAll<HTMLElement>(".template-card");
  cards.forEach((card) => {
    card.addEventListener("click", () => {
      const templateId = parseInt(card.dataset.templateId ?? "0", 10);
      const template = cachedTemplates.find((t) => t.id === templateId);
      if (template) {
        showTemplateDetailsModal(template);
      }
    });
  });
}

/**
 * Show template details in a nested modal
 */
function showTemplateDetailsModal(template: PromptTemplate): void {
  const health = calculateTemplateHealth(template);
  const healthRating = getHealthRating(health);
  const healthColor = getHealthColorClass(healthRating);

  const detailModal = new ModalBuilder({
    title: "Template Details",
    width: "700px",
    maxHeight: "80vh",
    id: "template-detail-modal",
  });

  detailModal.setContent(`
    <div class="space-y-6">
      <!-- Header -->
      <div class="flex items-center gap-3">
        <span class="px-3 py-1 text-sm font-medium rounded ${getCategoryBadgeClass(template.category)}">
          ${capitalizeFirst(template.category)}
        </span>
        ${!template.active ? '<span class="px-3 py-1 text-sm font-medium rounded bg-gray-100 dark:bg-gray-700 text-gray-600 dark:text-gray-400">Retired</span>' : ""}
        <span class="text-sm ${healthColor}">Health: ${capitalizeFirst(healthRating)} (${health})</span>
      </div>

      <!-- Template Text -->
      <div>
        <h3 class="text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">Template</h3>
        <pre class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700 text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap overflow-x-auto">${escapeHtml(template.template_text)}</pre>
      </div>

      <!-- Placeholders -->
      ${
        template.placeholders.length > 0
          ? `
        <div>
          <h3 class="text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">Placeholders</h3>
          <div class="flex flex-wrap gap-2">
            ${template.placeholders.map((p) => `<span class="px-2 py-1 text-sm bg-blue-50 dark:bg-blue-900/30 text-blue-700 dark:text-blue-300 rounded">{${p}}</span>`).join("")}
          </div>
        </div>
      `
          : ""
      }

      <!-- Description -->
      <div>
        <h3 class="text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">Description</h3>
        <p class="text-sm text-gray-600 dark:text-gray-400">
          ${template.description ?? describeTemplate(template)}
        </p>
      </div>

      <!-- Example -->
      ${
        template.example
          ? `
        <div>
          <h3 class="text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">Example Usage</h3>
          <pre class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700 text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap overflow-x-auto">${escapeHtml(template.example)}</pre>
        </div>
      `
          : ""
      }

      <!-- Metrics -->
      <div>
        <h3 class="text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">Metrics</h3>
        <div class="grid grid-cols-2 md:grid-cols-4 gap-3">
          ${createStatCard("Success Rate", formatSuccessRate(template.success_rate), `${template.success_count} successes, ${template.failure_count} failures`, getSuccessRateColorClass(template.success_rate))}
          ${createStatCard("Times Used", String(template.times_used), "Total instantiations")}
          ${createStatCard("Source Patterns", String(template.source_pattern_count), `${formatSuccessRate(template.source_success_rate)} source success`)}
          ${createStatCard("Retirement Threshold", formatSuccessRate(template.retirement_threshold), "Min success to stay active")}
        </div>
      </div>

      <!-- Timestamps -->
      <div class="text-xs text-gray-500 dark:text-gray-400 pt-4 border-t border-gray-200 dark:border-gray-700">
        ${template.created_at ? `<p>Created: ${formatDate(template.created_at)}</p>` : ""}
        ${template.last_used_at ? `<p>Last used: ${formatDate(template.last_used_at)}</p>` : ""}
      </div>
    </div>
  `);

  detailModal.addFooterButton("Copy Template", () => {
    copyToClipboard(template.template_text);
    showToast("Template copied to clipboard!", "success");
  });

  detailModal.addFooterButton("Close", () => detailModal.close(), "primary");

  detailModal.show();
}

/**
 * Copy text to clipboard
 */
async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch (error) {
    logger.error("Failed to copy to clipboard", error as Error);
    showToast("Failed to copy to clipboard", "error");
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
 * Capitalize first letter
 */
function capitalizeFirst(str: string): string {
  return str.charAt(0).toUpperCase() + str.slice(1);
}

/**
 * Truncate text with ellipsis
 */
function truncateText(text: string, maxLength: number): string {
  if (text.length <= maxLength) return text;
  return `${text.slice(0, maxLength - 3)}...`;
}

/**
 * Format date for display
 */
function formatDate(dateStr: string): string {
  try {
    return new Date(dateStr).toLocaleString();
  } catch {
    return dateStr;
  }
}

/**
 * Format relative date
 */
function formatRelativeDate(dateStr: string): string {
  try {
    const date = new Date(dateStr);
    const now = new Date();
    const diffMs = now.getTime() - date.getTime();
    const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

    if (diffDays === 0) return "Today";
    if (diffDays === 1) return "Yesterday";
    if (diffDays < 7) return `${diffDays} days ago`;
    if (diffDays < 30) return `${Math.floor(diffDays / 7)} weeks ago`;
    if (diffDays < 365) return `${Math.floor(diffDays / 30)} months ago`;
    return `${Math.floor(diffDays / 365)} years ago`;
  } catch {
    return dateStr;
  }
}
