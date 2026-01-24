/**
 * Tests for Template Generation Module
 */

import { describe, expect, it } from "vitest";
import {
  calculateTemplateHealth,
  describeTemplate,
  formatSuccessRate,
  formatTemplate,
  getCategoryBadgeClass,
  getHealthColorClass,
  getHealthRating,
  getPlaceholderIcon,
  getSuccessRateColorClass,
  isRetirementCandidate,
  type PromptTemplate,
} from "./template-generation";

// Helper to create a mock template
function createMockTemplate(overrides: Partial<PromptTemplate> = {}): PromptTemplate {
  return {
    id: 1,
    template_text: "Fix issue {issue_number} in {file_path}",
    category: "fix",
    placeholders: ["issue_number", "file_path"],
    source_pattern_count: 5,
    source_success_rate: 0.8,
    times_used: 10,
    success_rate: 0.75,
    success_count: 8,
    failure_count: 2,
    active: true,
    retirement_threshold: 0.3,
    created_at: "2026-01-01T00:00:00Z",
    last_used_at: "2026-01-20T00:00:00Z",
    description: "Fixes issues in specified files",
    example: "Fix issue #123 in src/auth.ts",
    ...overrides,
  };
}

describe("template-generation utilities", () => {
  describe("formatTemplate", () => {
    it("should highlight placeholders with brackets", () => {
      const template = createMockTemplate({
        template_text: "Fix issue {issue_number} in {file_path}",
        placeholders: ["issue_number", "file_path"],
      });
      const result = formatTemplate(template);
      expect(result).toBe("Fix issue [issue_number] in [file_path]");
    });

    it("should handle templates without placeholders", () => {
      const template = createMockTemplate({
        template_text: "Run all tests",
        placeholders: [],
      });
      const result = formatTemplate(template);
      expect(result).toBe("Run all tests");
    });
  });

  describe("formatSuccessRate", () => {
    it("should format as percentage with one decimal", () => {
      expect(formatSuccessRate(0.75)).toBe("75.0%");
      expect(formatSuccessRate(0.857)).toBe("85.7%");
      expect(formatSuccessRate(1.0)).toBe("100.0%");
      expect(formatSuccessRate(0)).toBe("0.0%");
    });
  });

  describe("getSuccessRateColorClass", () => {
    it("should return green for high rates", () => {
      expect(getSuccessRateColorClass(0.85)).toContain("green");
      expect(getSuccessRateColorClass(1.0)).toContain("green");
    });

    it("should return yellow for medium rates", () => {
      expect(getSuccessRateColorClass(0.65)).toContain("yellow");
    });

    it("should return orange for low-medium rates", () => {
      expect(getSuccessRateColorClass(0.45)).toContain("orange");
    });

    it("should return red for low rates", () => {
      expect(getSuccessRateColorClass(0.2)).toContain("red");
    });
  });

  describe("getCategoryBadgeClass", () => {
    it("should return blue for build category", () => {
      expect(getCategoryBadgeClass("build")).toContain("blue");
    });

    it("should return red for fix category", () => {
      expect(getCategoryBadgeClass("fix")).toContain("red");
    });

    it("should return purple for refactor category", () => {
      expect(getCategoryBadgeClass("refactor")).toContain("purple");
    });

    it("should return green for review category", () => {
      expect(getCategoryBadgeClass("review")).toContain("green");
    });

    it("should return gray for unknown categories", () => {
      expect(getCategoryBadgeClass("unknown")).toContain("gray");
    });
  });

  describe("getPlaceholderIcon", () => {
    it("should return # for issue_number", () => {
      expect(getPlaceholderIcon("issue_number")).toBe("#");
    });

    it("should return folder for file_path", () => {
      expect(getPlaceholderIcon("file_path")).toBe("folder");
    });

    it("should return edit for unknown placeholders", () => {
      expect(getPlaceholderIcon("custom")).toBe("edit");
    });
  });

  describe("describeTemplate", () => {
    it("should describe fix templates", () => {
      const template = createMockTemplate({ category: "fix" });
      const desc = describeTemplate(template);
      expect(desc).toContain("Fixes");
      expect(desc).toContain("issue number");
      expect(desc).toContain("file path");
    });

    it("should describe build templates", () => {
      const template = createMockTemplate({ category: "build" });
      const desc = describeTemplate(template);
      expect(desc).toContain("Creates");
    });

    it("should describe templates without placeholders", () => {
      const template = createMockTemplate({
        category: "build",
        placeholders: [],
      });
      const desc = describeTemplate(template);
      expect(desc).toContain("proven pattern");
      expect(desc).toContain("80.0%");
    });
  });

  describe("isRetirementCandidate", () => {
    it("should return true for underperforming templates", () => {
      const template = createMockTemplate({
        active: true,
        times_used: 10,
        success_rate: 0.25,
        retirement_threshold: 0.3,
      });
      expect(isRetirementCandidate(template)).toBe(true);
    });

    it("should return false for well-performing templates", () => {
      const template = createMockTemplate({
        active: true,
        times_used: 10,
        success_rate: 0.75,
        retirement_threshold: 0.3,
      });
      expect(isRetirementCandidate(template)).toBe(false);
    });

    it("should return false for inactive templates", () => {
      const template = createMockTemplate({
        active: false,
        times_used: 10,
        success_rate: 0.1,
        retirement_threshold: 0.3,
      });
      expect(isRetirementCandidate(template)).toBe(false);
    });

    it("should return false for templates with too few uses", () => {
      const template = createMockTemplate({
        active: true,
        times_used: 3,
        success_rate: 0.1,
        retirement_threshold: 0.3,
      });
      expect(isRetirementCandidate(template)).toBe(false);
    });
  });

  describe("calculateTemplateHealth", () => {
    it("should return 50 for unused templates", () => {
      const template = createMockTemplate({ times_used: 0 });
      expect(calculateTemplateHealth(template)).toBe(50);
    });

    it("should return higher score for high success rate", () => {
      const highSuccess = createMockTemplate({ success_rate: 0.9, times_used: 10 });
      const lowSuccess = createMockTemplate({ success_rate: 0.3, times_used: 10 });
      expect(calculateTemplateHealth(highSuccess)).toBeGreaterThan(
        calculateTemplateHealth(lowSuccess)
      );
    });

    it("should consider usage in score", () => {
      const highUse = createMockTemplate({ times_used: 100, success_rate: 0.7 });
      const lowUse = createMockTemplate({ times_used: 2, success_rate: 0.7 });
      expect(calculateTemplateHealth(highUse)).toBeGreaterThan(calculateTemplateHealth(lowUse));
    });
  });

  describe("getHealthRating", () => {
    it("should return excellent for 80+", () => {
      expect(getHealthRating(80)).toBe("excellent");
      expect(getHealthRating(95)).toBe("excellent");
    });

    it("should return good for 60-79", () => {
      expect(getHealthRating(60)).toBe("good");
      expect(getHealthRating(79)).toBe("good");
    });

    it("should return fair for 40-59", () => {
      expect(getHealthRating(40)).toBe("fair");
      expect(getHealthRating(59)).toBe("fair");
    });

    it("should return poor for under 40", () => {
      expect(getHealthRating(39)).toBe("poor");
      expect(getHealthRating(0)).toBe("poor");
    });
  });

  describe("getHealthColorClass", () => {
    it("should return green for excellent", () => {
      expect(getHealthColorClass("excellent")).toContain("green");
    });

    it("should return blue for good", () => {
      expect(getHealthColorClass("good")).toContain("blue");
    });

    it("should return yellow for fair", () => {
      expect(getHealthColorClass("fair")).toContain("yellow");
    });

    it("should return red for poor", () => {
      expect(getHealthColorClass("poor")).toContain("red");
    });

    it("should return gray for unknown", () => {
      expect(getHealthColorClass("unknown")).toContain("gray");
    });
  });
});
