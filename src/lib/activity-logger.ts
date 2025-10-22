/**
 * Activity Logger
 *
 * Logs agent activity for Manual Orchestration Mode (MOM) to enable:
 * - Activity tracking across slash command invocations
 * - Smart role selection heuristics for /loom command
 * - Future Loom Intelligence features
 *
 * Data is stored in .loom/activity.db (SQLite) within the workspace.
 */

import { invoke } from "@tauri-apps/api/tauri";

/**
 * Activity log entry
 *
 * Captures key information about agent work for analytics and heuristics.
 */
export interface ActivityEntry {
  /** ISO 8601 timestamp of activity */
  timestamp: string;

  /** Role name (e.g., "builder", "curator", "judge") */
  role: string;

  /** How the agent was triggered: "slash-command", "heuristic", "manual" */
  trigger: string;

  /** Whether work was found (e.g., loom:issue exists for builder) */
  work_found: boolean;

  /** Whether work was completed successfully (optional, may still be in progress) */
  work_completed?: boolean;

  /** GitHub issue number if applicable */
  issue_number?: number;

  /** Duration of work in milliseconds */
  duration_ms?: number;

  /** Outcome: "completed", "no-work", "blocked", "error" */
  outcome: string;

  /** Additional notes or error messages */
  notes?: string;
}

/**
 * Log an activity entry to SQLite database
 *
 * Non-blocking - errors are logged to console but not thrown.
 *
 * @param workspacePath - Absolute path to workspace
 * @param entry - Activity entry to log
 */
export async function logActivity(workspacePath: string, entry: ActivityEntry): Promise<void> {
  try {
    await invoke("log_activity", {
      workspacePath,
      entry,
    });
  } catch (_error) {
    // Non-blocking - don't throw
  }
}

/**
 * Read recent activity entries
 *
 * @param workspacePath - Absolute path to workspace
 * @param limit - Maximum number of entries to return (default: 100)
 * @returns Array of activity entries, newest first
 */
export async function readRecentActivity(
  workspacePath: string,
  limit = 100
): Promise<ActivityEntry[]> {
  try {
    return await invoke("read_recent_activity", {
      workspacePath,
      limit,
    });
  } catch (_error) {
    return [];
  }
}

/**
 * Get activity entries filtered by role
 *
 * @param workspacePath - Absolute path to workspace
 * @param role - Role name to filter by
 * @param limit - Maximum number of entries to return (default: 100)
 * @returns Array of activity entries for specified role, newest first
 */
export async function getActivityByRole(
  workspacePath: string,
  role: string,
  limit = 100
): Promise<ActivityEntry[]> {
  try {
    return await invoke("get_activity_by_role", {
      workspacePath,
      role,
      limit,
    });
  } catch (_error) {
    return [];
  }
}
