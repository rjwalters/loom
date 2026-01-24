import { describe, expect, it } from "vitest";
import {
  calculateQualityScore,
  formatConfidence,
  formatImprovement,
  getConfidenceColorClass,
  getIssueIcon,
  getIssueLabel,
  getOptimizationTypeName,
  getQualityColorClass,
  getQualityRating,
  getSeverityColorClass,
  getTypeBadgeClass,
  type PromptAnalysis,
} from "./prompt-optimization";

// Note: Full integration tests for Tauri invoke functions would require
// mocking @tauri-apps/api/core. These tests focus on the utility functions.

describe("Prompt Optimization Utilities", () => {
  describe("formatConfidence", () => {
    it("should format confidence as percentage", () => {
      expect(formatConfidence(0.5)).toBe("50%");
      expect(formatConfidence(0.75)).toBe("75%");
      expect(formatConfidence(1.0)).toBe("100%");
    });

    it("should round to nearest integer", () => {
      expect(formatConfidence(0.333)).toBe("33%");
      expect(formatConfidence(0.666)).toBe("67%");
    });

    it("should handle edge cases", () => {
      expect(formatConfidence(0)).toBe("0%");
      expect(formatConfidence(0.999)).toBe("100%");
    });
  });

  describe("formatImprovement", () => {
    it("should format positive improvements with + sign", () => {
      expect(formatImprovement(0.15)).toBe("+15%");
      expect(formatImprovement(0.25)).toBe("+25%");
    });

    it("should format negative improvements with - sign", () => {
      expect(formatImprovement(-0.1)).toBe("-10%");
      expect(formatImprovement(-0.05)).toBe("-5%");
    });

    it("should handle zero", () => {
      expect(formatImprovement(0)).toBe("+0%");
    });
  });

  describe("getOptimizationTypeName", () => {
    it("should return correct display names", () => {
      expect(getOptimizationTypeName("length")).toBe("Length Adjustment");
      expect(getOptimizationTypeName("specificity")).toBe("Specificity Enhancement");
      expect(getOptimizationTypeName("structure")).toBe("Structure Improvement");
      expect(getOptimizationTypeName("pattern")).toBe("Pattern Matching");
    });

    it("should return original type for unknown types", () => {
      expect(getOptimizationTypeName("unknown")).toBe("unknown");
      expect(getOptimizationTypeName("custom")).toBe("custom");
    });
  });

  describe("getConfidenceColorClass", () => {
    it("should return green for high confidence", () => {
      expect(getConfidenceColorClass(0.9)).toBe("text-green-600 dark:text-green-400");
      expect(getConfidenceColorClass(0.8)).toBe("text-green-600 dark:text-green-400");
    });

    it("should return yellow for moderate confidence", () => {
      expect(getConfidenceColorClass(0.7)).toBe("text-yellow-600 dark:text-yellow-400");
      expect(getConfidenceColorClass(0.6)).toBe("text-yellow-600 dark:text-yellow-400");
    });

    it("should return orange for low-moderate confidence", () => {
      expect(getConfidenceColorClass(0.5)).toBe("text-orange-600 dark:text-orange-400");
      expect(getConfidenceColorClass(0.4)).toBe("text-orange-600 dark:text-orange-400");
    });

    it("should return red for low confidence", () => {
      expect(getConfidenceColorClass(0.3)).toBe("text-red-600 dark:text-red-400");
      expect(getConfidenceColorClass(0.1)).toBe("text-red-600 dark:text-red-400");
    });
  });

  describe("getSeverityColorClass", () => {
    it("should return correct colors for severities", () => {
      expect(getSeverityColorClass("high")).toBe("text-red-600 dark:text-red-400");
      expect(getSeverityColorClass("medium")).toBe("text-yellow-600 dark:text-yellow-400");
      expect(getSeverityColorClass("low")).toBe("text-blue-600 dark:text-blue-400");
    });

    it("should return gray for unknown severities", () => {
      expect(getSeverityColorClass("unknown")).toBe("text-gray-600 dark:text-gray-400");
    });
  });

  describe("getIssueIcon", () => {
    it("should return correct icons for issue types", () => {
      expect(getIssueIcon("too_short")).toBe("warning");
      expect(getIssueIcon("too_long")).toBe("content_cut");
      expect(getIssueIcon("vague")).toBe("help_outline");
      expect(getIssueIcon("missing_issue_ref")).toBe("link_off");
      expect(getIssueIcon("passive_voice")).toBe("record_voice_over");
      expect(getIssueIcon("missing_test_mention")).toBe("science");
    });

    it("should return info for unknown issue types", () => {
      expect(getIssueIcon("unknown")).toBe("info");
    });
  });

  describe("getIssueLabel", () => {
    it("should return correct labels for issue types", () => {
      expect(getIssueLabel("too_short")).toBe("Too Short");
      expect(getIssueLabel("too_long")).toBe("Too Long");
      expect(getIssueLabel("vague")).toBe("Vague Language");
      expect(getIssueLabel("missing_issue_ref")).toBe("Missing Issue Reference");
      expect(getIssueLabel("passive_voice")).toBe("Passive Voice");
      expect(getIssueLabel("missing_test_mention")).toBe("Missing Test Mention");
    });

    it("should return original type for unknown types", () => {
      expect(getIssueLabel("custom_issue")).toBe("custom_issue");
    });
  });

  describe("getTypeBadgeClass", () => {
    it("should return correct badge classes for types", () => {
      expect(getTypeBadgeClass("length")).toContain("bg-blue-100");
      expect(getTypeBadgeClass("specificity")).toContain("bg-purple-100");
      expect(getTypeBadgeClass("structure")).toContain("bg-orange-100");
      expect(getTypeBadgeClass("pattern")).toContain("bg-green-100");
    });

    it("should return gray for unknown types", () => {
      expect(getTypeBadgeClass("unknown")).toContain("bg-gray-100");
    });
  });

  describe("calculateQualityScore", () => {
    it("should calculate score from analysis", () => {
      const analysis: PromptAnalysis = {
        prompt: "test prompt",
        word_count: 20,
        char_count: 100,
        category: "build",
        specificity_score: 0.8,
        structure_score: 0.7,
        issues: [],
        needs_optimization: false,
      };
      const score = calculateQualityScore(analysis);
      // 0.8 * 0.4 + 0.7 * 0.3 + 1.0 * 0.3 = 0.32 + 0.21 + 0.30 = 0.83
      expect(score).toBeCloseTo(0.83, 2);
    });

    it("should apply issue penalties", () => {
      const analysis: PromptAnalysis = {
        prompt: "test",
        word_count: 1,
        char_count: 4,
        category: null,
        specificity_score: 0.5,
        structure_score: 0.5,
        issues: [{ issue_type: "too_short", description: "Too short", severity: "high" }],
        needs_optimization: true,
      };
      const score = calculateQualityScore(analysis);
      // 0.5 * 0.4 + 0.5 * 0.3 + 0.8 * 0.3 = 0.20 + 0.15 + 0.24 = 0.59
      expect(score).toBeCloseTo(0.59, 2);
    });

    it("should apply multiple issue penalties", () => {
      const analysis: PromptAnalysis = {
        prompt: "fix it",
        word_count: 2,
        char_count: 6,
        category: null,
        specificity_score: 0.3,
        structure_score: 0.4,
        issues: [
          { issue_type: "too_short", description: "Too short", severity: "high" },
          { issue_type: "vague", description: "Vague", severity: "medium" },
          { issue_type: "missing_issue_ref", description: "No ref", severity: "low" },
        ],
        needs_optimization: true,
      };
      const score = calculateQualityScore(analysis);
      // Issue penalty: 0.2 + 0.1 + 0.05 = 0.35, issue score = 0.65
      // 0.3 * 0.4 + 0.4 * 0.3 + 0.65 * 0.3 = 0.12 + 0.12 + 0.195 = 0.435
      expect(score).toBeCloseTo(0.435, 2);
    });
  });

  describe("getQualityRating", () => {
    it("should return excellent for high scores", () => {
      expect(getQualityRating(0.9)).toBe("excellent");
      expect(getQualityRating(0.8)).toBe("excellent");
    });

    it("should return good for moderate-high scores", () => {
      expect(getQualityRating(0.7)).toBe("good");
      expect(getQualityRating(0.6)).toBe("good");
    });

    it("should return fair for moderate scores", () => {
      expect(getQualityRating(0.5)).toBe("fair");
      expect(getQualityRating(0.4)).toBe("fair");
    });

    it("should return poor for low scores", () => {
      expect(getQualityRating(0.3)).toBe("poor");
      expect(getQualityRating(0.1)).toBe("poor");
    });
  });

  describe("getQualityColorClass", () => {
    it("should return correct colors for ratings", () => {
      expect(getQualityColorClass("excellent")).toBe("text-green-600 dark:text-green-400");
      expect(getQualityColorClass("good")).toBe("text-blue-600 dark:text-blue-400");
      expect(getQualityColorClass("fair")).toBe("text-yellow-600 dark:text-yellow-400");
      expect(getQualityColorClass("poor")).toBe("text-red-600 dark:text-red-400");
    });

    it("should return gray for unknown ratings", () => {
      expect(getQualityColorClass("unknown")).toBe("text-gray-600 dark:text-gray-400");
    });
  });
});
