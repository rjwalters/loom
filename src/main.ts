import "./style.css";
import { ask, open } from "@tauri-apps/api/dialog";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/tauri";
import {
  loadWorkspaceConfig,
  saveConfig,
  saveState,
  setConfigWorkspace,
  splitTerminals,
} from "./lib/config";
import { getHealthMonitor } from "./lib/health-monitor";
import { getOutputPoller } from "./lib/output-poller";
import {
  handleAttachToSession,
  handleKillSession,
  handleRecoverAttachSession,
  handleRecoverNewSession,
} from "./lib/recovery-handlers";
import { AppState, setAppState, type Terminal, TerminalStatus } from "./lib/state";
import { handleRunNowClick, startRename } from "./lib/terminal-actions";
// NOTE: launchAgentsForTerminals, reconnectTerminals, and saveCurrentConfig
// are defined locally in this file, not imported from terminal-lifecycle
import { getTerminalManager } from "./lib/terminal-manager";
import { showTerminalSettingsModal } from "./lib/terminal-settings-modal";
import { initTheme, toggleTheme } from "./lib/theme";
import {
  renderHeader,
  renderLoadingState,
  renderMiniTerminals,
  renderPrimaryTerminal,
} from "./lib/ui";
import { attachWorkspaceEventListeners, setupTooltips } from "./lib/ui-event-handlers";
import { clearWorkspaceError, expandTildePath, showWorkspaceError } from "./lib/workspace-utils";

// NOTE: validateWorkspacePath, browseWorkspace, and handleWorkspacePathInput
// are defined locally in this file

// =================================================================
// CONSOLE LOGGING TO FILE - For MCP access to browser console
// =================================================================
// Intercept console methods and write to ~/.loom/console.log
// This allows MCP tools to read console output for debugging

const originalConsoleLog = console.log;
const originalConsoleError = console.error;
const originalConsoleWarn = console.warn;

async function writeToConsoleLog(level: string, ...args: unknown[]) {
  const timestamp = new Date().toISOString();
  const message = args
    .map((arg) => (typeof arg === "object" ? JSON.stringify(arg) : String(arg)))
    .join(" ");
  const logLine = `[${timestamp}] [${level}] ${message}\n`;

  try {
    await invoke("append_to_console_log", { content: logLine });
  } catch (error) {
    // Silent fail - don't want logging errors to break the app
    // Only log to original console if something goes wrong
    originalConsoleError("[console-logger] Failed to write to log file:", error);
  }
}

// Override console methods
console.log = (...args: unknown[]) => {
  originalConsoleLog(...args);
  writeToConsoleLog("INFO", ...args);
};

console.error = (...args: unknown[]) => {
  originalConsoleError(...args);
  writeToConsoleLog("ERROR", ...args);
};

console.warn = (...args: unknown[]) => {
  originalConsoleWarn(...args);
  writeToConsoleLog("WARN", ...args);
};

// Initialize theme
initTheme();

// Initialize state (no agents until workspace is selected)
const state = new AppState();
setAppState(state); // Register singleton so terminal-manager can access it

// Get terminal manager, output poller, and health monitor
const terminalManager = getTerminalManager();
const outputPoller = getOutputPoller();
const healthMonitor = getHealthMonitor();

// Register activity callback - notify health monitor when output is received
outputPoller.onActivity((terminalId) => {
  healthMonitor.recordActivity(terminalId);
});

// Register error callback for polling failures
outputPoller.onError((terminalId, errorMessage) => {
  console.warn(
    `[outputPoller] Terminal ${terminalId} encountered fatal errors (${errorMessage}), marking as error state`
  );

  // Update terminal state
  const terminal = state.getTerminal(terminalId);
  if (terminal) {
    state.updateTerminal(terminal.id, {
      status: TerminalStatus.Error,
      missingSession: true,
    });
  }
});

// Start health monitoring
healthMonitor.start();
console.log("[main] Health monitoring started");

// Subscribe to health updates to trigger re-renders
healthMonitor.onHealthUpdate(() => {
  // Trigger a re-render when health status changes
  render();
});
console.log("[main] Subscribed to health monitor updates");

// Update timer displays every second
let renderLoopCount = 0;
window.setInterval(() => {
  // Re-render to update timer displays without full state change
  // This ensures busy/idle timers update in real-time
  const terminals = state.getTerminals();
  if (terminals.length > 0) {
    renderLoopCount++;
    console.log(
      `[render-loop] [Phase 3] Render loop #${renderLoopCount} triggered (${terminals.length} terminals)`
    );
    render();
  }
}, 1000);
console.log("[main] Timer update interval started");

// =================================================================
// EVENT LISTENER DEDUPLICATION
// =================================================================
// Track if event listeners have been registered to prevent duplicates
// This is critical because HMR (Hot Module Replacement) doesn't clean up
// old listeners, causing duplicate event firings and multiple agent launches
let eventListenersRegistered = false;

// =================================================================
// MCP COMMAND FILE WATCHER - For MCP tool automation
// =================================================================
// File watching is now handled by the Rust backend (mcp_watcher.rs) using the notify crate
// This provides event-driven file watching with 0 CPU usage when idle (vs 120 fs reads/min with polling)
console.log("[main] MCP command watcher started in backend (using notify crate)");

// Track which terminal is currently attached
let currentAttachedTerminalId: string | null = null;

// Track which terminals have had their health checked (Phase 3: Debouncing)
// This prevents redundant health checks during the 1-second render loop
const healthCheckedTerminals = new Set<string>();

// Render function
function render() {
  const hasWorkspace = state.hasWorkspace();
  const isResetting = state.isWorkspaceResetting();
  const isInitializing = state.isAppInitializing();
  console.log(
    "[render] hasWorkspace:",
    hasWorkspace,
    "displayedWorkspace:",
    state.getDisplayedWorkspace(),
    "isResetting:",
    isResetting,
    "isInitializing:",
    isInitializing
  );

  // Get health data from health monitor
  const systemHealth = healthMonitor.getHealth();

  // Render header with daemon health
  renderHeader(
    state.getDisplayedWorkspace(),
    hasWorkspace,
    systemHealth.daemon.connected,
    systemHealth.daemon.lastPing
  );

  // Show loading state if initializing
  if (isInitializing) {
    renderLoadingState("Initializing Loom...");
    // Don't render terminals or workspace selector while initializing
    return;
  }

  // Show loading state if factory reset is in progress
  if (isResetting) {
    renderLoadingState("Resetting workspace...");
    // Don't render terminals or workspace selector while resetting
    return;
  }

  renderPrimaryTerminal(state.getPrimary(), hasWorkspace, state.getDisplayedWorkspace());

  // Render mini terminals with health data
  const terminalHealthMap = new Map(
    Array.from(systemHealth.terminals.entries()).map(([id, health]) => [
      id,
      {
        lastActivity: health.lastActivity,
        isStale: health.isStale,
      },
    ])
  );
  renderMiniTerminals(state.getTerminals(), hasWorkspace, terminalHealthMap);

  // Re-attach workspace event listeners if they were just rendered
  if (!hasWorkspace) {
    attachWorkspaceEventListeners(
      handleWorkspacePathInput,
      browseWorkspace,
      () => state.getWorkspace() || ""
    );
  }

  // Set up tooltips for all elements with data-tooltip attributes
  setupTooltips();

  // Initialize xterm.js terminal for primary terminal
  const primary = state.getPrimary();
  if (primary && hasWorkspace) {
    initializeTerminalDisplay(primary.id);
  }
}

// Initialize xterm.js terminal display
async function initializeTerminalDisplay(terminalId: string) {
  const containerId = `xterm-container-${terminalId}`;

  // Skip placeholder IDs - they're already broken and will show error UI
  if (terminalId === "__unassigned__") {
    console.warn(`[initializeTerminalDisplay] Skipping placeholder terminal ID`);
    return;
  }

  // Phase 3: Skip health check if already checked (debouncing)
  if (healthCheckedTerminals.has(terminalId)) {
    console.log(
      `[initializeTerminalDisplay] [Phase 3] Terminal ${terminalId} already health-checked, skipping redundant check (Set size: ${healthCheckedTerminals.size})`
    );
    // Continue with xterm initialization without re-checking
  } else {
    // Check session health before initializing
    try {
      console.log(
        `[initializeTerminalDisplay] [Phase 3] Performing NEW health check for terminal ${terminalId} (Set size before: ${healthCheckedTerminals.size})`
      );
      const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });
      console.log(
        `[initializeTerminalDisplay] check_session_health returned: ${hasSession} for terminal ${terminalId}`
      );

      if (!hasSession) {
        console.warn(`[initializeTerminalDisplay] Terminal ${terminalId} has no tmux session`);

        // Mark terminal as having missing session (only if not already marked)
        const terminal = state.getTerminal(terminalId);
        console.log(`[initializeTerminalDisplay] Terminal state before update:`, terminal);
        if (terminal && !terminal.missingSession) {
          console.log(
            `[initializeTerminalDisplay] Setting missingSession=true for terminal ${terminalId}`
          );
          state.updateTerminal(terminal.id, {
            status: TerminalStatus.Error,
            missingSession: true,
          });
        }

        // Add to checked set even for failures to prevent repeated checks
        healthCheckedTerminals.add(terminalId);
        console.log(
          `[initializeTerminalDisplay] [Phase 3] Added ${terminalId} to healthCheckedTerminals (failed check, Set size: ${healthCheckedTerminals.size})`
        );
        return; // Don't create xterm instance - error UI will show instead
      }

      console.log(
        `[initializeTerminalDisplay] Session health check passed for terminal ${terminalId}, proceeding with xterm initialization`
      );

      // Add to checked set after successful health check (Phase 3: Debouncing)
      healthCheckedTerminals.add(terminalId);
      console.log(
        `[initializeTerminalDisplay] [Phase 3] Added ${terminalId} to healthCheckedTerminals (passed check, Set size: ${healthCheckedTerminals.size})`
      );
    } catch (error) {
      console.error(`[initializeTerminalDisplay] Failed to check session health:`, error);
      // Add to checked set even on error to prevent retry spam
      healthCheckedTerminals.add(terminalId);
      console.log(
        `[initializeTerminalDisplay] [Phase 3] Added ${terminalId} to healthCheckedTerminals (error during check, Set size: ${healthCheckedTerminals.size})`
      );
      // Continue anyway - better to try than not
    }
  }

  // Check if terminal already exists
  const existingManaged = terminalManager.getTerminal(terminalId);
  if (existingManaged) {
    // Terminal exists - just show/hide as needed
    console.log(
      `[initializeTerminalDisplay] Terminal ${terminalId} already exists, using show/hide`
    );

    // Hide previous terminal (if different)
    if (currentAttachedTerminalId && currentAttachedTerminalId !== terminalId) {
      console.log(`[initializeTerminalDisplay] Hiding terminal ${currentAttachedTerminalId}`);
      terminalManager.hideTerminal(currentAttachedTerminalId);
      outputPoller.pausePolling(currentAttachedTerminalId);
    }

    // Show current terminal
    console.log(`[initializeTerminalDisplay] Showing terminal ${terminalId}`);
    terminalManager.showTerminal(terminalId);

    // Resume polling for current terminal
    console.log(`[initializeTerminalDisplay] Resuming polling for ${terminalId}`);
    outputPoller.resumePolling(terminalId);

    currentAttachedTerminalId = terminalId;
    return;
  }

  // Terminal doesn't exist yet - create it
  console.log(`[initializeTerminalDisplay] Creating new terminal ${terminalId}`);

  // Wait for DOM to be ready
  setTimeout(() => {
    // Hide all other terminals first
    if (currentAttachedTerminalId) {
      terminalManager.hideTerminal(currentAttachedTerminalId);
      outputPoller.pausePolling(currentAttachedTerminalId);
    }

    // Create new terminal (will be shown by default in createTerminal)
    const managed = terminalManager.createTerminal(terminalId, containerId);
    if (managed) {
      // Show this terminal
      terminalManager.showTerminal(terminalId);

      // Start polling for output
      outputPoller.startPolling(terminalId);
      currentAttachedTerminalId = terminalId;

      console.log(`[initializeTerminalDisplay] Created and showing terminal ${terminalId}`);
    }
  }, 0);
}

// Check dependencies on startup
async function checkDependenciesOnStartup(): Promise<boolean> {
  const { checkAndReportDependencies } = await import("./lib/dependency-checker");
  const hasAllDependencies = await checkAndReportDependencies();

  if (!hasAllDependencies) {
    // User chose not to retry, close the app gracefully
    console.error("[checkDependenciesOnStartup] Missing dependencies, exiting");
    const { exit } = await import("@tauri-apps/api/process");
    await exit(1);
    return false;
  }

  return true;
}

// Initialize app with auto-load workspace
async function initializeApp() {
  // Set initializing state to show loading UI
  state.setInitializing(true);
  console.log("[initializeApp] Starting initialization...");

  // Check dependencies first
  const hasAllDependencies = await checkDependenciesOnStartup();
  if (!hasAllDependencies) {
    state.setInitializing(false);
    return; // Exit early if dependencies are missing
  }

  // Try to restore workspace from localStorage first (for HMR survival)
  const localStorageWorkspace = state.restoreWorkspaceFromLocalStorage();
  if (localStorageWorkspace) {
    console.log("[initializeApp] Restored workspace from localStorage:", localStorageWorkspace);
    console.log("[initializeApp] This prevents HMR from clearing the workspace during hot reload");
  }

  try {
    // Check for stored workspace in Tauri storage
    const storedPath = await invoke<string | null>("get_stored_workspace");

    if (storedPath) {
      console.log("[initializeApp] Found stored workspace:", storedPath);

      // Validate stored workspace is still valid
      const isValid = await validateWorkspacePath(storedPath);

      if (isValid) {
        // Load workspace automatically
        console.log("[initializeApp] Loading stored workspace");
        await handleWorkspacePathInput(storedPath);
        state.setInitializing(false);
        return;
      }

      // Path no longer valid - clear it and show picker
      console.log("[initializeApp] Stored workspace invalid, clearing");
      await invoke("clear_stored_workspace");
      localStorage.removeItem("loom:workspace"); // Also clear localStorage
    } else if (localStorageWorkspace) {
      // No Tauri storage but have localStorage (HMR case)
      console.log("[initializeApp] Using localStorage workspace after HMR");
      const isValid = await validateWorkspacePath(localStorageWorkspace);

      if (isValid) {
        await handleWorkspacePathInput(localStorageWorkspace);
        state.setInitializing(false);
        return;
      }

      // Invalid - clear it
      localStorage.removeItem("loom:workspace");
    }
  } catch (error) {
    console.error("[initializeApp] Failed to load stored workspace:", error);

    // If Tauri storage failed but we have localStorage, try that
    if (localStorageWorkspace) {
      console.log("[initializeApp] Tauri storage failed, trying localStorage workspace");
      const isValid = await validateWorkspacePath(localStorageWorkspace);
      if (isValid) {
        await handleWorkspacePathInput(localStorageWorkspace);
        return;
      }
    }
  }

  // No stored workspace or validation failed - show picker
  console.log("[initializeApp] Showing workspace picker");
  state.setInitializing(false);
  render();
}

// Re-render on state changes
state.onChange(render);

// Register all event listeners (with deduplication guard)
if (!eventListenersRegistered) {
  console.log("[main] Registering event listeners (first time only)");

  // Render immediately so users see the loading screen before async init
  render();
  eventListenersRegistered = true;

  // Listen for CLI workspace argument from Rust backend
  listen("cli-workspace", (event) => {
    const workspacePath = event.payload as string;
    console.log(`[CLI] Loading workspace from CLI argument: ${workspacePath}`);
    handleWorkspacePathInput(workspacePath);
  });

  // Listen for menu events
  listen("new-terminal", () => {
    if (state.hasWorkspace()) {
      createPlainTerminal();
    }
  });

  listen("close-terminal", async () => {
    const primary = state.getPrimary();
    if (primary) {
      const confirmed = await ask(`Are you sure you want to close "${primary.name}"?`, {
        title: "Close Terminal",
        type: "warning",
      });

      if (confirmed) {
        // Stop autonomous mode if running
        const { getAutonomousManager } = await import("./lib/autonomous-manager");
        const autonomousManager = getAutonomousManager();
        autonomousManager.stopAutonomous(primary.id);

        // Stop polling and destroy terminal
        outputPoller.stopPolling(primary.id);
        terminalManager.destroyTerminal(primary.id);
        if (currentAttachedTerminalId === primary.id) {
          currentAttachedTerminalId = null;
        }

        // Remove from state
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

    // Clear localStorage workspace (for HMR survival)
    localStorage.removeItem("loom:workspace");
    console.log("[close-workspace] Cleared localStorage workspace");

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

    // Phase 3: Clear health check tracking when workspace closes
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    console.log(
      `[close-workspace] [Phase 3] Cleared healthCheckedTerminals Set (was ${previousSize}, now ${healthCheckedTerminals.size})`
    );

    // Re-render to show workspace picker
    console.log("[close-workspace] Rendering workspace picker");
    render();
  });

  // Start engine - create sessions for existing config (with confirmation)
  listen("start-workspace", async () => {
    if (!state.hasWorkspace()) return;
    const workspace = state.getWorkspaceOrThrow();

    const confirmed = await ask(
      "This will:\n" +
        "• Close all current terminal sessions\n" +
        "• Create new sessions for configured terminals\n" +
        "• Launch agents as configured\n\n" +
        "Your configuration will NOT be changed.\n\n" +
        "Continue?",
      {
        title: "Start Loom Engine",
        type: "info",
      }
    );

    if (!confirmed) return;

    // Phase 3: Clear health check tracking when starting workspace (terminals will be recreated)
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    console.log(
      `[start-workspace] [Phase 3] Cleared healthCheckedTerminals Set (was ${previousSize}, now ${healthCheckedTerminals.size})`
    );

    // Use the workspace start module (reads existing config)
    const { startWorkspaceEngine } = await import("./lib/workspace-start");
    await startWorkspaceEngine(
      workspace,
      {
        state,
        outputPoller,
        terminalManager,
        setCurrentAttachedTerminalId: (id) => {
          currentAttachedTerminalId = id;
        },
        launchAgentsForTerminals,
        render,
        markTerminalsHealthChecked: (terminalIds) => {
          terminalIds.forEach((id) => healthCheckedTerminals.add(id));
          console.log(
            `[start-workspace] [Phase 3] Marked ${terminalIds.length} terminals as health-checked, Set size: ${healthCheckedTerminals.size}`
          );
        },
      },
      "start-workspace"
    );
  });

  // Force Start engine - NO confirmation dialog (for MCP automation)
  listen("force-start-workspace", async () => {
    if (!state.hasWorkspace()) return;
    const workspace = state.getWorkspaceOrThrow();

    console.log("[force-start-workspace] Starting engine (no confirmation)");

    // Phase 3: Clear health check tracking when starting workspace (terminals will be recreated)
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    console.log(
      `[force-start-workspace] [Phase 3] Cleared healthCheckedTerminals Set (was ${previousSize}, now ${healthCheckedTerminals.size})`
    );

    // Use the workspace start module (no confirmation)
    const { startWorkspaceEngine } = await import("./lib/workspace-start");
    await startWorkspaceEngine(
      workspace,
      {
        state,
        outputPoller,
        terminalManager,
        setCurrentAttachedTerminalId: (id) => {
          currentAttachedTerminalId = id;
        },
        launchAgentsForTerminals,
        render,
        markTerminalsHealthChecked: (terminalIds) => {
          terminalIds.forEach((id) => healthCheckedTerminals.add(id));
          console.log(
            `[force-start-workspace] [Phase 3] Marked ${terminalIds.length} terminals as health-checked, Set size: ${healthCheckedTerminals.size}`
          );
        },
      },
      "force-start-workspace"
    );
  });

  // Factory Reset - overwrite config with defaults (with confirmation)
  listen("factory-reset-workspace", async () => {
    if (!state.hasWorkspace()) return;
    const workspace = state.getWorkspaceOrThrow();

    const confirmed = await ask(
      "⚠️ WARNING: Factory Reset ⚠️\n\n" +
        "This will:\n" +
        "• DELETE all terminal configurations\n" +
        "• OVERWRITE .loom/ with default config\n" +
        "• Reset all roles to defaults\n" +
        "• Close all current terminals\n" +
        "• Recreate 6 default terminals\n\n" +
        "This action CANNOT be undone!\n\n" +
        "Continue with Factory Reset?",
      {
        title: "⚠️ Factory Reset Warning",
        type: "warning",
      }
    );

    if (!confirmed) return;

    // Phase 3: Clear health check tracking when resetting workspace (terminals will be recreated)
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    console.log(
      `[factory-reset-workspace] [Phase 3] Cleared healthCheckedTerminals Set (was ${previousSize}, now ${healthCheckedTerminals.size})`
    );

    // Set loading state before reset
    state.setResettingWorkspace(true);

    try {
      // Use the workspace reset module (overwrites config with defaults)
      const { resetWorkspaceToDefaults } = await import("./lib/workspace-reset");
      await resetWorkspaceToDefaults(
        workspace,
        {
          state,
          outputPoller,
          terminalManager,
          setCurrentAttachedTerminalId: (id) => {
            currentAttachedTerminalId = id;
          },
          launchAgentsForTerminals,
          render,
        },
        "factory-reset-workspace"
      );
    } finally {
      // Clear loading state when done (even if error)
      state.setResettingWorkspace(false);
    }
  });

  // Force Factory Reset - NO confirmation dialog (for MCP automation)
  listen("force-factory-reset-workspace", async () => {
    if (!state.hasWorkspace()) return;
    const workspace = state.getWorkspaceOrThrow();

    console.log("[force-factory-reset-workspace] Resetting workspace (no confirmation)");

    // Phase 3: Clear health check tracking when resetting workspace (terminals will be recreated)
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    console.log(
      `[force-factory-reset-workspace] [Phase 3] Cleared healthCheckedTerminals Set (was ${previousSize}, now ${healthCheckedTerminals.size})`
    );

    // Set loading state before reset
    state.setResettingWorkspace(true);

    try {
      // Use the workspace reset module (no confirmation)
      const { resetWorkspaceToDefaults } = await import("./lib/workspace-reset");
      await resetWorkspaceToDefaults(
        workspace,
        {
          state,
          outputPoller,
          terminalManager,
          setCurrentAttachedTerminalId: (id) => {
            currentAttachedTerminalId = id;
          },
          launchAgentsForTerminals,
          render,
        },
        "force-factory-reset-workspace"
      );
    } finally {
      // Clear loading state when done (even if error)
      state.setResettingWorkspace(false);
    }
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

  listen("show-shortcuts", async () => {
    const { showKeyboardShortcutsModal } = await import("./lib/keyboard-shortcuts-modal");
    showKeyboardShortcutsModal();
  });

  listen("show-daemon-status", async () => {
    showDaemonStatusDialog();
  });

  console.log("[main] Event listeners registered successfully");
} else {
  console.log("[main] Event listeners already registered, skipping duplicate registration");
}

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

    const hasWorkspace = state.hasWorkspace();

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

// Drag and drop state (uses configId for stable identification)
let draggedConfigId: string | null = null;
let dropTargetConfigId: string | null = null;
let dropInsertBefore: boolean = false;
let isDragging: boolean = false;

// Save current state to config and state files
async function saveCurrentConfig() {
  if (!state.hasWorkspace()) {
    return;
  }

  const terminals = state.getTerminals();
  const { config: terminalConfigs, state: terminalStates } = splitTerminals(terminals);

  await saveConfig({ terminals: terminalConfigs });
  await saveState({
    nextAgentNumber: state.getCurrentTerminalNumber(),
    terminals: terminalStates,
  });
}

// Workspace error UI helpers and path utilities are now in src/lib/workspace-utils.ts

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

// Helper to generate next config ID
function generateNextConfigId(): string {
  const terminals = state.getTerminals();
  const existingIds = new Set(terminals.map((t) => t.id));

  // Find the next available terminal-N ID
  let i = 1;
  while (existingIds.has(`terminal-${i}`)) {
    i++;
  }

  return `terminal-${i}`;
}

// Create a plain shell terminal
async function createPlainTerminal() {
  if (!state.hasWorkspace()) {
    alert("No workspace selected");
    return;
  }
  const workspacePath = state.getWorkspaceOrThrow();

  // Generate terminal name
  const terminalCount = state.getTerminals().length + 1;
  const name = `Terminal ${terminalCount}`;

  try {
    // Generate stable ID first
    const id = generateNextConfigId();

    // Get instance number for this terminal
    const instanceNumber = state.getNextTerminalNumber();

    // Create terminal in workspace directory
    const terminalId = await invoke<string>("create_terminal", {
      configId: id,
      name,
      workingDir: workspacePath,
      role: "default",
      instanceNumber,
    });

    console.log(`[createPlainTerminal] Created terminal ${name} (id: ${id}, tmux: ${terminalId})`);

    // Create worktree for this terminal
    console.log(`[createPlainTerminal] Creating worktree for ${name}...`);
    const { setupWorktreeForAgent } = await import("./lib/worktree-manager");
    const worktreePath = await setupWorktreeForAgent(id, workspacePath);
    console.log(`[createPlainTerminal] ✓ Created worktree at ${worktreePath}`);

    // Add to state (no role assigned - plain shell)
    state.addTerminal({
      id,
      name,
      worktreePath,
      status: TerminalStatus.Idle,
      isPrimary: false,
      theme: "default",
    });

    // Save updated state to config
    await saveCurrentConfig();

    // Switch to new terminal
    state.setPrimary(id);
  } catch (error) {
    console.error("[createPlainTerminal] Failed to create terminal:", error);
    alert(`Failed to create terminal: ${error}`);
  }
}

/**
 * Launch agents for terminals that have role configurations
 *
 * This function is called after workspace initialization or factory reset
 * to automatically start Claude agents for terminals with roleConfig.
 *
 * @param workspacePath - The workspace directory path
 * @param terminals - Array of terminal configurations
 */
async function launchAgentsForTerminals(workspacePath: string, terminals: Terminal[]) {
  console.log("[launchAgentsForTerminals] Launching agents for configured terminals");
  console.log("[launchAgentsForTerminals] workspacePath:", workspacePath);
  console.log(
    "[launchAgentsForTerminals] terminals:",
    terminals.map((t) => `${t.name}=${t.id}, role=${t.role}`)
  );

  // Filter terminals that have Claude Code worker role
  const workersToLaunch = terminals.filter(
    (t) => t.role === "claude-code-worker" && t.roleConfig && t.roleConfig.roleFile
  );

  console.log(
    `[launchAgentsForTerminals] Found ${workersToLaunch.length} terminals with role configs`,
    workersToLaunch.map((t) => `${t.name}=${t.id}`)
  );

  // Track terminals that were successfully launched
  const launchedTerminalIds: string[] = [];

  // Launch each worker
  for (const terminal of workersToLaunch) {
    try {
      const roleConfig = terminal.roleConfig;
      if (!roleConfig || !roleConfig.roleFile) {
        continue;
      }

      console.log(`[launchAgentsForTerminals] Launching ${terminal.name} (${terminal.id})`);

      // Set terminal to busy status BEFORE launching agent
      // This prevents HealthMonitor from incorrectly marking it as missing during the launch process
      state.updateTerminal(terminal.id, { status: TerminalStatus.Busy });
      console.log(`[launchAgentsForTerminals] Set ${terminal.name} to busy status`);

      // Get worker type from config (default to claude)
      const workerType = (roleConfig.workerType as string) || "claude";

      // Launch based on worker type
      if (workerType === "github-copilot") {
        const { launchGitHubCopilotAgent } = await import("./lib/agent-launcher");
        await launchGitHubCopilotAgent(terminal.id);
      } else if (workerType === "gemini") {
        const { launchGeminiCLIAgent } = await import("./lib/agent-launcher");
        await launchGeminiCLIAgent(terminal.id);
      } else if (workerType === "deepseek") {
        const { launchDeepSeekAgent } = await import("./lib/agent-launcher");
        await launchDeepSeekAgent(terminal.id);
      } else if (workerType === "grok") {
        const { launchGrokAgent } = await import("./lib/agent-launcher");
        await launchGrokAgent(terminal.id);
      } else if (workerType === "codex") {
        // Codex with worktree support (optional - starts in main workspace if empty)
        console.log(`[launchAgentsForTerminals] Launching Codex for ${terminal.name}...`);
        const { launchCodexAgent } = await import("./lib/agent-launcher");

        // Use worktree path if available, otherwise main workspace
        const locationDesc = terminal.worktreePath
          ? `worktree ${terminal.worktreePath}`
          : "main workspace";
        console.log(
          `[launchAgentsForTerminals] Launching Codex agent for ${terminal.name} (id=${terminal.id}) in ${locationDesc}...`
        );

        // Launch Codex agent (will use main workspace if worktreePath is empty)
        await launchCodexAgent(
          terminal.id,
          roleConfig.roleFile as string,
          workspacePath,
          terminal.worktreePath || ""
        );

        console.log(`[launchAgentsForTerminals] Codex agent launched in ${locationDesc}`);
      } else {
        // Claude with worktree support (optional - starts in main workspace if empty)
        console.log(`[launchAgentsForTerminals] Importing agent-launcher for ${terminal.name}...`);
        const { launchAgentInTerminal } = await import("./lib/agent-launcher");

        // Use worktree path if available, otherwise main workspace
        const locationDesc = terminal.worktreePath
          ? `worktree ${terminal.worktreePath}`
          : "main workspace";
        console.log(
          `[launchAgentsForTerminals] Launching agent for ${terminal.name} (id=${terminal.id}) in ${locationDesc}...`
        );

        // Launch agent (will use main workspace if worktreePath is empty)
        await launchAgentInTerminal(
          terminal.id,
          roleConfig.roleFile as string,
          workspacePath,
          terminal.worktreePath || ""
        );

        console.log(`[launchAgentsForTerminals] Agent launched in ${locationDesc}`);
      }

      console.log(`[launchAgentsForTerminals] Successfully launched ${terminal.name}`);

      // Track successfully launched terminals (will reset to idle AFTER all launches complete)
      launchedTerminalIds.push(terminal.id);
    } catch (error) {
      const errorMessage = `Failed to launch agent for ${terminal.name}: ${error}`;
      console.error(`[launchAgentsForTerminals] ${errorMessage}`);

      // Still track this terminal ID - reset to idle after all launches complete
      // (agent launch failed but terminal exists and should not stay in busy state forever)
      launchedTerminalIds.push(terminal.id);

      // Show error to user so they know what failed
      alert(errorMessage);

      // Continue with other terminals even if one fails
    }
  }

  // Reset ALL launched terminals to idle status AFTER all launches complete
  // This prevents the periodic HealthMonitor (30s interval) from catching terminals in idle
  // state before all agent launches finish (which can take 2+ minutes for 6 terminals)
  console.log(
    `[launchAgentsForTerminals] All agent launches complete, resetting ${launchedTerminalIds.length} terminals to idle`
  );
  for (const terminalId of launchedTerminalIds) {
    state.updateTerminal(terminalId, { status: TerminalStatus.Idle });
    console.log(`[launchAgentsForTerminals] Reset ${terminalId} to idle status`);
  }

  console.log("[launchAgentsForTerminals] Agent launch complete");
}

/**
 * Verify terminal sessions health BEFORE rendering
 * This prevents false positives from stale missingSession flags
 * by batch-checking all terminals and updating state synchronously
 */
async function verifyTerminalSessions(): Promise<void> {
  const terminals = state.getTerminals();
  if (terminals.length === 0) {
    return;
  }

  console.log(`[verifyTerminalSessions] Checking health for ${terminals.length} terminals...`);

  // Batch check all terminals in parallel
  const checks = terminals.map(async (terminal) => {
    // Skip placeholder IDs
    if (terminal.id === "__unassigned__" || terminal.id === "__needs_session__") {
      return { terminal, hasSession: false };
    }

    try {
      const hasSession = await invoke<boolean>("check_session_health", { id: terminal.id });
      return { terminal, hasSession };
    } catch (error) {
      console.error(`[verifyTerminalSessions] Failed to check ${terminal.id}:`, error);
      return { terminal, hasSession: false };
    }
  });

  const results = await Promise.all(checks);

  // Update state for all terminals based on actual session health
  let clearedCount = 0;
  let markedMissingCount = 0;

  for (const { terminal, hasSession } of results) {
    if (hasSession && terminal.missingSession) {
      // Clear stale missingSession flag
      console.log(`[verifyTerminalSessions] Clearing stale missingSession flag for ${terminal.id}`);
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
      clearedCount++;
    } else if (!hasSession && !terminal.missingSession) {
      // Mark as missing if not already marked
      console.log(`[verifyTerminalSessions] Marking ${terminal.id} as missing session`);
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Error,
        missingSession: true,
      });
      markedMissingCount++;
    }
  }

  console.log(
    `[verifyTerminalSessions] Verification complete: ${clearedCount} cleared, ${markedMissingCount} marked missing`
  );
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
          `[reconnectTerminals] Agent ${agent.name} has placeholder ID, skipping (already in error state)`
        );

        // Don't call state.updateTerminal() here - it triggers infinite render loop
        // The terminal already shows as missing because check_session_health will fail for "__unassigned__"
        missingCount++;
        continue;
      }

      if (activeTerminalIds.has(agent.id)) {
        console.log(`[reconnectTerminals] Reconnecting agent ${agent.name} (${agent.id})`);

        // Clear any error state from previous connection issues (use configId for state)
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

        // Mark terminal as having missing session so user can see it needs recovery (use configId for state)
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
      const config = await loadWorkspaceConfig();
      state.setNextTerminalNumber(config.nextAgentNumber);

      if (config.agents && config.agents.length > 0) {
        console.log("[handleWorkspacePathInput] Creating terminals for fresh workspace");

        // Create terminal sessions for each agent in the config
        for (const agent of config.agents) {
          try {
            // Get instance number
            const instanceNumber = state.getNextTerminalNumber();

            // Create terminal in daemon
            const terminalId = await invoke<string>("create_terminal", {
              configId: agent.id,
              name: agent.name,
              workingDir: expandedPath,
              role: agent.role || "default",
              instanceNumber,
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

        // Launch agents for terminals with role configs
        await launchAgentsForTerminals(expandedPath, config.agents);

        // Save the updated config with real terminal IDs (including worktree paths)
        const terminalsToSave = state.getTerminals();
        const { config: terminalConfigs, state: terminalStates } = splitTerminals(terminalsToSave);
        await saveConfig({ terminals: terminalConfigs });
        await saveState({
          nextAgentNumber: state.getCurrentTerminalNumber(),
          terminals: terminalStates,
        });
        console.log("[handleWorkspacePathInput] Saved config with real terminal IDs");
      }
    } else {
      // Workspace already initialized - load existing config
      setConfigWorkspace(expandedPath);
      const config = await loadWorkspaceConfig();
      state.setNextTerminalNumber(config.nextAgentNumber);

      // Load agents from config
      if (config.agents && config.agents.length > 0) {
        console.log(
          `[handleWorkspacePathInput] Config agents before session creation:`,
          config.agents.map((a) => `${a.name}=${a.id}`)
        );

        // IMPORTANT: Create sessions for migrated terminals with placeholder IDs
        // After migration, terminals have configId but id="__needs_session__"
        let createdSessionCount = 0;
        for (const agent of config.agents) {
          if (agent.id === "__needs_session__") {
            try {
              // Get instance number
              const instanceNumber = state.getNextTerminalNumber();

              console.log(
                `[handleWorkspacePathInput] Creating session for migrated terminal "${agent.name}" (${agent.id})`
              );

              // Create terminal session in daemon
              const sessionId = await invoke<string>("create_terminal", {
                configId: agent.id,
                name: agent.name,
                workingDir: expandedPath,
                role: agent.role || "default",
                instanceNumber,
              });

              // Update agent with real session ID (keep configId stable)
              agent.id = sessionId;
              createdSessionCount++;

              console.log(
                `[handleWorkspacePathInput] ✓ Created session for ${agent.name}: ${sessionId}`
              );
            } catch (error) {
              console.error(
                `[handleWorkspacePathInput] Failed to create session for ${agent.name}:`,
                error
              );
              // Keep placeholder ID - terminal will show as missing session
              // User can use recovery options
            }
          }
        }

        if (createdSessionCount > 0) {
          console.log(
            `[handleWorkspacePathInput] Created ${createdSessionCount} sessions for migrated terminals`
          );
        }

        // Now load agents into state with their session IDs
        console.log(
          `[handleWorkspacePathInput] Config agents before loadAgents:`,
          config.agents.map((a) => `${a.name}=${a.id}`)
        );
        state.loadAgents(config.agents);
        console.log(
          `[handleWorkspacePathInput] State after loadAgents:`,
          state.getTerminals().map((a) => `${a.name}=${a.id}`)
        );

        // If we created sessions, save the updated config with real IDs
        if (createdSessionCount > 0) {
          const terminalsToSave = state.getTerminals();
          const { config: terminalConfigs, state: terminalStates } =
            splitTerminals(terminalsToSave);
          await saveConfig({ terminals: terminalConfigs });
          await saveState({
            nextAgentNumber: state.getCurrentTerminalNumber(),
            terminals: terminalStates,
          });
          console.log(
            `[handleWorkspacePathInput] Saved config with ${createdSessionCount} new session IDs`
          );
        }

        // Reconnect agents to existing daemon terminals
        await reconnectTerminals();

        // Verify terminal sessions health to clear any stale flags
        await verifyTerminalSessions();
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

// Terminal action handlers are now in src/lib/terminal-actions.ts
// Recovery handlers are now in src/lib/recovery-handlers.ts

// Set up event listeners (only once, since parent elements are static)
function setupEventListeners() {
  // Theme toggle
  document.getElementById("theme-toggle")?.addEventListener("click", () => {
    toggleTheme();
  });

  // Close workspace button
  document.getElementById("close-workspace-btn")?.addEventListener("click", async () => {
    if (state.hasWorkspace()) {
      await invoke("emit_event", { event: "close-workspace" });
    }
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

      // Run Now button (interval mode)
      const runNowBtn = target.closest(".run-now-btn");
      if (runNowBtn) {
        e.stopPropagation();
        const id = runNowBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRunNowClick(id, { state });
        }
        return;
      }

      // Close button
      const closeBtn = target.closest("#terminal-close-btn");
      if (closeBtn) {
        e.stopPropagation();
        const id = closeBtn.getAttribute("data-terminal-id");
        if (id) {
          const terminal = state.getTerminal(id);
          const terminalName = terminal ? terminal.name : "this terminal";
          ask(`Are you sure you want to close "${terminalName}"?`, {
            title: "Close Terminal",
            type: "warning",
          }).then(async (confirmed) => {
            if (confirmed) {
              console.log(`[terminal-close-btn] Closing terminal ${id}`);

              // Stop autonomous mode if running
              const { getAutonomousManager } = await import("./lib/autonomous-manager");
              const autonomousManager = getAutonomousManager();
              autonomousManager.stopAutonomous(id);

              // Stop polling and destroy terminal
              outputPoller.stopPolling(id);
              terminalManager.destroyTerminal(id);
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

      // Recovery - Create new session
      const recoverNewBtn = target.closest("#recover-new-session-btn");
      if (recoverNewBtn) {
        e.stopPropagation();
        const id = recoverNewBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRecoverNewSession(id, {
            state,
            generateNextConfigId,
            saveCurrentConfig,
          });
        }
        return;
      }

      // Recovery - Attach to existing session
      const recoverAttachBtn = target.closest("#recover-attach-session-btn");
      if (recoverAttachBtn) {
        e.stopPropagation();
        const id = recoverAttachBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRecoverAttachSession(id, state);
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
          handleAttachToSession(id, sessionName, {
            state,
            saveCurrentConfig,
          });
        }
        return;
      }

      // Recovery - Kill session
      const killSessionBtn = target.closest(".kill-session-btn");
      if (killSessionBtn) {
        e.stopPropagation();
        const sessionName = killSessionBtn.getAttribute("data-session-name");
        if (sessionName) {
          handleKillSession(sessionName, state);
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
          startRename(id, target, { state, saveCurrentConfig, render });
        }
      }
    });
  }

  // Mini terminal row - event delegation for dynamic children
  const miniRow = document.getElementById("mini-terminal-row");
  if (miniRow) {
    miniRow.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;

      // Handle Run Now button clicks (interval mode)
      const runNowBtn = target.closest(".run-now-btn");
      if (runNowBtn) {
        e.stopPropagation();
        const id = runNowBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRunNowClick(id, { state });
        }
        return;
      }

      // Handle close button clicks
      if (target.classList.contains("close-terminal-btn")) {
        e.stopPropagation();
        const id = target.getAttribute("data-terminal-id");

        if (id) {
          // Look up the terminal to get its name
          const terminal = state.getTerminal(id);
          const terminalName = terminal ? terminal.name : "this terminal";

          ask(`Are you sure you want to close "${terminalName}"?`, {
            title: "Close Terminal",
            type: "warning",
          }).then(async (confirmed) => {
            if (confirmed) {
              // Look up the terminal again for the rest of the logic
              const terminal = state.getTerminal(id);
              if (!terminal) {
                console.error(`Terminal with id ${id} not found`);
                return;
              }

              // Stop autonomous mode if running
              const { getAutonomousManager } = await import("./lib/autonomous-manager");
              const autonomousManager = getAutonomousManager();
              autonomousManager.stopAutonomous(id);

              // Stop polling and clean up xterm.js instance
              outputPoller.stopPolling(terminal.id);
              terminalManager.destroyTerminal(terminal.id);

              // If this was the current attached terminal, clear it
              if (currentAttachedTerminalId === terminal.id) {
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
        if (!state.hasWorkspace()) {
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
          startRename(id, target, { state, saveCurrentConfig, render });
        }
      }
    });

    // HTML5 drag events for visual feedback
    miniRow.addEventListener("dragstart", (e) => {
      const target = e.target as HTMLElement;
      const card = target.closest(".terminal-card") as HTMLElement;

      if (card) {
        isDragging = true;
        draggedConfigId = card.getAttribute("data-terminal-id"); // Will be configId after Phase 3
        card.classList.add("dragging");

        if (e.dataTransfer) {
          e.dataTransfer.effectAllowed = "move";
          e.dataTransfer.setData("text/html", card.innerHTML);
        }
      }
    });

    miniRow.addEventListener("dragend", (e) => {
      // Perform reorder if valid (uses configId for state operation)
      if (draggedConfigId && dropTargetConfigId && dropTargetConfigId !== draggedConfigId) {
        state.reorderTerminal(draggedConfigId, dropTargetConfigId, dropInsertBefore);
        saveCurrentConfig();
      }

      // Select the terminal that was dragged (uses configId for state operation)
      if (draggedConfigId) {
        state.setPrimary(draggedConfigId);
      }

      // Cleanup
      const target = e.target as HTMLElement;
      const card = target.closest(".terminal-card");
      if (card) {
        card.classList.remove("dragging");
      }

      document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());
      draggedConfigId = null;
      dropTargetConfigId = null;
      dropInsertBefore = false;
      isDragging = false;
    });

    // dragover for tracking position and showing indicator
    miniRow.addEventListener("dragover", (e) => {
      e.preventDefault();
      if (e.dataTransfer) {
        e.dataTransfer.dropEffect = "move";
      }

      if (!isDragging || !draggedConfigId) return;

      const target = e.target as HTMLElement;
      const card = target.closest(".terminal-card") as HTMLElement;

      if (card && card.getAttribute("data-terminal-id") !== draggedConfigId) {
        const targetId = card.getAttribute("data-terminal-id"); // Will be configId after Phase 3

        // Remove old indicators
        document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());

        // Calculate if we should insert before or after
        const rect = card.getBoundingClientRect();
        const midpoint = rect.left + rect.width / 2;
        const insertBefore = e.clientX < midpoint;

        // Store drop target info (configId)
        dropTargetConfigId = targetId;
        dropInsertBefore = insertBefore;

        // Create and position insertion indicator - insert at wrapper level
        const wrapper = card.parentElement;
        const indicator = document.createElement("div");
        indicator.className =
          "w-1 h-32 my-1 bg-blue-500 rounded flex-shrink-0 pointer-events-none animate-pulse";
        wrapper?.parentElement?.insertBefore(
          indicator,
          insertBefore ? wrapper : wrapper.nextSibling
        );
      } else if (!card) {
        // In empty space - find all cards and determine position
        const allCards = Array.from(miniRow.querySelectorAll(".terminal-card")) as HTMLElement[];
        const lastCard = allCards[allCards.length - 1];

        if (lastCard && !lastCard.classList.contains("dragging")) {
          const lastId = lastCard.getAttribute("data-terminal-id"); // Will be configId after Phase 3
          if (lastId && lastId !== draggedConfigId) {
            // Remove old indicators
            document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());

            // Drop after the last card
            dropTargetConfigId = lastId;
            dropInsertBefore = false;

            // Create and position insertion indicator after last card - insert at wrapper level
            const wrapper = lastCard.parentElement;
            const indicator = document.createElement("div");
            indicator.className =
              "w-1 h-32 my-1 bg-blue-500 rounded flex-shrink-0 pointer-events-none animate-pulse";
            wrapper?.parentElement?.insertBefore(indicator, wrapper?.nextSibling || null);
          }
        }
      }
    });
  }
}

// Set up all event listeners once
setupEventListeners();
