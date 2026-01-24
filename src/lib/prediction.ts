/**
 * Success Prediction Module
 *
 * Provides machine learning-based prediction of success likelihood for prompts/tasks.
 * Uses logistic regression trained on historical agent activity data.
 *
 * Part of Phase 4 (Advanced Analytics) - Builds on Phase 3 correlation analysis.
 *
 * @example
 * import { predictPromptSuccess, trainModel, getModelStats } from "./prediction";
 *
 * // Predict success for a prompt
 * const result = await predictPromptSuccess(workspacePath, {
 *   promptText: "Add error handling to the API client",
 *   role: "builder",
 * });
 *
 * console.log(`Success probability: ${result.successProbability}`);
 * console.log(`Key factors: ${result.keyFactors.map(f => f.name).join(", ")}`);
 *
 * // Train/retrain the model with new data
 * const training = await trainModel(workspacePath);
 * console.log(`Model accuracy: ${training.accuracy}`);
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("prediction");

// ============================================================================
// Types
// ============================================================================

/**
 * Request for a success prediction
 */
export interface PredictionRequest {
  /** The prompt text to analyze */
  promptText: string;
  /** Optional agent role (builder, judge, curator, etc.) */
  role?: string;
  /** Optional additional context */
  context?: {
    hourOfDay?: number;
    dayOfWeek?: number;
    [key: string]: unknown;
  };
}

/**
 * Result of a success prediction
 */
export interface PredictionResult {
  /** Predicted probability of success (0.0 - 1.0) */
  success_probability: number;
  /** Model confidence in the prediction (0.0 - 1.0) */
  confidence: number;
  /** Confidence interval as [lower, upper] */
  confidence_interval: [number, number];
  /** Key factors that influenced the prediction */
  key_factors: PredictionFactor[];
  /** Suggested alternative prompts with higher predicted success */
  suggested_alternatives: PromptAlternative[];
  /** Warning if prediction may be unreliable */
  warning: string | null;
}

/**
 * A factor that influenced the prediction
 */
export interface PredictionFactor {
  /** Name of the factor (e.g., "prompt_length", "role_history") */
  name: string;
  /** How much this factor contributed to the prediction (-1.0 to 1.0) */
  contribution: number;
  /** Whether this factor had a positive or negative effect */
  direction: "positive" | "negative";
  /** Human-readable explanation */
  explanation: string;
}

/**
 * A suggested alternative prompt
 */
export interface PromptAlternative {
  /** The suggestion text */
  suggestion: string;
  /** Predicted improvement in success probability */
  predicted_improvement: number;
  /** Reason for the suggestion */
  reason: string;
}

/**
 * Model coefficients for logistic regression
 */
export interface ModelCoefficients {
  intercept: number;
  length_coef: number;
  word_count_coef: number;
  has_code_block_coef: number;
  has_file_refs_coef: number;
  question_count_coef: number;
  imperative_verb_coef: number;
  hour_coef: number;
  day_coef: number;
  role_success_rate_coef: number;
}

/**
 * Training result summary
 */
export interface TrainingResult {
  /** Number of samples used for training */
  samples_used: number;
  /** Model accuracy on training data */
  accuracy: number;
  /** Precision score */
  precision: number;
  /** Recall score */
  recall: number;
  /** F1 score */
  f1_score: number;
  /** When the model was trained */
  trained_at: string;
}

/**
 * Model statistics
 */
export interface ModelStats {
  /** Whether a trained model exists */
  is_trained: boolean;
  /** Number of available training samples */
  samples_count: number;
  /** When the model was last trained */
  last_trained: string | null;
  /** Model accuracy */
  accuracy: number | null;
  /** Model coefficients */
  coefficients: ModelCoefficients | null;
}

// ============================================================================
// Prediction Functions
// ============================================================================

/**
 * Predict success likelihood for a prompt
 *
 * @param workspacePath - Path to the workspace
 * @param request - Prediction request with prompt text and optional context
 * @returns Prediction result with probability, factors, and suggestions
 *
 * @example
 * const result = await predictPromptSuccess("/path/to/workspace", {
 *   promptText: "Implement user authentication for the API",
 *   role: "builder",
 * });
 *
 * if (result.success_probability < 0.5) {
 *   console.log("Consider these improvements:");
 *   result.suggested_alternatives.forEach(alt => {
 *     console.log(`- ${alt.suggestion} (${alt.reason})`);
 *   });
 * }
 */
export async function predictPromptSuccess(
  workspacePath: string,
  request: PredictionRequest
): Promise<PredictionResult> {
  try {
    // Transform the request to match Rust struct naming
    const rustRequest = {
      prompt_text: request.promptText,
      role: request.role ?? null,
      context: request.context
        ? {
            hour_of_day: request.context.hourOfDay,
            day_of_week: request.context.dayOfWeek,
            ...request.context,
          }
        : null,
    };

    return await invoke<PredictionResult>("predict_prompt_success", {
      workspacePath,
      request: rustRequest,
    });
  } catch (error) {
    logger.error("Failed to predict prompt success", error as Error);
    // Return a neutral prediction on error
    return {
      success_probability: 0.5,
      confidence: 0,
      confidence_interval: [0.2, 0.8],
      key_factors: [],
      suggested_alternatives: [],
      warning: "Prediction failed - using default values",
    };
  }
}

/**
 * Train or retrain the prediction model with historical data
 *
 * @param workspacePath - Path to the workspace
 * @returns Training result with accuracy metrics
 *
 * @example
 * const result = await trainModel("/path/to/workspace");
 * console.log(`Trained on ${result.samples_used} samples`);
 * console.log(`Accuracy: ${(result.accuracy * 100).toFixed(1)}%`);
 * console.log(`F1 Score: ${result.f1_score.toFixed(3)}`);
 */
export async function trainModel(workspacePath: string): Promise<TrainingResult> {
  try {
    return await invoke<TrainingResult>("train_prediction_model", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to train prediction model", error as Error);
    throw error;
  }
}

/**
 * Get statistics about the prediction model
 *
 * @param workspacePath - Path to the workspace
 * @returns Model statistics including training status and accuracy
 *
 * @example
 * const stats = await getModelStats("/path/to/workspace");
 * if (stats.is_trained) {
 *   console.log(`Model accuracy: ${stats.accuracy}`);
 *   console.log(`Last trained: ${stats.last_trained}`);
 * } else {
 *   console.log(`${stats.samples_count} samples available for training`);
 * }
 */
export async function getModelStats(workspacePath: string): Promise<ModelStats> {
  try {
    return await invoke<ModelStats>("get_prediction_model_stats", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to get model stats", error as Error);
    return {
      is_trained: false,
      samples_count: 0,
      last_trained: null,
      accuracy: null,
      coefficients: null,
    };
  }
}

/**
 * Record the actual outcome of a prediction for model improvement
 *
 * @param workspacePath - Path to the workspace
 * @param promptText - The original prompt text
 * @param outcome - The actual outcome ("success" or "failure")
 *
 * @example
 * // After a task completes
 * await recordPredictionOutcome(
 *   "/path/to/workspace",
 *   "Implement user authentication",
 *   "success"
 * );
 */
export async function recordPredictionOutcome(
  workspacePath: string,
  promptText: string,
  outcome: "success" | "failure"
): Promise<void> {
  try {
    await invoke("record_prediction_outcome", {
      workspacePath,
      promptText,
      actualOutcome: outcome,
    });
  } catch (error) {
    logger.error("Failed to record prediction outcome", error as Error);
    // Non-blocking - don't throw
  }
}

// ============================================================================
// Display Utilities
// ============================================================================

/**
 * Format a probability as a percentage string
 */
export function formatProbability(probability: number): string {
  return `${(probability * 100).toFixed(1)}%`;
}

/**
 * Get a human-readable description of the probability
 */
export function describeProbability(probability: number): string {
  if (probability >= 0.8) return "Very likely to succeed";
  if (probability >= 0.6) return "Likely to succeed";
  if (probability >= 0.4) return "Moderate chance of success";
  if (probability >= 0.2) return "May need improvement";
  return "Low chance of success";
}

/**
 * Get a color class based on probability
 */
export function getProbabilityColorClass(probability: number): string {
  if (probability >= 0.8) return "text-green-600 dark:text-green-400";
  if (probability >= 0.6) return "text-green-500 dark:text-green-500";
  if (probability >= 0.4) return "text-yellow-600 dark:text-yellow-400";
  if (probability >= 0.2) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Get a color class based on confidence
 */
export function getConfidenceColorClass(confidence: number): string {
  if (confidence >= 0.8) return "text-blue-600 dark:text-blue-400";
  if (confidence >= 0.5) return "text-blue-500 dark:text-blue-500";
  return "text-gray-500 dark:text-gray-400";
}

/**
 * Format a factor contribution for display
 */
export function formatContribution(contribution: number): string {
  const sign = contribution >= 0 ? "+" : "";
  return `${sign}${(contribution * 100).toFixed(1)}%`;
}

/**
 * Get an icon for the factor direction
 */
export function getDirectionIcon(direction: "positive" | "negative"): string {
  return direction === "positive" ? "+" : "-";
}

/**
 * Check if the model needs training based on stats
 */
export function shouldRetrain(stats: ModelStats): boolean {
  // Retrain if:
  // 1. No model exists
  // 2. Model is old (more than 7 days)
  // 3. Accuracy is below threshold
  // 4. Many new samples available

  if (!stats.is_trained) return true;

  if (stats.last_trained) {
    const lastTrained = new Date(stats.last_trained);
    const daysSinceTraining = (Date.now() - lastTrained.getTime()) / (1000 * 60 * 60 * 24);
    if (daysSinceTraining > 7) return true;
  }

  if (stats.accuracy !== null && stats.accuracy < 0.6) return true;

  // Would need to track samples at last training to implement this properly
  // For now, suggest retraining if we have many samples
  if (stats.samples_count > 500) return true;

  return false;
}

/**
 * Get a summary of key factors for display
 */
export function summarizeKeyFactors(factors: PredictionFactor[]): string {
  if (factors.length === 0) return "No significant factors identified";

  const positive = factors.filter((f) => f.direction === "positive");
  const negative = factors.filter((f) => f.direction === "negative");

  const parts: string[] = [];

  if (positive.length > 0) {
    parts.push(`Helpful: ${positive.map((f) => f.name.replace(/_/g, " ")).join(", ")}`);
  }

  if (negative.length > 0) {
    parts.push(`Could improve: ${negative.map((f) => f.name.replace(/_/g, " ")).join(", ")}`);
  }

  return parts.join(". ");
}

/**
 * Calculate the overall improvement if all suggestions are applied
 */
export function calculateTotalImprovement(alternatives: PromptAlternative[]): number {
  // Improvements are not additive, use diminishing returns
  let totalImprovement = 0;
  for (const alt of alternatives) {
    totalImprovement += alt.predicted_improvement * (1 - totalImprovement);
  }
  return totalImprovement;
}
