import { invoke } from "@tauri-apps/api/tauri";
import { saveConfig, saveState, splitTerminals } from "./config";
import { DEFAULT_WORKER_PROMPT, formatPrompt } from "./prompts";
import type { AppState } from "./state";
import { TerminalStatus } from "./state";

export function createWorkerModal(_workspacePath: string): HTMLElement {
  const modal = document.createElement("div");
  modal.id = "worker-modal";
  modal.className =
    "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50 hidden";

  modal.innerHTML = `
    <div class="bg-white dark:bg-gray-800 rounded-lg p-6 w-[700px] max-h-[85vh] flex flex-col border border-gray-200 dark:border-gray-700">
      <h2 class="text-xl font-bold mb-4 text-gray-900 dark:text-gray-100">Add Worker</h2>

      <div class="flex-1 overflow-y-auto space-y-4">
        <!-- Worker Name -->
        <div>
          <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
            Worker Name
          </label>
          <input
            type="text"
            id="worker-name"
            class="w-full px-3 py-2 bg-white dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 text-gray-900 dark:text-gray-100"
            placeholder="Worker 1"
          />
        </div>

        <!-- AI Provider -->
        <div>
          <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
            AI Provider
          </label>
          <div class="text-sm text-gray-500 dark:text-gray-400">
            Claude Code (default)
          </div>
        </div>

        <!-- System Prompt -->
        <div>
          <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
            System Prompt
          </label>
          <textarea
            id="worker-prompt"
            rows="16"
            class="w-full px-3 py-2 bg-white dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 font-mono text-xs text-gray-900 dark:text-gray-100"
            placeholder="System prompt..."
          ></textarea>
          <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">
            This prompt will be sent to Claude Code on startup
          </div>
        </div>
      </div>

      <!-- Buttons -->
      <div class="flex justify-end gap-2 mt-4 pt-4 border-t border-gray-200 dark:border-gray-700">
        <button
          id="cancel-worker-btn"
          class="px-4 py-2 bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 rounded text-gray-900 dark:text-gray-100"
        >
          Cancel
        </button>
        <button
          id="launch-worker-btn"
          class="px-4 py-2 bg-blue-600 hover:bg-blue-500 rounded text-white font-medium"
        >
          Launch Worker
        </button>
      </div>
    </div>
  `;

  return modal;
}

export async function showWorkerModal(state: AppState, renderFn: () => void): Promise<void> {
  // Get workspace path from state
  const workspacePath = state.getWorkspace();
  if (!workspacePath) {
    alert("No workspace selected");
    return;
  }

  // Check prerequisites (warn but don't block)
  try {
    // Check for API key (warn if missing)
    const hasApiKey = await invoke<string>("get_env_var", {
      key: "ANTHROPIC_API_KEY",
    })
      .then(() => true)
      .catch(() => false);

    if (!hasApiKey) {
      console.warn("ANTHROPIC_API_KEY not set in .env file");
      alert(
        "Warning: ANTHROPIC_API_KEY not set in .env file.\n\nWorkers won't function without it. Add the key to your .env file and restart Loom."
      );
    }

    // Check for Claude Code (warn if missing)
    const hasClaudeCode = await invoke<boolean>("check_claude_code");
    if (!hasClaudeCode) {
      console.warn("Claude Code not found in PATH");
      alert(
        "Warning: Claude Code not found in PATH.\n\nWorkers won't function without it. Install with:\nnpm install -g @anthropic-ai/claude-code"
      );
    }

    // Create modal
    const modal = createWorkerModal(workspacePath);
    document.body.appendChild(modal);

    // Pre-fill
    const nameInput = modal.querySelector("#worker-name") as HTMLInputElement;
    const promptTextarea = modal.querySelector("#worker-prompt") as HTMLTextAreaElement;

    const workerCount = state.getTerminals().length + 1;
    nameInput.value = `Worker ${workerCount}`;
    promptTextarea.value = formatPrompt(DEFAULT_WORKER_PROMPT, workspacePath);

    // Show modal
    modal.classList.remove("hidden");
    nameInput.focus();
    nameInput.select();

    // Wire up events
    const cancelBtn = modal.querySelector("#cancel-worker-btn");
    const launchBtn = modal.querySelector("#launch-worker-btn");

    cancelBtn?.addEventListener("click", () => modal.remove());

    launchBtn?.addEventListener("click", async () => {
      await launchWorker(modal, state, workspacePath, renderFn);
    });

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
  } catch (error) {
    alert(`Failed to open worker modal: ${error}`);
  }
}

// Helper to generate next config ID
function generateNextConfigId(state: AppState): string {
  const terminals = state.getTerminals();
  const existingIds = new Set(terminals.map((t) => t.id));

  // Find the next available terminal-N ID
  let i = 1;
  while (existingIds.has(`terminal-${i}`)) {
    i++;
  }

  return `terminal-${i}`;
}

async function launchWorker(
  modal: HTMLElement,
  state: AppState,
  workspacePath: string,
  renderFn: () => void
): Promise<void> {
  const nameInput = modal.querySelector("#worker-name") as HTMLInputElement;
  const promptTextarea = modal.querySelector("#worker-prompt") as HTMLTextAreaElement;

  const name = nameInput.value.trim();
  const prompt = promptTextarea.value.trim();

  if (!name) {
    alert("Please enter a worker name");
    return;
  }

  if (!prompt) {
    alert("Please enter a system prompt");
    return;
  }

  try {
    // Get instance number
    const instanceNumber = state.getNextAgentNumber();

    // Create terminal in workspace directory
    const terminalId = await invoke<string>("create_terminal", {
      name,
      workingDir: workspacePath,
      role: "worker",
      instanceNumber,
    });

    // Generate stable ID
    const id = generateNextConfigId(state);

    // Add to state
    state.addTerminal({
      id,
      name,
      status: TerminalStatus.Busy,
      isPrimary: false,
      role: "worker",
    });

    // Save updated state to config and state files
    const terminals = state.getTerminals();
    const { config: terminalConfigs, state: terminalStates } = splitTerminals(terminals);

    await saveConfig({ terminals: terminalConfigs });
    await saveState({
      nextAgentNumber: state.getCurrentAgentNumber(),
      terminals: terminalStates,
    });

    // Launch Claude Code by sending commands to terminal
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "claude\r",
    });

    // Wait for Claude Code to start
    await new Promise((resolve) => setTimeout(resolve, 1000));

    // Send the prompt
    await invoke("send_terminal_input", {
      id: terminalId,
      data: prompt,
    });

    await invoke("send_terminal_input", {
      id: terminalId,
      data: "\r",
    });

    // Close modal
    modal.remove();

    // Switch to new terminal
    state.setPrimary(id);
    renderFn();
  } catch (error) {
    alert(`Failed to launch worker: ${error}`);
  }
}
