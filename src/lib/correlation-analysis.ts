/**
 * Correlation Analysis Module
 *
 * Provides functions to analyze success correlations in agent activity.
 * Part of Phase 3 (Intelligence & Learning) - uses Phase 2 correlation data.
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("correlation-analysis");

// ============================================================================
// Types
// ============================================================================

/**
 * Correlation result between two factors
 */
export interface CorrelationResult {
  id: number | null;
  factor_a: string;
  factor_b: string;
  correlation_coefficient: number;
  p_value: number;
  sample_size: number;
  analysis_date: string;
  notes: string | null;
}

/**
 * Success factor entry for an agent input
 */
export interface SuccessFactor {
  id: number | null;
  input_id: number;
  prompt_length: number | null;
  hour_of_day: number | null;
  day_of_week: number | null;
  has_tests_first: boolean | null;
  review_cycles: number | null;
  outcome: "success" | "failure" | "partial" | "no_work" | "unknown";
}

/**
 * Success rate breakdown by a specific factor
 */
export interface SuccessRateByFactor {
  factor_name: string;
  factor_value: string;
  total_count: number;
  success_count: number;
  success_rate: number;
}

/**
 * Insight derived from correlation analysis
 */
export interface CorrelationInsight {
  factor: string;
  insight: string;
  correlation_strength: "strong" | "moderate" | "weak";
  recommendation: string;
}

/**
 * Summary of correlation analysis
 */
export interface CorrelationSummary {
  total_samples: number;
  success_rate: number;
  significant_correlations: number;
  top_insights: CorrelationInsight[];
}

// ============================================================================
// Data Extraction Functions
// ============================================================================

/**
 * Extract success factors from historical activity data
 *
 * @param workspacePath - Path to the workspace
 * @returns Number of new factors extracted
 */
export async function extractSuccessFactors(workspacePath: string): Promise<number> {
  try {
    return await invoke<number>("extract_success_factors", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to extract success factors", error as Error);
    return 0;
  }
}

// ============================================================================
// Correlation Analysis Functions
// ============================================================================

/**
 * Analyze correlation between hour of day and success rate
 *
 * @param workspacePath - Path to the workspace
 * @returns Correlation result
 */
export async function analyzeHourSuccessCorrelation(
  workspacePath: string
): Promise<CorrelationResult> {
  try {
    return await invoke<CorrelationResult>("analyze_hour_success_correlation", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to analyze hour-success correlation", error as Error);
    return {
      id: null,
      factor_a: "hour_of_day",
      factor_b: "success",
      correlation_coefficient: 0,
      p_value: 1,
      sample_size: 0,
      analysis_date: new Date().toISOString(),
      notes: "Analysis failed",
    };
  }
}

/**
 * Analyze success rate by role
 *
 * @param workspacePath - Path to the workspace
 * @returns Success rates for each role
 */
export async function analyzeRoleSuccessCorrelation(
  workspacePath: string
): Promise<SuccessRateByFactor[]> {
  try {
    return await invoke<SuccessRateByFactor[]>("analyze_role_success_correlation", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to analyze role-success correlation", error as Error);
    return [];
  }
}

/**
 * Analyze success rate by time of day buckets (morning, afternoon, evening, night)
 *
 * @param workspacePath - Path to the workspace
 * @returns Success rates for each time bucket
 */
export async function analyzeTimeOfDaySuccess(
  workspacePath: string
): Promise<SuccessRateByFactor[]> {
  try {
    return await invoke<SuccessRateByFactor[]>("analyze_time_of_day_success", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to analyze time-of-day success", error as Error);
    return [];
  }
}

/**
 * Analyze success rate by day of week
 *
 * @param workspacePath - Path to the workspace
 * @returns Success rates for each day
 */
export async function analyzeDayOfWeekSuccess(
  workspacePath: string
): Promise<SuccessRateByFactor[]> {
  try {
    return await invoke<SuccessRateByFactor[]>("analyze_day_of_week_success", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to analyze day-of-week success", error as Error);
    return [];
  }
}

// ============================================================================
// Query Functions
// ============================================================================

/**
 * Get stored correlation results with optional significance filter
 *
 * @param workspacePath - Path to the workspace
 * @param minSignificance - Optional p-value threshold (e.g., 0.05 for 95% confidence)
 * @returns Array of correlation results
 */
export async function getCorrelations(
  workspacePath: string,
  minSignificance?: number
): Promise<CorrelationResult[]> {
  try {
    return await invoke<CorrelationResult[]>("get_correlations", {
      workspacePath,
      minSignificance,
    });
  } catch (error) {
    logger.error("Failed to get correlations", error as Error);
    return [];
  }
}

/**
 * Get success factors for a specific role
 *
 * @param workspacePath - Path to the workspace
 * @param role - Role name (e.g., "builder", "judge")
 * @returns Array of success factors
 */
export async function getSuccessFactorsForRole(
  workspacePath: string,
  role: string
): Promise<SuccessFactor[]> {
  try {
    return await invoke<SuccessFactor[]>("get_success_factors_for_role", {
      workspacePath,
      role,
    });
  } catch (error) {
    logger.error("Failed to get success factors for role", error as Error, { role });
    return [];
  }
}

/**
 * Run a comprehensive correlation analysis and get a summary with insights
 *
 * @param workspacePath - Path to the workspace
 * @returns Analysis summary with insights
 */
export async function runCorrelationAnalysis(
  workspacePath: string
): Promise<CorrelationSummary> {
  try {
    return await invoke<CorrelationSummary>("run_correlation_analysis", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to run correlation analysis", error as Error);
    return {
      total_samples: 0,
      success_rate: 0,
      significant_correlations: 0,
      top_insights: [],
    };
  }
}

/**
 * Predict success likelihood based on input features
 *
 * @param workspacePath - Path to the workspace
 * @param hourOfDay - Hour of day (0-23)
 * @param dayOfWeek - Day of week (0=Sunday, 6=Saturday)
 * @param role - Agent role
 * @returns Predicted success probability (0-1)
 */
export async function predictSuccess(
  workspacePath: string,
  options?: {
    hourOfDay?: number;
    dayOfWeek?: number;
    role?: string;
  }
): Promise<number> {
  try {
    return await invoke<number>("predict_success", {
      workspacePath,
      hourOfDay: options?.hourOfDay ?? null,
      dayOfWeek: options?.dayOfWeek ?? null,
      role: options?.role ?? null,
    });
  } catch (error) {
    logger.error("Failed to predict success", error as Error);
    return 0.5; // Return neutral prediction on error
  }
}

/**
 * Log a success factor entry manually
 *
 * @param workspacePath - Path to the workspace
 * @param entry - Success factor entry to log
 * @returns ID of the logged entry
 */
export async function logSuccessFactor(
  workspacePath: string,
  entry: Omit<SuccessFactor, "id">
): Promise<number> {
  try {
    return await invoke<number>("log_success_factor", {
      workspacePath,
      inputId: entry.input_id,
      promptLength: entry.prompt_length,
      hourOfDay: entry.hour_of_day,
      dayOfWeek: entry.day_of_week,
      hasTestsFirst: entry.has_tests_first,
      reviewCycles: entry.review_cycles,
      outcome: entry.outcome,
    });
  } catch (error) {
    logger.error("Failed to log success factor", error as Error);
    return -1;
  }
}

/**
 * Clear all stored correlation results
 *
 * @param workspacePath - Path to the workspace
 * @returns Number of results deleted
 */
export async function clearCorrelationResults(workspacePath: string): Promise<number> {
  try {
    return await invoke<number>("clear_correlation_results", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to clear correlation results", error as Error);
    return 0;
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format a correlation coefficient for display
 */
export function formatCorrelation(coefficient: number): string {
  const sign = coefficient >= 0 ? "+" : "";
  return `${sign}${coefficient.toFixed(3)}`;
}

/**
 * Format a p-value for display
 */
export function formatPValue(pValue: number): string {
  if (pValue < 0.001) return "< 0.001";
  if (pValue < 0.01) return `< 0.01`;
  if (pValue < 0.05) return `< 0.05`;
  return pValue.toFixed(3);
}

/**
 * Interpret correlation strength
 */
export function interpretCorrelation(coefficient: number): string {
  const abs = Math.abs(coefficient);
  if (abs >= 0.7) return "strong";
  if (abs >= 0.4) return "moderate";
  if (abs >= 0.2) return "weak";
  return "negligible";
}

/**
 * Check if a correlation is statistically significant
 */
export function isSignificant(pValue: number, alpha: number = 0.05): boolean {
  return pValue < alpha;
}

/**
 * Get display name for a day of week
 */
export function getDayName(dayOfWeek: number): string {
  const days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  return days[dayOfWeek] ?? "Unknown";
}

/**
 * Get display name for a time bucket
 */
export function getTimeBucketName(hour: number): string {
  if (hour >= 6 && hour < 12) return "Morning";
  if (hour >= 12 && hour < 18) return "Afternoon";
  if (hour >= 18 && hour < 22) return "Evening";
  return "Night";
}

/**
 * Get a color class based on correlation strength
 */
export function getCorrelationColorClass(coefficient: number): string {
  const abs = Math.abs(coefficient);
  if (abs >= 0.7) return coefficient > 0 ? "text-green-600" : "text-red-600";
  if (abs >= 0.4) return coefficient > 0 ? "text-green-500" : "text-red-500";
  if (abs >= 0.2) return coefficient > 0 ? "text-green-400" : "text-red-400";
  return "text-gray-500";
}

/**
 * Get a color class based on success rate
 */
export function getSuccessRateColorClass(rate: number): string {
  if (rate >= 0.8) return "text-green-600 dark:text-green-400";
  if (rate >= 0.6) return "text-yellow-600 dark:text-yellow-400";
  if (rate >= 0.4) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}
