/**
 * Weekly Intelligence Report Module
 *
 * Generates automated weekly intelligence reports that summarize development metrics,
 * highlight patterns and anti-patterns, surface anomalies, and provide recommendations.
 *
 * Features:
 * - Scheduled report generation (configurable day/time)
 * - Week-over-week metric comparison with trend indicators
 * - Pattern detection from correlation analysis
 * - Anomaly detection (stuck PRs, cost spikes, etc.)
 * - Report history with view/export capabilities
 * - In-app notifications when reports are ready
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1111
 */

import { invoke } from "@tauri-apps/api/core";
import {
  type AgentMetrics,
  formatChangePercent,
  formatCurrency,
  formatCycleTime,
  formatNumber,
  formatPercent,
  formatTokens,
  getAgentMetrics,
  getMetricsByRole,
  getTrendIcon,
  getVelocitySummary,
  type RoleMetrics,
  type TrendDirection,
  type VelocitySummary,
} from "./agent-metrics";
import {
  type CorrelationInsight,
  type CorrelationSummary,
  runCorrelationAnalysis,
} from "./correlation-analysis";
import { Logger } from "./logger";

const logger = Logger.forComponent("weekly-report");

// ============================================================================
// Types
// ============================================================================

/**
 * Weekly report schedule configuration
 */
export interface ReportSchedule {
  /** Day of week (0=Sunday, 6=Saturday) */
  dayOfWeek: number;
  /** Hour of day (0-23) */
  hourOfDay: number;
  /** Timezone offset in minutes from UTC */
  timezoneOffset: number;
  /** Whether automatic report generation is enabled */
  enabled: boolean;
}

/**
 * Weekly report summary statistics
 */
export interface WeekSummary {
  // Current week metrics
  features_completed: number;
  prs_merged: number;
  total_prompts: number;
  total_tokens: number;
  total_cost: number;
  success_rate: number;
  avg_cycle_time_hours: number | null;

  // Previous week metrics for comparison
  prev_features_completed: number;
  prev_prs_merged: number;
  prev_total_prompts: number;
  prev_total_tokens: number;
  prev_total_cost: number;
  prev_success_rate: number;
  prev_avg_cycle_time_hours: number | null;

  // Trends
  features_trend: TrendDirection;
  prs_trend: TrendDirection;
  cost_trend: TrendDirection;
  success_trend: TrendDirection;
  cycle_time_trend: TrendDirection;
}

/**
 * Pattern identified from correlation analysis
 */
export interface IdentifiedPattern {
  type: "success" | "improvement";
  factor: string;
  description: string;
  impact: string;
  strength: "strong" | "moderate" | "weak";
}

/**
 * Anomaly detected in the data
 */
export interface DetectedAnomaly {
  severity: "warning" | "critical";
  type: string;
  message: string;
  details: string;
  detected_at: string;
}

/**
 * Recommendation based on analysis
 */
export interface Recommendation {
  priority: "high" | "medium" | "low";
  category: string;
  title: string;
  description: string;
  action: string;
}

/**
 * "Did you know?" insight for the report
 */
export interface DidYouKnow {
  icon: string;
  fact: string;
  context: string;
}

/**
 * Complete weekly intelligence report
 */
export interface WeeklyReport {
  /** Unique report ID */
  id: string;
  /** Report generation timestamp */
  generated_at: string;
  /** Week start date (Monday) */
  week_start: string;
  /** Week end date (Sunday) */
  week_end: string;
  /** Summary statistics */
  summary: WeekSummary;
  /** Role-specific metrics */
  role_metrics: RoleMetrics[];
  /** Identified success patterns */
  success_patterns: IdentifiedPattern[];
  /** Areas for improvement */
  improvement_areas: IdentifiedPattern[];
  /** Detected anomalies */
  anomalies: DetectedAnomaly[];
  /** Recommendations based on data */
  recommendations: Recommendation[];
  /** Did you know insights */
  did_you_know: DidYouKnow[];
  /** Report status */
  status: "generated" | "viewed" | "exported";
}

/**
 * Report history entry (lightweight)
 */
export interface ReportHistoryEntry {
  id: string;
  generated_at: string;
  week_start: string;
  week_end: string;
  summary_features: number;
  summary_cost: number;
  status: "generated" | "viewed" | "exported";
}

// ============================================================================
// Report Generation
// ============================================================================

/**
 * Generate a new weekly intelligence report
 *
 * @param workspacePath - Path to the workspace
 * @returns Generated weekly report
 */
export async function generateWeeklyReport(workspacePath: string): Promise<WeeklyReport> {
  logger.info("Generating weekly intelligence report", { workspacePath });

  const now = new Date();
  const weekEnd = getLastSunday(now);
  const weekStart = new Date(weekEnd);
  weekStart.setDate(weekStart.getDate() - 6);

  try {
    // Fetch all data in parallel
    const [weekMetrics, prevWeekMetrics, velocity, roleMetrics, correlationSummary] =
      await Promise.all([
        getAgentMetrics(workspacePath, "week"),
        getPreviousWeekMetrics(workspacePath),
        getVelocitySummary(workspacePath),
        getMetricsByRole(workspacePath, "week"),
        runCorrelationAnalysis(workspacePath),
      ]);

    // Build summary
    const summary = buildWeekSummary(weekMetrics, prevWeekMetrics, velocity);

    // Identify patterns
    const { successPatterns, improvementAreas } = identifyPatterns(
      correlationSummary,
      roleMetrics,
      summary
    );

    // Detect anomalies
    const anomalies = await detectAnomalies(workspacePath, summary, roleMetrics);

    // Generate recommendations
    const recommendations = generateRecommendations(
      summary,
      successPatterns,
      improvementAreas,
      anomalies
    );

    // Generate "Did you know?" insights
    const didYouKnow = generateDidYouKnow(summary, roleMetrics, correlationSummary);

    const report: WeeklyReport = {
      id: generateReportId(),
      generated_at: now.toISOString(),
      week_start: weekStart.toISOString().split("T")[0],
      week_end: weekEnd.toISOString().split("T")[0],
      summary,
      role_metrics: roleMetrics,
      success_patterns: successPatterns,
      improvement_areas: improvementAreas,
      anomalies,
      recommendations,
      did_you_know: didYouKnow,
      status: "generated",
    };

    // Save report to storage
    await saveReport(workspacePath, report);

    logger.info("Weekly report generated successfully", { reportId: report.id });

    return report;
  } catch (error) {
    logger.error("Failed to generate weekly report", error as Error);
    throw error;
  }
}

/**
 * Get metrics for the previous week
 */
async function getPreviousWeekMetrics(workspacePath: string): Promise<AgentMetrics> {
  try {
    return await invoke<AgentMetrics>("get_previous_week_metrics", {
      workspacePath,
    });
  } catch {
    // If the command doesn't exist yet, return zeros
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
 * Build week summary from metrics
 */
function buildWeekSummary(
  current: AgentMetrics,
  previous: AgentMetrics,
  velocity: VelocitySummary
): WeekSummary {
  return {
    features_completed: current.issues_closed,
    prs_merged: velocity.prs_merged,
    total_prompts: current.prompt_count,
    total_tokens: current.total_tokens,
    total_cost: current.total_cost,
    success_rate: current.success_rate,
    avg_cycle_time_hours: velocity.avg_cycle_time_hours,

    prev_features_completed: previous.issues_closed,
    prev_prs_merged: velocity.prev_prs_merged,
    prev_total_prompts: previous.prompt_count,
    prev_total_tokens: previous.total_tokens,
    prev_total_cost: previous.total_cost,
    prev_success_rate: previous.success_rate,
    prev_avg_cycle_time_hours: velocity.prev_avg_cycle_time_hours,

    features_trend: velocity.issues_trend,
    prs_trend: velocity.prs_trend,
    cost_trend: determineTrend(current.total_cost, previous.total_cost, true),
    success_trend: determineTrend(current.success_rate, previous.success_rate),
    cycle_time_trend: velocity.cycle_time_trend,
  };
}

/**
 * Determine trend direction
 */
function determineTrend(current: number, previous: number, lowerIsBetter = false): TrendDirection {
  if (previous === 0) return "stable";
  const changePercent = ((current - previous) / previous) * 100;

  if (Math.abs(changePercent) < 5) return "stable";

  if (lowerIsBetter) {
    return changePercent < 0 ? "improving" : "declining";
  }
  return changePercent > 0 ? "improving" : "declining";
}

/**
 * Identify success patterns and improvement areas
 */
function identifyPatterns(
  correlationSummary: CorrelationSummary,
  roleMetrics: RoleMetrics[],
  summary: WeekSummary
): { successPatterns: IdentifiedPattern[]; improvementAreas: IdentifiedPattern[] } {
  const successPatterns: IdentifiedPattern[] = [];
  const improvementAreas: IdentifiedPattern[] = [];

  // Convert correlation insights to patterns
  for (const insight of correlationSummary.top_insights) {
    const pattern: IdentifiedPattern = {
      type: "success",
      factor: insight.factor,
      description: insight.insight,
      impact: insight.recommendation,
      strength: insight.correlation_strength,
    };

    if (insight.correlation_strength === "strong" || insight.correlation_strength === "moderate") {
      successPatterns.push(pattern);
    }
  }

  // Identify role-specific patterns
  for (const role of roleMetrics) {
    if (role.success_rate >= 0.9 && role.prompt_count > 10) {
      successPatterns.push({
        type: "success",
        factor: `${role.role}_high_performance`,
        description: `${role.role} role achieving ${formatPercent(role.success_rate)} success rate`,
        impact: `Maintaining high quality with ${formatNumber(role.prompt_count)} prompts`,
        strength: "strong",
      });
    } else if (role.success_rate < 0.6 && role.prompt_count > 5) {
      improvementAreas.push({
        type: "improvement",
        factor: `${role.role}_low_success`,
        description: `${role.role} role has ${formatPercent(role.success_rate)} success rate`,
        impact: "Consider reviewing prompts or adjusting workflow",
        strength: role.success_rate < 0.4 ? "strong" : "moderate",
      });
    }
  }

  // Identify trends as patterns
  if (summary.features_trend === "improving") {
    successPatterns.push({
      type: "success",
      factor: "velocity_improvement",
      description: "Feature completion velocity is improving",
      impact: `${summary.features_completed} features this week vs ${summary.prev_features_completed} last week`,
      strength: "moderate",
    });
  } else if (summary.features_trend === "declining" && summary.features_completed > 0) {
    improvementAreas.push({
      type: "improvement",
      factor: "velocity_decline",
      description: "Feature completion velocity has declined",
      impact: `${summary.features_completed} features this week vs ${summary.prev_features_completed} last week`,
      strength: "moderate",
    });
  }

  if (summary.cycle_time_trend === "improving") {
    successPatterns.push({
      type: "success",
      factor: "cycle_time_improvement",
      description: "Average cycle time is improving",
      impact: `Now averaging ${formatCycleTime(summary.avg_cycle_time_hours)}`,
      strength: "moderate",
    });
  }

  // Limit to top 3 of each
  return {
    successPatterns: successPatterns.slice(0, 3),
    improvementAreas: improvementAreas.slice(0, 3),
  };
}

/**
 * Detect anomalies in the data
 */
async function detectAnomalies(
  workspacePath: string,
  summary: WeekSummary,
  roleMetrics: RoleMetrics[]
): Promise<DetectedAnomaly[]> {
  const anomalies: DetectedAnomaly[] = [];
  const now = new Date().toISOString();

  // Check for stuck PRs (PRs in review > 48 hours)
  try {
    const stuckPRs = await invoke<number>("get_stuck_pr_count", {
      workspacePath,
      hoursThreshold: 48,
    });
    if (stuckPRs > 0) {
      anomalies.push({
        severity: stuckPRs >= 3 ? "critical" : "warning",
        type: "stuck_prs",
        message: `${stuckPRs} PR${stuckPRs === 1 ? "" : "s"} stuck in review > 48 hours`,
        details: "PRs waiting for review may block development velocity",
        detected_at: now,
      });
    }
  } catch {
    // Command may not exist yet
  }

  // Check for unusual cost spike (>50% increase)
  if (summary.prev_total_cost > 0) {
    const costIncrease =
      ((summary.total_cost - summary.prev_total_cost) / summary.prev_total_cost) * 100;
    if (costIncrease > 50) {
      anomalies.push({
        severity: costIncrease > 100 ? "critical" : "warning",
        type: "cost_spike",
        message: `API cost increased ${costIncrease.toFixed(0)}% vs last week`,
        details: `This week: ${formatCurrency(summary.total_cost)} vs Last week: ${formatCurrency(summary.prev_total_cost)}`,
        detected_at: now,
      });
    }
  }

  // Check for sharp success rate drop
  if (summary.prev_success_rate > 0) {
    const successDrop = (summary.prev_success_rate - summary.success_rate) * 100;
    if (successDrop > 15) {
      anomalies.push({
        severity: successDrop > 25 ? "critical" : "warning",
        type: "success_rate_drop",
        message: `Success rate dropped ${successDrop.toFixed(0)}%`,
        details: `This week: ${formatPercent(summary.success_rate)} vs Last week: ${formatPercent(summary.prev_success_rate)}`,
        detected_at: now,
      });
    }
  }

  // Check for role with zero activity
  const activeRoles = roleMetrics.filter((r) => r.prompt_count > 0);
  const expectedRoles = ["builder", "judge", "curator"];
  for (const expected of expectedRoles) {
    if (!activeRoles.some((r) => r.role.toLowerCase() === expected)) {
      anomalies.push({
        severity: "warning",
        type: "inactive_role",
        message: `${expected} role had no activity this week`,
        details: "This role may not be configured or running",
        detected_at: now,
      });
    }
  }

  return anomalies;
}

/**
 * Generate recommendations based on analysis
 */
function generateRecommendations(
  summary: WeekSummary,
  successPatterns: IdentifiedPattern[],
  improvementAreas: IdentifiedPattern[],
  anomalies: DetectedAnomaly[]
): Recommendation[] {
  const recommendations: Recommendation[] = [];

  // Recommendations based on anomalies
  for (const anomaly of anomalies) {
    if (anomaly.type === "stuck_prs") {
      recommendations.push({
        priority: anomaly.severity === "critical" ? "high" : "medium",
        category: "Workflow",
        title: "Review stuck PRs",
        description: anomaly.message,
        action: "Check PR queue and address blocking reviews",
      });
    } else if (anomaly.type === "cost_spike") {
      recommendations.push({
        priority: "medium",
        category: "Cost",
        title: "Investigate cost increase",
        description: anomaly.message,
        action: "Review token usage by role and identify high-cost activities",
      });
    } else if (anomaly.type === "success_rate_drop") {
      recommendations.push({
        priority: "high",
        category: "Quality",
        title: "Address success rate decline",
        description: anomaly.message,
        action: "Review recent failures and identify common issues",
      });
    }
  }

  // Recommendations based on improvement areas
  for (const area of improvementAreas) {
    if (area.factor.includes("low_success")) {
      const role = area.factor.replace("_low_success", "");
      recommendations.push({
        priority: area.strength === "strong" ? "high" : "medium",
        category: "Quality",
        title: `Improve ${role} success rate`,
        description: area.description,
        action: "Review prompts and consider additional context or guidance",
      });
    } else if (area.factor === "velocity_decline") {
      recommendations.push({
        priority: "medium",
        category: "Velocity",
        title: "Address velocity decline",
        description: area.description,
        action: "Check for blockers or process bottlenecks",
      });
    }
  }

  // Recommendations to reinforce success patterns
  for (const pattern of successPatterns.slice(0, 1)) {
    recommendations.push({
      priority: "low",
      category: "Success",
      title: "Continue successful practices",
      description: `${pattern.description} - keep this up!`,
      action: "Document and share what's working well",
    });
  }

  // General recommendations based on metrics
  if (summary.total_prompts > 0 && summary.features_completed === 0) {
    recommendations.push({
      priority: "high",
      category: "Effectiveness",
      title: "No features completed this week",
      description: `${formatNumber(summary.total_prompts)} prompts used but no features shipped`,
      action: "Review work in progress and identify blockers",
    });
  }

  // Sort by priority
  const priorityOrder: Record<string, number> = { high: 0, medium: 1, low: 2 };
  recommendations.sort((a, b) => priorityOrder[a.priority] - priorityOrder[b.priority]);

  return recommendations.slice(0, 5); // Top 5 recommendations
}

/**
 * Generate "Did you know?" insights
 */
function generateDidYouKnow(
  summary: WeekSummary,
  roleMetrics: RoleMetrics[],
  correlationSummary: CorrelationSummary
): DidYouKnow[] {
  const insights: DidYouKnow[] = [];

  // Cost per feature
  if (summary.features_completed > 0) {
    const costPerFeature = summary.total_cost / summary.features_completed;
    insights.push({
      icon: "dollar",
      fact: `Each feature cost an average of ${formatCurrency(costPerFeature)} in API usage`,
      context:
        summary.prev_features_completed > 0
          ? `Last week: ${formatCurrency(summary.prev_total_cost / summary.prev_features_completed)}`
          : "This is your first week with completed features!",
    });
  }

  // Most active role
  const sortedRoles = [...roleMetrics].sort((a, b) => b.prompt_count - a.prompt_count);
  if (sortedRoles.length > 0 && sortedRoles[0].prompt_count > 0) {
    const topRole = sortedRoles[0];
    const percentOfTotal =
      summary.total_prompts > 0
        ? ((topRole.prompt_count / summary.total_prompts) * 100).toFixed(0)
        : "0";
    insights.push({
      icon: "zap",
      fact: `The ${topRole.role} role was most active with ${formatNumber(topRole.prompt_count)} prompts`,
      context: `That's ${percentOfTotal}% of all activity this week`,
    });
  }

  // Best day insight from correlations
  const dayInsight = correlationSummary.top_insights.find((i) =>
    i.factor.toLowerCase().includes("day")
  );
  if (dayInsight) {
    insights.push({
      icon: "calendar",
      fact: dayInsight.insight,
      context: dayInsight.recommendation,
    });
  }

  // Token efficiency
  if (summary.total_tokens > 0 && summary.features_completed > 0) {
    const tokensPerFeature = summary.total_tokens / summary.features_completed;
    insights.push({
      icon: "cpu",
      fact: `On average, ${formatTokens(tokensPerFeature)} tokens were used per feature`,
      context:
        tokensPerFeature > 100000
          ? "Consider using more concise prompts"
          : "Good token efficiency!",
    });
  }

  // Cycle time insight
  if (summary.avg_cycle_time_hours !== null) {
    insights.push({
      icon: "clock",
      fact: `Features take about ${formatCycleTime(summary.avg_cycle_time_hours)} from issue to merge`,
      context:
        summary.avg_cycle_time_hours < 4
          ? "That's fast! Great velocity."
          : summary.avg_cycle_time_hours > 24
            ? "Consider breaking down larger issues"
            : "Healthy development pace",
    });
  }

  return insights.slice(0, 3); // Top 3 insights
}

// ============================================================================
// Report Storage
// ============================================================================

/**
 * Save a report to storage
 */
async function saveReport(workspacePath: string, report: WeeklyReport): Promise<void> {
  try {
    await invoke("save_weekly_report", {
      workspacePath,
      reportJson: JSON.stringify(report),
    });
  } catch (error) {
    logger.error("Failed to save weekly report", error as Error);
    throw error;
  }
}

/**
 * Get a specific report by ID
 *
 * @param workspacePath - Path to the workspace
 * @param reportId - Report ID
 * @returns The report or null if not found
 */
export async function getReport(
  workspacePath: string,
  reportId: string
): Promise<WeeklyReport | null> {
  try {
    const reportJson = await invoke<string | null>("get_weekly_report", {
      workspacePath,
      reportId,
    });
    if (!reportJson) return null;
    return JSON.parse(reportJson) as WeeklyReport;
  } catch (error) {
    logger.error("Failed to get weekly report", error as Error, { reportId });
    return null;
  }
}

/**
 * Get report history (last N weeks)
 *
 * @param workspacePath - Path to the workspace
 * @param limit - Maximum number of reports to return (default: 4)
 * @returns Array of report history entries
 */
export async function getReportHistory(
  workspacePath: string,
  limit: number = 4
): Promise<ReportHistoryEntry[]> {
  try {
    return await invoke<ReportHistoryEntry[]>("get_weekly_report_history", {
      workspacePath,
      limit,
    });
  } catch (error) {
    logger.error("Failed to get report history", error as Error);
    return [];
  }
}

/**
 * Get the latest report
 *
 * @param workspacePath - Path to the workspace
 * @returns The latest report or null if none exists
 */
export async function getLatestReport(workspacePath: string): Promise<WeeklyReport | null> {
  try {
    const reportJson = await invoke<string | null>("get_latest_weekly_report", {
      workspacePath,
    });
    if (!reportJson) return null;
    return JSON.parse(reportJson) as WeeklyReport;
  } catch (error) {
    logger.error("Failed to get latest weekly report", error as Error);
    return null;
  }
}

/**
 * Mark a report as viewed
 *
 * @param workspacePath - Path to the workspace
 * @param reportId - Report ID
 */
export async function markReportViewed(workspacePath: string, reportId: string): Promise<void> {
  try {
    await invoke("mark_weekly_report_viewed", {
      workspacePath,
      reportId,
    });
  } catch (error) {
    logger.error("Failed to mark report as viewed", error as Error, { reportId });
  }
}

// ============================================================================
// Scheduling
// ============================================================================

let scheduleIntervalId: ReturnType<typeof setInterval> | null = null;

/**
 * Get the default report schedule
 */
export function getDefaultSchedule(): ReportSchedule {
  return {
    dayOfWeek: 1, // Monday
    hourOfDay: 9, // 9 AM
    timezoneOffset: new Date().getTimezoneOffset(),
    enabled: true,
  };
}

/**
 * Get the current report schedule
 *
 * @param workspacePath - Path to the workspace
 * @returns Current schedule configuration
 */
export async function getReportSchedule(workspacePath: string): Promise<ReportSchedule> {
  try {
    return await invoke<ReportSchedule>("get_weekly_report_schedule", {
      workspacePath,
    });
  } catch {
    return getDefaultSchedule();
  }
}

/**
 * Save the report schedule
 *
 * @param workspacePath - Path to the workspace
 * @param schedule - New schedule configuration
 */
export async function saveReportSchedule(
  workspacePath: string,
  schedule: ReportSchedule
): Promise<void> {
  try {
    await invoke("save_weekly_report_schedule", {
      workspacePath,
      scheduleJson: JSON.stringify(schedule),
    });
  } catch (error) {
    logger.error("Failed to save report schedule", error as Error);
    throw error;
  }
}

/**
 * Start the report scheduler
 *
 * @param workspacePath - Path to the workspace
 * @param onReportReady - Callback when a new report is generated
 */
export async function startReportScheduler(
  workspacePath: string,
  onReportReady?: (report: WeeklyReport) => void
): Promise<void> {
  // Stop any existing scheduler
  stopReportScheduler();

  const schedule = await getReportSchedule(workspacePath);
  if (!schedule.enabled) {
    logger.info("Report scheduler disabled");
    return;
  }

  logger.info("Starting report scheduler", {
    dayOfWeek: schedule.dayOfWeek,
    hourOfDay: schedule.hourOfDay,
  });

  // Check every hour if it's time to generate a report
  scheduleIntervalId = setInterval(
    async () => {
      const now = new Date();
      const dayOfWeek = now.getDay();
      const hourOfDay = now.getHours();

      if (dayOfWeek === schedule.dayOfWeek && hourOfDay === schedule.hourOfDay) {
        // Check if we already have a report for this week
        const latest = await getLatestReport(workspacePath);
        const weekStart = getLastMonday(now).toISOString().split("T")[0];

        if (latest && latest.week_start === weekStart) {
          logger.debug("Report already exists for this week, skipping");
          return;
        }

        try {
          const report = await generateWeeklyReport(workspacePath);
          logger.info("Scheduled report generated", { reportId: report.id });
          onReportReady?.(report);
        } catch (error) {
          logger.error("Scheduled report generation failed", error as Error);
        }
      }
    },
    60 * 60 * 1000
  ); // Check every hour
}

/**
 * Stop the report scheduler
 */
export function stopReportScheduler(): void {
  if (scheduleIntervalId) {
    clearInterval(scheduleIntervalId);
    scheduleIntervalId = null;
    logger.info("Report scheduler stopped");
  }
}

/**
 * Check if the scheduler is running
 */
export function isSchedulerRunning(): boolean {
  return scheduleIntervalId !== null;
}

// ============================================================================
// Export Functions
// ============================================================================

/**
 * Export a report as markdown
 *
 * @param report - The report to export
 * @returns Markdown string
 */
export function exportReportAsMarkdown(report: WeeklyReport): string {
  const lines: string[] = [];

  lines.push(`# Weekly Intelligence Report`);
  lines.push("");
  lines.push(`**Week**: ${report.week_start} to ${report.week_end}`);
  lines.push(`**Generated**: ${new Date(report.generated_at).toLocaleString()}`);
  lines.push("");

  // Summary
  lines.push("## Summary");
  lines.push("");
  lines.push("| Metric | This Week | Last Week | Trend |");
  lines.push("|--------|-----------|-----------|-------|");
  lines.push(
    `| Features | ${report.summary.features_completed} | ${report.summary.prev_features_completed} | ${getTrendIcon(report.summary.features_trend)} |`
  );
  lines.push(
    `| PRs Merged | ${report.summary.prs_merged} | ${report.summary.prev_prs_merged} | ${getTrendIcon(report.summary.prs_trend)} |`
  );
  lines.push(
    `| Cost | ${formatCurrency(report.summary.total_cost)} | ${formatCurrency(report.summary.prev_total_cost)} | ${getTrendIcon(report.summary.cost_trend)} |`
  );
  lines.push(
    `| Success Rate | ${formatPercent(report.summary.success_rate)} | ${formatPercent(report.summary.prev_success_rate)} | ${getTrendIcon(report.summary.success_trend)} |`
  );
  lines.push(
    `| Avg Cycle Time | ${formatCycleTime(report.summary.avg_cycle_time_hours)} | ${formatCycleTime(report.summary.prev_avg_cycle_time_hours)} | ${getTrendIcon(report.summary.cycle_time_trend)} |`
  );
  lines.push("");

  // Success Patterns
  if (report.success_patterns.length > 0) {
    lines.push("## What Worked Well");
    lines.push("");
    for (const pattern of report.success_patterns) {
      lines.push(`- **${pattern.description}**`);
      lines.push(`  - ${pattern.impact}`);
    }
    lines.push("");
  }

  // Improvement Areas
  if (report.improvement_areas.length > 0) {
    lines.push("## Areas for Improvement");
    lines.push("");
    for (const area of report.improvement_areas) {
      lines.push(`- **${area.description}**`);
      lines.push(`  - ${area.impact}`);
    }
    lines.push("");
  }

  // Anomalies
  if (report.anomalies.length > 0) {
    lines.push("## Alerts");
    lines.push("");
    for (const anomaly of report.anomalies) {
      const icon = anomaly.severity === "critical" ? "!!!" : "!";
      lines.push(`- ${icon} **${anomaly.message}**`);
      lines.push(`  - ${anomaly.details}`);
    }
    lines.push("");
  }

  // Recommendations
  if (report.recommendations.length > 0) {
    lines.push("## Recommendations");
    lines.push("");
    for (const rec of report.recommendations) {
      const priority =
        rec.priority === "high" ? "[HIGH]" : rec.priority === "medium" ? "[MED]" : "";
      lines.push(`### ${priority} ${rec.title}`);
      lines.push("");
      lines.push(rec.description);
      lines.push("");
      lines.push(`**Action**: ${rec.action}`);
      lines.push("");
    }
  }

  // Did You Know
  if (report.did_you_know.length > 0) {
    lines.push("## Did You Know?");
    lines.push("");
    for (const insight of report.did_you_know) {
      lines.push(`- ${insight.fact}`);
      lines.push(`  - _${insight.context}_`);
    }
    lines.push("");
  }

  // Role Metrics
  if (report.role_metrics.length > 0) {
    lines.push("## Performance by Role");
    lines.push("");
    lines.push("| Role | Prompts | Tokens | Cost | Success Rate |");
    lines.push("|------|---------|--------|------|--------------|");
    for (const role of report.role_metrics) {
      lines.push(
        `| ${role.role} | ${formatNumber(role.prompt_count)} | ${formatTokens(role.total_tokens)} | ${formatCurrency(role.total_cost)} | ${formatPercent(role.success_rate)} |`
      );
    }
    lines.push("");
  }

  lines.push("---");
  lines.push("_Generated by Loom Intelligence_");

  return lines.join("\n");
}

/**
 * Export a report as HTML
 *
 * @param report - The report to export
 * @returns HTML string
 */
export function exportReportAsHtml(report: WeeklyReport): string {
  // Convert markdown to basic HTML
  const markdown = exportReportAsMarkdown(report);

  // Simple markdown to HTML conversion
  const html = markdown
    .replace(/^### (.*$)/gm, "<h3>$1</h3>")
    .replace(/^## (.*$)/gm, "<h2>$1</h2>")
    .replace(/^# (.*$)/gm, "<h1>$1</h1>")
    .replace(/^\*\*(.*)\*\*$/gm, "<strong>$1</strong>")
    .replace(/\*\*(.*)\*\*/g, "<strong>$1</strong>")
    .replace(/\*(.*)\*/g, "<em>$1</em>")
    .replace(/^- (.*)$/gm, "<li>$1</li>")
    .replace(/^---$/gm, "<hr>")
    .replace(/\n\n/g, "</p><p>")
    .replace(/^\|(.+)\|$/gm, (match, content) => {
      const cells = content.split("|").map((c: string) => `<td>${c.trim()}</td>`);
      return `<tr>${cells.join("")}</tr>`;
    });

  // Wrap in basic HTML structure
  return `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Weekly Intelligence Report - ${report.week_start}</title>
  <style>
    body { font-family: system-ui, sans-serif; max-width: 800px; margin: 0 auto; padding: 20px; }
    table { border-collapse: collapse; width: 100%; margin: 16px 0; }
    td, th { border: 1px solid #ddd; padding: 8px; text-align: left; }
    h1, h2, h3 { color: #333; }
    li { margin: 8px 0; }
    hr { margin: 24px 0; border: 0; border-top: 1px solid #ddd; }
  </style>
</head>
<body>
  ${html}
</body>
</html>`;
}

// ============================================================================
// Utility Functions
// ============================================================================

/**
 * Generate a unique report ID
 */
function generateReportId(): string {
  return `report-${Date.now()}-${Math.random().toString(36).substring(2, 9)}`;
}

/**
 * Get the last Sunday before or on the given date
 */
function getLastSunday(date: Date): Date {
  const result = new Date(date);
  const day = result.getDay();
  result.setDate(result.getDate() - day);
  result.setHours(23, 59, 59, 999);
  return result;
}

/**
 * Get the last Monday before or on the given date
 */
function getLastMonday(date: Date): Date {
  const result = new Date(date);
  const day = result.getDay();
  const diff = day === 0 ? -6 : 1 - day;
  result.setDate(result.getDate() + diff);
  result.setHours(0, 0, 0, 0);
  return result;
}

/**
 * Format week range for display
 *
 * @param weekStart - Start date string (YYYY-MM-DD)
 * @param weekEnd - End date string (YYYY-MM-DD)
 * @returns Formatted range string
 */
export function formatWeekRange(weekStart: string, weekEnd: string): string {
  const start = new Date(weekStart);
  const end = new Date(weekEnd);
  const options: Intl.DateTimeFormatOptions = { month: "short", day: "numeric" };
  return `${start.toLocaleDateString(undefined, options)} - ${end.toLocaleDateString(undefined, options)}`;
}

/**
 * Get day name from day of week number
 *
 * @param dayOfWeek - Day of week (0=Sunday, 6=Saturday)
 * @returns Day name
 */
export function getDayName(dayOfWeek: number): string {
  const days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  return days[dayOfWeek] ?? "Unknown";
}

/**
 * Format hour for display
 *
 * @param hour - Hour of day (0-23)
 * @returns Formatted time string
 */
export function formatHour(hour: number): string {
  const period = hour >= 12 ? "PM" : "AM";
  const displayHour = hour % 12 || 12;
  return `${displayHour}:00 ${period}`;
}
