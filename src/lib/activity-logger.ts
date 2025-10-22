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

import { invoke } from "@tauri-apps/api/core";

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

  /** Token usage tracking (optional) */
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;

  /** Model used for this activity (e.g., "claude-sonnet-4-5") */
  model?: string;
}

/**
 * Log an activity entry to SQLite database
 *
 * Non-blocking - errors are logged to console but not thrown.
 *
 * @param workspacePath - Absolute path to workspace
 * @param entry - Activity entry to log
 */
export async function logActivity(
  workspacePath: string,
  entry: ActivityEntry,
): Promise<void> {
  try {
    await invoke("log_activity", {
      workspacePath,
      entry,
    });
  } catch (error) {
    console.error("[activity-logger] Failed to log activity:", error);
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
  limit = 100,
): Promise<ActivityEntry[]> {
  try {
    return await invoke("read_recent_activity", {
      workspacePath,
      limit,
    });
  } catch (error) {
    console.error("[activity-logger] Failed to read recent activity:", error);
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
  limit = 100,
): Promise<ActivityEntry[]> {
  try {
    return await invoke("get_activity_by_role", {
      workspacePath,
      role,
      limit,
    });
  } catch (error) {
    console.error(
      `[activity-logger] Failed to get activity for role ${role}:`,
      error,
    );
    return [];
  }
}

/**
 * Token usage summary by role
 */
export interface TokenUsageSummary {
  role: string;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_tokens: number;
  activity_count: number;
  avg_tokens_per_activity: number;
}

/**
 * Query token usage statistics grouped by role
 *
 * @param workspacePath - Absolute path to workspace
 * @param since - Optional ISO 8601 timestamp to filter activities after this time
 * @returns Array of token usage summaries by role
 */
export async function queryTokenUsageByRole(
  workspacePath: string,
  since?: string,
): Promise<TokenUsageSummary[]> {
  try {
    return await invoke("query_token_usage_by_role", {
      workspacePath,
      since,
    });
  } catch (error) {
    console.error("[activity-logger] Failed to query token usage:", error);
    return [];
  }
}

/**
 * Daily token usage entry
 */
export interface DailyTokenUsage {
  date: string;
  role: string;
  total_tokens: number;
}

/**
 * Query token usage timeline (daily aggregation)
 *
 * @param workspacePath - Absolute path to workspace
 * @param days - Number of days to look back (default: 30)
 * @returns Array of daily token usage entries
 */
export async function queryTokenUsageTimeline(
  workspacePath: string,
  days = 30,
): Promise<DailyTokenUsage[]> {
  try {
    return await invoke("query_token_usage_timeline", {
      workspacePath,
      days,
    });
  } catch (error) {
    console.error("[activity-logger] Failed to query token timeline:", error);
    return [];
  }
}
