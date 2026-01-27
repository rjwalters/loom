/**
 * Tests for Prompt Library Modal
 *
 * Tests the UI components and filtering logic for the prompt library browser.
 * These tests focus on the exported functions and their behavior.
 */

import { describe, expect, it } from "vitest";

// Import the utility functions from template-generation that we use in the modal
import {
  calculateTemplateHealth,
  formatSuccessRate,
  formatTemplate,
  getCategoryBadgeClass,
  getHealthColorClass,
  getHealthRating,
  getSuccessRateColorClass,
  type PromptTemplate,
} from "./template-generation";

// Helper to create a mock template for testing
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

describe("prompt library modal utility functions", () => {
  describe("template formatting", () => {
    it("should format template with placeholders highlighted", () => {
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

  describe("success rate formatting", () => {
    it("should format success rates as percentages", () => {
      expect(formatSuccessRate(0.75)).toBe("75.0%");
      expect(formatSuccessRate(0.857)).toBe("85.7%");
      expect(formatSuccessRate(1.0)).toBe("100.0%");
      expect(formatSuccessRate(0)).toBe("0.0%");
    });
  });

  describe("success rate color classes", () => {
    it("should return green for high success rates (>= 80%)", () => {
      expect(getSuccessRateColorClass(0.85)).toContain("green");
      expect(getSuccessRateColorClass(1.0)).toContain("green");
    });

    it("should return yellow for medium success rates (60-79%)", () => {
      expect(getSuccessRateColorClass(0.65)).toContain("yellow");
    });

    it("should return orange for low-medium success rates (40-59%)", () => {
      expect(getSuccessRateColorClass(0.45)).toContain("orange");
    });

    it("should return red for low success rates (< 40%)", () => {
      expect(getSuccessRateColorClass(0.2)).toContain("red");
    });
  });

  describe("category badge classes", () => {
    it("should return appropriate colors for each category", () => {
      expect(getCategoryBadgeClass("build")).toContain("blue");
      expect(getCategoryBadgeClass("fix")).toContain("red");
      expect(getCategoryBadgeClass("refactor")).toContain("purple");
      expect(getCategoryBadgeClass("review")).toContain("green");
      expect(getCategoryBadgeClass("curate")).toContain("yellow");
      expect(getCategoryBadgeClass("unknown")).toContain("gray");
    });
  });

  describe("template health scoring", () => {
    it("should return neutral score (50) for unused templates", () => {
      const template = createMockTemplate({ times_used: 0 });
      expect(calculateTemplateHealth(template)).toBe(50);
    });

    it("should score higher for better success rates", () => {
      const highSuccess = createMockTemplate({ success_rate: 0.9, times_used: 10 });
      const lowSuccess = createMockTemplate({ success_rate: 0.3, times_used: 10 });
      expect(calculateTemplateHealth(highSuccess)).toBeGreaterThan(
        calculateTemplateHealth(lowSuccess)
      );
    });

    it("should consider usage count in scoring", () => {
      const highUse = createMockTemplate({ times_used: 100, success_rate: 0.7 });
      const lowUse = createMockTemplate({ times_used: 2, success_rate: 0.7 });
      expect(calculateTemplateHealth(highUse)).toBeGreaterThan(calculateTemplateHealth(lowUse));
    });
  });

  describe("health rating classification", () => {
    it("should classify scores correctly", () => {
      expect(getHealthRating(80)).toBe("excellent");
      expect(getHealthRating(95)).toBe("excellent");
      expect(getHealthRating(60)).toBe("good");
      expect(getHealthRating(79)).toBe("good");
      expect(getHealthRating(40)).toBe("fair");
      expect(getHealthRating(59)).toBe("fair");
      expect(getHealthRating(39)).toBe("poor");
      expect(getHealthRating(0)).toBe("poor");
    });
  });

  describe("health color classes", () => {
    it("should return appropriate colors for each rating", () => {
      expect(getHealthColorClass("excellent")).toContain("green");
      expect(getHealthColorClass("good")).toContain("blue");
      expect(getHealthColorClass("fair")).toContain("yellow");
      expect(getHealthColorClass("poor")).toContain("red");
      expect(getHealthColorClass("unknown")).toContain("gray");
    });
  });
});

describe("filter and sort logic", () => {
  const mockTemplates = [
    createMockTemplate({ id: 1, category: "fix", success_rate: 0.9, times_used: 5 }),
    createMockTemplate({ id: 2, category: "build", success_rate: 0.7, times_used: 20 }),
    createMockTemplate({ id: 3, category: "fix", success_rate: 0.5, times_used: 15 }),
    createMockTemplate({ id: 4, category: "refactor", success_rate: 0.85, times_used: 10 }),
  ];

  describe("sorting by success rate", () => {
    it("should order templates by success rate descending", () => {
      const sorted = [...mockTemplates].sort((a, b) => b.success_rate - a.success_rate);
      expect(sorted[0].id).toBe(1); // 90%
      expect(sorted[1].id).toBe(4); // 85%
      expect(sorted[2].id).toBe(2); // 70%
      expect(sorted[3].id).toBe(3); // 50%
    });
  });

  describe("sorting by most used", () => {
    it("should order templates by usage count descending", () => {
      const sorted = [...mockTemplates].sort((a, b) => b.times_used - a.times_used);
      expect(sorted[0].id).toBe(2); // 20 uses
      expect(sorted[1].id).toBe(3); // 15 uses
      expect(sorted[2].id).toBe(4); // 10 uses
      expect(sorted[3].id).toBe(1); // 5 uses
    });
  });

  describe("filtering by category", () => {
    it("should filter to only matching category", () => {
      const filtered = mockTemplates.filter((t) => t.category === "fix");
      expect(filtered).toHaveLength(2);
      expect(filtered.every((t) => t.category === "fix")).toBe(true);
    });
  });

  describe("filtering by minimum success rate", () => {
    it("should filter templates below threshold", () => {
      const minRate = 0.8;
      const filtered = mockTemplates.filter((t) => t.success_rate >= minRate);
      expect(filtered).toHaveLength(2);
      expect(filtered.every((t) => t.success_rate >= minRate)).toBe(true);
    });
  });

  describe("search filtering", () => {
    it("should filter by search query in template text", () => {
      const templates = [
        createMockTemplate({ id: 1, template_text: "Fix authentication bug" }),
        createMockTemplate({ id: 2, template_text: "Add new feature" }),
        createMockTemplate({ id: 3, template_text: "Fix login issue" }),
      ];

      const query = "fix";
      const filtered = templates.filter((t) =>
        t.template_text.toLowerCase().includes(query.toLowerCase())
      );

      expect(filtered).toHaveLength(2);
      expect(filtered.map((t) => t.id)).toContain(1);
      expect(filtered.map((t) => t.id)).toContain(3);
    });

    it("should filter by placeholder names", () => {
      const templates = [
        createMockTemplate({ id: 1, placeholders: ["issue_number", "file_path"] }),
        createMockTemplate({ id: 2, placeholders: ["function_name"] }),
        createMockTemplate({ id: 3, placeholders: ["issue_number"] }),
      ];

      const query = "issue";
      const filtered = templates.filter((t) =>
        t.placeholders.some((p) => p.toLowerCase().includes(query.toLowerCase()))
      );

      expect(filtered).toHaveLength(2);
      expect(filtered.map((t) => t.id)).toContain(1);
      expect(filtered.map((t) => t.id)).toContain(3);
    });
  });
});

describe("pagination logic", () => {
  const PAGE_SIZE = 10;

  describe("page calculation", () => {
    it("should calculate correct number of pages", () => {
      expect(Math.ceil(5 / PAGE_SIZE)).toBe(1);
      expect(Math.ceil(10 / PAGE_SIZE)).toBe(1);
      expect(Math.ceil(11 / PAGE_SIZE)).toBe(2);
      expect(Math.ceil(25 / PAGE_SIZE)).toBe(3);
    });
  });

  describe("page slicing", () => {
    it("should return correct items for first page", () => {
      const items = Array.from({ length: 25 }, (_, i) => i);
      const currentPage = 0;
      const startIndex = currentPage * PAGE_SIZE;
      const endIndex = startIndex + PAGE_SIZE;
      const pageItems = items.slice(startIndex, endIndex);

      expect(pageItems).toHaveLength(10);
      expect(pageItems[0]).toBe(0);
      expect(pageItems[9]).toBe(9);
    });

    it("should return correct items for middle page", () => {
      const items = Array.from({ length: 25 }, (_, i) => i);
      const currentPage = 1;
      const startIndex = currentPage * PAGE_SIZE;
      const endIndex = startIndex + PAGE_SIZE;
      const pageItems = items.slice(startIndex, endIndex);

      expect(pageItems).toHaveLength(10);
      expect(pageItems[0]).toBe(10);
      expect(pageItems[9]).toBe(19);
    });

    it("should return partial page for last page", () => {
      const items = Array.from({ length: 25 }, (_, i) => i);
      const currentPage = 2;
      const startIndex = currentPage * PAGE_SIZE;
      const endIndex = startIndex + PAGE_SIZE;
      const pageItems = items.slice(startIndex, endIndex);

      expect(pageItems).toHaveLength(5);
      expect(pageItems[0]).toBe(20);
      expect(pageItems[4]).toBe(24);
    });
  });
});

describe("date formatting", () => {
  describe("relative date calculation", () => {
    it("should calculate days difference correctly", () => {
      const now = new Date();
      const yesterday = new Date(now.getTime() - 1000 * 60 * 60 * 24);
      const lastWeek = new Date(now.getTime() - 1000 * 60 * 60 * 24 * 7);

      const diffYesterday = Math.floor(
        (now.getTime() - yesterday.getTime()) / (1000 * 60 * 60 * 24)
      );
      const diffLastWeek = Math.floor((now.getTime() - lastWeek.getTime()) / (1000 * 60 * 60 * 24));

      expect(diffYesterday).toBe(1);
      expect(diffLastWeek).toBe(7);
    });
  });
});
