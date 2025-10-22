/**
 * Keyboard Shortcuts Modal
 *
 * Displays a modal showing all available keyboard shortcuts organized by category
 * Replaces the old alert() implementation with a styled, themeable modal
 */

export function showKeyboardShortcutsModal(): void {
  const modal = createKeyboardShortcutsModal();
  document.body.appendChild(modal);

  // Show modal
  modal.classList.remove("hidden");

  // Close button
  const closeBtn = modal.querySelector("#close-shortcuts-btn");
  closeBtn?.addEventListener("click", () => modal.remove());

  // Close on background click
  modal.addEventListener("click", (e) => {
    if (e.target === modal) {
      modal.remove();
    }
  });

  // Close on Escape
  const escapeHandler = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      modal.remove();
      document.removeEventListener("keydown", escapeHandler);
    }
  };
  document.addEventListener("keydown", escapeHandler);
}

function createKeyboardShortcutsModal(): HTMLElement {
  const modal = document.createElement("div");
  modal.id = "keyboard-shortcuts-modal";
  modal.className =
    "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50 hidden";

  modal.innerHTML = `
    <div
      class="bg-white dark:bg-gray-800 rounded-lg p-6 w-[600px] max-h-[90vh] flex flex-col border border-gray-200 dark:border-gray-700"
      role="dialog"
      aria-modal="true"
      aria-labelledby="keyboard-shortcuts-title"
    >
      <h2 id="keyboard-shortcuts-title" class="text-xl font-bold mb-4 text-gray-900 dark:text-gray-100">Keyboard Shortcuts</h2>

      <!-- Scrollable content -->
      <div class="overflow-y-auto">
        <!-- File shortcuts -->
        <div class="mb-6">
          <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">File</h3>
          <div class="space-y-2">
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">New Terminal</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+T</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Close Terminal</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+Shift+W</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Close Workspace</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+W</kbd>
            </div>
          </div>
        </div>

        <!-- Edit shortcuts -->
        <div class="mb-6">
          <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">Edit</h3>
          <div class="space-y-2">
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Copy</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+C</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Paste</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+V</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Select All</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+A</kbd>
            </div>
          </div>
        </div>

        <!-- View shortcuts -->
        <div class="mb-6">
          <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">View</h3>
          <div class="space-y-2">
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Toggle Theme</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+Shift+T</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Zoom In</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd++</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Zoom Out</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+-</kbd>
            </div>
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Reset Zoom</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+0</kbd>
            </div>
          </div>
        </div>

        <!-- Help shortcuts -->
        <div class="mb-4">
          <h3 class="text-lg font-semibold mb-3 text-gray-800 dark:text-gray-200">Help</h3>
          <div class="space-y-2">
            <div class="flex justify-between items-center py-1">
              <span class="text-gray-700 dark:text-gray-300">Show Shortcuts</span>
              <kbd class="px-2 py-1 bg-gray-100 dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded text-sm font-mono text-gray-900 dark:text-gray-100">Cmd+/</kbd>
            </div>
          </div>
        </div>
      </div>

      <!-- Close button -->
      <div class="flex justify-end mt-4 pt-4 border-t border-gray-200 dark:border-gray-700">
        <button
          id="close-shortcuts-btn"
          class="px-4 py-2 bg-blue-600 hover:bg-blue-500 rounded text-white font-medium"
        >
          Close
        </button>
      </div>
    </div>
  `;

  return modal;
}
