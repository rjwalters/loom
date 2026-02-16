/**
 * Prompt Library Modal
 *
 * Displays auto-generated prompt templates from successful patterns.
 * Users can browse templates by category, view success rates,
 * and see template details.
 *
 * Part of Phase 5 (Autonomous Learning) - Issue #2262
 */

import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import { getAppState } from "./state";
import {
  formatTemplateSuccessRate,
  getCategoryColorClass,
  getCategoryDisplayName,
  getTemplateStats,
  getTemplateStatusBadge,
  getTemplates,
  type PromptTemplate,
  type TemplateStats,
} from "./template-library";

const logger = Logger.forComponent("prompt-library-modal");

let currentModal: ModalBuilder | null = null;

/**
 * Show the Prompt Library modal
 */
export async function showPromptLibraryModal(): Promise<void> {
  if (currentModal?.isVisible()) {
    currentModal.close();
  }

  const modal = new ModalBuilder({
    title: "Prompt Library",
    width: "800px",
    maxHeight: "90vh",
    id: "prompt-library-modal",
    onClose: () => {
      currentModal = null;
    },
  });

  currentModal = modal;

  modal.setContent(createLoadingContent());
  modal.addFooterButton("Close", () => modal.close(), "primary");
  modal.show();

  await refreshLibrary(modal);
}

/**
 * Refresh library data
 */
async function refreshLibrary(modal: ModalBuilder, category?: string): Promise<void> {
  const state = getAppState();
  const workspacePath = state.workspace.getWorkspace();

  if (!workspacePath) {
    modal.setContent(createErrorContent("No workspace selected"));
    return;
  }

  try {
    const [stats, templates] = await Promise.all([
      getTemplateStats(workspacePath),
      getTemplates(workspacePath, category, true),
    ]);

    modal.setContent(createLibraryContent(stats, templates, category));
    setupEventHandlers(modal, workspacePath);
  } catch (error) {
    logger.error("Failed to load prompt library", error as Error);
    modal.setContent(createErrorContent(error));
  }
}

/**
 * Set up click handlers for category filters and template details
 */
function setupEventHandlers(modal: ModalBuilder, workspacePath: string): void {
  // Category filter buttons
  const filterButtons = modal.querySelectorAll<HTMLButtonElement>("[data-category-filter]");
  for (const btn of filterButtons) {
    btn.addEventListener("click", async () => {
      const category = btn.dataset.categoryFilter;
      await refreshLibrary(modal, category === "all" ? undefined : category);
    });
  }

  // Template detail buttons
  const detailButtons = modal.querySelectorAll<HTMLButtonElement>("[data-template-id]");
  for (const btn of detailButtons) {
    btn.addEventListener("click", async () => {
      const templateId = parseInt(btn.dataset.templateId ?? "0", 10);
      const templates = await getTemplates(workspacePath);
      const template = templates.find((t) => t.id === templateId);
      if (template) {
        showTemplateDetail(modal, template, workspacePath);
      }
    });
  }
}

/**
 * Show detailed view for a single template
 */
function showTemplateDetail(
  modal: ModalBuilder,
  template: PromptTemplate,
  _workspacePath: string
): void {
  const badge = getTemplateStatusBadge(template);

  modal.setContent(`
    <div class="space-y-4">
      <!-- Back button -->
      <button id="back-to-library" class="flex items-center gap-2 text-sm text-blue-600 dark:text-blue-400 hover:text-blue-800 dark:hover:text-blue-300">
        <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 19l-7-7 7-7" />
        </svg>
        Back to Library
      </button>

      <!-- Template header -->
      <div class="flex items-start justify-between">
        <div>
          <h3 class="text-lg font-semibold text-gray-800 dark:text-gray-200">
            ${escapeHtml(template.description ?? template.category)}
          </h3>
          <div class="flex items-center gap-2 mt-1">
            <span class="text-xs px-2 py-0.5 rounded-full ${getCategoryColorClass(template.category)}">${getCategoryDisplayName(template.category)}</span>
            <span class="text-xs px-2 py-0.5 rounded-full ${badge.colorClass}">${badge.label}</span>
          </div>
        </div>
      </div>

      <!-- Template text -->
      <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <h4 class="text-xs font-medium text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-2">Template</h4>
        <pre class="text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap font-mono">${escapeHtml(template.template_text)}</pre>
      </div>

      <!-- Placeholders -->
      ${
        template.placeholders.length > 0
          ? `
        <div>
          <h4 class="text-xs font-medium text-gray-500 dark:text-gray-400 uppercase tracking-wide mb-2">Placeholders</h4>
          <div class="flex flex-wrap gap-2">
            ${template.placeholders.map((p) => `<span class="text-xs px-2 py-1 bg-blue-50 dark:bg-blue-900/30 text-blue-700 dark:text-blue-300 rounded border border-blue-200 dark:border-blue-700 font-mono">{{${escapeHtml(p)}}}</span>`).join("")}
          </div>
        </div>
      `
          : ""
      }

      <!-- Example -->
      ${
        template.example
          ? `
        <div class="p-4 bg-green-50 dark:bg-green-900/20 rounded-lg border border-green-200 dark:border-green-700">
          <h4 class="text-xs font-medium text-green-700 dark:text-green-400 uppercase tracking-wide mb-2">Example</h4>
          <pre class="text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap">${escapeHtml(template.example)}</pre>
        </div>
      `
          : ""
      }

      <!-- Stats -->
      <div class="grid grid-cols-4 gap-3">
        ${createStatCard("Times Used", String(template.times_used))}
        ${createStatCard("Success Rate", formatTemplateSuccessRate(template))}
        ${createStatCard("Successes", String(template.success_count))}
        ${createStatCard("Source Patterns", String(template.source_pattern_count))}
      </div>

      <!-- Dates -->
      <div class="text-xs text-gray-400 dark:text-gray-500 flex gap-4">
        ${template.created_at ? `<span>Created: ${formatDate(template.created_at)}</span>` : ""}
        ${template.last_used_at ? `<span>Last used: ${formatDate(template.last_used_at)}</span>` : ""}
      </div>
    </div>
  `);

  // Wire back button
  const backBtn = modal.querySelector("#back-to-library");
  if (backBtn) {
    backBtn.addEventListener("click", async () => {
      await refreshLibrary(modal);
    });
  }
}

/**
 * Create the main library content
 */
function createLibraryContent(
  stats: TemplateStats,
  templates: PromptTemplate[],
  activeCategory?: string
): string {
  return `
    <div class="space-y-4">
      <!-- Stats overview -->
      ${createStatsOverview(stats)}

      <!-- Category filters -->
      ${createCategoryFilters(stats, activeCategory)}

      <!-- Template list -->
      ${createTemplateList(templates)}
    </div>
  `;
}

/**
 * Create stats overview cards
 */
function createStatsOverview(stats: TemplateStats): string {
  return `
    <div class="grid grid-cols-4 gap-3 mb-4">
      ${createStatCard("Active", String(stats.active_templates))}
      ${createStatCard("Retired", String(stats.retired_templates))}
      ${createStatCard("Avg Success", stats.avg_success_rate > 0 ? `${Math.round(stats.avg_success_rate * 100)}%` : "N/A")}
      ${createStatCard("At Risk", String(stats.retirement_candidates.length))}
    </div>
  `;
}

/**
 * Create category filter buttons
 */
function createCategoryFilters(stats: TemplateStats, activeCategory?: string): string {
  const categories = stats.templates_by_category;

  if (categories.length === 0) return "";

  const allActive = !activeCategory
    ? "bg-blue-600 text-white"
    : "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300";

  return `
    <div class="flex flex-wrap gap-2 mb-4">
      <button data-category-filter="all" class="text-xs px-3 py-1.5 rounded-full ${allActive} hover:opacity-80 transition-opacity">
        All (${stats.total_templates})
      </button>
      ${categories
        .map((cat) => {
          const isActive = activeCategory === cat.category;
          const btnClass = isActive
            ? "bg-blue-600 text-white"
            : "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300";
          return `
            <button data-category-filter="${escapeHtml(cat.category)}" class="text-xs px-3 py-1.5 rounded-full ${btnClass} hover:opacity-80 transition-opacity">
              ${getCategoryDisplayName(cat.category)} (${cat.count})
            </button>
          `;
        })
        .join("")}
    </div>
  `;
}

/**
 * Create the template list
 */
function createTemplateList(templates: PromptTemplate[]): string {
  if (templates.length === 0) {
    return `
      <div class="p-8 text-center text-gray-500 dark:text-gray-400 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
        <p class="mb-2">No templates found.</p>
        <p class="text-sm">Templates are auto-generated from successful prompt patterns as agents work.</p>
      </div>
    `;
  }

  return `
    <div class="space-y-2">
      ${templates.map((t) => createTemplateRow(t)).join("")}
    </div>
  `;
}

/**
 * Create a single template row
 */
function createTemplateRow(template: PromptTemplate): string {
  const badge = getTemplateStatusBadge(template);
  const preview =
    template.description ??
    template.template_text.substring(0, 80) + (template.template_text.length > 80 ? "..." : "");

  return `
    <button data-template-id="${template.id}" class="w-full text-left p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700 hover:border-blue-300 dark:hover:border-blue-600 transition-colors">
      <div class="flex items-start justify-between">
        <div class="flex-1 min-w-0">
          <div class="flex items-center gap-2 mb-1">
            <span class="text-xs px-2 py-0.5 rounded-full ${getCategoryColorClass(template.category)}">${getCategoryDisplayName(template.category)}</span>
            <span class="text-xs px-2 py-0.5 rounded-full ${badge.colorClass}">${badge.label}</span>
          </div>
          <p class="text-sm text-gray-800 dark:text-gray-200 truncate">${escapeHtml(preview)}</p>
        </div>
        <div class="text-right ml-4 flex-shrink-0">
          <div class="text-sm font-medium text-gray-800 dark:text-gray-200">${formatTemplateSuccessRate(template)}</div>
          <div class="text-xs text-gray-400">${template.times_used} uses</div>
        </div>
      </div>
    </button>
  `;
}

/**
 * Create a small stat card
 */
function createStatCard(label: string, value: string): string {
  return `
    <div class="p-3 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700 text-center">
      <div class="text-xs text-gray-500 dark:text-gray-400 uppercase tracking-wide">${label}</div>
      <div class="text-lg font-bold text-gray-900 dark:text-gray-100 mt-1">${escapeHtml(value)}</div>
    </div>
  `;
}

function createLoadingContent(): string {
  return `
    <div class="flex items-center justify-center py-16">
      <div class="flex flex-col items-center gap-4">
        <div class="animate-spin h-8 w-8 border-4 border-blue-500 border-t-transparent rounded-full"></div>
        <span class="text-gray-500 dark:text-gray-400">Loading prompt library...</span>
      </div>
    </div>
  `;
}

function createErrorContent(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return `
    <div class="p-6 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
      <h3 class="text-lg font-semibold text-red-700 dark:text-red-300">Failed to load library</h3>
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
