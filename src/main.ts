import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";

// Wait for Tauri to be ready before initializing the app
// Tries to actually invoke a Tauri command to verify IPC works
async function waitForTauri(): Promise<void> {
  const maxAttempts = 30;
  const delayMs = 200;

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      // Try to invoke a simple Tauri command
      // This will throw if Tauri IPC isn't ready
      await invoke("get_stored_workspace");
      console.log(`[Loom] Tauri IPC ready after ${attempt} attempt(s)`);

      // Also test the console logging command specifically
      try {
        await invoke("append_to_console_log", { message: "[TEST] Tauri IPC verified\n" });
        console.log("[Loom] append_to_console_log command verified working");
      } catch (testError) {
        console.error("[Loom] append_to_console_log command failed:", testError);
      }

      return;
    } catch (error: any) {
      // Log every attempt to help debug
      console.log(`[Loom] Tauri wait attempt ${attempt}/${maxAttempts}:`, error);

      // Check if it's a "command not found" error (IPC works but command doesn't exist)
      // vs an IPC error (Tauri not ready)
      const errorStr = String(error);
      if (errorStr.includes("command") || errorStr.includes("not found")) {
        // IPC is working, just the command might not exist (shouldn't happen but let's be safe)
        console.log(`[Loom] Tauri IPC ready after ${attempt} attempt(s) (command exists)`);
        return;
      }

      // If this is the last attempt, throw error
      if (attempt === maxAttempts) {
        const errorMsg = `Tauri IPC not available after ${maxAttempts} attempts (${(maxAttempts * delayMs) / 1000}s)`;
        console.error(`[Loom] ${errorMsg}`, error);
        throw new Error(errorMsg);
      }

      // Wait before next attempt
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
}

import { initializeApp } from "./lib/app-initializer";
import { appLevelState } from "./lib/app-state";
import { saveCurrentConfiguration, setConfigWorkspace } from "./lib/config";
import { initConsoleLogger } from "./lib/console-logger";
import { setupDragAndDrop } from "./lib/drag-drop-manager";
import { getHealthMonitor } from "./lib/health-monitor";
import {
  initializeKeyboardNavigation,
  initializeModalEscapeHandler,
} from "./lib/keyboard-navigation";
import { Logger } from "./lib/logger";
import { getOutputPoller } from "./lib/output-poller";
import { initializeScreenReaderAnnouncer } from "./lib/screen-reader-announcer";
// Note: Recovery handlers removed - app now auto-recovers missing sessions
import { AppState, setAppState, type Terminal, TerminalStatus } from "./lib/state";
import {
  closeTerminalWithConfirmation,
  createPlainTerminal,
  handleRestartTerminal,
} from "./lib/terminal-actions";
import {
  launchAgentsForTerminals as launchAgentsForTerminalsCore,
  reconnectTerminals as reconnectTerminalsCore,
  verifyTerminalSessions as verifyTerminalSessionsCore,
} from "./lib/terminal-lifecycle";
// NOTE: saveCurrentConfig is defined locally in this file
import { getTerminalManager } from "./lib/terminal-manager";
import { initTheme, toggleTheme } from "./lib/theme";
import { showToast } from "./lib/toast";
import {
  renderHeader,
  renderLoadingState,
  renderMiniTerminals,
  renderPrimaryTerminal,
} from "./lib/ui";
import {
  attachWorkspaceEventListeners,
  setupMainEventListeners,
  setupTooltips,
} from "./lib/ui-event-handlers";
import { handleWorkspacePathInput as handleWorkspacePathInputCore } from "./lib/workspace-lifecycle";
import {
  browseWorkspace,
  generateNextConfigId,
  validateWorkspacePath,
} from "./lib/workspace-utils";

// NOTE: handleWorkspacePathInput is a local wrapper that calls handleWorkspacePathInputCore from src/lib/workspace-lifecycle.ts

// Logger will be initialized in async IIFE after Tauri IPC is ready
let logger = undefined as ReturnType<typeof Logger.forComponent> | undefined;

// Initialize theme
initTheme();

// Initialize accessibility features
initializeScreenReaderAnnouncer();
initializeModalEscapeHandler();

// Initialize state (no agents until workspace is selected)
const state = new AppState();
setAppState(state); // Register singleton so terminal-manager can access it

// Initialize keyboard navigation
initializeKeyboardNavigation(state);

// Set up auto-save (saves state 2 seconds after last change)
state.setAutoSave(async () => {
  if (state.workspace.hasWorkspace()) {
    // Logger will be initialized in async IIFE
    if (logger) logger?.info("Auto-saving state");
    await saveCurrentConfiguration(state);
  }
});
// logger?.info("Auto-save enabled (2-second debounce)"); // Moved to async IIFE

// Save state immediately on window close (before app quits)
// Handle both browser beforeunload and Tauri window close events
window.addEventListener("beforeunload", async () => {
  if (state.workspace.hasWorkspace()) {
    logger?.info("Window closing (beforeunload) - saving state immediately");
    try {
      // Save state synchronously if possible
      await state.saveNow();
      logger?.info("State saved successfully on beforeunload");
    } catch (error) {
      logger?.error("Failed to save state on beforeunload", error as Error);
    }
  }
});

// Listen for Tauri window close event (more reliable in Tauri apps)
// NOTE: The close-requested listener is registered in the async IIFE below
// after logger is initialized, since it uses logger

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
  logger?.warn("Terminal encountered fatal errors, marking as error state", {
    terminalId,
    errorMessage,
  });

  // Update terminal state
  const terminal = state.terminals.getTerminal(terminalId);
  if (terminal) {
    state.terminals.updateTerminal(terminal.id, {
      status: TerminalStatus.Error,
      missingSession: true,
    });
  }
});

// Start health monitoring
healthMonitor.start();
// logger?.info("Health monitoring started"); // Moved to async IIFE

// Subscribe to health updates to trigger re-renders
healthMonitor.onHealthUpdate(() => {
  // Trigger a re-render when health status changes
  render();
});
// logger?.info("Subscribed to health monitor updates"); // Moved to async IIFE

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
// logger?.info("MCP command watcher started in backend (using notify crate)"); // Moved to async IIFE

// Track which terminals have had their health checked (Phase 3: Debouncing)
// This prevents redundant health checks during the 1-second render loop
const healthCheckedTerminals = new Set<string>();

// Render function
function render() {
  const hasWorkspace = state.workspace.hasWorkspace();
  const isResetting = state.isWorkspaceResetting();
  const isInitializing = state.isAppInitializing();
  logger?.info("Rendering", {
    hasWorkspace,
    displayedWorkspace: state.workspace.getDisplayedWorkspace(),
    isResetting,
    isInitializing,
  });

  // Get health data from health monitor
  const systemHealth = healthMonitor.getHealth();

  // Render header with daemon health and offline mode indicator
  renderHeader(
    state.workspace.getDisplayedWorkspace(),
    hasWorkspace,
    systemHealth.daemon.connected,
    systemHealth.daemon.lastPing,
    state.isOfflineMode()
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

  renderPrimaryTerminal(state.terminals.getPrimary(), hasWorkspace, state.workspace.getDisplayedWorkspace());

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
  renderMiniTerminals(state.terminals.getTerminals(), hasWorkspace, terminalHealthMap);

  // Re-attach workspace event listeners if they were just rendered
  if (!hasWorkspace) {
    attachWorkspaceEventListeners(
      handleWorkspacePathInput,
      browseWorkspaceWithCallback,
      () => state.workspace.getWorkspace() || ""
    );
  }

  // Set up tooltips for all elements with data-tooltip attributes
  setupTooltips();

  // Initialize xterm.js terminal for primary terminal
  const primary = state.terminals.getPrimary();
  if (primary && hasWorkspace) {
    initializeTerminalDisplay(primary.id);
  }
}

// Initialize xterm.js terminal display
async function initializeTerminalDisplay(terminalId: string) {
  const containerId = `xterm-container-${terminalId}`;

  // Skip placeholder IDs - they're already broken and will show error UI
  if (terminalId === "__unassigned__") {
    logger?.warn("Skipping placeholder terminal ID", { terminalId });
    return;
  }

  // Phase 3: Skip health check if already checked (debouncing)
  if (healthCheckedTerminals.has(terminalId)) {
    logger?.info("Terminal already health-checked, skipping redundant check", {
      terminalId,
      setSize: healthCheckedTerminals.size,
    });
    // Continue with xterm initialization without re-checking
  } else {
    // Check session health before initializing
    try {
      logger?.info("Performing new health check for terminal", {
        terminalId,
        setSizeBefore: healthCheckedTerminals.size,
      });
      const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });
      logger?.info("Session health check result", {
        terminalId,
        hasSession,
      });

      if (!hasSession) {
        logger?.warn("Terminal has no tmux session", { terminalId });

        // Mark terminal as having missing session (only if not already marked)
        const terminal = state.terminals.getTerminal(terminalId);
        logger?.info("Terminal state before update", {
          terminalId,
          missingSession: terminal?.missingSession,
        });
        if (terminal && !terminal.missingSession) {
          logger?.info("Setting missingSession=true for terminal", { terminalId });
          state.terminals.updateTerminal(terminal.id, {
            status: TerminalStatus.Error,
            missingSession: true,
          });
        }

        // Add to checked set even for failures to prevent repeated checks
        healthCheckedTerminals.add(terminalId);
        logger?.info("Added terminal to health-checked set (failed check)", {
          terminalId,
          setSize: healthCheckedTerminals.size,
        });
        return; // Don't create xterm instance - error UI will show instead
      }

      logger?.info("Session health check passed, proceeding with xterm initialization", {
        terminalId,
      });

      // Add to checked set after successful health check (Phase 3: Debouncing)
      healthCheckedTerminals.add(terminalId);
      logger?.info("Added terminal to health-checked set (passed check)", {
        terminalId,
        setSize: healthCheckedTerminals.size,
      });
    } catch (error) {
      logger?.error("Failed to check session health", error, { terminalId });
      // Add to checked set even on error to prevent retry spam
      healthCheckedTerminals.add(terminalId);
      logger?.info("Added terminal to health-checked set (error during check)", {
        terminalId,
        setSize: healthCheckedTerminals.size,
      });
      // Continue anyway - better to try than not
    }
  }

  // Check if terminal already exists
  const existingManaged = terminalManager.getTerminal(terminalId);
  if (existingManaged) {
    // Terminal exists - just show/hide as needed
    logger?.info("Terminal already exists, using show/hide", { terminalId });

    // Hide previous terminal (if different)
    const currentAttachedTerminalId = appLevelState.currentAttachedTerminalId;
    if (currentAttachedTerminalId && currentAttachedTerminalId !== terminalId) {
      logger?.info("Hiding previous terminal", {
        terminalId: currentAttachedTerminalId,
      });
      terminalManager.hideTerminal(currentAttachedTerminalId);
      outputPoller.pausePolling(currentAttachedTerminalId);
    }

    // Show current terminal
    logger?.info("Showing terminal", { terminalId });
    terminalManager.showTerminal(terminalId);

    // Resume polling for current terminal
    logger?.info("Resuming polling for terminal", { terminalId });
    outputPoller.resumePolling(terminalId);

    appLevelState.currentAttachedTerminalId = terminalId;
    return;
  }

  // Terminal doesn't exist yet - create it
  logger?.info("Creating new terminal", { terminalId });

  // Wait for DOM to be ready
  setTimeout(() => {
    // Hide all other terminals first
    const currentAttachedTerminalId = appLevelState.currentAttachedTerminalId;
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
      appLevelState.currentAttachedTerminalId = terminalId;

      logger?.info("Created and showing terminal", { terminalId });
    }
  }, 0);
}

// Check dependencies on startup
// App initialization moved to src/lib/app-initializer.ts

// Re-render on state changes
state.onChange(render);

// Register all event listeners (with deduplication guard)
if (!eventListenersRegistered) {
  logger?.info("Registering event listeners (first time only)");

  // NOTE: Initial render is now done in the async IIFE after logger is initialized
  // render(); // Commented out - causes "Cannot access uninitialized variable" error
  eventListenersRegistered = true;

  // Listen for CLI workspace argument from Rust backend
  listen("cli-workspace", (event) => {
    const workspacePath = event.payload as string;
    logger?.info("Loading workspace from CLI argument", { workspacePath });
    handleWorkspacePathInput(workspacePath);
  });

  // Listen for menu events
  listen("new-terminal", () => {
    if (state.workspace.hasWorkspace()) {
      createPlainTerminal({
        state,
        workspacePath: state.workspace.getWorkspaceOrThrow(),
        generateNextConfigId,
        saveCurrentConfig,
      });
    }
  });

  listen("close-terminal", async () => {
    const primary = state.terminals.getPrimary();
    if (primary) {
      await closeTerminalWithConfirmation(primary.id, {
        state,
        outputPoller,
        terminalManager,
        appLevelState,
        saveCurrentConfig,
      });
    }
  });

  listen("close-workspace", async () => {
    logger?.info("Closing workspace");

    // Clear stored workspace
    try {
      await invoke("clear_stored_workspace");
      logger?.info("Cleared stored workspace");
    } catch (error) {
      logger?.error("Failed to clear stored workspace", error);
    }

    // Clear localStorage workspace (for HMR survival)
    localStorage.removeItem("loom:workspace");
    logger?.info("Cleared localStorage workspace");

    // Stop all interval prompts
    const { getIntervalPromptManager } = await import("./lib/interval-prompt-manager");
    const intervalManager = getIntervalPromptManager();
    intervalManager.stopAll();

    // Stop all polling
    outputPoller.stopAll();

    // Destroy all xterm instances
    terminalManager.destroyAll();

    // Clear runtime state
    state.clearAll();
    setConfigWorkspace("");
    appLevelState.currentAttachedTerminalId = null;

    // Phase 3: Clear health check tracking when workspace closes
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    logger?.info("Cleared health-checked terminals set", {
      previousSize,
      currentSize: healthCheckedTerminals.size,
    });

    // Re-render to show workspace picker
    logger?.info("Rendering workspace picker");
    render();
  });

  /**
   * Helper function to handle workspace start logic
   * Extracted from duplicate code in start-workspace and force-start-workspace event listeners
   */
  async function handleWorkspaceStart(logPrefix: string): Promise<void> {
    if (!state.workspace.hasWorkspace()) return;
    const workspace = state.workspace.getWorkspaceOrThrow();

    // Clear health check tracking when starting workspace (terminals will be recreated)
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    logger?.info("Cleared health-checked terminals set", {
      previousSize,
      currentSize: healthCheckedTerminals.size,
      source: logPrefix,
    });

    // Use the workspace start module (reads existing config)
    const { startWorkspaceEngine } = await import("./lib/workspace-start");
    await startWorkspaceEngine(
      workspace,
      {
        state,
        outputPoller,
        terminalManager,
        setCurrentAttachedTerminalId: (id) => {
          appLevelState.currentAttachedTerminalId = id;
        },
        launchAgentsForTerminals: async (workspacePath: string, terminals: Terminal[]) =>
          launchAgentsForTerminalsCore(workspacePath, terminals, { state }),
        render,
        markTerminalsHealthChecked: (terminalIds) => {
          terminalIds.forEach((id) => healthCheckedTerminals.add(id));
          logger?.info("Marked terminals as health-checked", {
            terminalCount: terminalIds.length,
            setSize: healthCheckedTerminals.size,
          });
        },
      },
      logPrefix
    );
  }

  // Start engine - create sessions for existing config (with confirmation)
  listen("start-workspace", async () => {
    if (!state.workspace.hasWorkspace()) return;

    const confirmed = await ask(
      "This will:\n" +
        "• Close all current terminal sessions\n" +
        "• Create new sessions for configured terminals\n" +
        "• Launch agents as configured\n\n" +
        "Your configuration will NOT be changed.\n\n" +
        "Continue?",
      {
        title: "Start Loom Engine",
        kind: "info",
      }
    );

    if (!confirmed) return;

    await handleWorkspaceStart("start-workspace");
  });

  // Force Start engine - NO confirmation dialog (for MCP automation)
  listen("force-start-workspace", async () => {
    if (!state.workspace.hasWorkspace()) return;

    logger?.info("Starting engine (no confirmation)");

    await handleWorkspaceStart("force-start-workspace");
  });

  // Helper function to handle factory reset logic (shared between confirmed and force variants)
  async function handleFactoryReset(logPrefix: string): Promise<void> {
    if (!state.workspace.hasWorkspace()) return;
    const workspace = state.workspace.getWorkspaceOrThrow();

    logger?.info("Starting factory reset", { source: logPrefix });

    // Phase 3: Clear health check tracking when resetting workspace (terminals will be recreated)
    const previousSize = healthCheckedTerminals.size;
    healthCheckedTerminals.clear();
    logger?.info("Cleared health-checked terminals set", {
      previousSize,
      currentSize: healthCheckedTerminals.size,
      source: logPrefix,
    });

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
            appLevelState.currentAttachedTerminalId = id;
          },
          launchAgentsForTerminals: async (workspacePath: string, terminals: Terminal[]) =>
            launchAgentsForTerminalsCore(workspacePath, terminals, { state }),
          render,
        },
        logPrefix
      );
    } finally {
      // Clear loading state when done (even if error)
      state.setResettingWorkspace(false);
    }
  }

  // Factory Reset - overwrite config with defaults (with confirmation)
  listen("factory-reset-workspace", async () => {
    if (!state.workspace.hasWorkspace()) return;

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
        kind: "warning",
      }
    );

    if (!confirmed) return;

    await handleFactoryReset("factory-reset-workspace");
  });

  // Force Factory Reset - NO confirmation dialog (for MCP automation)
  listen("force-factory-reset-workspace", async () => {
    if (!state.workspace.hasWorkspace()) return;

    logger?.info("Resetting workspace (no confirmation)");

    await handleFactoryReset("force-factory-reset-workspace");
  });

  // Restart Terminal - triggered by MCP command
  listen("restart-terminal", async (event) => {
    const terminalId = event.payload as string;
    if (!terminalId) {
      logger?.error(
        "Restart terminal event received without terminal ID",
        new Error("Missing terminal ID")
      );
      return;
    }

    logger?.info("Restarting terminal via MCP command", { terminalId });
    await handleRestartTerminal(terminalId, { state, saveCurrentConfig });
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

  logger?.info("Event listeners registered successfully");
} else {
  logger?.info("Event listeners already registered, skipping duplicate registration");
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

    const hasWorkspace = state.workspace.hasWorkspace();

    // Show different dialog based on whether daemon is running and workspace is loaded
    if (status.running && hasWorkspace) {
      const shouldReconnect = await ask(
        `Daemon Status\n\n${statusText}\n\nSocket: ${status.socket_path}\n\n` +
          `Would you like to reconnect terminals to the daemon?\n\n` +
          `This is useful if you hot-reloaded the frontend and lost connection to terminals.`,
        {
          title: "Daemon Status",
          kind: "info",
        }
      );

      if (shouldReconnect) {
        logger?.info("User requested terminal reconnection");
        await reconnectTerminalsCore({
          state,
          initializeTerminalDisplay,
          saveCurrentConfig,
        });
        showToast("Terminal reconnection complete! Check the console for details.", "success");
      }
    } else {
      // Just show status without reconnect option
      showToast(`Daemon Status: ${statusText}. Socket: ${status.socket_path}`, "info", 5000);
    }
  } catch (error) {
    showToast(`Failed to get daemon status: ${error}`, "error");
  }
}

// Drag and drop state moved to src/lib/drag-drop-manager.ts

// Save current state to config and state files
async function saveCurrentConfig() {
  if (!state.workspace.hasWorkspace()) {
    return;
  }

  await saveCurrentConfiguration(state);
}

// Workspace utilities (validation, browsing, ID generation) are now in src/lib/workspace-utils.ts
// Terminal creation (createPlainTerminal) moved to src/lib/terminal-actions.ts

// Handle manual workspace path entry (wrapper for workspace-lifecycle module)
async function handleWorkspacePathInput(path: string) {
  await handleWorkspacePathInputCore(path, {
    state,
    validateWorkspacePath,
    launchAgentsForTerminals: async (workspacePath: string, terminals: Terminal[]) =>
      launchAgentsForTerminalsCore(workspacePath, terminals, { state }),
    reconnectTerminals: async () =>
      reconnectTerminalsCore({ state, initializeTerminalDisplay, saveCurrentConfig }),
    verifyTerminalSessions: async () => verifyTerminalSessionsCore({ state }),
  });
}

// Wrapper for browseWorkspace to bind the handleWorkspacePathInput callback
const browseWorkspaceWithCallback = () => browseWorkspace(handleWorkspacePathInput);

// Terminal action handlers are now in src/lib/terminal-actions.ts
// Recovery handlers are now in src/lib/recovery-handlers.ts

// Wrap initialization in async function that waits for Tauri
(async () => {
  try {
    await waitForTauri();
    console.log("[Loom] Tauri IPC ready, initializing app...");

    // Now that Tauri IPC is ready, initialize console logging
    initConsoleLogger();

    // Test console logging immediately
    console.log("[Loom] Console logger initialized - this should appear in ~/.loom/console.log");

    // Create logger for main component
    logger = Logger.forComponent("main");

    // Log initialization complete
    logger?.info("Auto-save enabled (2-second debounce)");
    logger?.info("Health monitoring started");
    logger?.info("Subscribed to health monitor updates");
    logger?.info("Timer update interval started");
    logger?.info("MCP command watcher started in backend (using notify crate)");

    // Register close-requested listener (needs logger to be initialized)
    listen("tauri://close-requested", async () => {
      logger?.info("Window close requested (Tauri event) - saving state");
      if (state.workspace.hasWorkspace()) {
        try {
          await state.saveNow();
          logger?.info("State saved successfully on close-requested");
        } catch (error) {
          logger?.error("Failed to save state on close-requested", error as Error);
        }
      }
    }).catch((error) => {
      logger?.error("Failed to register close-requested listener", error as Error);
    });

    // Initialize app
    initializeApp({
      state,
      validateWorkspacePath,
      handleWorkspacePathInput,
      render,
    });

    // Set up all event listeners (consolidated in ui-event-handlers.ts)
    setupMainEventListeners({
      state,
      render,
      saveCurrentConfig,
      terminalManager,
      outputPoller,
      healthMonitor,
      appLevelState,
      createPlainTerminal,
      generateNextConfigId,
      setupDragAndDrop,
    });
  } catch (error) {
    console.error("[Loom] Failed to initialize:", error);
    // Show error to user
    document.body.innerHTML = `
      <div style="display: flex; align-items: center; justify-content: center; height: 100vh; font-family: system-ui;">
        <div style="text-align: center;">
          <h1 style="color: #ef4444;">Failed to Initialize</h1>
          <p style="color: #6b7280;">${error}</p>
          <p style="color: #6b7280; font-size: 0.875rem;">Check the console for more details</p>
        </div>
      </div>
    `;
  }
})();
