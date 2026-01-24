/**
 * Prompt Optimization Module
 *
 * Provides automated prompt optimization suggestions based on historical success patterns.
 * Part of Phase 4 (Advanced Analytics) - uses pattern catalog and correlation data.
 *
 * Features:
 * - Template matching: Map prompts to known successful patterns
 * - Feature optimization: Adjust length, structure, specificity
 * - A/B suggestions: Offer variants to test
 * - Learning loop: Track acceptance and outcome of suggestions
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("prompt-optimization");

// ============================================================================
// Types
// ============================================================================

/**
 * An optimization suggestion for a prompt
 */
export interface OptimizationSuggestion {
  id: number | null;
  /** The original prompt text */
  original_prompt: string;
  /** The suggested optimized prompt */
  optimized_prompt: string;
  /** The type of optimization applied */
  optimization_type: "length" | "specificity" | "structure" | "pattern";
  /** Reasoning/evidence for why this optimization should work */
  reasoning: string;
  /** Confidence score (0.0 to 1.0) */
  confidence: number;
  /** Reference pattern ID if matched */
  matched_pattern_id: number | null;
  /** Expected improvement in success rate (as a percentage) */
  expected_improvement: number;
  /** Whether this suggestion was accepted by the user */
  accepted: boolean | null;
  /** Outcome after acceptance (if tracked) */
  outcome: string | null;
  /** Timestamp when suggestion was created */
  created_at: string | null;
}

/**
 * Result of analyzing a prompt for optimization opportunities
 */
export interface PromptAnalysis {
  /** The analyzed prompt */
  prompt: string;
  /** Word count of the prompt */
  word_count: number;
  /** Character count */
  char_count: number;
  /** Detected category/intent */
  category: string | null;
  /** Specificity score (0-1, higher = more specific) */
  specificity_score: number;
  /** Structure quality score (0-1) */
  structure_score: number;
  /** List of detected issues with the prompt */
  issues: PromptIssue[];
  /** Whether optimization is recommended */
  needs_optimization: boolean;
}

/**
 * A detected issue with a prompt
 */
export interface PromptIssue {
  /** Issue type */
  issue_type:
    | "too_short"
    | "too_long"
    | "vague"
    | "missing_issue_ref"
    | "passive_voice"
    | "missing_test_mention";
  /** Human-readable description */
  description: string;
  /** Severity level */
  severity: "low" | "medium" | "high";
}

/**
 * Optimization rule that defines how to improve prompts
 */
export interface OptimizationRule {
  id: number | null;
  /** Rule name */
  name: string;
  /** Rule type */
  rule_type: "length" | "structure" | "specificity" | "pattern";
  /** Condition to trigger the rule (JSON) */
  condition: string;
  /** Template for the optimization suggestion */
  suggestion_template: string;
  /** Expected improvement percentage */
  expected_improvement: number;
  /** Whether the rule is active */
  active: boolean;
  /** Number of times applied */
  times_applied: number;
  /** Success rate when this rule's suggestions are accepted */
  success_rate: number;
}

/**
 * Summary of optimization activity
 */
export interface OptimizationStats {
  total_suggestions: number;
  accepted_suggestions: number;
  rejected_suggestions: number;
  pending_suggestions: number;
  acceptance_rate: number;
  avg_improvement_when_accepted: number;
  suggestions_by_type: OptimizationTypeStats[];
}

/**
 * Statistics for a specific optimization type
 */
export interface OptimizationTypeStats {
  optimization_type: string;
  count: number;
  acceptance_rate: number;
  avg_improvement: number;
}

// ============================================================================
// Analysis Functions
// ============================================================================

/**
 * Analyze a prompt and identify optimization opportunities
 *
 * @param workspacePath - Path to the workspace
 * @param prompt - The prompt text to analyze
 * @returns Analysis results with issues and scores
 */
export async function analyzePrompt(
  workspacePath: string,
  prompt: string
): Promise<PromptAnalysis> {
  try {
    return await invoke<PromptAnalysis>("analyze_prompt", {
      workspacePath,
      prompt,
    });
  } catch (error) {
    logger.error("Failed to analyze prompt", error as Error);
    return {
      prompt,
      word_count: prompt.split(/\s+/).length,
      char_count: prompt.length,
      category: null,
      specificity_score: 0.5,
      structure_score: 0.5,
      issues: [],
      needs_optimization: false,
    };
  }
}

/**
 * Generate optimization suggestions for a prompt
 *
 * @param workspacePath - Path to the workspace
 * @param prompt - The prompt text to optimize
 * @param maxSuggestions - Maximum number of suggestions to return (default: 3)
 * @returns Array of optimization suggestions
 */
export async function generateOptimizationSuggestions(
  workspacePath: string,
  prompt: string,
  maxSuggestions?: number
): Promise<OptimizationSuggestion[]> {
  try {
    return await invoke<OptimizationSuggestion[]>("generate_optimization_suggestions", {
      workspacePath,
      prompt,
      maxSuggestions: maxSuggestions ?? null,
    });
  } catch (error) {
    logger.error("Failed to generate optimization suggestions", error as Error);
    return [];
  }
}

// ============================================================================
// Tracking Functions
// ============================================================================

/**
 * Record that a suggestion was accepted or rejected
 *
 * @param workspacePath - Path to the workspace
 * @param suggestionId - ID of the suggestion
 * @param accepted - Whether the suggestion was accepted
 */
export async function recordSuggestionDecision(
  workspacePath: string,
  suggestionId: number,
  accepted: boolean
): Promise<void> {
  try {
    await invoke("record_suggestion_decision", {
      workspacePath,
      suggestionId,
      accepted,
    });
    logger.info("Recorded suggestion decision", { suggestionId, accepted });
  } catch (error) {
    logger.error("Failed to record suggestion decision", error as Error);
    throw error;
  }
}

/**
 * Record the outcome of an accepted suggestion
 *
 * @param workspacePath - Path to the workspace
 * @param suggestionId - ID of the suggestion
 * @param outcome - Outcome description (e.g., "success", "failure", "partial")
 */
export async function recordSuggestionOutcome(
  workspacePath: string,
  suggestionId: number,
  outcome: string
): Promise<void> {
  try {
    await invoke("record_suggestion_outcome", {
      workspacePath,
      suggestionId,
      outcome,
    });
    logger.info("Recorded suggestion outcome", { suggestionId, outcome });
  } catch (error) {
    logger.error("Failed to record suggestion outcome", error as Error);
    throw error;
  }
}

// ============================================================================
// Statistics and Query Functions
// ============================================================================

/**
 * Get optimization statistics
 *
 * @param workspacePath - Path to the workspace
 * @returns Optimization stats summary
 */
export async function getOptimizationStats(workspacePath: string): Promise<OptimizationStats> {
  try {
    return await invoke<OptimizationStats>("get_optimization_stats", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to get optimization stats", error as Error);
    return {
      total_suggestions: 0,
      accepted_suggestions: 0,
      rejected_suggestions: 0,
      pending_suggestions: 0,
      acceptance_rate: 0,
      avg_improvement_when_accepted: 0,
      suggestions_by_type: [],
    };
  }
}

/**
 * Get recent optimization suggestions
 *
 * @param workspacePath - Path to the workspace
 * @param limit - Maximum number of suggestions to return (default: 10)
 * @returns Array of recent suggestions
 */
export async function getRecentSuggestions(
  workspacePath: string,
  limit?: number
): Promise<OptimizationSuggestion[]> {
  try {
    return await invoke<OptimizationSuggestion[]>("get_recent_suggestions", {
      workspacePath,
      limit: limit ?? null,
    });
  } catch (error) {
    logger.error("Failed to get recent suggestions", error as Error);
    return [];
  }
}

/**
 * Get all active optimization rules
 *
 * @param workspacePath - Path to the workspace
 * @returns Array of optimization rules
 */
export async function getOptimizationRules(workspacePath: string): Promise<OptimizationRule[]> {
  try {
    return await invoke<OptimizationRule[]>("get_optimization_rules", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to get optimization rules", error as Error);
    return [];
  }
}

/**
 * Toggle a rule's active status
 *
 * @param workspacePath - Path to the workspace
 * @param ruleId - ID of the rule to toggle
 * @param active - Whether the rule should be active
 */
export async function toggleOptimizationRule(
  workspacePath: string,
  ruleId: number,
  active: boolean
): Promise<void> {
  try {
    await invoke("toggle_optimization_rule", {
      workspacePath,
      ruleId,
      active,
    });
    logger.info("Toggled optimization rule", { ruleId, active });
  } catch (error) {
    logger.error("Failed to toggle optimization rule", error as Error);
    throw error;
  }
}

/**
 * Refine optimization rules based on outcomes (learning loop)
 *
 * @param workspacePath - Path to the workspace
 * @returns Number of rules updated
 */
export async function refineOptimizationRules(workspacePath: string): Promise<number> {
  try {
    return await invoke<number>("refine_optimization_rules", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to refine optimization rules", error as Error);
    return 0;
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format a confidence score for display
 */
export function formatConfidence(confidence: number): string {
  return `${Math.round(confidence * 100)}%`;
}

/**
 * Format an expected improvement for display
 */
export function formatImprovement(improvement: number): string {
  const sign = improvement >= 0 ? "+" : "";
  return `${sign}${Math.round(improvement * 100)}%`;
}

/**
 * Get a display name for an optimization type
 */
export function getOptimizationTypeName(type: string): string {
  const names: Record<string, string> = {
    length: "Length Adjustment",
    specificity: "Specificity Enhancement",
    structure: "Structure Improvement",
    pattern: "Pattern Matching",
  };
  return names[type] ?? type;
}

/**
 * Get a color class based on confidence level
 */
export function getConfidenceColorClass(confidence: number): string {
  if (confidence >= 0.8) return "text-green-600 dark:text-green-400";
  if (confidence >= 0.6) return "text-yellow-600 dark:text-yellow-400";
  if (confidence >= 0.4) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Get a color class based on issue severity
 */
export function getSeverityColorClass(severity: string): string {
  switch (severity) {
    case "high":
      return "text-red-600 dark:text-red-400";
    case "medium":
      return "text-yellow-600 dark:text-yellow-400";
    case "low":
      return "text-blue-600 dark:text-blue-400";
    default:
      return "text-gray-600 dark:text-gray-400";
  }
}

/**
 * Get an icon for an issue type
 */
export function getIssueIcon(issueType: string): string {
  const icons: Record<string, string> = {
    too_short: "warning",
    too_long: "content_cut",
    vague: "help_outline",
    missing_issue_ref: "link_off",
    passive_voice: "record_voice_over",
    missing_test_mention: "science",
  };
  return icons[issueType] ?? "info";
}

/**
 * Get a human-readable label for an issue type
 */
export function getIssueLabel(issueType: string): string {
  const labels: Record<string, string> = {
    too_short: "Too Short",
    too_long: "Too Long",
    vague: "Vague Language",
    missing_issue_ref: "Missing Issue Reference",
    passive_voice: "Passive Voice",
    missing_test_mention: "Missing Test Mention",
  };
  return labels[issueType] ?? issueType;
}

/**
 * Get a badge color class for an optimization type
 */
export function getTypeBadgeClass(type: string): string {
  const classes: Record<string, string> = {
    length: "bg-blue-100 text-blue-800 dark:bg-blue-900 dark:text-blue-200",
    specificity: "bg-purple-100 text-purple-800 dark:bg-purple-900 dark:text-purple-200",
    structure: "bg-orange-100 text-orange-800 dark:bg-orange-900 dark:text-orange-200",
    pattern: "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-200",
  };
  return classes[type] ?? "bg-gray-100 text-gray-800 dark:bg-gray-900 dark:text-gray-200";
}

/**
 * Calculate an overall quality score from analysis
 */
export function calculateQualityScore(analysis: PromptAnalysis): number {
  // Weighted average of scores
  const weights = {
    specificity: 0.4,
    structure: 0.3,
    issues: 0.3,
  };

  // Issues penalty: each high=0.2, medium=0.1, low=0.05
  const issuePenalty = analysis.issues.reduce((acc, issue) => {
    switch (issue.severity) {
      case "high":
        return acc + 0.2;
      case "medium":
        return acc + 0.1;
      case "low":
        return acc + 0.05;
      default:
        return acc;
    }
  }, 0);

  const issueScore = Math.max(0, 1 - issuePenalty);

  return (
    analysis.specificity_score * weights.specificity +
    analysis.structure_score * weights.structure +
    issueScore * weights.issues
  );
}

/**
 * Get a quality rating from score
 */
export function getQualityRating(score: number): "excellent" | "good" | "fair" | "poor" {
  if (score >= 0.8) return "excellent";
  if (score >= 0.6) return "good";
  if (score >= 0.4) return "fair";
  return "poor";
}

/**
 * Get a color class for a quality rating
 */
export function getQualityColorClass(rating: string): string {
  switch (rating) {
    case "excellent":
      return "text-green-600 dark:text-green-400";
    case "good":
      return "text-blue-600 dark:text-blue-400";
    case "fair":
      return "text-yellow-600 dark:text-yellow-400";
    case "poor":
      return "text-red-600 dark:text-red-400";
    default:
      return "text-gray-600 dark:text-gray-400";
  }
}
