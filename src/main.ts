import "./style.css";
import { ask, open } from "@tauri-apps/api/dialog";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/tauri";
import { saveConfig, saveState, setConfigWorkspace, splitTerminals } from "./lib/config";
import { setupDragAndDrop } from "./lib/drag-drop-manager";
import { getHealthMonitor } from "./lib/health-monitor";
import { getOutputPoller } from "./lib/output-poller";
// Note: Recovery handlers removed - app now auto-recovers missing sessions
import { AppState, setAppState, type Terminal, TerminalStatus } from "./lib/state";
import { handleRunNowClick, startRename } from "./lib/terminal-actions";
import {
  launchAgentsForTerminals as launchAgentsForTerminalsCore,
  reconnectTerminals as reconnectTerminalsCore,
  verifyTerminalSessions as verifyTerminalSessionsCore,
} from "./lib/terminal-lifecycle";
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
import { handleWorkspacePathInput as handleWorkspacePathInputCore } from "./lib/workspace-lifecycle";
import { clearWorkspaceError, showWorkspaceError } from "./lib/workspace-utils";

// NOTE: validateWorkspacePath and browseWorkspace are defined locally in this file
// handleWorkspacePathInput is now in src/lib/workspace-lifecycle.ts
// launchAgentsForTerminals, reconnectTerminals, verifyTerminalSessions are now in src/lib/terminal-lifecycle.ts

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

  // PRIORITY 1: Check for CLI workspace argument (highest priority)
  try {
    const cliWorkspace = await invoke<string | null>("get_cli_workspace");
    if (cliWorkspace) {
      console.log("[initializeApp] [CLI] Found CLI workspace argument:", cliWorkspace);
      console.log(
        "[initializeApp] [CLI] Using CLI workspace (takes precedence over stored workspace)"
      );

      // Validate CLI workspace
      const isValid = await validateWorkspacePath(cliWorkspace);
      if (isValid) {
        console.log("[initializeApp] [CLI] CLI workspace is valid, loading...");
        await handleWorkspacePathInput(cliWorkspace);
        state.setInitializing(false);
        return; // CLI workspace loaded successfully - skip stored workspace
      }

      console.warn(
        "[initializeApp] [CLI] CLI workspace is invalid, falling back to stored workspace"
      );
    } else {
      console.log("[initializeApp] [CLI] No CLI workspace argument provided");
    }
  } catch (error) {
    console.error("[initializeApp] [CLI] Failed to get CLI workspace:", error);
    // Continue to stored workspace fallback
  }

  // PRIORITY 2: Try to restore workspace from localStorage (for HMR survival)
  const localStorageWorkspace = state.restoreWorkspaceFromLocalStorage();
  if (localStorageWorkspace) {
    console.log(
      "[initializeApp] [localStorage] Restored workspace from localStorage:",
      localStorageWorkspace
    );
    console.log(
      "[initializeApp] [localStorage] This prevents HMR from clearing the workspace during hot reload"
    );
  }

  try {
    // PRIORITY 3: Check for stored workspace in Tauri storage (lowest priority)
    const storedPath = await invoke<string | null>("get_stored_workspace");

    if (storedPath) {
      console.log("[initializeApp] [Tauri] Found stored workspace:", storedPath);

      // Validate stored workspace is still valid
      const isValid = await validateWorkspacePath(storedPath);

      if (isValid) {
        // Load workspace automatically
        console.log("[initializeApp] [Tauri] Loading stored workspace");
        await handleWorkspacePathInput(storedPath);
        state.setInitializing(false);
        return;
      }

      // Path no longer valid - clear it and show picker
      console.log("[initializeApp] [Tauri] Stored workspace invalid, clearing");
      await invoke("clear_stored_workspace");
      localStorage.removeItem("loom:workspace"); // Also clear localStorage
    } else if (localStorageWorkspace) {
      // No Tauri storage but have localStorage (HMR case)
      console.log("[initializeApp] [localStorage] Using localStorage workspace after HMR");
      const isValid = await validateWorkspacePath(localStorageWorkspace);

      if (isValid) {
        console.log("[initializeApp] [localStorage] localStorage workspace is valid, loading...");
        await handleWorkspacePathInput(localStorageWorkspace);
        state.setInitializing(false);
        return;
      }

      // Invalid - clear it
      console.log("[initializeApp] [localStorage] localStorage workspace is invalid, clearing");
      localStorage.removeItem("loom:workspace");
    }
  } catch (error) {
    console.error("[initializeApp] [Tauri] Failed to load stored workspace:", error);

    // If Tauri storage failed but we have localStorage, try that
    if (localStorageWorkspace) {
      console.log(
        "[initializeApp] [localStorage] Tauri storage failed, trying localStorage workspace"
      );
      const isValid = await validateWorkspacePath(localStorageWorkspace);
      if (isValid) {
        console.log("[initializeApp] [localStorage] localStorage workspace is valid, loading...");
        await handleWorkspacePathInput(localStorageWorkspace);
        state.setInitializing(false);
        return;
      }

      console.log("[initializeApp] [localStorage] localStorage workspace is invalid");
    }
  }

  // No workspace found or all validation failed - show picker
  console.log("[initializeApp] [Fallback] No valid workspace found, showing workspace picker");
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

// Drag and drop state moved to src/lib/drag-drop-manager.ts

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

// Launch agents for terminals (wrapper for terminal-lifecycle module)
async function launchAgentsForTerminals(workspacePath: string, terminals: Terminal[]) {
  await launchAgentsForTerminalsCore(workspacePath, terminals, { state });
}

// Verify terminal sessions health (wrapper for terminal-lifecycle module)
async function verifyTerminalSessions(): Promise<void> {
  await verifyTerminalSessionsCore({ state });
}

// Reconnect terminals to daemon (wrapper for terminal-lifecycle module)
async function reconnectTerminals() {
  await reconnectTerminalsCore({
    state,
    initializeTerminalDisplay,
    saveCurrentConfig,
  });
}

// Handle manual workspace path entry (wrapper for workspace-lifecycle module)
async function handleWorkspacePathInput(path: string) {
  await handleWorkspacePathInputCore(path, {
    state,
    validateWorkspacePath,
    launchAgentsForTerminals,
    reconnectTerminals,
    verifyTerminalSessions,
  });
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

      // Note: Manual recovery buttons removed - app now auto-recovers missing sessions

      // Health Check - Check Now button
      const checkNowBtn = target.closest("#check-now-btn");
      if (checkNowBtn) {
        e.stopPropagation();
        const id = checkNowBtn.getAttribute("data-terminal-id");
        if (id) {
          console.log(`[check-now-btn] Triggering immediate health check for terminal ${id}`);
          // Trigger immediate health check
          healthMonitor
            .performHealthCheck()
            .then(() => {
              console.log(`[check-now-btn] Health check complete for terminal ${id}`);
            })
            .catch((error: unknown) => {
              console.error(`[check-now-btn] Health check failed:`, error);
            });
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

    // Set up drag-and-drop handlers (extracted to drag-drop-manager.ts)
    setupDragAndDrop(miniRow, state, saveCurrentConfig);
  }
}

// Set up all event listeners once
setupEventListeners();
