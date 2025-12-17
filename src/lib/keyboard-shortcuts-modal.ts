/**
 * Keyboard Shortcuts Modal
 *
 * Displays a modal showing all available keyboard shortcuts organized by category
 * Replaces the old alert() implementation with a styled, themeable modal
 */

import { ModalBuilder } from "./modal-builder";

export function showKeyboardShortcutsModal(): void {
  const modal = new ModalBuilder({
    title: "Keyboard Shortcuts",
    width: "600px",
    id: "keyboard-shortcuts-modal",
  });

  modal.setContent(createShortcutsContent());
  modal.addFooterButton("Close", () => modal.close(), "primary");
  modal.show();
}

function createShortcutsContent(): string {
  return `
    <!-- File shortcuts -->
    <div class="mb-6">
      <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">File</h3>
      <div class="space-y-2">
        ${shortcutRow("New Terminal", "Cmd+T")}
        ${shortcutRow("Close Terminal", "Cmd+Shift+W")}
        ${shortcutRow("Close Workspace", "Cmd+W")}
      </div>
    </div>

    <!-- Edit shortcuts -->
    <div class="mb-6">
      <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">Edit</h3>
      <div class="space-y-2">
        ${shortcutRow("Copy", "Cmd+C")}
        ${shortcutRow("Paste", "Cmd+V")}
        ${shortcutRow("Select All", "Cmd+A")}
      </div>
    </div>

    <!-- View shortcuts -->
    <div class="mb-6">
      <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">View</h3>
      <div class="space-y-2">
        ${shortcutRow("Toggle Theme", "Cmd+Shift+T")}
        ${shortcutRow("Zoom In", "Cmd++")}
        ${shortcutRow("Zoom Out", "Cmd+-")}
        ${shortcutRow("Reset Zoom", "Cmd+0")}
      </div>
    </div>

    <!-- Help shortcuts -->
    <div class="mb-4">
      <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">Help</h3>
      <div class="space-y-2">
        ${shortcutRow("Show Shortcuts", "Cmd+/")}
      </div>
    </div>
  `;
}

function shortcutRow(label: string, shortcut: string): string {
  return `
    <div class="flex justify-between items-center py-1">
      <span class="text-gray-700 dark:text-gray-300">${label}</span>
      <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">${shortcut}</kbd>
    </div>
  `;
}
