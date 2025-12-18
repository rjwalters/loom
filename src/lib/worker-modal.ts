import { invoke } from "@tauri-apps/api/core";
import { saveCurrentConfiguration } from "./config";
import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import type { AppState } from "./state";
import { TerminalStatus } from "./state";
import { MODAL_RENDER_DELAY_MS } from "./timing-constants";
import { showToast } from "./toast";

const logger = Logger.forComponent("worker-modal");

export async function showWorkerModal(state: AppState, renderFn: () => void): Promise<void> {
  // Get workspace path from state
  const workspacePath = state.workspace.getWorkspace();
  if (!workspacePath) {
    showToast("No workspace selected", "error");
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
      logger.warn("ANTHROPIC_API_KEY not set", { workspacePath });
      showToast(
        "Warning: ANTHROPIC_API_KEY not set in .env file. Workers won't function without it.",
        "error",
        5000
      );
    }

    // Check for Claude Code (warn if missing)
    const hasClaudeCode = await invoke<boolean>("check_claude_code");
    if (!hasClaudeCode) {
      logger.warn("Claude Code not found in PATH", { workspacePath });
      showToast(
        "Warning: Claude Code not found in PATH. Install with: npm install -g @anthropic-ai/claude-code",
        "error",
        5000
      );
    }

    // Load worker.md role file content
    let roleContent = "Error loading worker role file";
    try {
      roleContent = await invoke<string>("read_role_file", {
        workspacePath,
        filename: "worker.md",
      });
    } catch (error) {
      logger.error("Failed to load worker.md role file", error as Error, {
        workspacePath,
      });
    }

    const workerCount = state.terminals.getTerminals().length + 1;
    const defaultName = `Worker ${workerCount}`;

    // Create modal using ModalBuilder
    const modal = new ModalBuilder({
      title: "Add Worker",
      width: "700px",
      id: "worker-modal",
    });

    modal.setContent(createWorkerFormContent(defaultName, roleContent));

    modal.addFooterButton("Cancel", () => modal.close());
    modal.addFooterButton(
      "Launch Worker",
      () => launchWorker(modal, state, workspacePath, renderFn),
      "primary"
    );

    modal.show();

    // Focus and select name input
    const nameInput = modal.querySelector("#worker-name") as HTMLInputElement;
    nameInput?.focus();
    nameInput?.select();
  } catch (error) {
    showToast(`Failed to open worker modal: ${error}`, "error");
  }
}

/**
 * Create the form content for the worker modal
 */
function createWorkerFormContent(defaultName: string, roleContent: string): string {
  return `
    <div class="space-y-4">
      <!-- Worker Name -->
      <div>
        <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
          Worker Name
        </label>
        <input
          type="text"
          id="worker-name"
          value="${escapeHtml(defaultName)}"
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
        >${escapeHtml(roleContent)}</textarea>
        <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">
          This prompt will be sent to Claude Code on startup
        </div>
      </div>
    </div>
  `;
}

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

// Helper to generate next config ID
function generateNextConfigId(state: AppState): string {
  const terminals = state.terminals.getTerminals();
  const existingIds = new Set(terminals.map((t) => t.id));

  // Find the next available terminal-N ID
  let i = 1;
  while (existingIds.has(`terminal-${i}`)) {
    i++;
  }

  return `terminal-${i}`;
}

async function launchWorker(
  modal: ModalBuilder,
  state: AppState,
  workspacePath: string,
  renderFn: () => void
): Promise<void> {
  const nameInput = modal.querySelector("#worker-name") as HTMLInputElement;
  const promptTextarea = modal.querySelector("#worker-prompt") as HTMLTextAreaElement;

  const name = nameInput.value.trim();
  const prompt = promptTextarea.value.trim();

  if (!name) {
    showToast("Please enter a worker name", "error");
    return;
  }

  if (!prompt) {
    showToast("Please enter a system prompt", "error");
    return;
  }

  try {
    // Generate stable ID first
    const id = generateNextConfigId(state);

    // Get instance number
    const instanceNumber = state.terminals.getNextTerminalNumber();

    // Create terminal in workspace directory
    const terminalId = await invoke<string>("create_terminal", {
      configId: id,
      name,
      workingDir: workspacePath,
      role: "worker",
      instanceNumber,
    });

    // Add to state
    state.terminals.addTerminal({
      id,
      name,
      status: TerminalStatus.Busy,
      isPrimary: false,
      role: "worker",
    });

    // Save updated state to config and state files
    await saveCurrentConfiguration(state);

    // Launch Claude Code by sending commands to terminal
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "claude\r",
    });

    // Wait for Claude Code to start
    await new Promise((resolve) => setTimeout(resolve, MODAL_RENDER_DELAY_MS));

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
    modal.close();

    // Switch to new terminal
    state.terminals.setPrimary(id);
    renderFn();
  } catch (error) {
    showToast(`Failed to launch worker: ${error}`, "error");
  }
}
