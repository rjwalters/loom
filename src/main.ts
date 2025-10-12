import "./style.css";
import { open } from "@tauri-apps/api/dialog";
import { listen } from "@tauri-apps/api/event";
import { homeDir } from "@tauri-apps/api/path";
import { invoke } from "@tauri-apps/api/tauri";
import { loadConfig, saveConfig, setConfigWorkspace } from "./lib/config";
import { getOutputPoller } from "./lib/output-poller";
import { AppState, TerminalStatus } from "./lib/state";
import { getTerminalManager } from "./lib/terminal-manager";
import { initTheme, toggleTheme } from "./lib/theme";
import { renderHeader, renderMiniTerminals, renderPrimaryTerminal } from "./lib/ui";

// Initialize theme
initTheme();

// Initialize state (no agents until workspace is selected)
const state = new AppState();

// Get terminal manager and output poller
const terminalManager = getTerminalManager();
const outputPoller = getOutputPoller();

// Track which terminal is currently attached
let currentAttachedTerminalId: string | null = null;

// Render function
function render() {
  const hasWorkspace = state.getWorkspace() !== null && state.getWorkspace() !== "";
  console.log(
    "[render] hasWorkspace:",
    hasWorkspace,
    "displayedWorkspace:",
    state.getDisplayedWorkspace()
  );
  renderHeader(state.getDisplayedWorkspace(), hasWorkspace);
  renderPrimaryTerminal(state.getPrimary(), hasWorkspace, state.getDisplayedWorkspace());
  renderMiniTerminals(state.getTerminals(), hasWorkspace);

  // Re-attach workspace event listeners if they were just rendered
  if (!hasWorkspace) {
    attachWorkspaceEventListeners();
  }

  // Initialize xterm.js terminal for primary terminal
  const primary = state.getPrimary();
  if (primary && hasWorkspace) {
    initializeTerminalDisplay(primary.id);
  }
}

// Initialize xterm.js terminal display
function initializeTerminalDisplay(terminalId: string) {
  const containerId = `terminal-content-${terminalId}`;

  // Check if terminal already exists
  if (terminalManager.getTerminal(terminalId)) {
    // Terminal already exists, just ensure polling is active
    if (currentAttachedTerminalId !== terminalId) {
      // Stop polling previous terminal
      if (currentAttachedTerminalId) {
        outputPoller.stopPolling(currentAttachedTerminalId);
      }
      // Start polling new terminal
      outputPoller.startPolling(terminalId);
      currentAttachedTerminalId = terminalId;
    }
    return;
  }

  // Wait for DOM to be ready
  setTimeout(() => {
    const managed = terminalManager.createTerminal(terminalId, containerId);
    if (managed) {
      // Start polling for output
      if (currentAttachedTerminalId !== terminalId) {
        // Stop polling previous terminal
        if (currentAttachedTerminalId) {
          outputPoller.stopPolling(currentAttachedTerminalId);
        }
        // Start polling new terminal
        outputPoller.startPolling(terminalId);
        currentAttachedTerminalId = terminalId;
      }
    }
  }, 0);
}

// Initialize app with auto-load workspace
async function initializeApp() {
  try {
    // Check for stored workspace
    const storedPath = await invoke<string | null>("get_stored_workspace");

    if (storedPath) {
      console.log("[initializeApp] Found stored workspace:", storedPath);

      // Validate stored workspace is still valid
      const isValid = await validateWorkspacePath(storedPath);

      if (isValid) {
        // Load workspace automatically
        console.log("[initializeApp] Loading stored workspace");
        await handleWorkspacePathInput(storedPath);
        return;
      }

      // Path no longer valid - clear it and show picker
      console.log("[initializeApp] Stored workspace invalid, clearing");
      await invoke("clear_stored_workspace");
    }
  } catch (error) {
    console.error("[initializeApp] Failed to load stored workspace:", error);
  }

  // No stored workspace or validation failed - show picker
  console.log("[initializeApp] Showing workspace picker");
  render();
}

// Re-render on state changes
state.onChange(render);

// Listen for close workspace events from menu
listen("close-workspace", async () => {
  console.log("[close-workspace] Closing workspace");

  // Clear stored workspace
  try {
    await invoke("clear_stored_workspace");
    console.log("[close-workspace] Cleared stored workspace");
  } catch (error) {
    console.error("Failed to clear stored workspace:", error);
  }

  // Stop all polling
  outputPoller.stopAll();

  // Destroy all xterm instances
  terminalManager.destroyAll();

  // Clear runtime state
  state.clearAll();
  setConfigWorkspace("");
  currentAttachedTerminalId = null;

  // Re-render to show workspace picker
  console.log("[close-workspace] Rendering workspace picker");
  render();
});

// Initialize app
initializeApp();

// Drag and drop state
let draggedTerminalId: string | null = null;
let dropTargetId: string | null = null;
let dropInsertBefore: boolean = false;
let isDragging: boolean = false;

// Save current state to config
async function saveCurrentConfig() {
  const workspace = state.getWorkspace();
  if (!workspace) {
    return;
  }

  const config = {
    nextAgentNumber: state.getCurrentAgentNumber(),
    agents: state.getTerminals(),
  };

  await saveConfig(config);
}

// Expand tilde (~) to home directory
async function expandTildePath(path: string): Promise<string> {
  if (path.startsWith("~")) {
    try {
      const home = await homeDir();
      return path.replace(/^~/, home);
    } catch (error) {
      console.error("Failed to get home directory:", error);
      return path;
    }
  }
  return path;
}

// Workspace error UI helpers
function showWorkspaceError(message: string) {
  console.log("[showWorkspaceError]", message);
  const input = document.getElementById("workspace-path") as HTMLInputElement;
  const errorDiv = document.getElementById("workspace-error");

  console.log("[showWorkspaceError] input:", input, "errorDiv:", errorDiv);

  if (input) {
    input.classList.remove("border-gray-300", "dark:border-gray-600");
    input.classList.add("border-red-500", "dark:border-red-500");
  }

  if (errorDiv) {
    errorDiv.textContent = message;
  }
}

function clearWorkspaceError() {
  console.log("[clearWorkspaceError]");
  const input = document.getElementById("workspace-path") as HTMLInputElement;
  const errorDiv = document.getElementById("workspace-error");

  if (input) {
    input.classList.remove("border-red-500", "dark:border-red-500");
    input.classList.add("border-gray-300", "dark:border-gray-600");
  }

  if (errorDiv) {
    errorDiv.textContent = "";
  }
}

// Validate workspace path
async function validateWorkspacePath(path: string): Promise<boolean> {
  console.log("[validateWorkspacePath] path:", path);
  if (!path || path.trim() === "") {
    console.log("[validateWorkspacePath] empty path, clearing error");
    clearWorkspaceError();
    return false;
  }

  try {
    await invoke<boolean>("validate_git_repo", { path });
    console.log("[validateWorkspacePath] validation passed");
    clearWorkspaceError();
    return true;
  } catch (error) {
    const errorMessage =
      typeof error === "string"
        ? error
        : (error as { message?: string })?.message || "Invalid workspace path";
    console.log("[validateWorkspacePath] validation failed:", errorMessage);
    showWorkspaceError(errorMessage);
    return false;
  }
}

// Browse for workspace folder
async function browseWorkspace() {
  try {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "Select workspace folder",
    });

    if (selected && typeof selected === "string") {
      await handleWorkspacePathInput(selected);
    }
  } catch (error) {
    console.error("Error selecting workspace:", error);
    alert("Failed to select workspace. Please try again.");
  }
}

// Initialize Loom in workspace
async function initializeLoomWorkspace(workspacePath: string): Promise<boolean> {
  try {
    // In dev mode, use relative path from cwd (project root)
    // TODO: For production, bundle defaults as a resource
    const defaultsPath = "defaults";

    await invoke("initialize_loom_workspace", {
      path: workspacePath,
      defaultsPath: defaultsPath,
    });

    return true;
  } catch (error) {
    console.error("Failed to initialize workspace:", error);
    alert(`Failed to initialize workspace: ${error}`);
    return false;
  }
}

// Create a plain shell terminal
async function createPlainTerminal() {
  const workspacePath = state.getWorkspace();
  if (!workspacePath) {
    alert("No workspace selected");
    return;
  }

  // Generate terminal name
  const terminalCount = state.getTerminals().length + 1;
  const name = `Terminal ${terminalCount}`;

  try {
    // Create terminal in workspace directory
    const terminalId = await invoke<string>("create_terminal", {
      name,
      workingDir: workspacePath,
    });

    // Add to state (no role assigned - plain shell)
    state.addTerminal({
      id: terminalId,
      name,
      status: TerminalStatus.Idle,
      isPrimary: false,
    });

    // Save updated state to config
    await saveCurrentConfig();

    // Switch to new terminal
    state.setPrimary(terminalId);

    console.log(`[createPlainTerminal] Created terminal ${name} (${terminalId})`);
  } catch (error) {
    console.error("[createPlainTerminal] Failed to create terminal:", error);
    alert(`Failed to create terminal: ${error}`);
  }
}

// Reconnect terminals to daemon after loading config
async function reconnectTerminals() {
  console.log("[reconnectTerminals] Querying daemon for active terminals...");

  try {
    // Get list of active terminals from daemon
    interface DaemonTerminalInfo {
      id: string;
      name: string;
      tmux_session: string;
      working_dir: string | null;
      created_at: number;
    }

    const daemonTerminals = await invoke<DaemonTerminalInfo[]>("list_terminals");
    console.log(`[reconnectTerminals] Found ${daemonTerminals.length} active daemon terminals`);

    // Create a set of active terminal IDs for quick lookup
    const activeTerminalIds = new Set(daemonTerminals.map((t) => t.id));

    // Get all agents from state
    const agents = state.getTerminals();
    console.log(`[reconnectTerminals] Config has ${agents.length} agents`);

    // For each agent in config, check if daemon has it
    for (const agent of agents) {
      if (activeTerminalIds.has(agent.id)) {
        console.log(`[reconnectTerminals] Reconnecting agent ${agent.name} (${agent.id})`);

        // Initialize xterm for this terminal (will fetch full history)
        initializeTerminalDisplay(agent.id);
      } else {
        console.log(
          `[reconnectTerminals] Agent ${agent.name} (${agent.id}) not found in daemon, skipping`
        );
        // Terminal not found in daemon - either tmux session died or daemon restarted
        // We could mark it as stopped, but for now just leave it in config
        // User can recreate it or remove it manually
      }
    }

    console.log("[reconnectTerminals] Reconnection complete");
  } catch (error) {
    console.error("[reconnectTerminals] Failed to reconnect terminals:", error);
    // Non-fatal - workspace is still loaded
  }
}

// Handle manual workspace path entry
async function handleWorkspacePathInput(path: string) {
  console.log("[handleWorkspacePathInput] input path:", path);

  // Expand tilde if present
  const expandedPath = await expandTildePath(path);
  console.log("[handleWorkspacePathInput] expanded path:", expandedPath);

  // Always update displayed workspace so bad paths are visible with error message
  state.setDisplayedWorkspace(expandedPath);
  console.log("[handleWorkspacePathInput] set displayedWorkspace, triggering render...");

  const isValid = await validateWorkspacePath(expandedPath);
  console.log("[handleWorkspacePathInput] isValid:", isValid);

  if (!isValid) {
    console.log("[handleWorkspacePathInput] invalid path, stopping");
    return;
  }

  // Check if Loom is initialized in this workspace
  try {
    const isInitialized = await invoke<boolean>("check_loom_initialized", { path: expandedPath });
    console.log("[handleWorkspacePathInput] isInitialized:", isInitialized);

    if (!isInitialized) {
      // Ask user to confirm initialization
      const confirmed = confirm(
        `Initialize Loom in this workspace?\n\n` +
          `This will:\n` +
          `• Create .loom/ directory with default configuration\n` +
          `• Add .loom/ to .gitignore\n` +
          `• Set up 3 default agents\n\n` +
          `Continue?`
      );

      if (!confirmed) {
        console.log("[handleWorkspacePathInput] user cancelled initialization");
        return;
      }

      // Initialize workspace
      const initialized = await initializeLoomWorkspace(expandedPath);
      if (!initialized) {
        console.log("[handleWorkspacePathInput] initialization failed");
        return;
      }
    }

    // Now load config from workspace
    state.setWorkspace(expandedPath);
    console.log("[handleWorkspacePathInput] set workspace, loading config...");

    setConfigWorkspace(expandedPath);
    const config = await loadConfig();
    state.setNextAgentNumber(config.nextAgentNumber);

    // Load agents from config
    if (config.agents && config.agents.length > 0) {
      state.loadAgents(config.agents);
      // Reconnect agents to existing daemon terminals
      await reconnectTerminals();
    }
    console.log("[handleWorkspacePathInput] workspace fully loaded");

    // Store workspace path for next app launch
    try {
      await invoke("set_stored_workspace", { path: expandedPath });
      console.log("[handleWorkspacePathInput] workspace path stored");
    } catch (error) {
      console.error("Failed to store workspace path:", error);
      // Non-fatal - workspace is still loaded
    }
  } catch (error) {
    console.error("Error handling workspace:", error);
    alert(`Error: ${error}`);
  }
}

// Helper function to start renaming a terminal
function startRename(terminalId: string, nameElement: HTMLElement) {
  const terminal = state.getTerminals().find((t) => t.id === terminalId);
  if (!terminal) return;

  const currentName = terminal.name;
  const input = document.createElement("input");
  input.type = "text";
  input.value = currentName;

  // Match the font size of the original element
  const fontSize = nameElement.classList.contains("text-sm") ? "text-sm" : "text-xs";
  input.className = `px-1 bg-white dark:bg-gray-900 border border-blue-500 rounded ${fontSize} font-medium w-full`;

  // Replace the name element with input
  const parent = nameElement.parentElement;
  if (!parent) return;

  parent.replaceChild(input, nameElement);
  input.focus();
  input.select();

  const commit = () => {
    const newName = input.value.trim();
    if (newName && newName !== currentName) {
      state.renameTerminal(terminalId, newName);
      saveCurrentConfig();
    } else {
      // Just re-render to restore original state
      render();
    }
  };

  const cancel = () => {
    render();
  };

  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancel();
    }
  });

  input.addEventListener("blur", () => {
    commit();
  });
}

// Attach workspace event listeners (called dynamically when workspace selector is rendered)
function attachWorkspaceEventListeners() {
  console.log("[attachWorkspaceEventListeners] attaching listeners...");
  // Workspace path input - validate on Enter or blur
  const workspaceInput = document.getElementById("workspace-path") as HTMLInputElement;
  console.log("[attachWorkspaceEventListeners] workspaceInput:", workspaceInput);
  if (workspaceInput) {
    workspaceInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        console.log("[workspaceInput keydown] Enter pressed, value:", workspaceInput.value);
        e.preventDefault();
        handleWorkspacePathInput(workspaceInput.value);
        workspaceInput.blur();
      }
    });

    workspaceInput.addEventListener("blur", () => {
      console.log(
        "[workspaceInput blur] value:",
        workspaceInput.value,
        "workspace:",
        state.getWorkspace()
      );
      if (workspaceInput.value !== state.getWorkspace()) {
        handleWorkspacePathInput(workspaceInput.value);
      }
    });
  }

  // Browse workspace button
  const browseBtn = document.getElementById("browse-workspace");
  console.log("[attachWorkspaceEventListeners] browseBtn:", browseBtn);
  browseBtn?.addEventListener("click", () => {
    console.log("[browseBtn click] clicked");
    browseWorkspace();
  });
}

// Set up event listeners (only once, since parent elements are static)
function setupEventListeners() {
  // Theme toggle
  document.getElementById("theme-toggle")?.addEventListener("click", () => {
    toggleTheme();
  });

  // Primary terminal - double-click to rename, click for settings
  const primaryTerminal = document.getElementById("primary-terminal");
  if (primaryTerminal) {
    // Settings button click
    primaryTerminal.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;
      const settingsBtn = target.closest("#terminal-settings-btn");

      if (settingsBtn) {
        e.stopPropagation();
        const id = settingsBtn.getAttribute("data-terminal-id");
        if (id) {
          console.log(`[terminal-settings-btn] Opening settings for terminal ${id}`);
          // TODO: Show terminal settings modal
          alert("Terminal settings modal coming soon!");
        }
      }
    });

    // Double-click to rename
    primaryTerminal.addEventListener("dblclick", (e) => {
      const target = e.target as HTMLElement;

      if (target.classList.contains("terminal-name")) {
        e.stopPropagation();
        const id = target.getAttribute("data-terminal-id");
        if (id) {
          startRename(id, target);
        }
      }
    });
  }

  // Mini terminal row - event delegation for dynamic children
  const miniRow = document.getElementById("mini-terminal-row");
  if (miniRow) {
    miniRow.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;

      // Handle close button clicks
      if (target.classList.contains("close-terminal-btn")) {
        e.stopPropagation();
        const id = target.getAttribute("data-terminal-id");

        if (id) {
          if (confirm("Close this terminal?")) {
            // Stop polling and clean up xterm.js instance
            outputPoller.stopPolling(id);
            terminalManager.destroyTerminal(id);

            // If this was the current attached terminal, clear it
            if (currentAttachedTerminalId === id) {
              currentAttachedTerminalId = null;
            }

            // Remove from state
            state.removeTerminal(id);
            saveCurrentConfig();
          }
        }
        return;
      }

      // Handle add terminal button
      if (target.id === "add-terminal-btn" || target.closest("#add-terminal-btn")) {
        // Don't add if no workspace selected
        if (!state.getWorkspace()) {
          return;
        }

        // Create plain terminal
        createPlainTerminal();
        return;
      }

      // Handle terminal card clicks (switch primary)
      const card = target.closest("[data-terminal-id]");
      if (card) {
        const id = card.getAttribute("data-terminal-id");
        if (id) {
          state.setPrimary(id);
        }
      }
    });

    // Handle mousedown to show immediate visual feedback
    miniRow.addEventListener("mousedown", (e) => {
      const target = e.target as HTMLElement;

      // Don't handle if clicking close button
      if (target.classList.contains("close-terminal-btn")) {
        return;
      }

      const card = target.closest(".terminal-card");
      if (card) {
        // Remove selection from all cards and restore default border
        document.querySelectorAll(".terminal-card").forEach((c) => {
          c.classList.remove("border-2", "border-blue-500");
          c.classList.add("border", "border-gray-200", "dark:border-gray-700");
        });

        // Add selection to clicked card immediately
        card.classList.remove("border", "border-gray-200", "dark:border-gray-700");
        card.classList.add("border-2", "border-blue-500");
      }
    });

    // Handle double-click to rename terminals
    miniRow.addEventListener("dblclick", (e) => {
      const target = e.target as HTMLElement;

      // Check if double-clicking on the terminal name in mini cards
      if (target.classList.contains("terminal-name")) {
        e.stopPropagation();
        const card = target.closest("[data-terminal-id]");
        const id = card?.getAttribute("data-terminal-id");
        if (id) {
          startRename(id, target);
        }
      }
    });

    // HTML5 drag events for visual feedback
    miniRow.addEventListener("dragstart", (e) => {
      const target = e.target as HTMLElement;
      const card = target.closest(".terminal-card") as HTMLElement;

      if (card) {
        isDragging = true;
        draggedTerminalId = card.getAttribute("data-terminal-id");
        card.classList.add("dragging");

        if (e.dataTransfer) {
          e.dataTransfer.effectAllowed = "move";
          e.dataTransfer.setData("text/html", card.innerHTML);
        }
      }
    });

    miniRow.addEventListener("dragend", (e) => {
      // Perform reorder if valid
      if (draggedTerminalId && dropTargetId && dropTargetId !== draggedTerminalId) {
        state.reorderTerminal(draggedTerminalId, dropTargetId, dropInsertBefore);
        saveCurrentConfig();
      }

      // Select the terminal that was dragged
      if (draggedTerminalId) {
        state.setPrimary(draggedTerminalId);
      }

      // Cleanup
      const target = e.target as HTMLElement;
      const card = target.closest(".terminal-card");
      if (card) {
        card.classList.remove("dragging");
      }

      document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());
      draggedTerminalId = null;
      dropTargetId = null;
      dropInsertBefore = false;
      isDragging = false;
    });

    // dragover for tracking position and showing indicator
    miniRow.addEventListener("dragover", (e) => {
      e.preventDefault();
      if (e.dataTransfer) {
        e.dataTransfer.dropEffect = "move";
      }

      if (!isDragging || !draggedTerminalId) return;

      const target = e.target as HTMLElement;
      const card = target.closest(".terminal-card") as HTMLElement;

      if (card && card.getAttribute("data-terminal-id") !== draggedTerminalId) {
        const targetId = card.getAttribute("data-terminal-id");

        // Remove old indicators
        document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());

        // Calculate if we should insert before or after
        const rect = card.getBoundingClientRect();
        const midpoint = rect.left + rect.width / 2;
        const insertBefore = e.clientX < midpoint;

        // Store drop target info
        dropTargetId = targetId;
        dropInsertBefore = insertBefore;

        // Create and position insertion indicator - insert at wrapper level
        const wrapper = card.parentElement;
        const indicator = document.createElement("div");
        indicator.className = "drop-indicator";
        wrapper?.parentElement?.insertBefore(
          indicator,
          insertBefore ? wrapper : wrapper.nextSibling
        );
      } else if (!card) {
        // In empty space - find all cards and determine position
        const allCards = Array.from(miniRow.querySelectorAll(".terminal-card")) as HTMLElement[];
        const lastCard = allCards[allCards.length - 1];

        if (lastCard && !lastCard.classList.contains("dragging")) {
          const lastId = lastCard.getAttribute("data-terminal-id");
          if (lastId && lastId !== draggedTerminalId) {
            // Remove old indicators
            document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());

            // Drop after the last card
            dropTargetId = lastId;
            dropInsertBefore = false;

            // Create and position insertion indicator after last card - insert at wrapper level
            const wrapper = lastCard.parentElement;
            const indicator = document.createElement("div");
            indicator.className = "drop-indicator";
            wrapper?.parentElement?.insertBefore(indicator, wrapper?.nextSibling || null);
          }
        }
      }
    });
  }
}

// Set up all event listeners once
setupEventListeners();
