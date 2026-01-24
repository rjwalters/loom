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

// ============================================================================
// Velocity Tracking Types
// ============================================================================

/**
 * Daily velocity snapshot
 */
export interface VelocitySnapshot {
  snapshot_date: string;
  issues_closed: number;
  prs_merged: number;
  avg_cycle_time_hours: number | null;
  total_prompts: number;
  total_cost_usd: number;
}

/**
 * Trend direction indicator
 */
export type TrendDirection = "improving" | "declining" | "stable";

/**
 * Velocity summary with week-over-week trends
 */
export interface VelocitySummary {
  // Current period metrics
  issues_closed: number;
  prs_merged: number;
  avg_cycle_time_hours: number | null;
  total_prompts: number;
  total_cost_usd: number;
  // Previous period metrics
  prev_issues_closed: number;
  prev_prs_merged: number;
  prev_avg_cycle_time_hours: number | null;
  // Trend directions
  issues_trend: TrendDirection;
  prs_trend: TrendDirection;
  cycle_time_trend: TrendDirection;
}

/**
 * Rolling average metrics
 */
export interface RollingAverage {
  period_days: number;
  avg_issues_per_day: number;
  avg_prs_per_day: number;
  avg_cycle_time_hours: number | null;
  avg_cost_per_day: number;
}

/**
 * Velocity trend data point for charting
 */
export interface VelocityTrendPoint {
  date: string;
  issues_closed: number;
  issues_closed_7day_avg: number;
  prs_merged: number;
  prs_merged_7day_avg: number;
  cycle_time_hours: number | null;
  cycle_time_7day_avg: number | null;
}

/**
 * Period comparison results
 */
export interface PeriodComparison {
  period1_label: string;
  period2_label: string;
  period1_issues: number;
  period2_issues: number;
  issues_change_pct: number;
  period1_prs: number;
  period2_prs: number;
  prs_change_pct: number;
  period1_cycle_time: number | null;
  period2_cycle_time: number | null;
  cycle_time_change_pct: number | null;
}

// ============================================================================
// Velocity Tracking API Functions
// ============================================================================

/**
 * Generate or update today's velocity snapshot
 *
 * @param workspacePath - Path to the workspace
 * @returns Today's velocity snapshot
 */
export async function generateVelocitySnapshot(workspacePath: string): Promise<VelocitySnapshot> {
  try {
    return await invoke<VelocitySnapshot>("generate_velocity_snapshot", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to generate velocity snapshot", error as Error);
    throw error;
  }
}

/**
 * Get velocity snapshots for a date range
 *
 * @param workspacePath - Path to the workspace
 * @param days - Number of days to retrieve (default: 30)
 * @returns Array of velocity snapshots
 */
export async function getVelocitySnapshots(
  workspacePath: string,
  days?: number
): Promise<VelocitySnapshot[]> {
  try {
    return await invoke<VelocitySnapshot[]>("get_velocity_snapshots", {
      workspacePath,
      days: days ?? null,
    });
  } catch (error) {
    logger.error("Failed to get velocity snapshots", error as Error, { days });
    return [];
  }
}

/**
 * Get velocity summary with week-over-week comparison
 *
 * @param workspacePath - Path to the workspace
 * @returns Velocity summary with trends
 */
export async function getVelocitySummary(workspacePath: string): Promise<VelocitySummary> {
  try {
    return await invoke<VelocitySummary>("get_velocity_summary", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to get velocity summary", error as Error);
    return {
      issues_closed: 0,
      prs_merged: 0,
      avg_cycle_time_hours: null,
      total_prompts: 0,
      total_cost_usd: 0,
      prev_issues_closed: 0,
      prev_prs_merged: 0,
      prev_avg_cycle_time_hours: null,
      issues_trend: "stable",
      prs_trend: "stable",
      cycle_time_trend: "stable",
    };
  }
}

/**
 * Get rolling average metrics
 *
 * @param workspacePath - Path to the workspace
 * @param periodDays - Number of days for the rolling average (default: 7)
 * @returns Rolling average metrics
 */
export async function getRollingAverage(
  workspacePath: string,
  periodDays?: number
): Promise<RollingAverage> {
  try {
    return await invoke<RollingAverage>("get_rolling_average", {
      workspacePath,
      periodDays: periodDays ?? null,
    });
  } catch (error) {
    logger.error("Failed to get rolling average", error as Error, {
      periodDays,
    });
    return {
      period_days: periodDays ?? 7,
      avg_issues_per_day: 0,
      avg_prs_per_day: 0,
      avg_cycle_time_hours: null,
      avg_cost_per_day: 0,
    };
  }
}

/**
 * Backfill historical velocity data from existing activity records
 *
 * @param workspacePath - Path to the workspace
 * @param days - Number of days to backfill (default: 30)
 * @returns Number of snapshots created
 */
export async function backfillVelocityHistory(
  workspacePath: string,
  days?: number
): Promise<number> {
  try {
    return await invoke<number>("backfill_velocity_history", {
      workspacePath,
      days: days ?? null,
    });
  } catch (error) {
    logger.error("Failed to backfill velocity history", error as Error, {
      days,
    });
    throw error;
  }
}

/**
 * Get velocity trend data with 7-day rolling averages
 *
 * @param workspacePath - Path to the workspace
 * @param days - Number of days to retrieve (default: 30)
 * @returns Array of trend data points
 */
export async function getVelocityTrends(
  workspacePath: string,
  days?: number
): Promise<VelocityTrendPoint[]> {
  try {
    return await invoke<VelocityTrendPoint[]>("get_velocity_trends", {
      workspacePath,
      days: days ?? null,
    });
  } catch (error) {
    logger.error("Failed to get velocity trends", error as Error, { days });
    return [];
  }
}

/**
 * Compare velocity between two time periods
 *
 * @param workspacePath - Path to the workspace
 * @param period1Start - Start date for period 1 (YYYY-MM-DD)
 * @param period1End - End date for period 1 (YYYY-MM-DD)
 * @param period2Start - Start date for period 2 (YYYY-MM-DD)
 * @param period2End - End date for period 2 (YYYY-MM-DD)
 * @returns Period comparison results
 */
export async function compareVelocityPeriods(
  workspacePath: string,
  period1Start: string,
  period1End: string,
  period2Start: string,
  period2End: string
): Promise<PeriodComparison> {
  try {
    return await invoke<PeriodComparison>("compare_velocity_periods", {
      workspacePath,
      period1Start,
      period1End,
      period2Start,
      period2End,
    });
  } catch (error) {
    logger.error("Failed to compare velocity periods", error as Error);
    throw error;
  }
}

// ============================================================================
// Velocity Formatting Utilities
// ============================================================================

/**
 * Get trend icon based on direction
 */
export function getTrendIcon(trend: TrendDirection): string {
  switch (trend) {
    case "improving":
      return "\u2191"; // Up arrow
    case "declining":
      return "\u2193"; // Down arrow
    case "stable":
      return "\u2192"; // Right arrow
  }
}

/**
 * Get CSS color class based on trend direction
 */
export function getTrendColor(trend: TrendDirection, lowerIsBetter = false): string {
  const colors = {
    improving: "text-green-600 dark:text-green-400",
    declining: "text-red-600 dark:text-red-400",
    stable: "text-gray-600 dark:text-gray-400",
  };

  if (lowerIsBetter) {
    return trend === "improving"
      ? colors.declining
      : trend === "declining"
        ? colors.improving
        : colors.stable;
  }

  return colors[trend];
}

/**
 * Format cycle time in hours with appropriate precision
 */
export function formatCycleTime(hours: number | null): string {
  if (hours === null) return "-";
  if (hours < 1) return `${Math.round(hours * 60)}m`;
  if (hours < 24) return `${hours.toFixed(1)}h`;
  return `${(hours / 24).toFixed(1)}d`;
}

/**
 * Format percentage change with sign
 */
export function formatChangePercent(pct: number | null): string {
  if (pct === null) return "-";
  const sign = pct >= 0 ? "+" : "";
  return `${sign}${pct.toFixed(1)}%`;
}
