/**
 * Template Generation Module
 *
 * Provides auto-generation of reusable prompt templates from successful patterns.
 * Part of Phase 5 (Autonomous Learning) - builds on pattern catalog from Phase 3.
 *
 * Features:
 * - Cluster similar successful prompts
 * - Extract common structure with variable placeholders
 * - Generate templates that can be instantiated with values
 * - Track template usage and effectiveness
 * - Retire underperforming templates automatically
 *
 * @example
 * import {
 *   generateTemplates,
 *   findMatchingTemplate,
 *   instantiateTemplate,
 *   getTemplateStats
 * } from "./template-generation";
 *
 * // Generate templates from successful patterns
 * const result = await generateTemplates(workspacePath);
 * console.log(`Created ${result.templates_created} new templates`);
 *
 * // Find a template for a prompt
 * const template = await findMatchingTemplate(workspacePath, "fix the login bug");
 * if (template) {
 *   // Instantiate with specific values
 *   const prompt = await instantiateTemplate(workspacePath, template.id!, {
 *     issue_number: "123",
 *     file_path: "src/auth.ts"
 *   });
 *   console.log(`Generated prompt: ${prompt}`);
 * }
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("template-generation");

// ============================================================================
// Types
// ============================================================================

/**
 * A generated prompt template with variable placeholders
 */
export interface PromptTemplate {
  id: number | null;
  /** The template text with placeholders like {file}, {issue_number} */
  template_text: string;
  /** Category of prompts this template applies to */
  category: string;
  /** List of placeholder names found in the template */
  placeholders: string[];
  /** Number of source patterns used to generate this template */
  source_pattern_count: number;
  /** Combined success rate of source patterns */
  source_success_rate: number;
  /** Number of times this template has been used */
  times_used: number;
  /** Success rate when this template is used */
  success_rate: number;
  /** Success count for this template */
  success_count: number;
  /** Failure count for this template */
  failure_count: number;
  /** Whether this template is active (not retired) */
  active: boolean;
  /** Minimum success rate threshold before retirement */
  retirement_threshold: number;
  /** When this template was generated */
  created_at: string | null;
  /** When this template was last used */
  last_used_at: string | null;
  /** Human-readable description of what this template does */
  description: string | null;
  /** Example instantiation of the template */
  example: string | null;
}

/**
 * Result of template generation
 */
export interface TemplateGenerationResult {
  templates_created: number;
  templates_updated: number;
  patterns_analyzed: number;
  clusters_found: number;
}

/**
 * Template usage statistics
 */
export interface TemplateStats {
  total_templates: number;
  active_templates: number;
  retired_templates: number;
  templates_by_category: TemplateCategoryStats[];
  avg_success_rate: number;
  top_templates: PromptTemplate[];
  retirement_candidates: PromptTemplate[];
}

/**
 * Statistics for a template category
 */
export interface TemplateCategoryStats {
  category: string;
  count: number;
  avg_success_rate: number;
  total_uses: number;
}

// ============================================================================
// Template Generation Functions
// ============================================================================

/**
 * Generate templates from successful prompt patterns
 *
 * This function:
 * 1. Finds clusters of similar successful prompts
 * 2. Extracts common structure from each cluster
 * 3. Identifies variable parts and creates placeholders
 * 4. Generates templates with usage examples
 *
 * @param workspacePath - Path to the workspace
 * @param options - Optional parameters
 * @returns Generation result with counts
 *
 * @example
 * const result = await generateTemplates("/path/to/workspace", {
 *   minClusterSize: 5,    // Require at least 5 similar patterns
 *   minSuccessRate: 0.7,  // Only use patterns with 70%+ success
 * });
 * console.log(`Created ${result.templates_created} templates from ${result.clusters_found} clusters`);
 */
export async function generateTemplates(
  workspacePath: string,
  options?: {
    minClusterSize?: number;
    minSuccessRate?: number;
  }
): Promise<TemplateGenerationResult> {
  try {
    return await invoke<TemplateGenerationResult>("generate_templates_from_patterns", {
      workspacePath,
      minClusterSize: options?.minClusterSize ?? null,
      minSuccessRate: options?.minSuccessRate ?? null,
    });
  } catch (error) {
    logger.error("Failed to generate templates", error as Error);
    return {
      templates_created: 0,
      templates_updated: 0,
      patterns_analyzed: 0,
      clusters_found: 0,
    };
  }
}

// ============================================================================
// Template Query Functions
// ============================================================================

/**
 * Get all templates, optionally filtered
 *
 * @param workspacePath - Path to the workspace
 * @param options - Optional filters
 * @returns Array of templates
 */
export async function getTemplates(
  workspacePath: string,
  options?: {
    category?: string;
    activeOnly?: boolean;
    limit?: number;
  }
): Promise<PromptTemplate[]> {
  try {
    return await invoke<PromptTemplate[]>("get_templates", {
      workspacePath,
      category: options?.category ?? null,
      activeOnly: options?.activeOnly ?? true,
      limit: options?.limit ?? null,
    });
  } catch (error) {
    logger.error("Failed to get templates", error as Error);
    return [];
  }
}

/**
 * Get a single template by ID
 *
 * @param workspacePath - Path to the workspace
 * @param templateId - Template ID
 * @returns Template or null if not found
 */
export async function getTemplate(
  workspacePath: string,
  templateId: number
): Promise<PromptTemplate | null> {
  try {
    return await invoke<PromptTemplate | null>("get_template", {
      workspacePath,
      templateId,
    });
  } catch (error) {
    logger.error("Failed to get template", error as Error, { templateId });
    return null;
  }
}

/**
 * Find the best matching template for a prompt intent
 *
 * @param workspacePath - Path to the workspace
 * @param prompt - The prompt text to match
 * @param category - Optional category to filter by
 * @returns Best matching template or null
 *
 * @example
 * const template = await findMatchingTemplate(
 *   "/path/to/workspace",
 *   "fix the authentication bug in the login form"
 * );
 * if (template) {
 *   console.log(`Found template: ${template.template_text}`);
 *   console.log(`Placeholders: ${template.placeholders.join(", ")}`);
 * }
 */
export async function findMatchingTemplate(
  workspacePath: string,
  prompt: string,
  category?: string
): Promise<PromptTemplate | null> {
  try {
    return await invoke<PromptTemplate | null>("find_matching_template", {
      workspacePath,
      prompt,
      category: category ?? null,
    });
  } catch (error) {
    logger.error("Failed to find matching template", error as Error);
    return null;
  }
}

/**
 * Instantiate a template with values
 *
 * @param workspacePath - Path to the workspace
 * @param templateId - Template ID
 * @param values - Map of placeholder names to values
 * @returns Instantiated prompt text
 *
 * @example
 * const prompt = await instantiateTemplate(workspacePath, 1, {
 *   issue_number: "123",
 *   file_path: "src/auth.ts",
 *   error_message: "undefined is not a function"
 * });
 * // Result: "Fix issue #123 in src/auth.ts: undefined is not a function"
 */
export async function instantiateTemplate(
  workspacePath: string,
  templateId: number,
  values: Record<string, string>
): Promise<string> {
  try {
    return await invoke<string>("instantiate_template", {
      workspacePath,
      templateId,
      values,
    });
  } catch (error) {
    logger.error("Failed to instantiate template", error as Error, { templateId });
    throw error;
  }
}

// ============================================================================
// Template Usage Tracking
// ============================================================================

/**
 * Record that a template was used
 *
 * @param workspacePath - Path to the workspace
 * @param templateId - Template ID
 * @param instantiatedPrompt - The resulting prompt after instantiation
 * @param activityId - Optional activity ID to link to
 * @returns Usage ID for tracking outcome
 */
export async function recordTemplateUsage(
  workspacePath: string,
  templateId: number,
  instantiatedPrompt: string,
  activityId?: number
): Promise<number> {
  try {
    return await invoke<number>("record_template_usage", {
      workspacePath,
      templateId,
      instantiatedPrompt,
      activityId: activityId ?? null,
    });
  } catch (error) {
    logger.error("Failed to record template usage", error as Error, { templateId });
    throw error;
  }
}

/**
 * Record the outcome of a template usage
 *
 * @param workspacePath - Path to the workspace
 * @param usageId - Usage ID from recordTemplateUsage
 * @param wasSuccessful - Whether the usage was successful
 */
export async function recordTemplateOutcome(
  workspacePath: string,
  usageId: number,
  wasSuccessful: boolean
): Promise<void> {
  try {
    await invoke("record_template_outcome", {
      workspacePath,
      usageId,
      wasSuccessful,
    });
    logger.info("Recorded template outcome", { usageId, wasSuccessful });
  } catch (error) {
    logger.error("Failed to record template outcome", error as Error, { usageId });
    // Non-blocking
  }
}

// ============================================================================
// Template Lifecycle Management
// ============================================================================

/**
 * Retire underperforming templates
 *
 * Templates are retired when they have been used enough times
 * but their success rate falls below their retirement threshold.
 *
 * @param workspacePath - Path to the workspace
 * @param minUses - Minimum uses before considering retirement (default: 10)
 * @returns Number of templates retired
 */
export async function retireUnderperformingTemplates(
  workspacePath: string,
  minUses?: number
): Promise<number> {
  try {
    return await invoke<number>("retire_underperforming_templates", {
      workspacePath,
      minUses: minUses ?? null,
    });
  } catch (error) {
    logger.error("Failed to retire templates", error as Error);
    return 0;
  }
}

/**
 * Reactivate a retired template
 *
 * @param workspacePath - Path to the workspace
 * @param templateId - Template ID to reactivate
 */
export async function reactivateTemplate(workspacePath: string, templateId: number): Promise<void> {
  try {
    await invoke("reactivate_template", {
      workspacePath,
      templateId,
    });
    logger.info("Reactivated template", { templateId });
  } catch (error) {
    logger.error("Failed to reactivate template", error as Error, { templateId });
    throw error;
  }
}

/**
 * Get template statistics
 *
 * @param workspacePath - Path to the workspace
 * @returns Template statistics summary
 */
export async function getTemplateStats(workspacePath: string): Promise<TemplateStats> {
  try {
    return await invoke<TemplateStats>("get_template_stats", {
      workspacePath,
    });
  } catch (error) {
    logger.error("Failed to get template stats", error as Error);
    return {
      total_templates: 0,
      active_templates: 0,
      retired_templates: 0,
      templates_by_category: [],
      avg_success_rate: 0,
      top_templates: [],
      retirement_candidates: [],
    };
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format a template for display with highlighted placeholders
 */
export function formatTemplate(template: PromptTemplate): string {
  let text = template.template_text;

  // Highlight placeholders
  for (const placeholder of template.placeholders) {
    const pattern = `{${placeholder}}`;
    text = text.replace(pattern, `[${placeholder}]`);
  }

  return text;
}

/**
 * Format success rate as percentage
 */
export function formatSuccessRate(rate: number): string {
  return `${(rate * 100).toFixed(1)}%`;
}

/**
 * Get color class for success rate
 */
export function getSuccessRateColorClass(rate: number): string {
  if (rate >= 0.8) return "text-green-600 dark:text-green-400";
  if (rate >= 0.6) return "text-yellow-600 dark:text-yellow-400";
  if (rate >= 0.4) return "text-orange-600 dark:text-orange-400";
  return "text-red-600 dark:text-red-400";
}

/**
 * Get a badge color for a category
 */
export function getCategoryBadgeClass(category: string): string {
  const classes: Record<string, string> = {
    build: "bg-blue-100 text-blue-800 dark:bg-blue-900 dark:text-blue-200",
    fix: "bg-red-100 text-red-800 dark:bg-red-900 dark:text-red-200",
    refactor: "bg-purple-100 text-purple-800 dark:bg-purple-900 dark:text-purple-200",
    review: "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-200",
    curate: "bg-yellow-100 text-yellow-800 dark:bg-yellow-900 dark:text-yellow-200",
  };
  return classes[category] ?? "bg-gray-100 text-gray-800 dark:bg-gray-900 dark:text-gray-200";
}

/**
 * Get an icon for a placeholder type
 */
export function getPlaceholderIcon(placeholder: string): string {
  const icons: Record<string, string> = {
    issue_number: "#",
    pr_number: "git_pull_request",
    file_path: "folder",
    function_name: "code",
    error_message: "warning",
    target: "adjust",
  };
  return icons[placeholder] ?? "edit";
}

/**
 * Describe what a template does based on its category and placeholders
 */
export function describeTemplate(template: PromptTemplate): string {
  const actions: Record<string, string> = {
    build: "Creates or implements",
    fix: "Fixes or resolves",
    refactor: "Refactors or improves",
    review: "Reviews or analyzes",
    curate: "Documents or enhances",
  };

  const action = actions[template.category] ?? "Performs action on";

  if (template.placeholders.length === 0) {
    return `${action} based on a proven pattern (${formatSuccessRate(template.source_success_rate)} source success rate)`;
  }

  const placeholderList = template.placeholders.map((p) => p.replace(/_/g, " ")).join(", ");

  return `${action} with customizable ${placeholderList}`;
}

/**
 * Check if a template is at risk of retirement
 */
export function isRetirementCandidate(template: PromptTemplate): boolean {
  return (
    template.active &&
    template.times_used >= 5 &&
    template.success_rate < template.retirement_threshold
  );
}

/**
 * Calculate template health score (0-100)
 */
export function calculateTemplateHealth(template: PromptTemplate): number {
  if (template.times_used === 0) {
    return 50; // Neutral for unused templates
  }

  // Weighted factors
  const successWeight = 0.6;
  const usageWeight = 0.3;
  const recencyWeight = 0.1;

  // Success score (0-100)
  const successScore = template.success_rate * 100;

  // Usage score (0-100, logarithmic scale)
  const usageScore = Math.min(100, Math.log10(template.times_used + 1) * 50);

  // Recency score (0-100)
  let recencyScore = 50;
  if (template.last_used_at) {
    const daysSinceUse =
      (Date.now() - new Date(template.last_used_at).getTime()) / (1000 * 60 * 60 * 24);
    recencyScore = Math.max(0, 100 - daysSinceUse * 2);
  }

  return Math.round(
    successScore * successWeight + usageScore * usageWeight + recencyScore * recencyWeight
  );
}

/**
 * Get health rating from score
 */
export function getHealthRating(score: number): "excellent" | "good" | "fair" | "poor" {
  if (score >= 80) return "excellent";
  if (score >= 60) return "good";
  if (score >= 40) return "fair";
  return "poor";
}

/**
 * Get color class for health rating
 */
export function getHealthColorClass(rating: string): string {
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
