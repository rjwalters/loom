import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";
import type { Terminal } from "./state";

const logger = Logger.forComponent("offline-scheduler");

/**
 * offline-scheduler.ts - Manages periodic status echoes for offline mode
 *
 * When offline mode is enabled, terminals don't run AI agents.
 * Instead, this scheduler sends periodic status echoes to validate
 * terminals are responding and alive.
 */

/**
 * Interval ID for the offline scheduler
 * Stored globally so it can be stopped when workspace is closed
 */
let offlineSchedulerInterval: number | null = null;

/**
 * Default interval for offline status echoes (30 seconds)
 */
const DEFAULT_OFFLINE_INTERVAL = 30000;

/**
 * Start the offline scheduler
 *
 * Sends periodic status echoes to all terminals to validate they're responsive.
 * Each echo shows timestamp and confirms terminal is alive without running AI agents.
 *
 * @param terminals - Array of terminal configurations
 * @param workspacePath - The workspace directory path
 * @param intervalMs - Interval in milliseconds (default: 30 seconds)
 */
export function startOfflineScheduler(
  terminals: Terminal[],
  workspacePath: string,
  intervalMs: number = DEFAULT_OFFLINE_INTERVAL
): void {
  // Stop any existing scheduler
  stopOfflineScheduler();

  logger.info("Starting offline scheduler", {
    workspacePath,
    terminalCount: terminals.length,
    intervalMs,
  });

  // Send initial status echo immediately
  sendOfflineStatusEchoes(terminals, workspacePath);

  // Schedule periodic status echoes
  offlineSchedulerInterval = window.setInterval(() => {
    sendOfflineStatusEchoes(terminals, workspacePath);
  }, intervalMs);

  logger.info("Offline scheduler started", {
    workspacePath,
    intervalMs,
  });
}

/**
 * Stop the offline scheduler
 *
 * Clears the interval timer and stops sending status echoes.
 */
export function stopOfflineScheduler(): void {
  if (offlineSchedulerInterval !== null) {
    logger.info("Stopping offline scheduler");
    clearInterval(offlineSchedulerInterval);
    offlineSchedulerInterval = null;
    logger.info("Offline scheduler stopped");
  }
}

/**
 * Send status echoes to all terminals
 *
 * Sends a simple echo command with timestamp to each terminal.
 * This validates terminals are responsive without requiring AI agents.
 *
 * @param terminals - Array of terminal configurations
 * @param workspacePath - The workspace directory path
 */
async function sendOfflineStatusEchoes(
  terminals: Terminal[],
  workspacePath: string
): Promise<void> {
  logger.info("Sending offline status echoes", {
    workspacePath,
    terminalCount: terminals.length,
  });

  const timestamp = new Date().toISOString();

  for (const terminal of terminals) {
    try {
      // Send echo command with timestamp to validate terminal is responsive
      // Using ANSI color codes for visibility: green for "Status echo", cyan for timestamp
      const echoCommand = `echo "\\033[32m✓ Status echo - still alive\\033[0m - \\033[36m${timestamp}\\033[0m"`;

      await invoke("send_terminal_input", {
        id: terminal.id,
        data: echoCommand,
      });

      // Press Enter to execute
      await invoke("send_terminal_input", {
        id: terminal.id,
        data: "\r",
      });

      logger.info("Sent status echo to terminal", {
        workspacePath,
        terminalId: terminal.id,
        terminalName: terminal.name,
        timestamp,
      });
    } catch (error) {
      logger.error("Failed to send status echo", error as Error, {
        workspacePath,
        terminalId: terminal.id,
        terminalName: terminal.name,
      });
    }
  }

  logger.info("Offline status echoes sent", {
    workspacePath,
    terminalCount: terminals.length,
    timestamp,
  });
}
