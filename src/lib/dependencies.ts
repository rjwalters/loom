/**
 * Common Dependency Interfaces
 *
 * This module provides base interfaces for dependency injection across the codebase.
 * Individual modules extend these interfaces to declare their specific dependencies,
 * reducing duplication and creating a clear hierarchy of dependency patterns.
 *
 * Design Principles:
 * - CoreDependencies: All modules need access to app state
 * - Renderable: Modules that trigger UI updates
 * - TerminalInfrastructure: Modules managing terminal sessions and output
 * - Configurable: Modules that persist configuration changes
 * - AgentLauncher: Modules that launch or reconnect agent processes
 */

import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";

/**
 * Core dependencies needed by all modules.
 *
 * Every operation in the application needs access to the app state.
 */
export interface CoreDependencies {
  state: AppState;
}

/**
 * Dependencies for modules that trigger UI updates.
 *
 * Used by modules that need to notify the UI layer to re-render
 * after state changes.
 */
export interface RenderableDependencies {
  render: () => void;
}

/**
 * Dependencies for modules that manage terminal infrastructure.
 *
 * Used by modules that interact with terminal sessions and output polling.
 */
export interface TerminalInfrastructureDependencies {
  outputPoller: OutputPoller;
  terminalManager: TerminalManager;
}

/**
 * Dependencies for modules that persist configuration.
 *
 * Used by modules that need to save the current configuration to disk.
 */
export interface ConfigurableDependencies {
  saveCurrentConfig: () => Promise<void>;
}

/**
 * Dependencies for modules that launch and manage agents.
 *
 * Used by workspace lifecycle modules that start, stop, or reconnect
 * agent processes for terminals.
 */
export interface AgentLauncherDependencies {
  launchAgentsForTerminals: (workspacePath: string, terminals: Terminal[]) => Promise<void>;
}

/**
 * Dependencies for modules that manage terminal attachment state.
 *
 * Used by modules that track which terminal is currently attached/focused.
 */
export interface TerminalAttachmentDependencies {
  setCurrentAttachedTerminalId: (id: string | null) => void;
}
