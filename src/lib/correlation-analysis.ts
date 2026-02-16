/**
 * Correlation Analysis Module
 *
 * TypeScript wrapper for the Rust correlation analysis backend.
 * Provides success factor extraction, correlation analysis by time/role,
 * and statistical significance testing.
 *
 * Part of Phase 3 (Intelligence & Learning) - Issue #2262
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("correlation-analysis");

// ============================================================================
// Types
// ============================================================================

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

export interface SuccessFactor {
  id: number | null;
  input_id: number;
  prompt_length: number | null;
  hour_of_day: number | null;
  day_of_week: number | null;
  has_tests_first: boolean | null;
  review_cycles: number | null;
  outcome: string;
}

export interface SuccessRateByFactor {
  factor_name: string;
  factor_value: string;
  total_count: number;
  success_count: number;
  success_rate: number;
}

export interface CorrelationInsight {
  factor: string;
  insight: string;
  correlation_strength: string;
  recommendation: string;
}

export interface CorrelationSummary {
  total_samples: number;
  success_rate: number;
  significant_correlations: number;
  top_insights: CorrelationInsight[];
}

// ============================================================================
// Analysis Functions
// ============================================================================

/**
 * Extract success factors from activity data
 */
export async function extractSuccessFactors(workspacePath: string): Promise<number> {
  try {
    return await invoke<number>("extract_success_factors", { workspacePath });
  } catch (error) {
    logger.error("Failed to extract success factors", error as Error);
    return 0;
  }
}

/**
 * Run full correlation analysis and get summary with insights
 */
export async function runCorrelationAnalysis(workspacePath: string): Promise<CorrelationSummary> {
  try {
    return await invoke<CorrelationSummary>("run_correlation_analysis", { workspacePath });
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
 * Analyze success rates by hour of day
 */
export async function analyzeHourSuccessCorrelation(
  workspacePath: string
): Promise<CorrelationResult> {
  return await invoke<CorrelationResult>("analyze_hour_success_correlation", { workspacePath });
}

/**
 * Analyze success rates by role
 */
export async function analyzeRoleSuccessCorrelation(
  workspacePath: string
): Promise<SuccessRateByFactor[]> {
  try {
    return await invoke<SuccessRateByFactor[]>("analyze_role_success_correlation", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to analyze role success correlation", error as Error);
    return [];
  }
}

/**
 * Analyze success rates by time of day (morning/afternoon/evening/night)
 */
export async function analyzeTimeOfDaySuccess(
  workspacePath: string
): Promise<SuccessRateByFactor[]> {
  try {
    return await invoke<SuccessRateByFactor[]>("analyze_time_of_day_success", { workspacePath });
  } catch (error) {
    logger.error("Failed to analyze time of day success", error as Error);
    return [];
  }
}

/**
 * Analyze success rates by day of week
 */
export async function analyzeDayOfWeekSuccess(
  workspacePath: string
): Promise<SuccessRateByFactor[]> {
  try {
    return await invoke<SuccessRateByFactor[]>("analyze_day_of_week_success", { workspacePath });
  } catch (error) {
    logger.error("Failed to analyze day of week success", error as Error);
    return [];
  }
}

/**
 * Get stored correlations with optional significance filter
 */
export async function getCorrelations(
  workspacePath: string,
  minSignificance?: number
): Promise<CorrelationResult[]> {
  try {
    return await invoke<CorrelationResult[]>("get_correlations", {
      workspacePath,
      minSignificance: minSignificance ?? null,
    });
  } catch (error) {
    logger.error("Failed to get correlations", error as Error);
    return [];
  }
}

/**
 * Get success factors for a specific role
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
    logger.error("Failed to get success factors for role", error as Error);
    return [];
  }
}

/**
 * Predict success probability based on conditions
 */
export async function predictSuccess(
  workspacePath: string,
  hourOfDay?: number,
  dayOfWeek?: number,
  role?: string
): Promise<number> {
  try {
    return await invoke<number>("predict_success", {
      workspacePath,
      hourOfDay: hourOfDay ?? null,
      dayOfWeek: dayOfWeek ?? null,
      role: role ?? null,
    });
  } catch (error) {
    logger.error("Failed to predict success", error as Error);
    return 0.5;
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format correlation coefficient for display
 */
export function formatCorrelation(coefficient: number): string {
  const sign = coefficient >= 0 ? "+" : "";
  return `${sign}${coefficient.toFixed(3)}`;
}

/**
 * Get correlation strength label
 */
export function getCorrelationStrength(coefficient: number): string {
  const abs = Math.abs(coefficient);
  if (abs >= 0.7) return "Strong";
  if (abs >= 0.4) return "Moderate";
  if (abs >= 0.2) return "Weak";
  return "Negligible";
}

/**
 * Get color class based on correlation strength
 */
export function getCorrelationColorClass(coefficient: number): string {
  const abs = Math.abs(coefficient);
  if (abs >= 0.7) return "text-green-600 dark:text-green-400";
  if (abs >= 0.4) return "text-yellow-600 dark:text-yellow-400";
  if (abs >= 0.2) return "text-orange-600 dark:text-orange-400";
  return "text-gray-500 dark:text-gray-400";
}

/**
 * Get color class based on success rate
 */
export function getSuccessRateColorClass(rate: number): string {
  if (rate >= 0.8) return "text-green-600 dark:text-green-400";
  if (rate >= 0.6) return "text-yellow-600 dark:text-yellow-400";
  if (rate >= 0.4) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Format p-value for display
 */
export function formatPValue(pValue: number): string {
  if (pValue < 0.001) return "< 0.001";
  if (pValue < 0.01) return `< 0.01`;
  return pValue.toFixed(3);
}

/**
 * Check if a correlation is statistically significant
 */
export function isSignificant(pValue: number, threshold: number = 0.05): boolean {
  return pValue < threshold;
}

/**
 * Get day of week name from number (0 = Sunday)
 */
export function getDayName(dayOfWeek: number): string {
  const days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  return days[dayOfWeek] ?? `Day ${dayOfWeek}`;
}

/**
 * Get time of day label from hour
 */
export function getTimeOfDayLabel(hour: number): string {
  if (hour < 6) return "Night";
  if (hour < 12) return "Morning";
  if (hour < 18) return "Afternoon";
  return "Evening";
}
