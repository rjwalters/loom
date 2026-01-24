/**
 * Agent Metrics Module
 *
 * Provides functions to fetch and display agent effectiveness metrics.
 * Data includes prompt counts, token usage, costs, success rates, and GitHub activity.
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("agent-metrics");

// ============================================================================
// Types
// ============================================================================

/**
 * Time range for metrics queries
 */
export type TimeRange = "today" | "week" | "month" | "all";

/**
 * Agent metrics summary
 */
export interface AgentMetrics {
  /** Total number of prompts/activities */
  prompt_count: number;
  /** Total tokens consumed */
  total_tokens: number;
  /** Estimated cost in USD */
  total_cost: number;
  /** Success rate (0-1) */
  success_rate: number;
  /** Number of PRs created */
  prs_created: number;
  /** Number of issues closed */
  issues_closed: number;
}

/**
 * Metrics for a specific role
 */
export interface RoleMetrics {
  role: string;
  prompt_count: number;
  total_tokens: number;
  total_cost: number;
  success_rate: number;
}

// ============================================================================
// API Functions
// ============================================================================

/**
 * Get agent metrics for a time range
 *
 * @param workspacePath - Path to the workspace
 * @param timeRange - Time range: "today", "week", "month", or "all"
 * @returns Agent metrics summary
 */
export async function getAgentMetrics(
  workspacePath: string,
  timeRange: TimeRange
): Promise<AgentMetrics> {
  try {
    return await invoke<AgentMetrics>("get_agent_metrics", {
      workspacePath,
      timeRange,
    });
  } catch (error) {
    logger.error("Failed to get agent metrics", error as Error, { timeRange });
    // Return empty metrics on error
    return {
      prompt_count: 0,
      total_tokens: 0,
      total_cost: 0,
      success_rate: 0,
      prs_created: 0,
      issues_closed: 0,
    };
  }
}

/**
 * Get metrics broken down by role
 *
 * @param workspacePath - Path to the workspace
 * @param timeRange - Time range for the query
 * @returns Array of role metrics
 */
export async function getMetricsByRole(
  workspacePath: string,
  timeRange: TimeRange
): Promise<RoleMetrics[]> {
  try {
    return await invoke<RoleMetrics[]>("get_metrics_by_role", {
      workspacePath,
      timeRange,
    });
  } catch (error) {
    logger.error("Failed to get metrics by role", error as Error, { timeRange });
    return [];
  }
}

/**
 * Log a GitHub event for tracking
 *
 * @param workspacePath - Path to the workspace
 * @param eventType - Type of event (pr_created, issue_closed, etc.)
 * @param options - Optional event details
 */
export async function logGitHubEvent(
  workspacePath: string,
  eventType: string,
  options?: {
    prNumber?: number;
    issueNumber?: number;
    commitSha?: string;
    author?: string;
  }
): Promise<void> {
  try {
    await invoke("log_github_event", {
      workspacePath,
      eventType,
      prNumber: options?.prNumber ?? null,
      issueNumber: options?.issueNumber ?? null,
      commitSha: options?.commitSha ?? null,
      author: options?.author ?? null,
    });
  } catch (error) {
    logger.error("Failed to log GitHub event", error as Error, { eventType });
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format a number with commas for display
 */
export function formatNumber(num: number): string {
  return num.toLocaleString();
}

/**
 * Format currency for display
 */
export function formatCurrency(amount: number): string {
  return `$${amount.toFixed(2)}`;
}

/**
 * Format a percentage for display
 */
export function formatPercent(rate: number): string {
  return `${(rate * 100).toFixed(1)}%`;
}

/**
 * Format token count with K/M suffix for large numbers
 */
export function formatTokens(tokens: number): string {
  if (tokens >= 1_000_000) {
    return `${(tokens / 1_000_000).toFixed(1)}M`;
  }
  if (tokens >= 1_000) {
    return `${(tokens / 1_000).toFixed(1)}K`;
  }
  return tokens.toString();
}

/**
 * Get a human-readable label for a time range
 */
export function getTimeRangeLabel(timeRange: TimeRange): string {
  switch (timeRange) {
    case "today":
      return "Today";
    case "week":
      return "This Week";
    case "month":
      return "This Month";
    case "all":
      return "All Time";
  }
}

/**
 * Get a color class based on success rate
 */
export function getSuccessRateColor(rate: number): string {
  if (rate >= 0.9) return "text-green-600 dark:text-green-400";
  if (rate >= 0.7) return "text-yellow-600 dark:text-yellow-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Get display name for a role
 */
export function getRoleDisplayName(role: string): string {
  // Capitalize first letter and add any special formatting
  const displayNames: Record<string, string> = {
    builder: "Builder",
    judge: "Judge",
    curator: "Curator",
    architect: "Architect",
    hermit: "Hermit",
    doctor: "Doctor",
    guide: "Guide",
    champion: "Champion",
    shepherd: "Shepherd",
    loom: "Loom Daemon",
    driver: "Driver",
  };
  return displayNames[role.toLowerCase()] ?? role;
}
