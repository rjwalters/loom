/**
 * Tests for the Self-Tuning module
 */

import { describe, expect, it } from "vitest";
import {
  DEFAULT_TUNING_CONFIG,
  formatChangePercent,
  formatConfidence,
  formatParameterValue,
  getConfidenceColor,
  getProposalStatusColor,
  getProposalStatusLabel,
  type ProposalStatus,
} from "./self-tuning";

describe("self-tuning", () => {
  describe("DEFAULT_TUNING_CONFIG", () => {
    it("has conservative safety settings", () => {
      expect(DEFAULT_TUNING_CONFIG.maxAdjustmentPercent).toBe(10.0);
      expect(DEFAULT_TUNING_CONFIG.approvalThresholdPercent).toBe(20.0);
      expect(DEFAULT_TUNING_CONFIG.minSampleSize).toBe(10);
      expect(DEFAULT_TUNING_CONFIG.minAutoApprovalConfidence).toBe(0.8);
      expect(DEFAULT_TUNING_CONFIG.rollbackThresholdPercent).toBe(15.0);
      expect(DEFAULT_TUNING_CONFIG.observationPeriodHours).toBe(24);
    });
  });

  describe("formatParameterValue", () => {
    it("formats milliseconds correctly", () => {
      expect(formatParameterValue(500, "ms")).toBe("500ms");
      expect(formatParameterValue(1500, "ms")).toBe("1.5s");
      expect(formatParameterValue(90000, "ms")).toBe("1.5m");
      expect(formatParameterValue(5400000, "ms")).toBe("1.5h");
    });

    it("formats ratios as percentages", () => {
      expect(formatParameterValue(0.85, "ratio")).toBe("85.0%");
      expect(formatParameterValue(0.5, "percent")).toBe("50.0%");
    });

    it("formats counts as integers", () => {
      expect(formatParameterValue(3.7, "count")).toBe("4");
      expect(formatParameterValue(5.0, "count")).toBe("5");
    });

    it("formats unknown units with two decimals", () => {
      expect(formatParameterValue(3.14159, "unknown")).toBe("3.14");
    });
  });

  describe("getProposalStatusColor", () => {
    const testCases: [ProposalStatus, string][] = [
      ["pending", "text-yellow-600 dark:text-yellow-400"],
      ["approved", "text-blue-600 dark:text-blue-400"],
      ["applied", "text-green-600 dark:text-green-400"],
      ["rejected", "text-gray-600 dark:text-gray-400"],
      ["rolled_back", "text-red-600 dark:text-red-400"],
    ];

    it.each(testCases)("returns correct color for %s status", (status, expected) => {
      expect(getProposalStatusColor(status)).toBe(expected);
    });
  });

  describe("getProposalStatusLabel", () => {
    const testCases: [ProposalStatus, string][] = [
      ["pending", "Pending"],
      ["approved", "Approved"],
      ["applied", "Applied"],
      ["rejected", "Rejected"],
      ["rolled_back", "Rolled Back"],
    ];

    it.each(testCases)("returns correct label for %s status", (status, expected) => {
      expect(getProposalStatusLabel(status)).toBe(expected);
    });
  });

  describe("formatConfidence", () => {
    it("formats confidence as percentage", () => {
      expect(formatConfidence(0.85)).toBe("85%");
      expect(formatConfidence(0.5)).toBe("50%");
      expect(formatConfidence(1.0)).toBe("100%");
    });
  });

  describe("getConfidenceColor", () => {
    it("returns green for high confidence", () => {
      expect(getConfidenceColor(0.9)).toBe("text-green-600 dark:text-green-400");
      expect(getConfidenceColor(0.8)).toBe("text-green-600 dark:text-green-400");
    });

    it("returns yellow for medium confidence", () => {
      expect(getConfidenceColor(0.7)).toBe("text-yellow-600 dark:text-yellow-400");
      expect(getConfidenceColor(0.6)).toBe("text-yellow-600 dark:text-yellow-400");
    });

    it("returns red for low confidence", () => {
      expect(getConfidenceColor(0.5)).toBe("text-red-600 dark:text-red-400");
      expect(getConfidenceColor(0.3)).toBe("text-red-600 dark:text-red-400");
    });
  });

  describe("formatChangePercent", () => {
    it("adds plus sign for positive changes", () => {
      expect(formatChangePercent(5.5)).toBe("+5.5%");
      expect(formatChangePercent(0.1)).toBe("+0.1%");
    });

    it("keeps minus sign for negative changes", () => {
      expect(formatChangePercent(-5.5)).toBe("-5.5%");
      expect(formatChangePercent(-0.1)).toBe("-0.1%");
    });

    it("handles zero correctly", () => {
      expect(formatChangePercent(0)).toBe("+0.0%");
    });
  });
});
