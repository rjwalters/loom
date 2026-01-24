/**
 * Self-Tuning Module
 *
 * Provides functions for automatic parameter adjustment based on effectiveness data.
 * Implements safety rails including gradual changes, human approval requirements,
 * and automatic rollback on degradation.
 *
 * @see Issue #1074 - Implement self-tuning based on effectiveness data
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("self-tuning");

// ============================================================================
// Types
// ============================================================================

/**
 * Status of a tuning proposal
 */
export type ProposalStatus = "pending" | "approved" | "rejected" | "applied" | "rolled_back";

/**
 * A tunable system parameter
 */
export interface TunableParameter {
  /** Unique parameter identifier */
  name: string;
  /** Human-readable description */
  description: string;
  /** Current value */
  currentValue: number;
  /** Default value (used for reset) */
  defaultValue: number;
  /** Minimum allowed value */
  minValue: number;
  /** Maximum allowed value */
  maxValue: number;
  /** Unit of measurement (e.g., "ms", "percent", "count") */
  unit: string;
  /** Whether this parameter can be auto-tuned */
  autoTunable: boolean;
  /** Last modified timestamp */
  updatedAt: string;
}

/**
 * A proposed parameter adjustment
 */
export interface TuningProposal {
  /** Unique proposal ID */
  id: number | null;
  /** Parameter being tuned */
  parameterName: string;
  /** Value before proposed change */
  oldValue: number;
  /** Proposed new value */
  newValue: number;
  /** Percentage change */
  changePercent: number;
  /** Reason for the proposal */
  reason: string;
  /** Evidence/metrics supporting the proposal */
  evidence: string;
  /** Current status */
  status: ProposalStatus;
  /** Confidence score (0.0-1.0) */
  confidence: number;
  /** Whether human approval is required */
  requiresApproval: boolean;
  /** Created timestamp */
  createdAt: string;
  /** Applied timestamp (if applied) */
  appliedAt: string | null;
  /** ID of the proposal that rolled back this one (if rolled back) */
  rollbackProposalId: number | null;
}

/**
 * Record of a parameter value change
 */
export interface TuningHistory {
  /** Unique history entry ID */
  id: number | null;
  /** Parameter that was changed */
  parameterName: string;
  /** Value before change */
  oldValue: number;
  /** Value after change */
  newValue: number;
  /** Associated proposal ID (if any) */
  proposalId: number | null;
  /** Who/what made the change */
  changedBy: string;
  /** Reason for the change */
  reason: string;
  /** Timestamp of the change */
  timestamp: string;
}

/**
 * Effectiveness metrics snapshot for tuning decisions
 */
export interface EffectivenessSnapshot {
  /** Snapshot timestamp */
  timestamp: string;
  /** Overall success rate (0.0-1.0) */
  successRate: number;
  /** Average cycle time in hours */
  avgCycleTimeHours: number;
  /** Average cost per task in USD */
  avgCostPerTask: number;
  /** Number of tasks completed */
  tasksCompleted: number;
  /** Number of PRs merged */
  prsMerged: number;
  /** Average rework cycles per PR */
  avgReworkCount: number;
}

/**
 * Configuration for the tuning engine
 */
export interface TuningConfig {
  /** Maximum adjustment percentage per cycle (default: 10%) */
  maxAdjustmentPercent: number;
  /** Cumulative change threshold requiring human approval (default: 20%) */
  approvalThresholdPercent: number;
  /** Minimum sample size before making adjustments */
  minSampleSize: number;
  /** Minimum confidence level for auto-approval (default: 0.8) */
  minAutoApprovalConfidence: number;
  /** Degradation threshold for automatic rollback (default: 15%) */
  rollbackThresholdPercent: number;
  /** Observation period after applying changes (hours) */
  observationPeriodHours: number;
}

/**
 * Summary of tuning activity
 */
export interface TuningSummary {
  /** Total parameters being tracked */
  totalParameters: number;
  /** Parameters that are auto-tunable */
  autoTunableCount: number;
  /** Pending proposals awaiting approval */
  pendingProposals: number;
  /** Total proposals applied */
  appliedProposals: number;
  /** Total rollbacks performed */
  rollbacksCount: number;
  /** Average effectiveness improvement (%) */
  avgImprovementPercent: number;
  /** Last tuning cycle timestamp */
  lastTuningCycle: string | null;
}

// ============================================================================
// Default Configuration
// ============================================================================

/**
 * Default tuning configuration with conservative safety settings
 */
export const DEFAULT_TUNING_CONFIG: TuningConfig = {
  maxAdjustmentPercent: 10.0,
  approvalThresholdPercent: 20.0,
  minSampleSize: 10,
  minAutoApprovalConfidence: 0.8,
  rollbackThresholdPercent: 15.0,
  observationPeriodHours: 24,
};

// ============================================================================
// API Functions
// ============================================================================

/**
 * Get all tunable parameters
 *
 * @param workspacePath - Path to the workspace
 * @returns Array of tunable parameters
 */
export async function getTunableParameters(workspacePath: string): Promise<TunableParameter[]> {
  try {
    return await invoke<TunableParameter[]>("get_tunable_parameters", { workspacePath });
  } catch (error) {
    logger.error("Failed to get tunable parameters", error as Error);
    return [];
  }
}

/**
 * Get a single tunable parameter by name
 *
 * @param workspacePath - Path to the workspace
 * @param name - Parameter name
 * @returns The parameter or null if not found
 */
export async function getParameter(
  workspacePath: string,
  name: string
): Promise<TunableParameter | null> {
  try {
    return await invoke<TunableParameter | null>("get_tunable_parameter", { workspacePath, name });
  } catch (error) {
    logger.error("Failed to get parameter", error as Error, { name });
    return null;
  }
}

/**
 * Update a parameter value manually
 *
 * @param workspacePath - Path to the workspace
 * @param name - Parameter name
 * @param newValue - New value to set
 * @param reason - Reason for the change
 */
export async function updateParameter(
  workspacePath: string,
  name: string,
  newValue: number,
  reason: string
): Promise<void> {
  try {
    await invoke("update_tunable_parameter", {
      workspacePath,
      name,
      newValue,
      changedBy: "human",
      reason,
    });
    logger.info("Parameter updated", { name, newValue, reason });
  } catch (error) {
    logger.error("Failed to update parameter", error as Error, { name });
    throw error;
  }
}

/**
 * Reset a parameter to its default value
 *
 * @param workspacePath - Path to the workspace
 * @param name - Parameter name
 * @param reason - Reason for the reset
 */
export async function resetParameter(
  workspacePath: string,
  name: string,
  reason: string
): Promise<void> {
  try {
    await invoke("reset_tunable_parameter", { workspacePath, name, reason });
    logger.info("Parameter reset to default", { name, reason });
  } catch (error) {
    logger.error("Failed to reset parameter", error as Error, { name });
    throw error;
  }
}

/**
 * Get pending tuning proposals awaiting approval
 *
 * @param workspacePath - Path to the workspace
 * @returns Array of pending proposals
 */
export async function getPendingProposals(workspacePath: string): Promise<TuningProposal[]> {
  try {
    return await invoke<TuningProposal[]>("get_pending_tuning_proposals", { workspacePath });
  } catch (error) {
    logger.error("Failed to get pending proposals", error as Error);
    return [];
  }
}

/**
 * Get recent tuning proposals (all statuses)
 *
 * @param workspacePath - Path to the workspace
 * @param limit - Maximum number of proposals to return
 * @returns Array of recent proposals
 */
export async function getRecentProposals(
  workspacePath: string,
  limit = 20
): Promise<TuningProposal[]> {
  try {
    return await invoke<TuningProposal[]>("get_recent_tuning_proposals", { workspacePath, limit });
  } catch (error) {
    logger.error("Failed to get recent proposals", error as Error);
    return [];
  }
}

/**
 * Approve a tuning proposal
 *
 * @param workspacePath - Path to the workspace
 * @param proposalId - ID of the proposal to approve
 */
export async function approveProposal(workspacePath: string, proposalId: number): Promise<void> {
  try {
    await invoke("approve_tuning_proposal", { workspacePath, proposalId });
    logger.info("Proposal approved", { proposalId });
  } catch (error) {
    logger.error("Failed to approve proposal", error as Error, { proposalId });
    throw error;
  }
}

/**
 * Reject a tuning proposal
 *
 * @param workspacePath - Path to the workspace
 * @param proposalId - ID of the proposal to reject
 */
export async function rejectProposal(workspacePath: string, proposalId: number): Promise<void> {
  try {
    await invoke("reject_tuning_proposal", { workspacePath, proposalId });
    logger.info("Proposal rejected", { proposalId });
  } catch (error) {
    logger.error("Failed to reject proposal", error as Error, { proposalId });
    throw error;
  }
}

/**
 * Apply an approved proposal
 *
 * @param workspacePath - Path to the workspace
 * @param proposalId - ID of the proposal to apply
 */
export async function applyProposal(workspacePath: string, proposalId: number): Promise<void> {
  try {
    await invoke("apply_tuning_proposal", { workspacePath, proposalId });
    logger.info("Proposal applied", { proposalId });
  } catch (error) {
    logger.error("Failed to apply proposal", error as Error, { proposalId });
    throw error;
  }
}

/**
 * Get effectiveness history for trend analysis
 *
 * @param workspacePath - Path to the workspace
 * @param days - Number of days of history to retrieve
 * @returns Array of effectiveness snapshots
 */
export async function getEffectivenessHistory(
  workspacePath: string,
  days = 30
): Promise<EffectivenessSnapshot[]> {
  try {
    return await invoke<EffectivenessSnapshot[]>("get_effectiveness_history", {
      workspacePath,
      days,
    });
  } catch (error) {
    logger.error("Failed to get effectiveness history", error as Error);
    return [];
  }
}

/**
 * Get tuning history for a parameter
 *
 * @param workspacePath - Path to the workspace
 * @param parameterName - Name of the parameter
 * @param limit - Maximum number of entries to return
 * @returns Array of history entries
 */
export async function getParameterHistory(
  workspacePath: string,
  parameterName: string,
  limit = 20
): Promise<TuningHistory[]> {
  try {
    return await invoke<TuningHistory[]>("get_parameter_tuning_history", {
      workspacePath,
      parameterName,
      limit,
    });
  } catch (error) {
    logger.error("Failed to get parameter history", error as Error, { parameterName });
    return [];
  }
}

/**
 * Get tuning summary statistics
 *
 * @param workspacePath - Path to the workspace
 * @returns Tuning summary
 */
export async function getTuningSummary(workspacePath: string): Promise<TuningSummary> {
  try {
    return await invoke<TuningSummary>("get_tuning_summary", { workspacePath });
  } catch (error) {
    logger.error("Failed to get tuning summary", error as Error);
    return {
      totalParameters: 0,
      autoTunableCount: 0,
      pendingProposals: 0,
      appliedProposals: 0,
      rollbacksCount: 0,
      avgImprovementPercent: 0,
      lastTuningCycle: null,
    };
  }
}

/**
 * Trigger a tuning analysis cycle
 *
 * This analyzes effectiveness data and creates proposals for parameter adjustments.
 *
 * @param workspacePath - Path to the workspace
 * @param config - Optional tuning configuration
 * @returns Array of newly created proposals
 */
export async function runTuningAnalysis(
  workspacePath: string,
  config?: Partial<TuningConfig>
): Promise<TuningProposal[]> {
  try {
    const fullConfig = { ...DEFAULT_TUNING_CONFIG, ...config };
    return await invoke<TuningProposal[]>("run_tuning_analysis", {
      workspacePath,
      config: fullConfig,
    });
  } catch (error) {
    logger.error("Failed to run tuning analysis", error as Error);
    return [];
  }
}

/**
 * Check for and execute automatic rollbacks
 *
 * This checks if any recently applied proposals have caused performance degradation
 * and triggers automatic rollbacks if needed.
 *
 * @param workspacePath - Path to the workspace
 * @param config - Optional tuning configuration
 * @returns Array of rollback proposal IDs
 */
export async function checkForRollbacks(
  workspacePath: string,
  config?: Partial<TuningConfig>
): Promise<number[]> {
  try {
    const fullConfig = { ...DEFAULT_TUNING_CONFIG, ...config };
    return await invoke<number[]>("check_for_tuning_rollbacks", {
      workspacePath,
      config: fullConfig,
    });
  } catch (error) {
    logger.error("Failed to check for rollbacks", error as Error);
    return [];
  }
}

/**
 * Record an effectiveness snapshot for trend tracking
 *
 * @param workspacePath - Path to the workspace
 * @param snapshot - The effectiveness metrics to record
 * @returns ID of the created snapshot
 */
export async function recordEffectivenessSnapshot(
  workspacePath: string,
  snapshot: Omit<EffectivenessSnapshot, "timestamp">
): Promise<number> {
  try {
    return await invoke<number>("record_effectiveness_snapshot", {
      workspacePath,
      snapshot: {
        ...snapshot,
        timestamp: new Date().toISOString(),
      },
    });
  } catch (error) {
    logger.error("Failed to record effectiveness snapshot", error as Error);
    throw error;
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format a parameter value with its unit
 */
export function formatParameterValue(value: number, unit: string): string {
  switch (unit) {
    case "ms":
      if (value >= 3600000) {
        return `${(value / 3600000).toFixed(1)}h`;
      }
      if (value >= 60000) {
        return `${(value / 60000).toFixed(1)}m`;
      }
      if (value >= 1000) {
        return `${(value / 1000).toFixed(1)}s`;
      }
      return `${value}ms`;
    case "ratio":
    case "percent":
      return `${(value * 100).toFixed(1)}%`;
    case "count":
      return value.toFixed(0);
    default:
      return value.toFixed(2);
  }
}

/**
 * Get a color class based on proposal status
 */
export function getProposalStatusColor(status: ProposalStatus): string {
  switch (status) {
    case "pending":
      return "text-yellow-600 dark:text-yellow-400";
    case "approved":
      return "text-blue-600 dark:text-blue-400";
    case "applied":
      return "text-green-600 dark:text-green-400";
    case "rejected":
      return "text-gray-600 dark:text-gray-400";
    case "rolled_back":
      return "text-red-600 dark:text-red-400";
  }
}

/**
 * Get a human-readable label for a proposal status
 */
export function getProposalStatusLabel(status: ProposalStatus): string {
  switch (status) {
    case "pending":
      return "Pending";
    case "approved":
      return "Approved";
    case "applied":
      return "Applied";
    case "rejected":
      return "Rejected";
    case "rolled_back":
      return "Rolled Back";
  }
}

/**
 * Format a confidence score for display
 */
export function formatConfidence(confidence: number): string {
  return `${(confidence * 100).toFixed(0)}%`;
}

/**
 * Get a color class based on confidence level
 */
export function getConfidenceColor(confidence: number): string {
  if (confidence >= 0.8) return "text-green-600 dark:text-green-400";
  if (confidence >= 0.6) return "text-yellow-600 dark:text-yellow-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Format a change percentage with sign
 */
export function formatChangePercent(percent: number): string {
  const sign = percent >= 0 ? "+" : "";
  return `${sign}${percent.toFixed(1)}%`;
}
