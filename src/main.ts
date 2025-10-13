import "./style.css";
import { ask, open } from "@tauri-apps/api/dialog";
import { listen } from "@tauri-apps/api/event";
import { homeDir } from "@tauri-apps/api/path";
import { invoke } from "@tauri-apps/api/tauri";
import { loadConfig, saveConfig, setConfigWorkspace } from "./lib/config";
import { getOutputPoller } from "./lib/output-poller";
import { AppState, TerminalStatus } from "./lib/state";
import { getTerminalManager } from "./lib/terminal-manager";
import { showTerminalSettingsModal } from "./lib/terminal-settings-modal";
import { initTheme, toggleTheme } from "./lib/theme";
import { renderHeader, renderMiniTerminals, renderPrimaryTerminal } from "./lib/ui";

// Initialize theme
initTheme();

// Initialize state (no agents until workspace is selected)
const state = new AppState();

// Get terminal manager and output poller
const terminalManager = getTerminalManager();
const outputPoller = getOutputPoller();

// Register error callback for polling failures
outputPoller.onError((terminalId, errorMessage) => {
  console.warn(
    `[outputPoller] Terminal ${terminalId} encountered fatal errors (${errorMessage}), marking as error state`
  );

  // Mark terminal as having connection issues
  const terminal = state.getTerminals().find((t) => t.id === terminalId);
  if (terminal) {
    state.updateTerminal(terminalId, {
      status: TerminalStatus.Error,
      missingSession: true,
    });
  }
});

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
async function initializeTerminalDisplay(terminalId: string) {
  const containerId = `terminal-content-${terminalId}`;

  // Check session health before initializing
  try {
    const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });

    if (!hasSession) {
      console.warn(`[initializeTerminalDisplay] Terminal ${terminalId} has no tmux session`);

      // Mark terminal as having missing session
      const terminal = state.getTerminals().find((t) => t.id === terminalId);
      if (terminal) {
        state.updateTerminal(terminalId, {
          status: TerminalStatus.Error,
          missingSession: true,
        });
      }

      return; // Don't create xterm instance - error UI will show instead
    }
  } catch (error) {
    console.error(`[initializeTerminalDisplay] Failed to check session health:`, error);
    // Continue anyway - better to try than not
  }

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

// Listen for menu events
listen("new-terminal", () => {
  if (state.getWorkspace()) {
    createPlainTerminal();
  }
});

listen("close-terminal", async () => {
  const primary = state.getPrimary();
  if (primary) {
    const confirmed = await ask("Are you sure you want to close this terminal?", {
      title: "Close Terminal",
      type: "warning",
    });

    if (confirmed) {
      // Stop autonomous mode if running
      const { getAutonomousManager } = await import("./lib/autonomous-manager");
      const autonomousManager = getAutonomousManager();
      autonomousManager.stopAutonomous(primary.id);

      outputPoller.stopPolling(primary.id);
      terminalManager.destroyTerminal(primary.id);
      if (currentAttachedTerminalId === primary.id) {
        currentAttachedTerminalId = null;
      }
      state.removeTerminal(primary.id);
      saveCurrentConfig();
    }
  }
});

listen("close-workspace", async () => {
  console.log("[close-workspace] Closing workspace");

  // Clear stored workspace
  try {
    await invoke("clear_stored_workspace");
    console.log("[close-workspace] Cleared stored workspace");
  } catch (error) {
    console.error("Failed to clear stored workspace:", error);
  }

  // Stop all autonomous intervals
  const { getAutonomousManager } = await import("./lib/autonomous-manager");
  const autonomousManager = getAutonomousManager();
  autonomousManager.stopAll();

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

listen("factory-reset-workspace", async () => {
  const workspace = state.getWorkspace();
  if (!workspace) return;

  const confirmed = await ask(
    "This will:\n" +
      "• Delete all terminal configurations\n" +
      "• Reset all roles to defaults\n" +
      "• Close all current terminals\n" +
      "• Recreate 6 default terminals\n\n" +
      "This action CANNOT be undone!\n\n" +
      "Continue with factory reset?",
    {
      title: "⚠️ Factory Reset Warning",
      type: "warning",
    }
  );

  if (!confirmed) return;

  console.log("[factory-reset-workspace] Resetting workspace to defaults");

  // Stop all polling
  const terminals = state.getTerminals();
  terminals.forEach((t) => outputPoller.stopPolling(t.id));

  // Destroy all xterm instances
  terminalManager.destroyAll();

  // Call backend reset
  try {
    await invoke("reset_workspace_to_defaults", {
      workspacePath: workspace,
      defaultsPath: "defaults",
    });
    console.log("[factory-reset-workspace] Backend reset complete");
  } catch (error) {
    console.error("Failed to reset workspace:", error);
    alert(`Failed to reset workspace: ${error}`);
    return;
  }

  // Clear state
  state.clearAll();
  currentAttachedTerminalId = null;

  // Reload config and recreate terminals
  try {
    setConfigWorkspace(workspace);
    const config = await loadConfig();
    state.setNextAgentNumber(config.nextAgentNumber);

    // Load agents from fresh config and create terminal sessions for each
    if (config.agents && config.agents.length > 0) {
      // Create new terminal sessions for each agent in the config
      for (const agent of config.agents) {
        try {
          // Create terminal in daemon
          const terminalId = await invoke<string>("create_terminal", {
            name: agent.name,
            workingDir: workspace,
          });

          // Update agent ID to match the newly created terminal
          agent.id = terminalId;
          console.log(`[factory-reset-workspace] Created terminal ${agent.name} (${terminalId})`);
        } catch (error) {
          console.error(
            `[factory-reset-workspace] Failed to create terminal ${agent.name}:`,
            error
          );
          alert(`Failed to create terminal ${agent.name}: ${error}`);
        }
      }

      // Now load the agents into state with their new IDs
      state.loadAgents(config.agents);

      // Save the updated config with new terminal IDs
      await saveCurrentConfig();
    }

    // Set workspace as active (so render() shows terminals instead of picker)
    state.setWorkspace(workspace);

    console.log("[factory-reset-workspace] Workspace reset complete");
  } catch (error) {
    console.error("Failed to reload config after reset:", error);
    alert(`Failed to reload config: ${error}`);
  }

  // Re-render
  render();
});

listen("toggle-theme", () => {
  toggleTheme();
});

listen("zoom-in", () => {
  terminalManager.adjustAllFontSizes(2);
});

listen("zoom-out", () => {
  terminalManager.adjustAllFontSizes(-2);
});

listen("reset-zoom", () => {
  terminalManager.resetAllFontSizes();
});

listen("show-shortcuts", () => {
  // TODO: Implement keyboard shortcuts dialog
  alert(
    "Keyboard Shortcuts:\n\n" +
      "File:\n" +
      "  Cmd+T - New Terminal\n" +
      "  Cmd+Shift+W - Close Terminal\n" +
      "  Cmd+W - Close Workspace\n\n" +
      "Edit:\n" +
      "  Cmd+C - Copy\n" +
      "  Cmd+V - Paste\n" +
      "  Cmd+A - Select All\n\n" +
      "View:\n" +
      "  Cmd+Shift+T - Toggle Theme\n" +
      "  Cmd++ - Zoom In\n" +
      "  Cmd+- - Zoom Out\n" +
      "  Cmd+0 - Reset Zoom\n\n" +
      "Help:\n" +
      "  Cmd+/ - Show Shortcuts"
  );
});

listen("show-daemon-status", async () => {
  showDaemonStatusDialog();
});

// Show daemon status dialog with reconnect option
async function showDaemonStatusDialog() {
  try {
    interface DaemonStatus {
      running: boolean;
      socket_path: string;
      error: string | null;
    }

    const status = await invoke<DaemonStatus>("get_daemon_status");

    const statusText = status.running
      ? "✅ Running"
      : `❌ Not Running${status.error ? `\n\nError: ${status.error}` : ""}`;

    const workspace = state.getWorkspace();
    const hasWorkspace = workspace !== null && workspace !== "";

    // Show different dialog based on whether daemon is running and workspace is loaded
    if (status.running && hasWorkspace) {
      const shouldReconnect = await ask(
        `Daemon Status\n\n${statusText}\n\nSocket: ${status.socket_path}\n\n` +
          `Would you like to reconnect terminals to the daemon?\n\n` +
          `This is useful if you hot-reloaded the frontend and lost connection to terminals.`,
        {
          title: "Daemon Status",
          type: "info",
        }
      );

      if (shouldReconnect) {
        console.log("[show-daemon-status] User requested reconnection");
        await reconnectTerminals();
        alert("Terminal reconnection complete! Check the console for details.");
      }
    } else {
      // Just show status without reconnect option
      alert(`Daemon Status\n\n${statusText}\n\nSocket: ${status.socket_path}`);
    }
  } catch (error) {
    alert(`Failed to get daemon status: ${error}`);
  }
}

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
      theme: "default",
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

    let reconnectedCount = 0;
    let missingCount = 0;

    // For each agent in config, check if daemon has it
    for (const agent of agents) {
      // Check if agent has placeholder ID (shouldn't happen after proper initialization)
      if (agent.id === "__unassigned__") {
        console.log(
          `[reconnectTerminals] Agent ${agent.name} has placeholder ID, marking as missing`
        );

        // Mark terminal as having missing session so user can see it needs recovery
        state.updateTerminal(agent.id, {
          status: TerminalStatus.Error,
          missingSession: true,
        });

        missingCount++;
        continue;
      }

      if (activeTerminalIds.has(agent.id)) {
        console.log(`[reconnectTerminals] Reconnecting agent ${agent.name} (${agent.id})`);

        // Clear any error state from previous connection issues
        if (agent.missingSession) {
          state.updateTerminal(agent.id, {
            status: TerminalStatus.Idle,
            missingSession: undefined,
          });
        }

        // Initialize xterm for this terminal (will fetch full history)
        // Only initialize if this is the primary terminal to avoid creating too many instances
        if (agent.isPrimary) {
          initializeTerminalDisplay(agent.id);
        }

        reconnectedCount++;
      } else {
        console.log(
          `[reconnectTerminals] Agent ${agent.name} (${agent.id}) not found in daemon, marking as missing`
        );

        // Mark terminal as having missing session so user can see it needs recovery
        state.updateTerminal(agent.id, {
          status: TerminalStatus.Error,
          missingSession: true,
        });

        missingCount++;
      }
    }

    console.log(
      `[reconnectTerminals] Reconnection complete: ${reconnectedCount} reconnected, ${missingCount} missing`
    );

    // If we reconnected at least some terminals, save the updated state
    if (reconnectedCount > 0) {
      await saveCurrentConfig();
    }
  } catch (error) {
    console.error("[reconnectTerminals] Failed to reconnect terminals:", error);
    // Non-fatal - workspace is still loaded
    alert(
      `Warning: Could not reconnect to daemon terminals.\n\n` +
        `Error: ${error}\n\n` +
        `Terminals may need to be recreated. Check Help → Daemon Status for more info.`
    );
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
      // Ask user to confirm initialization with detailed information
      const confirmed = await ask(
        `This will create:\n\n` +
          `📁 .loom/ directory with:\n` +
          `  • config.json - Terminal configuration\n` +
          `  • roles/ - Agent role definitions\n\n` +
          `🤖 6 Default Terminals:\n` +
          `  • Shell - Plain shell (primary)\n` +
          `  • Architect - Claude Code worker\n` +
          `  • Curator - Claude Code worker\n` +
          `  • Reviewer - Claude Code worker\n` +
          `  • Worker 1 - Claude Code worker\n` +
          `  • Worker 2 - Claude Code worker\n\n` +
          `📝 .loom/ will be added to .gitignore\n\n` +
          `Continue?`,
        {
          title: "Initialize Loom in this workspace?",
          type: "info",
        }
      );

      if (!confirmed) {
        console.log("[handleWorkspacePathInput] user cancelled initialization");
        return;
      }

      // Initialize workspace using reset_workspace_to_defaults
      try {
        await invoke("reset_workspace_to_defaults", {
          workspacePath: expandedPath,
          defaultsPath: "defaults",
        });
        console.log("[handleWorkspacePathInput] Workspace initialized");
      } catch (error) {
        console.error("Failed to initialize workspace:", error);
        alert(`Failed to initialize workspace: ${error}`);
        return;
      }

      // After initialization, create terminals for the default config
      setConfigWorkspace(expandedPath);
      const config = await loadConfig();
      state.setNextAgentNumber(config.nextAgentNumber);

      if (config.agents && config.agents.length > 0) {
        console.log("[handleWorkspacePathInput] Creating terminals for fresh workspace");

        // Create terminal sessions for each agent in the config
        for (const agent of config.agents) {
          try {
            // Create terminal in daemon
            const terminalId = await invoke<string>("create_terminal", {
              name: agent.name,
              workingDir: expandedPath,
            });

            // Update agent ID to match the newly created terminal
            agent.id = terminalId;
            console.log(
              `[handleWorkspacePathInput] Created terminal ${agent.name} (${terminalId})`
            );
          } catch (error) {
            console.error(
              `[handleWorkspacePathInput] Failed to create terminal ${agent.name}:`,
              error
            );
            alert(`Failed to create terminal ${agent.name}: ${error}`);
          }
        }

        // Load agents into state with their new IDs
        state.loadAgents(config.agents);

        // Save the updated config with real terminal IDs
        await saveConfig(config);
        console.log("[handleWorkspacePathInput] Saved config with real terminal IDs");
      }
    } else {
      // Workspace already initialized - load existing config
      setConfigWorkspace(expandedPath);
      const config = await loadConfig();
      state.setNextAgentNumber(config.nextAgentNumber);

      // Load agents from config
      if (config.agents && config.agents.length > 0) {
        state.loadAgents(config.agents);
        // Reconnect agents to existing daemon terminals
        await reconnectTerminals();
      }
    }

    // Start autonomous mode for eligible terminals
    const { getAutonomousManager } = await import("./lib/autonomous-manager");
    const autonomousManager = getAutonomousManager();
    autonomousManager.startAllAutonomous(state);
    console.log("[handleWorkspacePathInput] Started autonomous agents");

    // Now set workspace as active
    state.setWorkspace(expandedPath);
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

// Recovery handlers for terminals with missing sessions
async function handleRecoverNewSession(terminalId: string) {
  console.log(`[handleRecoverNewSession] Creating new session for terminal ${terminalId}`);

  try {
    const workspacePath = state.getWorkspace();
    const terminal = state.getTerminals().find((t) => t.id === terminalId);

    if (!terminal || !workspacePath) {
      alert("Cannot recover: terminal or workspace not found");
      return;
    }

    // Create a new terminal in the daemon
    const newTerminalId = await invoke<string>("create_terminal", {
      name: terminal.name,
      workingDir: workspacePath,
    });

    console.log(`[handleRecoverNewSession] Created new terminal ${newTerminalId}`);

    // Update the terminal in state with the new ID
    state.removeTerminal(terminalId);
    state.addTerminal({
      ...terminal,
      id: newTerminalId,
      status: TerminalStatus.Idle,
      missingSession: undefined,
    });

    // Set as primary
    state.setPrimary(newTerminalId);

    // Save config
    await saveCurrentConfig();

    console.log(`[handleRecoverNewSession] Recovery complete`);
  } catch (error) {
    console.error(`[handleRecoverNewSession] Failed to recover:`, error);
    alert(`Failed to create new session: ${error}`);
  }
}

async function handleRecoverAttachSession(terminalId: string) {
  console.log(`[handleRecoverAttachSession] Loading available sessions for terminal ${terminalId}`);

  try {
    const sessions = await invoke<string[]>("list_available_sessions");
    console.log(`[handleRecoverAttachSession] Found ${sessions.length} sessions:`, sessions);

    // Import renderAvailableSessionsList
    const { renderAvailableSessionsList } = await import("./lib/ui");
    renderAvailableSessionsList(terminalId, sessions);
  } catch (error) {
    console.error(`[handleRecoverAttachSession] Failed to list sessions:`, error);
    alert(`Failed to list available sessions: ${error}`);
  }
}

async function handleAttachToSession(terminalId: string, sessionName: string) {
  console.log(`[handleAttachToSession] Attaching terminal ${terminalId} to session ${sessionName}`);

  try {
    await invoke("attach_to_session", {
      id: terminalId,
      sessionName,
    });

    // Update terminal status
    const terminal = state.getTerminals().find((t) => t.id === terminalId);
    if (terminal) {
      state.updateTerminal(terminalId, {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
    }

    // Save config
    await saveCurrentConfig();

    console.log(`[handleAttachToSession] Attached successfully`);
  } catch (error) {
    console.error(`[handleAttachToSession] Failed to attach:`, error);
    alert(`Failed to attach to session: ${error}`);
  }
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

  // Primary terminal - double-click to rename, click for settings/clear
  const primaryTerminal = document.getElementById("primary-terminal");
  if (primaryTerminal) {
    // Button clicks (settings, clear)
    primaryTerminal.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;

      // Settings button
      const settingsBtn = target.closest("#terminal-settings-btn");
      if (settingsBtn) {
        e.stopPropagation();
        const id = settingsBtn.getAttribute("data-terminal-id");
        if (id) {
          console.log(`[terminal-settings-btn] Opening settings for terminal ${id}`);
          const terminal = state.getTerminals().find((t) => t.id === id);
          if (terminal) {
            showTerminalSettingsModal(terminal, state, render);
          }
        }
        return;
      }

      // Clear button
      const clearBtn = target.closest("#terminal-clear-btn");
      if (clearBtn) {
        e.stopPropagation();
        const id = clearBtn.getAttribute("data-terminal-id");
        if (id) {
          console.log(`[terminal-clear-btn] Clearing terminal ${id}`);
          terminalManager.clearTerminal(id);
        }
        return;
      }

      // Recovery - Create new session
      const recoverNewBtn = target.closest("#recover-new-session-btn");
      if (recoverNewBtn) {
        e.stopPropagation();
        const id = recoverNewBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRecoverNewSession(id);
        }
        return;
      }

      // Recovery - Attach to existing session
      const recoverAttachBtn = target.closest("#recover-attach-session-btn");
      if (recoverAttachBtn) {
        e.stopPropagation();
        const id = recoverAttachBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRecoverAttachSession(id);
        }
        return;
      }

      // Recovery - Attach to specific session
      const attachSessionItem = target.closest(".attach-session-item");
      if (attachSessionItem) {
        e.stopPropagation();
        const id = attachSessionItem.getAttribute("data-terminal-id");
        const sessionName = attachSessionItem.getAttribute("data-session-name");
        if (id && sessionName) {
          handleAttachToSession(id, sessionName);
        }
        return;
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
          ask("Are you sure you want to close this terminal?", {
            title: "Close Terminal",
            type: "warning",
          }).then(async (confirmed) => {
            if (confirmed) {
              // Stop autonomous mode if running
              const { getAutonomousManager } = await import("./lib/autonomous-manager");
              const autonomousManager = getAutonomousManager();
              autonomousManager.stopAutonomous(id);

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
          });
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
