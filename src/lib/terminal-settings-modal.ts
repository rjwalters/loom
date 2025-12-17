import { saveCurrentConfiguration } from "./config";
import { Logger } from "./logger";
import { ModalBuilder } from "./modal-builder";
import type { AppState, Terminal } from "./state";
import { TERMINAL_THEMES } from "./themes";
import { showToast } from "./toast";

const logger = Logger.forComponent("terminal-settings-modal");

// Role display metadata (maps role file to display info)
const ROLE_INFO: Record<
  string,
  {
    archetype: string;
    description: string;
    workflow: string;
    cardFile: string;
  }
> = {
  "builder.md": {
    archetype: "The Magician",
    description:
      "Transforms ideas into reality through implementation and testing. Claims loom:issue and creates PRs.",
    workflow: "Claims loom:issue → implements → tests → creates PR",
    cardFile: "worker.svg",
  },
  "curator.md": {
    archetype: "The High Priestess",
    description:
      "Refines chaos into clarity by enhancing issues with context and acceptance criteria.",
    workflow: "Finds unlabeled issues → enhances → marks as loom:curated",
    cardFile: "curator.svg",
  },
  "architect.md": {
    archetype: "The Emperor",
    description: "Envisions system architecture and creates technical proposals for improvements.",
    workflow: "Analyzes codebase → creates proposals → marks loom:architect",
    cardFile: "architect.svg",
  },
  "judge.md": {
    archetype: "Justice",
    description: "Maintains quality through thorough code review and constructive feedback.",
    workflow: "Finds loom:review-requested → reviews → approves or requests changes",
    cardFile: "reviewer.svg",
  },
  "hermit.md": {
    archetype: "The Hermit",
    description: "Questions assumptions and identifies code simplification opportunities.",
    workflow: "Analyzes complexity → creates removal proposals → marks loom:hermit",
    cardFile: "critic.svg",
  },
  "doctor.md": {
    archetype: "The Hanged Man",
    description: "Heals bugs and addresses PR feedback with patience and alternative perspectives.",
    workflow: "Finds loom:changes-requested → addresses feedback → updates PR",
    cardFile: "fixer.svg",
  },
  "guide.md": {
    archetype: "The Star",
    description: "Illuminates priorities by focusing team energy on critical issues.",
    workflow: "Reviews backlog → updates priorities → manages loom:urgent label",
    cardFile: "guide.svg",
  },
  "driver.md": {
    archetype: "The Chariot",
    description: "Represents human agency and direct control through manual command execution.",
    workflow: "Plain shell environment for custom tasks and ad-hoc work",
    cardFile: "driver.svg",
  },
  "champion.md": {
    archetype: "The Sun",
    description: "Auto-merges safe PRs that pass all safety criteria and quality checks.",
    workflow: "Finds loom:pr → verifies safety → auto-merges if safe",
    cardFile: "champion.svg",
  },
};

export async function showTerminalSettingsModal(
  terminal: Terminal,
  state: AppState,
  renderFn: () => void
): Promise<void> {
  const roleConfig = terminal.roleConfig || {};
  const targetIntervalMs = (roleConfig.targetInterval as number) || 300000;
  const targetIntervalSeconds = Math.floor(targetIntervalMs / 1000);
  const intervalPrompt = (roleConfig.intervalPrompt as string) || "Continue working on open tasks";
  const autonomousEnabled = targetIntervalMs > 0;

  const modal = new ModalBuilder({
    title: `Terminal Settings: ${terminal.name}`,
    width: "800px",
    id: "terminal-settings-modal",
  });

  modal.setContent(
    createSettingsContent(terminal, targetIntervalSeconds, intervalPrompt, autonomousEnabled)
  );

  modal.addFooterButton("Cancel", () => modal.close());

  // Track selected theme
  let selectedTheme = terminal.theme || "default";

  modal.addFooterButton(
    "Apply",
    () => applySettings(modal, terminal, state, renderFn, selectedTheme),
    "primary"
  );

  modal.show();

  // Load available worker types
  const workerTypeSelect = modal.querySelector("#worker-type-select") as HTMLSelectElement;
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
  const workspacePath = state.workspace.getWorkspace();
  if (workspacePath) {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      const roleFiles = await invoke<string[]>("list_role_files", { workspacePath });

      const roleFileSelect = modal.querySelector("#role-file") as HTMLSelectElement;
      if (roleFileSelect && roleFiles.length > 0) {
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

      tabBtns.forEach((b) => {
        if (b.getAttribute("data-tab") === tabName) {
          b.className =
            "tab-btn px-4 py-2 font-medium text-blue-600 dark:text-blue-400 border-b-2 border-blue-600 dark:border-blue-400";
        } else {
          b.className =
            "tab-btn px-4 py-2 font-medium text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300";
        }
      });

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
  modal.querySelectorAll(".theme-card").forEach((card) => {
    card.addEventListener("click", () => {
      const themeId = card.getAttribute("data-theme-id");
      if (themeId) {
        selectedTheme = themeId;
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

    updateRolePreview(modal, selectedFile);

    const nameInput = modal.querySelector("#terminal-name") as HTMLInputElement;
    if (selectedFile && nameInput) {
      const roleName = selectedFile.replace(".md", "");
      const defaultName = roleName.charAt(0).toUpperCase() + roleName.slice(1);
      nameInput.value = defaultName;
    }

    if (!selectedFile || !workspacePath) return;

    const { getThemeForRole } = await import("./themes");
    const autoTheme = getThemeForRole(selectedFile);
    selectedTheme = autoTheme;

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
      const { invoke } = await import("@tauri-apps/api/core");
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

        if (metadata.defaultInterval !== undefined) {
          targetIntervalInput.value = Math.floor(metadata.defaultInterval / 1000).toString();
        }
        if (metadata.defaultIntervalPrompt !== undefined) {
          intervalPromptTextarea.value = metadata.defaultIntervalPrompt;
        }
        if (metadata.autonomousRecommended !== undefined) {
          autonomousCheckbox.checked = metadata.autonomousRecommended;
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

  // Initial preview load
  if (roleFileSelect?.value) {
    updateRolePreview(modal, roleFileSelect.value);
  }

  // Wire up autonomous checkbox
  autonomousCheckbox?.addEventListener("change", () => {
    if (autonomousCheckbox.checked) {
      autonomousConfig?.classList.remove("opacity-50", "pointer-events-none");
    } else {
      autonomousConfig?.classList.add("opacity-50", "pointer-events-none");
    }
  });
}

function createSettingsContent(
  terminal: Terminal,
  targetIntervalSeconds: number,
  intervalPrompt: string,
  autonomousEnabled: boolean
): string {
  return `
    <!-- Tabs -->
    <div class="flex border-b border-gray-200 dark:border-gray-700 mb-4 -mt-2">
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
    <div class="h-[380px] overflow-y-auto">
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
                class="theme-card flex flex-col items-center gap-1 p-2 bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 rounded-lg border-2 ${terminal.theme === id ? "border-blue-500" : "border-gray-300 dark:border-gray-600"} transition-all cursor-pointer"
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
      <div data-tab-content="agent" class="hidden">
        <div id="role-preview" class="grid grid-cols-5 gap-4 hidden">
          <div class="col-span-2 flex items-start justify-center">
            <img
              id="role-preview-card"
              src=""
              alt=""
              class="w-full h-auto object-contain"
              style="max-height: 400px"
            />
          </div>

          <div class="col-span-3 space-y-4">
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
              <p class="text-xs text-gray-500 dark:text-gray-400 mt-1">
                Only installed AI coding agents are shown
              </p>
            </div>

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

            <div class="p-4 bg-gray-50 dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-700">
              <h3 id="role-preview-title" class="text-lg font-bold text-gray-900 dark:text-gray-100 mb-2"></h3>
              <p id="role-preview-description" class="text-sm text-gray-700 dark:text-gray-300 mb-3"></p>
              <div class="text-xs text-gray-600 dark:text-gray-400">
                <span class="font-semibold">Workflow:</span>
                <p id="role-preview-workflow" class="mt-1 font-mono"></p>
              </div>
            </div>
          </div>
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
  `;
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function updateRolePreview(modal: ModalBuilder, roleFile: string): void {
  const previewPanel = modal.querySelector("#role-preview") as HTMLElement;
  const cardImg = modal.querySelector("#role-preview-card") as HTMLImageElement;
  const titleEl = modal.querySelector("#role-preview-title") as HTMLElement;
  const descEl = modal.querySelector("#role-preview-description") as HTMLElement;
  const workflowEl = modal.querySelector("#role-preview-workflow") as HTMLElement;

  if (!previewPanel || !roleFile || roleFile === "") {
    previewPanel?.classList.add("hidden");
    return;
  }

  const roleInfo = ROLE_INFO[roleFile];
  if (!roleInfo) {
    previewPanel.classList.add("hidden");
    return;
  }

  previewPanel.classList.remove("hidden");

  const roleName =
    roleFile.replace(".md", "").charAt(0).toUpperCase() + roleFile.replace(".md", "").slice(1);

  cardImg.src = `/assets/tarot-cards/${roleInfo.cardFile}`;
  cardImg.alt = `${roleName} - ${roleInfo.archetype}`;
  titleEl.textContent = `${roleName} - ${roleInfo.archetype}`;
  descEl.textContent = roleInfo.description;
  workflowEl.textContent = `Workflow: ${roleInfo.workflow}`;
}

async function applySettings(
  modal: ModalBuilder,
  terminal: Terminal,
  state: AppState,
  renderFn: () => void,
  selectedTheme: string
): Promise<void> {
  try {
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

    const workerType = workerTypeSelect?.value || "claude";
    const role = roleFile ? "claude-code-worker" : undefined;

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

    const previousRole = terminal.role;
    const roleChanged = previousRole !== role;
    const hasNewRole = role !== undefined && roleConfig !== undefined;

    state.terminals.updateTerminal(terminal.id, { name });
    state.terminals.setTerminalRole(terminal.id, role, roleConfig);
    state.terminals.setTerminalTheme(terminal.id, selectedTheme);

    await saveCurrentConfiguration(state);

    if (roleChanged && hasNewRole) {
      const workspacePath = state.workspace.getWorkspace();
      if (workspacePath && roleConfig.roleFile) {
        try {
          if (workerType === "github-copilot") {
            const { launchGitHubCopilotAgent } = await import("./agent-launcher");
            await launchGitHubCopilotAgent(terminal.id);
          } else if (workerType === "gemini") {
            const { launchGeminiCLIAgent } = await import("./agent-launcher");
            await launchGeminiCLIAgent(terminal.id);
          } else if (workerType === "deepseek") {
            const { launchDeepSeekAgent } = await import("./agent-launcher");
            await launchDeepSeekAgent(terminal.id);
          } else if (workerType === "grok") {
            const { launchGrokAgent } = await import("./agent-launcher");
            await launchGrokAgent(terminal.id);
          } else {
            const { launchAgentInTerminal } = await import("./agent-launcher");
            const { invoke } = await import("@tauri-apps/api/core");
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

            let worktreePath = terminal.worktreePath;
            if (!worktreePath) {
              logger.info("Terminal missing worktree, creating now", {
                terminalId: terminal.id,
                terminalName: terminal.name,
              });
              const { setupWorktreeForAgent } = await import("./worktree-manager");
              worktreePath = await setupWorktreeForAgent(terminal.id, workspacePath, gitIdentity);
              state.terminals.updateTerminal(terminal.id, { worktreePath });
            }

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

    const { getIntervalPromptManager } = await import("./interval-prompt-manager");
    const intervalManager = getIntervalPromptManager();

    if (
      hasNewRole &&
      roleConfig.targetInterval !== undefined &&
      (roleConfig.targetInterval as number) >= 0
    ) {
      const updatedTerminal = state.terminals.getTerminal(terminal.id);
      if (updatedTerminal) {
        intervalManager.restart(updatedTerminal);
      }
    } else {
      intervalManager.stop(terminal.id);
    }

    modal.close();
    renderFn();
  } catch (error) {
    showToast(`Failed to apply settings: ${error}`, "error");
  }
}
