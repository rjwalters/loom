import { ask } from "@tauri-apps/plugin-dialog";

import { appLevelState } from "./app-state";
import { Logger } from "./logger";
import { getOutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import { getTerminalManager } from "./terminal-manager";

const logger = Logger.forComponent("engine-handlers");

/** Dependencies for engine handlers */
export interface EngineHandlerDeps {
  state: AppState;
  healthCheckedTerminals: Set<string>;
  render: () => void;
}

/**
 * Handle workspace start logic.
 * Reads existing config and starts the engine.
 */
export async function handleWorkspaceStart(
  deps: EngineHandlerDeps,
  logPrefix: string
): Promise<void> {
  const { state, healthCheckedTerminals, render } = deps;

  if (!state.workspace.hasWorkspace()) return;
  const workspace = state.workspace.getWorkspaceOrThrow();

  // Clear health check tracking when starting workspace (terminals will be recreated)
  const previousSize = healthCheckedTerminals.size;
  healthCheckedTerminals.clear();
  logger.info("Cleared health-checked terminals set", {
    previousSize,
    currentSize: healthCheckedTerminals.size,
    source: logPrefix,
  });

  // Use the workspace start module (reads existing config)
  const { startWorkspaceEngine } = await import("./workspace-start");
  const { launchAgentsForTerminals: launchAgentsForTerminalsCore } = await import(
    "./terminal-lifecycle"
  );
  const outputPoller = getOutputPoller();
  const terminalManager = getTerminalManager();

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
        logger.info("Marked terminals as health-checked", {
          terminalCount: terminalIds.length,
          setSize: healthCheckedTerminals.size,
        });
      },
    },
    logPrefix
  );
}

/**
 * Handle factory reset logic.
 * Overwrites config with defaults and restarts the engine.
 */
export async function handleFactoryReset(
  deps: EngineHandlerDeps,
  logPrefix: string
): Promise<void> {
  const { state, healthCheckedTerminals, render } = deps;

  if (!state.workspace.hasWorkspace()) return;
  const workspace = state.workspace.getWorkspaceOrThrow();

  logger.info("Starting factory reset", { source: logPrefix });

  // Phase 3: Clear health check tracking when resetting workspace (terminals will be recreated)
  const previousSize = healthCheckedTerminals.size;
  healthCheckedTerminals.clear();
  logger.info("Cleared health-checked terminals set", {
    previousSize,
    currentSize: healthCheckedTerminals.size,
    source: logPrefix,
  });

  // Set loading state before reset
  state.setResettingWorkspace(true);

  try {
    // Use the workspace reset module (overwrites config with defaults)
    const { resetWorkspaceToDefaults } = await import("./workspace-reset");
    const { launchAgentsForTerminals: launchAgentsForTerminalsCore } = await import(
      "./terminal-lifecycle"
    );
    const outputPoller = getOutputPoller();
    const terminalManager = getTerminalManager();

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

/**
 * Show start engine confirmation dialog and start if confirmed.
 */
export async function confirmAndStartEngine(deps: EngineHandlerDeps): Promise<void> {
  if (!deps.state.workspace.hasWorkspace()) return;

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

  await handleWorkspaceStart(deps, "start-workspace");
}

/**
 * Show factory reset confirmation dialog and reset if confirmed.
 */
export async function confirmAndFactoryReset(deps: EngineHandlerDeps): Promise<void> {
  if (!deps.state.workspace.hasWorkspace()) return;

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

  await handleFactoryReset(deps, "factory-reset-workspace");
}
