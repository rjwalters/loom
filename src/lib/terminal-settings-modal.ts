import { saveCurrentConfiguration } from "./config";
import { Logger } from "./logger";
import type { AppState, Terminal } from "./state";
import { TERMINAL_THEMES } from "./themes";
import { showToast } from "./toast";

const logger = Logger.forComponent("terminal-settings-modal");

export function createTerminalSettingsModal(terminal: Terminal): HTMLElement {
  const modal = document.createElement("div");
  modal.id = "terminal-settings-modal";
  modal.className =
    "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50 hidden";

  // Determine current role config
  const roleConfig = terminal.roleConfig || {};
  const targetIntervalMs = (roleConfig.targetInterval as number) || 300000; // 5 minutes default
  const targetIntervalSeconds = Math.floor(targetIntervalMs / 1000); // Convert to seconds for display
  const intervalPrompt = (roleConfig.intervalPrompt as string) || "Continue working on open tasks";
  const autonomousEnabled = targetIntervalMs > 0;

  modal.innerHTML = `
    <div class="bg-white dark:bg-gray-800 rounded-lg p-6 w-[800px] min-w-[600px] max-h-[90vh] flex flex-col border border-gray-200 dark:border-gray-700">
      <h2 class="text-xl font-bold mb-4 text-gray-900 dark:text-gray-100">Terminal Settings: ${escapeHtml(terminal.name)}</h2>

      <!-- Tabs -->
      <div class="flex border-b border-gray-200 dark:border-gray-700 mb-4">
        <button
          data-tab="appearance"
          class="tab-btn px-4 py-2 font-medium text-blue-600 dark:text-blue-400 border-b-2 border-blue-600 dark:border-blue-400"
        >
          Appearance
        </button>
        <button
          data-tab="agent"
          class="tab-btn px-4 py-2 font-medium text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300"
        >
          Agent
        </button>
        <button
          data-tab="interval"
          class="tab-btn px-4 py-2 font-medium text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300"
        >
          Interval Mode
        </button>
      </div>

      <!-- Tab Content Container -->
      <div class="overflow-y-auto h-[420px]">
        <!-- Appearance Tab -->
        <div data-tab-content="appearance" class="space-y-4">
          <div>
            <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
              Terminal Name
            </label>
            <input
              type="text"
              id="terminal-name"
              value="${escapeHtml(terminal.name)}"
              class="w-full px-3 py-2 bg-white dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 text-gray-900 dark:text-gray-100"
              placeholder="Terminal 1"
            />
          </div>

          <div>
            <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
              Color Theme
            </label>
            <p class="text-sm text-gray-600 dark:text-gray-400 mb-3">
              Choose a color theme to visually distinguish this terminal
            </p>
            <div class="grid grid-cols-4 gap-2">
              ${Object.entries(TERMINAL_THEMES)
                .map(
                  ([id, theme]) => `
                <button
                  class="theme-card flex flex-col items-center gap-1 p-2 bg-white dark:bg-gray-700 hover:bg-gray-50 dark:hover:bg-gray-600 rounded-lg border-2 ${terminal.theme === id ? "border-blue-500" : "border-gray-300 dark:border-gray-600"} transition-all cursor-pointer"
                  data-theme-id="${id}"
                >
                  <div class="w-8 h-8 rounded" style="background-color: ${theme.primary}"></div>
                  <span class="text-xs font-medium text-gray-700 dark:text-gray-300">${theme.name}</span>
                </button>
              `
                )
                .join("")}
            </div>
          </div>
        </div>

        <!-- Agent Configuration Tab -->
        <div data-tab-content="agent" class="space-y-4 hidden">
          <div>
            <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
              Role
            </label>
            <select
              id="role-file"
              class="w-full px-3 py-2 bg-transparent border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:border-blue-500 dark:focus:border-blue-400 text-gray-900 dark:text-gray-100"
            >
              <option value="">Loading roles...</option>
            </select>
            <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">
              Role files are stored in <code class="px-1 py-0.5 bg-gray-100 dark:bg-gray-800 rounded font-mono text-xs">.loom/roles/</code>
            </div>
          </div>

          <div>
            <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
              Worker Type
            </label>
            <select
              id="worker-type-select"
              class="w-full px-3 py-2 bg-transparent border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:border-blue-500 dark:focus:border-blue-400 text-gray-900 dark:text-gray-100"
            >
              <option value="">Loading available agents...</option>
            </select>
            <p class="text-xs text-gray-500 dark:text-gray-400 mt-2">
              Only installed AI coding agents are shown
            </p>
          </div>
        </div>

        <!-- Interval Mode Tab -->
        <div data-tab-content="interval" class="space-y-4 hidden">
          <div>
            <label class="flex items-center">
              <input
                type="checkbox"
                id="autonomous-enabled"
                ${autonomousEnabled ? "checked" : ""}
                class="mr-2"
              />
              <span class="text-sm font-medium text-gray-700 dark:text-gray-300">
                Enable Autonomous Operation
              </span>
            </label>
            <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">
              Worker will automatically continue working at specified intervals
            </div>
          </div>

          <div id="autonomous-config" ${autonomousEnabled ? "" : 'class="opacity-50 pointer-events-none"'}>
            <div class="mb-4">
              <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
                Target Interval (seconds)
              </label>
              <input
                type="number"
                id="target-interval"
                value="${targetIntervalSeconds}"
                min="0"
                step="1"
                class="w-full px-3 py-2 bg-white dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 text-gray-900 dark:text-gray-100"
              />
              <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">
                Default: 300 (5 minutes)
              </div>
            </div>

            <div>
              <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
                Interval Prompt
              </label>
              <textarea
                id="interval-prompt"
                rows="4"
                class="w-full px-3 py-2 bg-white dark:bg-gray-700 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 font-mono text-xs text-gray-900 dark:text-gray-100"
                placeholder="Enter prompt to send at each interval..."
              >${escapeHtml(intervalPrompt)}</textarea>
              <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">
                This message will be sent to the worker at each interval
              </div>
            </div>
          </div>
        </div>
      </div>

      <!-- Buttons -->
      <div class="flex justify-end items-center gap-2 mt-4 pt-4 border-t border-gray-200 dark:border-gray-700">
        <button
          id="cancel-settings-btn"
          class="px-4 py-2 bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 rounded text-gray-900 dark:text-gray-100"
        >
          Cancel
        </button>
        <button
          id="apply-settings-btn"
          class="px-4 py-2 bg-blue-600 hover:bg-blue-500 rounded text-white font-medium"
        >
          Apply
        </button>
      </div>
    </div>
  `;

  return modal;
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

export async function showTerminalSettingsModal(
  terminal: Terminal,
  state: AppState,
  renderFn: () => void
): Promise<void> {
  const modal = createTerminalSettingsModal(terminal);
  document.body.appendChild(modal);

  // Show modal
  modal.classList.remove("hidden");

  // Load available worker types
  const workerTypeSelect = modal.querySelector("#worker-type-select") as HTMLSelectElement;
  const roleConfig = terminal.roleConfig || {};
  const currentWorkerType = (roleConfig.workerType as string) || "claude";

  try {
    const { getAvailableWorkerTypes } = await import("./dependency-checker");
    const availableWorkers = await getAvailableWorkerTypes();

    if (availableWorkers.length > 0) {
      workerTypeSelect.innerHTML = availableWorkers
        .map((worker) => {
          const selected = worker.value === currentWorkerType ? "selected" : "";
          return `<option value="${worker.value}" ${selected}>${worker.label}</option>`;
        })
        .join("");
    } else {
      workerTypeSelect.innerHTML = '<option value="">No agents available</option>';
    }
  } catch (error) {
    logger.error("Failed to load available worker types", error as Error);
    workerTypeSelect.innerHTML = '<option value="">Error loading agents</option>';
  }

  // Load available role files
  const workspacePath = state.getWorkspace();
  if (workspacePath) {
    try {
      const { invoke } = await import("@tauri-apps/api/tauri");
      const roleFiles = await invoke<string[]>("list_role_files", { workspacePath });

      const roleFileSelect = modal.querySelector("#role-file") as HTMLSelectElement;
      if (roleFileSelect && roleFiles.length > 0) {
        const roleConfig = terminal.roleConfig || {};
        const currentRoleFile = (roleConfig.roleFile as string) || "worker.md";

        roleFileSelect.innerHTML = roleFiles
          .map((file) => {
            const selected = file === currentRoleFile ? "selected" : "";
            return `<option value="${escapeHtml(file)}" ${selected}>${escapeHtml(file)}</option>`;
          })
          .join("");
      } else if (roleFileSelect) {
        roleFileSelect.innerHTML = '<option value="">No role files found</option>';
      }
    } catch (error) {
      logger.error("Failed to load role files", error as Error, {
        workspacePath: workspacePath || "unknown",
      });
      const roleFileSelect = modal.querySelector("#role-file") as HTMLSelectElement;
      if (roleFileSelect) {
        roleFileSelect.innerHTML = '<option value="">Error loading roles</option>';
      }
    }
  }

  // Wire up tab switching
  const tabBtns = modal.querySelectorAll(".tab-btn");
  const tabContents = modal.querySelectorAll("[data-tab-content]");

  tabBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const tabName = btn.getAttribute("data-tab");

      // Update button styles
      tabBtns.forEach((b) => {
        if (b.getAttribute("data-tab") === tabName) {
          b.className =
            "tab-btn px-4 py-2 font-medium text-blue-600 dark:text-blue-400 border-b-2 border-blue-600 dark:border-blue-400";
        } else {
          b.className =
            "tab-btn px-4 py-2 font-medium text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300";
        }
      });

      // Show/hide content
      tabContents.forEach((content) => {
        if (content.getAttribute("data-tab-content") === tabName) {
          content.classList.remove("hidden");
        } else {
          content.classList.add("hidden");
        }
      });
    });
  });

  // Wire up theme cards
  let selectedTheme = terminal.theme || "default";
  modal.querySelectorAll(".theme-card").forEach((card) => {
    card.addEventListener("click", () => {
      const themeId = card.getAttribute("data-theme-id");
      if (themeId) {
        selectedTheme = themeId;
        // Update visual selection
        modal.querySelectorAll(".theme-card").forEach((c) => {
          c.className = c.className.replace(
            "border-blue-500",
            "border-gray-300 dark:border-gray-600"
          );
        });
        card.className = card.className.replace(
          "border-gray-300 dark:border-gray-600",
          "border-blue-500"
        );
      }
    });
  });

  // Wire up role file dropdown to load metadata
  const roleFileSelect = modal.querySelector("#role-file") as HTMLSelectElement;
  const targetIntervalInput = modal.querySelector("#target-interval") as HTMLInputElement;
  const intervalPromptTextarea = modal.querySelector("#interval-prompt") as HTMLTextAreaElement;
  const autonomousCheckbox = modal.querySelector("#autonomous-enabled") as HTMLInputElement;
  const autonomousConfig = modal.querySelector("#autonomous-config") as HTMLElement;

  roleFileSelect?.addEventListener("change", async () => {
    const selectedFile = roleFileSelect.value;
    if (!selectedFile || !workspacePath) return;

    // Auto-assign theme based on role file
    const { getThemeForRole } = await import("./themes");
    const autoTheme = getThemeForRole(selectedFile);
    selectedTheme = autoTheme;

    // Update theme card selection in UI
    modal.querySelectorAll(".theme-card").forEach((c) => {
      const themeId = c.getAttribute("data-theme-id");
      if (themeId === autoTheme) {
        c.className = c.className.replace(
          "border-gray-300 dark:border-gray-600",
          "border-blue-500"
        );
      } else {
        c.className = c.className.replace(
          "border-blue-500",
          "border-gray-300 dark:border-gray-600"
        );
      }
    });

    try {
      const { invoke } = await import("@tauri-apps/api/tauri");
      const metadataJson = await invoke<string | null>("read_role_metadata", {
        workspacePath,
        filename: selectedFile,
      });

      if (metadataJson) {
        const metadata = JSON.parse(metadataJson) as {
          defaultInterval?: number;
          defaultIntervalPrompt?: string;
          autonomousRecommended?: boolean;
        };

        // Populate interval settings from metadata (convert ms to seconds)
        if (metadata.defaultInterval !== undefined) {
          targetIntervalInput.value = Math.floor(metadata.defaultInterval / 1000).toString();
        }
        if (metadata.defaultIntervalPrompt !== undefined) {
          intervalPromptTextarea.value = metadata.defaultIntervalPrompt;
        }
        if (metadata.autonomousRecommended !== undefined) {
          autonomousCheckbox.checked = metadata.autonomousRecommended;
          // Trigger change event to update UI
          autonomousCheckbox.dispatchEvent(new Event("change"));
        }
      }
    } catch (error) {
      logger.error("Failed to load role metadata", error as Error, {
        workspacePath: workspacePath || "unknown",
        roleFile: selectedFile,
      });
    }
  });

  // Wire up autonomous checkbox to enable/disable config
  autonomousCheckbox?.addEventListener("change", () => {
    if (autonomousCheckbox.checked) {
      autonomousConfig?.classList.remove("opacity-50", "pointer-events-none");
    } else {
      autonomousConfig?.classList.add("opacity-50", "pointer-events-none");
    }
  });

  // Worker type dropdown is now handled in applySettings

  // Wire up buttons
  const cancelBtn = modal.querySelector("#cancel-settings-btn");
  const applyBtn = modal.querySelector("#apply-settings-btn");

  cancelBtn?.addEventListener("click", () => modal.remove());

  applyBtn?.addEventListener("click", async () => {
    await applySettings(modal, terminal, state, renderFn, selectedTheme);
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
}

async function applySettings(
  modal: HTMLElement,
  terminal: Terminal,
  state: AppState,
  renderFn: () => void,
  selectedTheme: string
): Promise<void> {
  try {
    // Get values from form
    const nameInput = modal.querySelector("#terminal-name") as HTMLInputElement;
    const roleFileSelect = modal.querySelector("#role-file") as HTMLSelectElement;
    const workerTypeSelect = modal.querySelector("#worker-type-select") as HTMLSelectElement;
    const autonomousCheckbox = modal.querySelector("#autonomous-enabled") as HTMLInputElement;
    const targetIntervalInput = modal.querySelector("#target-interval") as HTMLInputElement;
    const intervalPromptTextarea = modal.querySelector("#interval-prompt") as HTMLTextAreaElement;

    const name = nameInput.value.trim();
    const roleFile = roleFileSelect.value;

    if (!name) {
      showToast("Please enter a terminal name", "error");
      return;
    }

    // Get current worker type from dropdown
    const workerType = workerTypeSelect?.value || "claude";

    // Determine role based on role file selection
    const role = roleFile ? "claude-code-worker" : undefined;

    // Build role config (convert seconds to milliseconds for storage)
    const roleConfig = roleFile
      ? {
          workerType,
          roleFile,
          targetInterval: autonomousCheckbox.checked
            ? Number.parseInt(targetIntervalInput.value, 10) * 1000
            : 0,
          intervalPrompt: intervalPromptTextarea.value.trim(),
        }
      : undefined;

    // Check if role changed and we need to launch agent
    const previousRole = terminal.role;
    const roleChanged = previousRole !== role;
    const hasNewRole = role !== undefined && roleConfig !== undefined;

    // Update terminal in state (use configId for state operations)
    state.updateTerminal(terminal.id, { name });
    state.setTerminalRole(terminal.id, role, roleConfig);
    state.setTerminalTheme(terminal.id, selectedTheme);

    // Save config and state files
    await saveCurrentConfiguration(state);

    // Launch agent if role was set/changed
    if (roleChanged && hasNewRole) {
      const workspacePath = state.getWorkspace();
      if (workspacePath && roleConfig.roleFile) {
        try {
          if (workerType === "github-copilot") {
            // Launch GitHub Copilot (no worktree support for now)
            const { launchGitHubCopilotAgent } = await import("./agent-launcher");
            await launchGitHubCopilotAgent(terminal.id);
          } else if (workerType === "gemini") {
            // Launch Google Gemini CLI (no worktree support for now)
            const { launchGeminiCLIAgent } = await import("./agent-launcher");
            await launchGeminiCLIAgent(terminal.id);
          } else if (workerType === "deepseek") {
            // Launch DeepSeek CLI (no worktree support for now)
            const { launchDeepSeekAgent } = await import("./agent-launcher");
            await launchDeepSeekAgent(terminal.id);
          } else if (workerType === "grok") {
            // Launch xAI Grok CLI (no worktree support for now)
            const { launchGrokAgent } = await import("./agent-launcher");
            await launchGrokAgent(terminal.id);
          } else {
            // Launch Claude or Codex with full worktree support
            const { launchAgentInTerminal } = await import("./agent-launcher");

            // Load role metadata to get git identity
            const { invoke } = await import("@tauri-apps/api/tauri");
            let gitIdentity: { name: string; email: string } | undefined;

            try {
              const metadataJson = await invoke<string | null>("read_role_metadata", {
                workspacePath,
                filename: roleConfig.roleFile,
              });

              if (metadataJson) {
                const metadata = JSON.parse(metadataJson) as {
                  gitIdentity?: { name: string; email: string };
                };
                gitIdentity = metadata.gitIdentity;
              }
            } catch (error) {
              logger.warn("Failed to load git identity from role metadata", {
                workspacePath,
                roleFile: roleConfig.roleFile,
                error: String(error),
              });
            }

            // Verify terminal has worktree, create if it doesn't exist yet
            let worktreePath = terminal.worktreePath;
            if (!worktreePath) {
              logger.info("Terminal missing worktree, creating now", {
                terminalId: terminal.id,
                terminalName: terminal.name,
              });
              const { setupWorktreeForAgent } = await import("./worktree-manager");
              worktreePath = await setupWorktreeForAgent(terminal.id, workspacePath, gitIdentity);
              // Store worktree path in terminal state
              state.updateTerminal(terminal.id, { worktreePath });
            }

            // Launch agent using existing/new worktree
            await launchAgentInTerminal(
              terminal.id,
              roleConfig.roleFile as string,
              workspacePath,
              worktreePath
            );
          }
        } catch (error) {
          logger.error("Failed to launch agent", error as Error, {
            terminalId: terminal.id,
            workspacePath,
            workerType,
          });
          showToast(`Failed to launch agent: ${error}`, "error");
        }
      }
    }

    // Handle autonomous mode based on configuration
    const { getAutonomousManager } = await import("./autonomous-manager");
    const autonomousManager = getAutonomousManager();

    if (hasNewRole && roleConfig.targetInterval && (roleConfig.targetInterval as number) > 0) {
      // Start or restart autonomous mode
      const updatedTerminal = state.getTerminal(terminal.id);
      if (updatedTerminal) {
        await autonomousManager.restartAutonomous(updatedTerminal);
      }
    } else {
      // Stop autonomous mode if disabled
      await autonomousManager.stopAutonomous(terminal.id);
    }

    // Close modal and re-render
    modal.remove();
    renderFn();
  } catch (error) {
    showToast(`Failed to apply settings: ${error}`, "error");
  }
}
