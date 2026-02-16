/**
 * Prediction Model Module
 *
 * TypeScript wrapper for the Rust prediction backend.
 * Provides prompt success prediction using logistic regression,
 * model training, and outcome recording.
 *
 * Part of Phase 4 (Advanced Analytics) - Issue #2262
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("prediction-model");

// ============================================================================
// Types
// ============================================================================

export interface PredictionRequest {
  prompt_text: string;
  role: string | null;
  context: Record<string, unknown> | null;
}

export interface PredictionFactor {
  name: string;
  contribution: number;
  direction: string;
  explanation: string;
}

export interface PromptAlternative {
  suggestion: string;
  predicted_improvement: number;
  reason: string;
}

export interface PredictionResult {
  success_probability: number;
  confidence: number;
  confidence_interval: [number, number];
  key_factors: PredictionFactor[];
  suggested_alternatives: PromptAlternative[];
  warning: string | null;
}

export interface TrainingResult {
  samples_used: number;
  accuracy: number;
  precision: number;
  recall: number;
  f1_score: number;
  trained_at: string;
}

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

export interface ModelStats {
  is_trained: boolean;
  samples_count: number;
  last_trained: string | null;
  accuracy: number | null;
  coefficients: ModelCoefficients | null;
}

// ============================================================================
// Prediction Functions
// ============================================================================

/**
 * Predict success probability for a prompt
 */
export async function predictPromptSuccess(
  workspacePath: string,
  request: PredictionRequest
): Promise<PredictionResult> {
  try {
    return await invoke<PredictionResult>("predict_prompt_success", {
      workspacePath,
      request,
    });
  } catch (error) {
    logger.error("Failed to predict prompt success", error as Error);
    return {
      success_probability: 0.5,
      confidence: 0,
      confidence_interval: [0, 1],
      key_factors: [],
      suggested_alternatives: [],
      warning: "Prediction unavailable",
    };
  }
}

/**
 * Train or retrain the prediction model
 */
export async function trainPredictionModel(workspacePath: string): Promise<TrainingResult> {
  try {
    return await invoke<TrainingResult>("train_prediction_model", { workspacePath });
  } catch (error) {
    logger.error("Failed to train prediction model", error as Error);
    throw error;
  }
}

/**
 * Get prediction model statistics
 */
export async function getPredictionModelStats(workspacePath: string): Promise<ModelStats> {
  try {
    return await invoke<ModelStats>("get_prediction_model_stats", { workspacePath });
  } catch (error) {
    logger.error("Failed to get prediction model stats", error as Error);
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
 * Record the actual outcome of a prompt for future training
 */
export async function recordPredictionOutcome(
  workspacePath: string,
  promptText: string,
  actualOutcome: string
): Promise<void> {
  try {
    await invoke("record_prediction_outcome", {
      workspacePath,
      promptText,
      actualOutcome,
    });
  } catch (error) {
    logger.error("Failed to record prediction outcome", error as Error);
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format success probability for display
 */
export function formatProbability(probability: number): string {
  return `${Math.round(probability * 100)}%`;
}

/**
 * Get color class based on probability
 */
export function getProbabilityColorClass(probability: number): string {
  if (probability >= 0.8) return "text-green-600 dark:text-green-400";
  if (probability >= 0.6) return "text-yellow-600 dark:text-yellow-400";
  if (probability >= 0.4) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Get a label for probability level
 */
export function getProbabilityLabel(probability: number): string {
  if (probability >= 0.8) return "High";
  if (probability >= 0.6) return "Moderate";
  if (probability >= 0.4) return "Low";
  return "Very Low";
}

/**
 * Format a confidence interval for display
 */
export function formatConfidenceInterval(interval: [number, number]): string {
  return `${Math.round(interval[0] * 100)}% - ${Math.round(interval[1] * 100)}%`;
}

/**
 * Get a direction icon for a prediction factor
 */
export function getDirectionIcon(direction: string): string {
  return direction === "positive" ? "+" : "-";
}

/**
 * Get color class for factor direction
 */
export function getDirectionColorClass(direction: string): string {
  return direction === "positive"
    ? "text-green-600 dark:text-green-400"
    : "text-red-600 dark:text-red-400";
}

/**
 * Format model accuracy for display
 */
export function formatAccuracy(accuracy: number): string {
  return `${(accuracy * 100).toFixed(1)}%`;
}
