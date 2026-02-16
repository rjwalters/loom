/**
 * Template Library Module
 *
 * TypeScript wrapper for the Rust template generation backend.
 * Provides template browsing, instantiation, usage tracking,
 * and lifecycle management.
 *
 * Part of Phase 5 (Autonomous Learning) - Issue #2262
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("template-library");

// ============================================================================
// Types
// ============================================================================

export interface PromptTemplate {
  id: number | null;
  template_text: string;
  category: string;
  placeholders: string[];
  source_pattern_count: number;
  source_success_rate: number;
  times_used: number;
  success_rate: number;
  success_count: number;
  failure_count: number;
  active: boolean;
  retirement_threshold: number;
  created_at: string | null;
  last_used_at: string | null;
  description: string | null;
  example: string | null;
}

export interface TemplateGenerationResult {
  templates_created: number;
  templates_updated: number;
  patterns_analyzed: number;
  clusters_found: number;
}

export interface TemplateCategoryStats {
  category: string;
  count: number;
  avg_success_rate: number;
  total_uses: number;
}

export interface TemplateStats {
  total_templates: number;
  active_templates: number;
  retired_templates: number;
  templates_by_category: TemplateCategoryStats[];
  avg_success_rate: number;
  top_templates: PromptTemplate[];
  retirement_candidates: PromptTemplate[];
}

// ============================================================================
// Template Generation
// ============================================================================

/**
 * Generate templates from successful prompt patterns
 */
export async function generateTemplatesFromPatterns(
  workspacePath: string,
  minClusterSize?: number,
  minSuccessRate?: number
): Promise<TemplateGenerationResult> {
  try {
    return await invoke<TemplateGenerationResult>("generate_templates_from_patterns", {
      workspacePath,
      minClusterSize: minClusterSize ?? null,
      minSuccessRate: minSuccessRate ?? null,
    });
  } catch (error) {
    logger.error("Failed to generate templates from patterns", error as Error);
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
 * Get templates with optional filters
 */
export async function getTemplates(
  workspacePath: string,
  category?: string,
  activeOnly?: boolean,
  limit?: number
): Promise<PromptTemplate[]> {
  try {
    return await invoke<PromptTemplate[]>("get_templates", {
      workspacePath,
      category: category ?? null,
      activeOnly: activeOnly ?? null,
      limit: limit ?? null,
    });
  } catch (error) {
    logger.error("Failed to get templates", error as Error);
    return [];
  }
}

/**
 * Get a single template by ID
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
    logger.error("Failed to get template", error as Error);
    return null;
  }
}

/**
 * Find a template that matches a given prompt
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
 * Get template statistics
 */
export async function getTemplateStats(workspacePath: string): Promise<TemplateStats> {
  try {
    return await invoke<TemplateStats>("get_template_stats", { workspacePath });
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
// Template Usage Functions
// ============================================================================

/**
 * Instantiate a template with provided values
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
    logger.error("Failed to instantiate template", error as Error);
    throw error;
  }
}

/**
 * Record that a template was used
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
    logger.error("Failed to record template usage", error as Error);
    return 0;
  }
}

/**
 * Record the outcome of a template usage
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
  } catch (error) {
    logger.error("Failed to record template outcome", error as Error);
  }
}

// ============================================================================
// Template Lifecycle
// ============================================================================

/**
 * Retire underperforming templates
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
    logger.error("Failed to retire underperforming templates", error as Error);
    return 0;
  }
}

/**
 * Reactivate a retired template
 */
export async function reactivateTemplate(workspacePath: string, templateId: number): Promise<void> {
  try {
    await invoke("reactivate_template", { workspacePath, templateId });
  } catch (error) {
    logger.error("Failed to reactivate template", error as Error);
    throw error;
  }
}

// ============================================================================
// Formatting Utilities
// ============================================================================

/**
 * Format template success rate for display
 */
export function formatTemplateSuccessRate(template: PromptTemplate): string {
  if (template.times_used === 0) return "No data";
  return `${Math.round(template.success_rate * 100)}%`;
}

/**
 * Get status badge for a template
 */
export function getTemplateStatusBadge(template: PromptTemplate): {
  label: string;
  colorClass: string;
} {
  if (!template.active) {
    return {
      label: "Retired",
      colorClass: "bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-400",
    };
  }
  if (template.times_used === 0) {
    return {
      label: "New",
      colorClass: "bg-blue-100 text-blue-700 dark:bg-blue-900 dark:text-blue-300",
    };
  }
  if (template.success_rate >= 0.8) {
    return {
      label: "Top Performer",
      colorClass: "bg-green-100 text-green-700 dark:bg-green-900 dark:text-green-300",
    };
  }
  if (template.success_rate < template.retirement_threshold) {
    return {
      label: "At Risk",
      colorClass: "bg-yellow-100 text-yellow-700 dark:bg-yellow-900 dark:text-yellow-300",
    };
  }
  return {
    label: "Active",
    colorClass: "bg-green-100 text-green-700 dark:bg-green-900 dark:text-green-300",
  };
}

/**
 * Get category display name
 */
export function getCategoryDisplayName(category: string): string {
  const names: Record<string, string> = {
    feature: "Feature Request",
    bugfix: "Bug Fix",
    refactor: "Refactoring",
    test: "Testing",
    docs: "Documentation",
    config: "Configuration",
    review: "Code Review",
    general: "General",
  };
  return names[category] ?? category.charAt(0).toUpperCase() + category.slice(1);
}

/**
 * Get category color class
 */
export function getCategoryColorClass(category: string): string {
  const colors: Record<string, string> = {
    feature: "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300",
    bugfix: "bg-red-100 text-red-700 dark:bg-red-900 dark:text-red-300",
    refactor: "bg-orange-100 text-orange-700 dark:bg-orange-900 dark:text-orange-300",
    test: "bg-blue-100 text-blue-700 dark:bg-blue-900 dark:text-blue-300",
    docs: "bg-teal-100 text-teal-700 dark:bg-teal-900 dark:text-teal-300",
    config: "bg-gray-100 text-gray-700 dark:bg-gray-800 dark:text-gray-300",
    review: "bg-yellow-100 text-yellow-700 dark:bg-yellow-900 dark:text-yellow-300",
    general: "bg-gray-100 text-gray-700 dark:bg-gray-800 dark:text-gray-300",
  };
  return colors[category] ?? "bg-gray-100 text-gray-700 dark:bg-gray-800 dark:text-gray-300";
}
