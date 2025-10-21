/**
 * Terminal Activity Modal
 *
 * Displays a modal showing the activity history for a specific terminal.
 * Shows chronological list of inputs (prompts) and outputs with full details.
 */

import { save } from "@tauri-apps/api/dialog";
import { writeTextFile } from "@tauri-apps/api/fs";
import { invoke } from "@tauri-apps/api/tauri";
import type { ActivityEntry } from "./state";

/**
 * Show the activity modal for a specific terminal
 */
export async function showTerminalActivityModal(
  terminalId: string,
  terminalName: string
): Promise<void> {
  const modal = createActivityModal(terminalId, terminalName);
  document.body.appendChild(modal);
  modal.classList.remove("hidden");

  // Setup event handlers
  setupCloseHandlers(modal, terminalId);

  // Load activity data
  await loadActivityData(modal, terminalId);
}

/**
 * Create the modal DOM structure
 */
function createActivityModal(terminalId: string, terminalName: string): HTMLElement {
  const modal = document.createElement("div");
  modal.id = "terminal-activity-modal";
  modal.className =
    "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50 hidden";
  modal.dataset.terminalId = terminalId;

  modal.innerHTML = `
    <div class="bg-white dark:bg-gray-800 rounded-lg w-[800px] max-h-[90vh] flex flex-col border border-gray-200 dark:border-gray-700">
      <!-- Header -->
      <div class="flex items-center justify-between p-4 border-b border-gray-200 dark:border-gray-700">
        <h2 class="text-xl font-bold text-gray-900 dark:text-gray-100">
          📊 Terminal Activity: ${escapeHtml(terminalName)}
        </h2>
        <div class="flex gap-2">
          <button id="export-csv-btn" class="px-3 py-1 text-sm bg-blue-500 hover:bg-blue-600 text-white rounded transition-colors">
            Export CSV
          </button>
          <button id="export-json-btn" class="px-3 py-1 text-sm bg-blue-500 hover:bg-blue-600 text-white rounded transition-colors">
            Export JSON
          </button>
          <button id="close-activity-btn" class="text-gray-400 hover:text-gray-600 dark:hover:text-gray-300 font-bold text-2xl transition-colors">
            ×
          </button>
        </div>
      </div>

      <!-- Loading state -->
      <div id="activity-loading" class="p-8 text-center text-gray-500 dark:text-gray-400">
        Loading activity...
      </div>

      <!-- Content (will be populated) -->
      <div id="activity-content" class="flex-1 overflow-y-auto p-4 hidden">
        <!-- Timeline entries will go here -->
      </div>

      <!-- Empty state -->
      <div id="activity-empty" class="p-8 text-center text-gray-500 dark:text-gray-400 hidden">
        No activity recorded for this terminal yet.
      </div>
    </div>
  `;

  return modal;
}

/**
 * Load activity data from backend and render it
 */
async function loadActivityData(modal: HTMLElement, terminalId: string): Promise<void> {
  const loadingEl = modal.querySelector("#activity-loading");
  const contentEl = modal.querySelector("#activity-content");
  const emptyEl = modal.querySelector("#activity-empty");

  try {
    const entries = await invoke<ActivityEntry[]>("get_terminal_activity", {
      terminalId,
      limit: 100, // Last 100 entries
    });

    loadingEl?.classList.add("hidden");

    if (entries.length === 0) {
      emptyEl?.classList.remove("hidden");
    } else {
      contentEl?.classList.remove("hidden");
      renderActivityEntries(contentEl as HTMLElement, entries);
    }
  } catch (error) {
    loadingEl?.classList.add("hidden");
    contentEl?.classList.remove("hidden");
    if (contentEl) {
      contentEl.innerHTML = `
        <div class="p-4 bg-red-50 dark:bg-red-900/20 text-red-700 dark:text-red-300 rounded">
          Error loading activity: ${error}
        </div>
      `;
    }
  }
}

/**
 * Render the activity entries timeline
 */
function renderActivityEntries(container: HTMLElement, entries: ActivityEntry[]): void {
  container.innerHTML = entries
    .map((entry, index) => createActivityEntryHTML(entry, index))
    .join("");

  // Setup expand/collapse handlers
  container.querySelectorAll(".activity-entry-toggle").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      const target = e.currentTarget as HTMLElement;
      const entryId = target.dataset.entryId;
      const detailsEl = container.querySelector(`#activity-details-${entryId}`);
      detailsEl?.classList.toggle("hidden");

      const icon = target.querySelector(".toggle-icon");
      if (icon) {
        icon.textContent = detailsEl?.classList.contains("hidden") ? "▶" : "▼";
      }
    });
  });
}

/**
 * Create HTML for a single activity entry
 */
function createActivityEntryHTML(entry: ActivityEntry, index: number): string {
  const timestamp = new Date(entry.timestamp).toLocaleString();
  const inputTypeBadge = getInputTypeBadge(entry.inputType);
  const successIndicator = entry.exitCode !== null ? (entry.exitCode === 0 ? "✓" : "✗") : "⋯";
  const successColor =
    entry.exitCode === 0
      ? "text-green-500"
      : entry.exitCode !== null
        ? "text-red-500"
        : "text-gray-400";

  const promptPreview =
    entry.prompt.length > 100 ? `${entry.prompt.substring(0, 100)}...` : entry.prompt;

  return `
    <div class="mb-4 border border-gray-200 dark:border-gray-700 rounded-lg overflow-hidden">
      <!-- Summary -->
      <div class="p-3 bg-gray-50 dark:bg-gray-700/50 flex items-start gap-3 cursor-pointer activity-entry-toggle hover:bg-gray-100 dark:hover:bg-gray-700 transition-colors" data-entry-id="${index}">
        <span class="toggle-icon text-gray-500 dark:text-gray-400 text-sm">▶</span>
        <div class="flex-1 min-w-0">
          <div class="flex items-center gap-2 mb-1 flex-wrap">
            <span class="text-xs font-mono text-gray-500 dark:text-gray-400">${timestamp}</span>
            ${inputTypeBadge}
            ${entry.agentRole ? `<span class="text-xs text-gray-600 dark:text-gray-400">${escapeHtml(entry.agentRole)}</span>` : ""}
            ${entry.gitBranch ? `<span class="text-xs font-mono text-blue-600 dark:text-blue-400">${escapeHtml(entry.gitBranch)}</span>` : ""}
          </div>
          <div class="text-sm text-gray-700 dark:text-gray-300 truncate">${escapeHtml(promptPreview)}</div>
        </div>
        <span class="${successColor} text-xl font-bold">${successIndicator}</span>
      </div>

      <!-- Details (collapsed by default) -->
      <div id="activity-details-${index}" class="hidden p-3 bg-white dark:bg-gray-800">
        <div class="mb-3">
          <h4 class="text-xs font-semibold text-gray-500 dark:text-gray-400 mb-1">PROMPT</h4>
          <pre class="text-xs bg-gray-100 dark:bg-gray-900 p-2 rounded overflow-x-auto whitespace-pre-wrap break-words"><code>${escapeHtml(entry.prompt)}</code></pre>
        </div>

        ${
          entry.outputPreview
            ? `
          <div>
            <h4 class="text-xs font-semibold text-gray-500 dark:text-gray-400 mb-1">OUTPUT ${entry.exitCode !== null ? `(exit: ${entry.exitCode})` : ""}</h4>
            <pre class="text-xs bg-gray-100 dark:bg-gray-900 p-2 rounded overflow-x-auto whitespace-pre-wrap break-words"><code>${escapeHtml(entry.outputPreview)}</code></pre>
          </div>
        `
            : ""
        }
      </div>
    </div>
  `;
}

/**
 * Get display label for input type
 */
function getInputTypeLabel(type: string): string {
  const labels: Record<string, string> = {
    manual: "Manual",
    autonomous: "Autonomous",
    system: "System",
    user_instruction: "User Instruction",
  };
  return labels[type] || type;
}

/**
 * Get styled badge for input type
 */
function getInputTypeBadge(type: string): string {
  const colors: Record<string, string> = {
    manual: "bg-blue-100 dark:bg-blue-900 text-blue-700 dark:text-blue-300",
    autonomous: "bg-purple-100 dark:bg-purple-900 text-purple-700 dark:text-purple-300",
    system: "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300",
    user_instruction: "bg-green-100 dark:bg-green-900 text-green-700 dark:text-green-300",
  };
  const color = colors[type] || "bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300";

  return `<span class="text-xs px-2 py-0.5 rounded ${color}">${getInputTypeLabel(type)}</span>`;
}

/**
 * Setup close handlers and export handlers
 */
function setupCloseHandlers(modal: HTMLElement, terminalId: string): void {
  const closeBtn = modal.querySelector("#close-activity-btn");
  closeBtn?.addEventListener("click", () => modal.remove());

  modal.addEventListener("click", (e) => {
    if (e.target === modal) {
      modal.remove();
    }
  });

  const escapeHandler = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      modal.remove();
      document.removeEventListener("keydown", escapeHandler);
    }
  };
  document.addEventListener("keydown", escapeHandler);

  // Export handlers
  modal.querySelector("#export-csv-btn")?.addEventListener("click", async () => {
    await exportActivity(terminalId, "csv");
  });

  modal.querySelector("#export-json-btn")?.addEventListener("click", async () => {
    await exportActivity(terminalId, "json");
  });
}

/**
 * Export activity data to CSV or JSON file
 */
async function exportActivity(terminalId: string, format: "csv" | "json"): Promise<void> {
  try {
    const entries = await invoke<ActivityEntry[]>("get_terminal_activity", {
      terminalId,
      limit: 1000, // Export more for file
    });

    let content: string;
    let defaultPath: string;

    if (format === "csv") {
      content = exportToCSV(entries);
      defaultPath = `terminal-${terminalId}-activity.csv`;
    } else {
      content = JSON.stringify(entries, null, 2);
      defaultPath = `terminal-${terminalId}-activity.json`;
    }

    const filePath = await save({
      defaultPath,
      filters: [
        {
          name: format.toUpperCase(),
          extensions: [format],
        },
      ],
    });

    if (filePath) {
      await writeTextFile(filePath, content);
      // TODO: Show success toast instead of alert
      alert(`Activity exported to ${filePath}`);
    }
  } catch (error) {
    alert(`Export failed: ${error}`);
  }
}

/**
 * Convert activity entries to CSV format
 */
function exportToCSV(entries: ActivityEntry[]): string {
  const headers = [
    "Timestamp",
    "Input Type",
    "Prompt",
    "Agent Role",
    "Git Branch",
    "Exit Code",
    "Output Preview",
  ];

  const rows = entries.map((entry) => [
    entry.timestamp,
    entry.inputType,
    `"${entry.prompt.replace(/"/g, '""')}"`, // Escape quotes
    entry.agentRole || "",
    entry.gitBranch || "",
    entry.exitCode?.toString() || "",
    entry.outputPreview ? `"${entry.outputPreview.replace(/"/g, '""')}"` : "",
  ]);

  return [headers.join(","), ...rows.map((row) => row.join(","))].join("\n");
}

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
