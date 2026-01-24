/**
 * Tests for Agent Metrics Module
 */

import { describe, expect, it } from "vitest";
import {
  formatCurrency,
  formatNumber,
  formatPercent,
  formatTokens,
  getRoleDisplayName,
  getSuccessRateColor,
  getTimeRangeLabel,
} from "./agent-metrics";

describe("agent-metrics", () => {
  describe("formatNumber", () => {
    it("should format numbers with commas", () => {
      expect(formatNumber(1000)).toBe("1,000");
      expect(formatNumber(1000000)).toBe("1,000,000");
      expect(formatNumber(42)).toBe("42");
    });
  });

  describe("formatCurrency", () => {
    it("should format currency with dollar sign and 2 decimals", () => {
      expect(formatCurrency(12.5)).toBe("$12.50");
      expect(formatCurrency(0)).toBe("$0.00");
      expect(formatCurrency(100.999)).toBe("$101.00");
    });
  });

  describe("formatPercent", () => {
    it("should format rate as percentage with 1 decimal", () => {
      expect(formatPercent(0.85)).toBe("85.0%");
      expect(formatPercent(1.0)).toBe("100.0%");
      expect(formatPercent(0)).toBe("0.0%");
    });
  });

  describe("formatTokens", () => {
    it("should format small numbers as-is", () => {
      expect(formatTokens(500)).toBe("500");
      expect(formatTokens(999)).toBe("999");
    });

    it("should format thousands with K suffix", () => {
      expect(formatTokens(1000)).toBe("1.0K");
      expect(formatTokens(5500)).toBe("5.5K");
      expect(formatTokens(999999)).toBe("1000.0K");
    });

    it("should format millions with M suffix", () => {
      expect(formatTokens(1000000)).toBe("1.0M");
      expect(formatTokens(2500000)).toBe("2.5M");
    });
  });

  describe("getTimeRangeLabel", () => {
    it("should return correct labels for time ranges", () => {
      expect(getTimeRangeLabel("today")).toBe("Today");
      expect(getTimeRangeLabel("week")).toBe("This Week");
      expect(getTimeRangeLabel("month")).toBe("This Month");
      expect(getTimeRangeLabel("all")).toBe("All Time");
    });
  });

  describe("getSuccessRateColor", () => {
    it("should return green for high success rates", () => {
      expect(getSuccessRateColor(0.95)).toContain("green");
      expect(getSuccessRateColor(1.0)).toContain("green");
    });

    it("should return yellow for medium success rates", () => {
      expect(getSuccessRateColor(0.8)).toContain("yellow");
      expect(getSuccessRateColor(0.75)).toContain("yellow");
    });

    it("should return red for low success rates", () => {
      expect(getSuccessRateColor(0.5)).toContain("red");
      expect(getSuccessRateColor(0)).toContain("red");
    });
  });

  describe("getRoleDisplayName", () => {
    it("should return formatted display names for known roles", () => {
      expect(getRoleDisplayName("builder")).toBe("Builder");
      expect(getRoleDisplayName("judge")).toBe("Judge");
      expect(getRoleDisplayName("loom")).toBe("Loom Daemon");
    });

    it("should return the original role for unknown roles", () => {
      expect(getRoleDisplayName("custom-role")).toBe("custom-role");
    });

    it("should be case-insensitive", () => {
      expect(getRoleDisplayName("BUILDER")).toBe("Builder");
      expect(getRoleDisplayName("Judge")).toBe("Judge");
    });
  });
});
