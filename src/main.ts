import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";

import { initializeApp } from "./lib/app-initializer";
import { appLevelState } from "./lib/app-state";
import { saveCurrentConfiguration, setConfigWorkspace } from "./lib/config";
import { initConsoleLogger } from "./lib/console-logger";
import { setupDragAndDrop } from "./lib/drag-drop-manager";
import {
  confirmAndFactoryReset,
  confirmAndStartEngine,
  handleFactoryReset,
  handleWorkspaceStart,
} from "./lib/engine-handlers";
import { getHealthMonitor } from "./lib/health-monitor";
import { getHistoryCache, initializeHistoryCache } from "./lib/history-cache";
import {
  initializeKeyboardNavigation,
  initializeModalEscapeHandler,
} from "./lib/keyboard-navigation";
import { Logger } from "./lib/logger";
import { getOutputPoller } from "./lib/output-poller";
import { initializeScreenReaderAnnouncer } from "./lib/screen-reader-announcer";
// Note: Recovery handlers removed - app now auto-recovers missing sessions
import { AppState, setAppState, type Terminal, TerminalStatus } from "./lib/state";
import { waitForTauri } from "./lib/tauri-bootstrap";
import { initializeErrorTracking, trackEvent } from "./lib/telemetry";
import {
  closeTerminalWithConfirmation,
  createPlainTerminal,
  handleRestartTerminal,
} from "./lib/terminal-actions";
import {
  clearHealthCheckedTerminals,
  healthCheckedTerminals,
  initializeTerminalDisplay,
} from "./lib/terminal-display";
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

  // Flush history cache to disk before closing
  try {
    const historyCache = getHistoryCache();
    await historyCache.flushAll();
    logger?.info("History cache flushed on beforeunload");
  } catch (error) {
    logger?.error("Failed to flush history cache on beforeunload", error as Error);
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

// Phase 3: Debouncing - healthCheckedTerminals moved to ./lib/terminal-display.ts

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

  renderPrimaryTerminal(
    state.terminals.getPrimary(),
    hasWorkspace,
    state.workspace.getDisplayedWorkspace()
  );

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
    initializeTerminalDisplay(primary.id, state);
  }
}

// initializeTerminalDisplay moved to ./lib/terminal-display.ts

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
    clearHealthCheckedTerminals();

    // Re-render to show workspace picker
    logger?.info("Rendering workspace picker");
    render();
  });

  // handleWorkspaceStart moved to ./lib/engine-handlers.ts

  // Start engine - create sessions for existing config (with confirmation)
  listen("start-workspace", async () => {
    await confirmAndStartEngine({ state, healthCheckedTerminals, render });
  });

  // Force Start engine - NO confirmation dialog (for MCP automation)
  listen("force-start-workspace", async () => {
    if (!state.workspace.hasWorkspace()) return;
    logger?.info("Starting engine (no confirmation)");
    await handleWorkspaceStart({ state, healthCheckedTerminals, render }, "force-start-workspace");
  });

  // handleFactoryReset moved to ./lib/engine-handlers.ts

  // Factory Reset - overwrite config with defaults (with confirmation)
  listen("factory-reset-workspace", async () => {
    await confirmAndFactoryReset({ state, healthCheckedTerminals, render });
  });

  // Force Factory Reset - NO confirmation dialog (for MCP automation)
  listen("force-factory-reset-workspace", async () => {
    if (!state.workspace.hasWorkspace()) return;
    logger?.info("Resetting workspace (no confirmation)");
    await handleFactoryReset(
      { state, healthCheckedTerminals, render },
      "force-factory-reset-workspace"
    );
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

  // Stop Engine - triggered by MCP command
  listen("stop-engine", async () => {
    logger?.info("Stopping engine via MCP command");

    const { cleanupWorkspace } = await import("./lib/workspace-cleanup");
    await cleanupWorkspace({
      component: "mcp-stop-engine",
      state,
      outputPoller,
      terminalManager,
      setCurrentAttachedTerminalId: (id) => {
        appLevelState.currentAttachedTerminalId = id;
      },
    });

    // Save the empty state
    await saveCurrentConfig();

    logger?.info("Engine stopped via MCP command");
    showToast("Engine stopped. All terminals have been closed.", "info");
    render();
  });

  // Run Now - triggered by MCP command
  listen("run-now-terminal", async (event) => {
    const terminalId = event.payload as string;
    if (!terminalId) {
      logger?.error("Run now event received without terminal ID", new Error("Missing terminal ID"));
      return;
    }

    logger?.info("Running interval prompt via MCP command", { terminalId });
    const { handleRunNowClick } = await import("./lib/terminal-actions");
    await handleRunNowClick(terminalId, { state });
  });

  // Start Autonomous Mode - triggered by MCP command
  listen("start-autonomous-mode", async () => {
    logger?.info("Starting autonomous mode via MCP command");
    const { getIntervalPromptManager } = await import("./lib/interval-prompt-manager");
    const intervalManager = getIntervalPromptManager();
    intervalManager.startAll(state);
    logger?.info("Autonomous mode started for all configured terminals");
    showToast("Autonomous mode started for all terminals.", "success");
  });

  // Stop Autonomous Mode - triggered by MCP command
  listen("stop-autonomous-mode", async () => {
    logger?.info("Stopping autonomous mode via MCP command");
    const { getIntervalPromptManager } = await import("./lib/interval-prompt-manager");
    const intervalManager = getIntervalPromptManager();
    intervalManager.stopAll();
    logger?.info("Autonomous mode stopped for all terminals");
    showToast("Autonomous mode stopped for all terminals.", "info");
  });

  // Launch Interval - triggered by MCP command (same as run-now but with different event name)
  listen("launch-interval-terminal", async (event) => {
    const terminalId = event.payload as string;
    if (!terminalId) {
      logger?.error(
        "Launch interval event received without terminal ID",
        new Error("Missing terminal ID")
      );
      return;
    }

    logger?.info("Launching interval prompt via MCP command", { terminalId });
    const { handleRunNowClick } = await import("./lib/terminal-actions");
    await handleRunNowClick(terminalId, { state });
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

  listen("show-metrics", async () => {
    const { showMetricsModal } = await import("./lib/metrics-modal");
    showMetricsModal();
  });

  listen("show-agent-metrics", async () => {
    const { showAgentMetricsModal } = await import("./lib/agent-metrics-modal");
    showAgentMetricsModal();
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
          initializeTerminalDisplay: (terminalId) => initializeTerminalDisplay(terminalId, state),
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
      reconnectTerminalsCore({
        state,
        initializeTerminalDisplay: (terminalId) => initializeTerminalDisplay(terminalId, state),
        saveCurrentConfig,
      }),
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

    // Initialize error tracking for uncaught errors
    initializeErrorTracking();

    // Initialize history cache for terminal output persistence
    await initializeHistoryCache();

    // Test console logging immediately
    console.log("[Loom] Console logger initialized - this should appear in ~/.loom/console.log");

    // Create logger for main component
    logger = Logger.forComponent("main");

    // Track app startup
    trackEvent("app_started", "workflow");

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
