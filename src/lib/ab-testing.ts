/**
 * A/B Testing Module
 *
 * Provides functions for controlled experiments comparing different approaches
 * (prompts, roles, configurations) with statistical rigor.
 *
 * Part of Phase 4 (Advanced Analytics) - builds on velocity tracking (#1065)
 * and correlation analysis (#1066).
 *
 * @see Issue #1071 - Add A/B testing framework for approaches
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("ab-testing");

// ============================================================================
// Types
// ============================================================================

/**
 * Status of an experiment
 */
export type ExperimentStatus = "draft" | "active" | "concluded" | "cancelled";

/**
 * Target metric for an experiment
 */
export type TargetMetric = "success_rate" | "cycle_time" | "cost";

/**
 * Direction of improvement for the target metric
 */
export type TargetDirection = "higher" | "lower";

/**
 * Experiment definition
 */
export interface Experiment {
  /** Unique experiment ID */
  id: number | null;
  /** Unique experiment name */
  name: string;
  /** Description of what the experiment tests */
  description: string | null;
  /** Hypothesis being tested */
  hypothesis: string | null;
  /** Current status */
  status: ExperimentStatus;
  /** Creation timestamp */
  created_at: string;
  /** When the experiment was started */
  started_at: string | null;
  /** When the experiment was concluded */
  concluded_at: string | null;
  /** Minimum sample size per variant before analysis */
  min_sample_size: number;
  /** Metric being optimized */
  target_metric: TargetMetric;
  /** Whether higher or lower is better */
  target_direction: TargetDirection;
}

/**
 * Variant within an experiment
 */
export interface Variant {
  /** Unique variant ID */
  id: number | null;
  /** Parent experiment ID */
  experiment_id: number;
  /** Variant name (e.g., "Control", "Treatment A") */
  name: string;
  /** Description of this variant */
  description: string | null;
  /** JSON configuration for this variant */
  config_json: string | null;
  /** Weight for random assignment (default 1.0) */
  weight: number;
}

/**
 * Assignment of an issue/task to an experiment variant
 */
export interface Assignment {
  /** Unique assignment ID */
  id: number | null;
  /** Experiment ID */
  experiment_id: number;
  /** Assigned variant ID */
  variant_id: number;
  /** Issue number (if applicable) */
  issue_number: number | null;
  /** Terminal ID (if applicable) */
  terminal_id: string | null;
  /** When the assignment was made */
  assigned_at: string;
}

/**
 * Result recorded for an assignment
 */
export interface ExperimentResult {
  /** Unique result ID */
  id: number | null;
  /** Associated assignment ID */
  assignment_id: number;
  /** Linked success factor ID (from correlation analysis) */
  success_factor_id: number | null;
  /** Outcome of the task */
  outcome: "success" | "failure" | "partial" | "cancelled";
  /** Metric value (e.g., cycle time in hours) */
  metric_value: number | null;
  /** When the result was recorded */
  recorded_at: string;
}

/**
 * Statistics for a single variant
 */
export interface VariantStats {
  /** Variant name */
  variant_name: string;
  /** Variant ID */
  variant_id: number;
  /** Number of completed trials */
  sample_size: number;
  /** Success rate (0-1) */
  success_rate: number;
  /** Average metric value */
  avg_metric_value: number | null;
  /** Standard deviation of metric */
  std_dev: number | null;
  /** 95% confidence interval lower bound */
  ci_lower: number | null;
  /** 95% confidence interval upper bound */
  ci_upper: number | null;
}

/**
 * Analysis result for an experiment
 */
export interface ExperimentAnalysis {
  /** Experiment ID */
  experiment_id: number;
  /** Winning variant name (or null if no clear winner) */
  winner: string | null;
  /** Winning variant ID */
  winner_variant_id: number | null;
  /** Statistical confidence (0-1) */
  confidence: number;
  /** P-value from significance test */
  p_value: number;
  /** Effect size (Cohen's d for continuous, odds ratio for binary) */
  effect_size: number;
  /** Statistics per variant */
  stats_per_variant: VariantStats[];
  /** Human-readable recommendation */
  recommendation: string;
  /** Whether the experiment should be concluded */
  should_conclude: boolean;
  /** Analysis timestamp */
  analysis_date: string;
}

/**
 * Summary of all experiments
 */
export interface ExperimentsSummary {
  /** Total number of experiments */
  total_experiments: number;
  /** Number of active experiments */
  active_experiments: number;
  /** Number of concluded experiments */
  concluded_experiments: number;
  /** Total assignments made */
  total_assignments: number;
  /** Total results recorded */
  total_results: number;
}

// ============================================================================
// Experiment Lifecycle Functions
// ============================================================================

/**
 * Create a new experiment
 *
 * @param workspacePath - Path to the workspace
 * @param experiment - Experiment definition
 * @returns ID of the created experiment
 */
export async function createExperiment(
  workspacePath: string,
  experiment: Omit<Experiment, "id" | "created_at" | "started_at" | "concluded_at">
): Promise<number> {
  try {
    const id = await invoke<number>("create_experiment", {
      workspacePath,
      name: experiment.name,
      description: experiment.description,
      hypothesis: experiment.hypothesis,
      status: experiment.status,
      minSampleSize: experiment.min_sample_size,
      targetMetric: experiment.target_metric,
      targetDirection: experiment.target_direction,
    });
    logger.info("Experiment created", { id, name: experiment.name });
    return id;
  } catch (error) {
    logger.error("Failed to create experiment", error as Error, { name: experiment.name });
    throw error;
  }
}

/**
 * Add a variant to an experiment
 *
 * @param workspacePath - Path to the workspace
 * @param variant - Variant definition
 * @returns ID of the created variant
 */
export async function addVariant(
  workspacePath: string,
  variant: Omit<Variant, "id">
): Promise<number> {
  try {
    const id = await invoke<number>("add_experiment_variant", {
      workspacePath,
      experimentId: variant.experiment_id,
      name: variant.name,
      description: variant.description,
      configJson: variant.config_json,
      weight: variant.weight,
    });
    logger.info("Variant added", { id, name: variant.name, experimentId: variant.experiment_id });
    return id;
  } catch (error) {
    logger.error("Failed to add variant", error as Error, { name: variant.name });
    throw error;
  }
}

/**
 * Start an experiment (change status from draft to active)
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment to start
 */
export async function startExperiment(workspacePath: string, experimentId: number): Promise<void> {
  try {
    await invoke("start_experiment", { workspacePath, experimentId });
    logger.info("Experiment started", { experimentId });
  } catch (error) {
    logger.error("Failed to start experiment", error as Error, { experimentId });
    throw error;
  }
}

/**
 * Conclude an experiment
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment to conclude
 * @param winnerVariantId - Optional ID of the winning variant
 */
export async function concludeExperiment(
  workspacePath: string,
  experimentId: number,
  winnerVariantId?: number
): Promise<void> {
  try {
    await invoke("conclude_experiment", {
      workspacePath,
      experimentId,
      winnerVariantId: winnerVariantId ?? null,
    });
    logger.info("Experiment concluded", { experimentId, winnerVariantId });
  } catch (error) {
    logger.error("Failed to conclude experiment", error as Error, { experimentId });
    throw error;
  }
}

/**
 * Cancel an experiment
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment to cancel
 */
export async function cancelExperiment(workspacePath: string, experimentId: number): Promise<void> {
  try {
    await invoke("cancel_experiment", { workspacePath, experimentId });
    logger.info("Experiment cancelled", { experimentId });
  } catch (error) {
    logger.error("Failed to cancel experiment", error as Error, { experimentId });
    throw error;
  }
}

// ============================================================================
// Assignment Functions
// ============================================================================

/**
 * Get or create a variant assignment for an issue
 *
 * This function ensures consistent assignment - the same issue will always
 * get the same variant for a given experiment.
 *
 * @param workspacePath - Path to the workspace
 * @param experimentName - Name of the experiment
 * @param issueNumber - Issue number to assign
 * @returns The assigned variant
 */
export async function assignVariant(
  workspacePath: string,
  experimentName: string,
  issueNumber: number
): Promise<Variant> {
  try {
    const variant = await invoke<Variant>("assign_experiment_variant", {
      workspacePath,
      experimentName,
      issueNumber,
      terminalId: null,
    });
    logger.info("Variant assigned", {
      experimentName,
      issueNumber,
      variantName: variant.name,
    });
    return variant;
  } catch (error) {
    logger.error("Failed to assign variant", error as Error, { experimentName, issueNumber });
    throw error;
  }
}

/**
 * Get or create a variant assignment for a terminal
 *
 * @param workspacePath - Path to the workspace
 * @param experimentName - Name of the experiment
 * @param terminalId - Terminal ID to assign
 * @returns The assigned variant
 */
export async function assignVariantToTerminal(
  workspacePath: string,
  experimentName: string,
  terminalId: string
): Promise<Variant> {
  try {
    const variant = await invoke<Variant>("assign_experiment_variant", {
      workspacePath,
      experimentName,
      issueNumber: null,
      terminalId,
    });
    logger.info("Variant assigned to terminal", {
      experimentName,
      terminalId,
      variantName: variant.name,
    });
    return variant;
  } catch (error) {
    logger.error("Failed to assign variant to terminal", error as Error, {
      experimentName,
      terminalId,
    });
    throw error;
  }
}

/**
 * Get the current assignment for an issue (if any)
 *
 * @param workspacePath - Path to the workspace
 * @param experimentName - Name of the experiment
 * @param issueNumber - Issue number
 * @returns The assignment or null if not assigned
 */
export async function getAssignment(
  workspacePath: string,
  experimentName: string,
  issueNumber: number
): Promise<Assignment | null> {
  try {
    return await invoke<Assignment | null>("get_experiment_assignment", {
      workspacePath,
      experimentName,
      issueNumber,
    });
  } catch (error) {
    logger.error("Failed to get assignment", error as Error, { experimentName, issueNumber });
    return null;
  }
}

// ============================================================================
// Result Recording Functions
// ============================================================================

/**
 * Record a result for an experiment assignment
 *
 * @param workspacePath - Path to the workspace
 * @param experimentName - Name of the experiment
 * @param issueNumber - Issue number
 * @param outcome - Outcome of the task
 * @param metricValue - Optional metric value (e.g., cycle time)
 */
export async function recordResult(
  workspacePath: string,
  experimentName: string,
  issueNumber: number,
  outcome: "success" | "failure" | "partial" | "cancelled",
  metricValue?: number
): Promise<void> {
  try {
    await invoke("record_experiment_result", {
      workspacePath,
      experimentName,
      issueNumber,
      outcome,
      metricValue: metricValue ?? null,
    });
    logger.info("Result recorded", { experimentName, issueNumber, outcome, metricValue });
  } catch (error) {
    logger.error("Failed to record result", error as Error, {
      experimentName,
      issueNumber,
      outcome,
    });
    throw error;
  }
}

/**
 * Record a result with a link to a success factor
 *
 * @param workspacePath - Path to the workspace
 * @param experimentName - Name of the experiment
 * @param issueNumber - Issue number
 * @param outcome - Outcome of the task
 * @param successFactorId - ID of the linked success factor
 * @param metricValue - Optional metric value
 */
export async function recordResultWithFactor(
  workspacePath: string,
  experimentName: string,
  issueNumber: number,
  outcome: "success" | "failure" | "partial" | "cancelled",
  successFactorId: number,
  metricValue?: number
): Promise<void> {
  try {
    await invoke("record_experiment_result_with_factor", {
      workspacePath,
      experimentName,
      issueNumber,
      outcome,
      successFactorId,
      metricValue: metricValue ?? null,
    });
    logger.info("Result recorded with factor", {
      experimentName,
      issueNumber,
      outcome,
      successFactorId,
    });
  } catch (error) {
    logger.error("Failed to record result with factor", error as Error, {
      experimentName,
      issueNumber,
    });
    throw error;
  }
}

// ============================================================================
// Analysis Functions
// ============================================================================

/**
 * Analyze an experiment and get statistical results
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment to analyze
 * @returns Analysis results
 */
export async function analyzeExperiment(
  workspacePath: string,
  experimentId: number
): Promise<ExperimentAnalysis> {
  try {
    return await invoke<ExperimentAnalysis>("analyze_experiment", {
      workspacePath,
      experimentId,
    });
  } catch (error) {
    logger.error("Failed to analyze experiment", error as Error, { experimentId });
    throw error;
  }
}

/**
 * Analyze an experiment by name
 *
 * @param workspacePath - Path to the workspace
 * @param experimentName - Name of the experiment
 * @returns Analysis results
 */
export async function analyzeExperimentByName(
  workspacePath: string,
  experimentName: string
): Promise<ExperimentAnalysis> {
  try {
    return await invoke<ExperimentAnalysis>("analyze_experiment_by_name", {
      workspacePath,
      experimentName,
    });
  } catch (error) {
    logger.error("Failed to analyze experiment by name", error as Error, { experimentName });
    throw error;
  }
}

/**
 * Check if any active experiments should be concluded
 *
 * @param workspacePath - Path to the workspace
 * @returns List of experiment IDs that should be concluded
 */
export async function checkExperimentsForConclusion(workspacePath: string): Promise<number[]> {
  try {
    return await invoke<number[]>("check_experiments_for_conclusion", { workspacePath });
  } catch (error) {
    logger.error("Failed to check experiments for conclusion", error as Error);
    return [];
  }
}

// ============================================================================
// Query Functions
// ============================================================================

/**
 * Get all experiments
 *
 * @param workspacePath - Path to the workspace
 * @param status - Optional status filter
 * @returns List of experiments
 */
export async function getExperiments(
  workspacePath: string,
  status?: ExperimentStatus
): Promise<Experiment[]> {
  try {
    return await invoke<Experiment[]>("get_experiments", {
      workspacePath,
      status: status ?? null,
    });
  } catch (error) {
    logger.error("Failed to get experiments", error as Error, { status });
    return [];
  }
}

/**
 * Get active experiments
 *
 * @param workspacePath - Path to the workspace
 * @returns List of active experiments
 */
export async function getActiveExperiments(workspacePath: string): Promise<Experiment[]> {
  return getExperiments(workspacePath, "active");
}

/**
 * Get an experiment by ID
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment
 * @returns The experiment or null
 */
export async function getExperiment(
  workspacePath: string,
  experimentId: number
): Promise<Experiment | null> {
  try {
    return await invoke<Experiment | null>("get_experiment", {
      workspacePath,
      experimentId,
    });
  } catch (error) {
    logger.error("Failed to get experiment", error as Error, { experimentId });
    return null;
  }
}

/**
 * Get an experiment by name
 *
 * @param workspacePath - Path to the workspace
 * @param name - Name of the experiment
 * @returns The experiment or null
 */
export async function getExperimentByName(
  workspacePath: string,
  name: string
): Promise<Experiment | null> {
  try {
    return await invoke<Experiment | null>("get_experiment_by_name", {
      workspacePath,
      name,
    });
  } catch (error) {
    logger.error("Failed to get experiment by name", error as Error, { name });
    return null;
  }
}

/**
 * Get variants for an experiment
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment
 * @returns List of variants
 */
export async function getVariants(workspacePath: string, experimentId: number): Promise<Variant[]> {
  try {
    return await invoke<Variant[]>("get_experiment_variants", {
      workspacePath,
      experimentId,
    });
  } catch (error) {
    logger.error("Failed to get variants", error as Error, { experimentId });
    return [];
  }
}

/**
 * Get summary of all experiments
 *
 * @param workspacePath - Path to the workspace
 * @returns Experiments summary
 */
export async function getExperimentsSummary(workspacePath: string): Promise<ExperimentsSummary> {
  try {
    return await invoke<ExperimentsSummary>("get_experiments_summary", { workspacePath });
  } catch (error) {
    logger.error("Failed to get experiments summary", error as Error);
    return {
      total_experiments: 0,
      active_experiments: 0,
      concluded_experiments: 0,
      total_assignments: 0,
      total_results: 0,
    };
  }
}

/**
 * Get results for an experiment
 *
 * @param workspacePath - Path to the workspace
 * @param experimentId - ID of the experiment
 * @returns List of results
 */
export async function getExperimentResults(
  workspacePath: string,
  experimentId: number
): Promise<ExperimentResult[]> {
  try {
    return await invoke<ExperimentResult[]>("get_experiment_results", {
      workspacePath,
      experimentId,
    });
  } catch (error) {
    logger.error("Failed to get experiment results", error as Error, { experimentId });
    return [];
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format an experiment status for display
 */
export function formatStatus(status: ExperimentStatus): string {
  switch (status) {
    case "draft":
      return "Draft";
    case "active":
      return "Active";
    case "concluded":
      return "Concluded";
    case "cancelled":
      return "Cancelled";
  }
}

/**
 * Get a color class for an experiment status
 */
export function getStatusColor(status: ExperimentStatus): string {
  switch (status) {
    case "draft":
      return "text-gray-600 dark:text-gray-400";
    case "active":
      return "text-blue-600 dark:text-blue-400";
    case "concluded":
      return "text-green-600 dark:text-green-400";
    case "cancelled":
      return "text-red-600 dark:text-red-400";
  }
}

/**
 * Format a p-value for display
 */
export function formatPValue(pValue: number): string {
  if (pValue < 0.001) return "< 0.001";
  if (pValue < 0.01) return "< 0.01";
  if (pValue < 0.05) return "< 0.05";
  return pValue.toFixed(3);
}

/**
 * Format confidence as percentage
 */
export function formatConfidence(confidence: number): string {
  return `${(confidence * 100).toFixed(1)}%`;
}

/**
 * Format effect size with interpretation
 */
export function formatEffectSize(effectSize: number): string {
  const abs = Math.abs(effectSize);
  let interpretation: string;
  if (abs >= 0.8) interpretation = "large";
  else if (abs >= 0.5) interpretation = "medium";
  else if (abs >= 0.2) interpretation = "small";
  else interpretation = "negligible";

  const sign = effectSize >= 0 ? "+" : "";
  return `${sign}${effectSize.toFixed(2)} (${interpretation})`;
}

/**
 * Get color class for p-value significance
 */
export function getPValueColor(pValue: number): string {
  if (pValue < 0.01) return "text-green-600 dark:text-green-400"; // Highly significant
  if (pValue < 0.05) return "text-yellow-600 dark:text-yellow-400"; // Significant
  return "text-gray-600 dark:text-gray-400"; // Not significant
}

/**
 * Format a success rate as percentage
 */
export function formatSuccessRate(rate: number): string {
  return `${(rate * 100).toFixed(1)}%`;
}

/**
 * Format a metric value based on the target metric type
 */
export function formatMetricValue(value: number | null, metric: TargetMetric): string {
  if (value === null) return "-";

  switch (metric) {
    case "success_rate":
      return `${(value * 100).toFixed(1)}%`;
    case "cycle_time":
      if (value < 1) return `${Math.round(value * 60)}m`;
      if (value < 24) return `${value.toFixed(1)}h`;
      return `${(value / 24).toFixed(1)}d`;
    case "cost":
      return `$${value.toFixed(2)}`;
  }
}

/**
 * Interpret whether a result is significant
 */
export function isSignificant(pValue: number, alpha: number = 0.05): boolean {
  return pValue < alpha;
}

/**
 * Get a human-readable label for the target metric
 */
export function getMetricLabel(metric: TargetMetric): string {
  switch (metric) {
    case "success_rate":
      return "Success Rate";
    case "cycle_time":
      return "Cycle Time";
    case "cost":
      return "Cost";
  }
}

/**
 * Get the better direction indicator for a metric
 */
export function getDirectionIndicator(direction: TargetDirection): string {
  return direction === "higher" ? "\u2191 Higher is better" : "\u2193 Lower is better";
}
