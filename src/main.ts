/**
 * Loom - Single-Session + Analytics Entry Point
 *
 * Simplified entry point for the analytics-first Claude Code wrapper.
 * Flow: check deps → load workspace → create session → render split view.
 *
 * Phase 6 rewrite - Issue #1901
 */
import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { initializeApp } from "./lib/app-initializer";
import { setConfigWorkspace } from "./lib/config";
import { initConsoleLogger } from "./lib/console-logger";
import { getHistoryCache, initializeHistoryCache } from "./lib/history-cache";
import { getInputLogger } from "./lib/input-logger";
import {
  initializeKeyboardNavigation,
  initializeModalEscapeHandler,
} from "./lib/keyboard-navigation";
import { Logger } from "./lib/logger";
import { initializeResizeHandle } from "./lib/resize-handle";
import { initializeScreenReaderAnnouncer } from "./lib/screen-reader-announcer";
import { getSessionManager } from "./lib/session-manager";
import { AppState, setAppState } from "./lib/state";
import { waitForTauri } from "./lib/tauri-bootstrap";
import { initializeErrorTracking, trackEvent } from "./lib/telemetry";
import { initializeTerminalDisplay } from "./lib/terminal-display";
import { getTerminalManager } from "./lib/terminal-manager";
import { initTheme, toggleTheme } from "./lib/theme";
import { showToast } from "./lib/toast";
import {
  renderDashboardView,
  renderHeader,
  renderLoadingState,
  renderPrimaryTerminal,
  renderStatusBar,
  stopAutoRefresh,
  stopStatusRefresh,
} from "./lib/ui";
import { attachWorkspaceEventListeners, setupTooltips } from "./lib/ui-event-handlers";
import { handleWorkspacePathInput as handleWorkspacePathInputCore } from "./lib/workspace-lifecycle";
import { browseWorkspace, validateWorkspacePath } from "./lib/workspace-utils";

// Logger will be initialized in async IIFE after Tauri IPC is ready
let logger = undefined as ReturnType<typeof Logger.forComponent> | undefined;

// Initialize theme and accessibility features
initTheme();
initializeScreenReaderAnnouncer();
initializeModalEscapeHandler();

// Initialize state (single-session model)
const state = new AppState();
setAppState(state);

// Initialize keyboard navigation
initializeKeyboardNavigation(state);

// Get singleton instances
const terminalManager = getTerminalManager();
const sessionManager = getSessionManager();
const inputLogger = getInputLogger();

// Track if event listeners have been registered
let eventListenersRegistered = false;

// =================================================================
// Render Function - Split View Layout
// =================================================================
function render() {
  const hasWorkspace = state.workspace.hasWorkspace();
  const isInitializing = state.isAppInitializing();
  const workspacePath = state.workspace.getWorkspace();

  logger?.info("Rendering", {
    hasWorkspace,
    displayedWorkspace: state.workspace.getDisplayedWorkspace(),
    isInitializing,
  });

  // Render header
  renderHeader(state.workspace.getDisplayedWorkspace(), hasWorkspace);

  // Show loading state if initializing
  if (isInitializing) {
    renderLoadingState("Initializing...");
    return;
  }

  // Render terminal view
  renderPrimaryTerminal(
    state.terminals.getPrimary(),
    hasWorkspace,
    state.workspace.getDisplayedWorkspace()
  );

  // Re-attach workspace event listeners if showing workspace selector
  if (!hasWorkspace) {
    attachWorkspaceEventListeners(
      handleWorkspacePathInput,
      () => browseWorkspace(handleWorkspacePathInput),
      () => state.workspace.getWorkspace() || ""
    );
    // Render empty dashboard state
    renderDashboardView(null);
    renderStatusBar(null);
  } else {
    // Render analytics dashboard and status bar with session state
    const sessionStatus = sessionManager.getSessionStatus();
    renderDashboardView(workspacePath);
    renderStatusBar(workspacePath, sessionStatus.state);

    // Initialize terminal display for primary terminal
    const primary = state.terminals.getPrimary();
    if (primary) {
      initializeTerminalDisplay(primary.id, state);
    }
  }

  setupTooltips();
}

// Re-render on state changes
state.onChange(render);

// =================================================================
// Event Listeners (Simplified Set)
// =================================================================
if (!eventListenersRegistered) {
  eventListenersRegistered = true;

  // CLI workspace argument
  listen("cli-workspace", (event) => {
    const workspacePath = event.payload as string;
    logger?.info("Loading workspace from CLI argument", { workspacePath });
    handleWorkspacePathInput(workspacePath);
  });

  // Close workspace - destroy session, stop logger, clear state
  listen("close-workspace", async () => {
    logger?.info("Closing workspace");

    try {
      await invoke("clear_stored_workspace");
      logger?.info("Cleared stored workspace");
    } catch (error) {
      logger?.error("Failed to clear stored workspace", error);
    }

    localStorage.removeItem("loom:workspace");

    // Stop session and input logger
    await sessionManager.destroySession();
    await inputLogger.stop();
    stopAutoRefresh();
    stopStatusRefresh();

    // Destroy all xterm instances
    terminalManager.destroyAll();

    // Clear runtime state
    state.clearAll();
    setConfigWorkspace("");

    logger?.info("Workspace closed, rendering workspace picker");
    render();
  });

  // Theme and zoom controls
  listen("toggle-theme", () => toggleTheme());
  listen("zoom-in", () => terminalManager.adjustAllFontSizes(2));
  listen("zoom-out", () => terminalManager.adjustAllFontSizes(-2));
  listen("reset-zoom", () => terminalManager.resetAllFontSizes());

  // Modal show events
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

  listen("show-health-dashboard", async () => {
    const { showHealthDashboardModal } = await import("./lib/health-dashboard-modal");
    showHealthDashboardModal();
  });

  listen("show-prompt-library", async () => {
    const { showPromptLibraryModal } = await import("./lib/prompt-library-modal");
    showPromptLibraryModal();
  });

  listen("show-daemon-status", async () => {
    try {
      interface DaemonStatus {
        running: boolean;
        socket_path: string;
        error: string | null;
      }
      const status = await invoke<DaemonStatus>("get_daemon_status");
      const statusText = status.running
        ? "Running"
        : `Not Running${status.error ? ` - ${status.error}` : ""}`;
      showToast(`Daemon: ${statusText}. Socket: ${status.socket_path}`, "info", 5000);
    } catch (error) {
      showToast(`Failed to get daemon status: ${error}`, "error");
    }
  });

  listen("show-intelligence-dashboard", async () => {
    const { showIntelligenceDashboard } = await import("./lib/intelligence-dashboard");
    showIntelligenceDashboard();
  });

  listen("show-budget-management", async () => {
    const { showBudgetManagementModal } = await import("./lib/budget-management");
    showBudgetManagementModal();
  });

  listen("show-comparative-analysis", async () => {
    const { showComparativeAnalysisModal } = await import("./lib/comparative-analysis-modal");
    showComparativeAnalysisModal();
  });

  listen("show-activity-playback", async () => {
    const { showActivityPlaybackModal } = await import("./lib/activity-playback-modal");
    showActivityPlaybackModal();
  });

  listen("show-activity-playback-issue", async (event) => {
    const issueNumber = event.payload as number;
    const { showActivityPlaybackForIssue } = await import("./lib/activity-playback-modal");
    showActivityPlaybackForIssue(issueNumber);
  });

  listen("show-activity-playback-pr", async (event) => {
    const prNumber = event.payload as number;
    const { showActivityPlaybackForPR } = await import("./lib/activity-playback-modal");
    showActivityPlaybackForPR(prNumber);
  });

  logger?.info("Event listeners registered");
}

// =================================================================
// Helper Functions
// =================================================================
async function handleWorkspacePathInput(path: string) {
  await handleWorkspacePathInputCore(path, {
    state,
    validateWorkspacePath,
    launchAgentsForTerminals: async () => {
      // Session launch is handled by initializeSession
    },
    reconnectTerminals: async () => {
      // Single session model - no reconnect needed
    },
    verifyTerminalSessions: async () => {
      // Session verification handled by SessionManager
    },
  });

  // After workspace is loaded, initialize session
  if (state.workspace.hasWorkspace()) {
    const workspace = state.workspace.getWorkspaceOrThrow();
    await initializeSession(workspace);
  }
}

async function initializeSession(workspace: string) {
  try {
    await sessionManager.launchSession(workspace);
    inputLogger.start(workspace);
    logger?.info("Session initialized", { workspace });
  } catch (error) {
    logger?.error("Failed to initialize session", error as Error);
    showToast("Failed to start session. Check logs for details.", "error");
  }
}

// =================================================================
// Async Initialization
// =================================================================
(async () => {
  try {
    await waitForTauri();
    console.log("[Loom] Tauri IPC ready, initializing app...");

    initConsoleLogger();
    initializeErrorTracking();
    await initializeHistoryCache();

    logger = Logger.forComponent("main");
    trackEvent("app_started", "workflow");

    // Register close-requested listener
    listen("tauri://close-requested", async () => {
      logger?.info("Window close requested - saving state");
      if (state.workspace.hasWorkspace()) {
        try {
          await state.saveNow();
          logger?.info("State saved on close");
        } catch (error) {
          logger?.error("Failed to save state on close", error as Error);
        }
      }

      // Flush history cache
      try {
        const historyCache = getHistoryCache();
        await historyCache.flushAll();
      } catch (error) {
        logger?.error("Failed to flush history cache", error as Error);
      }
    }).catch((error) => {
      logger?.error("Failed to register close-requested listener", error as Error);
    });

    // Handle beforeunload for browser-style close
    window.addEventListener("beforeunload", async () => {
      if (state.workspace.hasWorkspace()) {
        try {
          await state.saveNow();
        } catch {
          // Ignore errors on beforeunload
        }
      }
      try {
        const historyCache = getHistoryCache();
        await historyCache.flushAll();
      } catch {
        // Ignore errors on beforeunload
      }
    });

    // Initialize app (loads workspace from storage)
    initializeApp({
      state,
      validateWorkspacePath,
      handleWorkspacePathInput,
      render,
    });

    initializeResizeHandle();

    // If workspace already loaded, initialize session
    if (state.workspace.hasWorkspace()) {
      const workspace = state.workspace.getWorkspaceOrThrow();
      await initializeSession(workspace);
    }

    render();
    logger?.info("App initialized");
  } catch (error) {
    console.error("[Loom] Failed to initialize:", error);
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
