import { describe, expect, it } from "vitest";
import {
  formatCorrelation,
  formatPValue,
  getCorrelationColorClass,
  getDayName,
  getSuccessRateColorClass,
  getTimeBucketName,
  interpretCorrelation,
  isSignificant,
} from "./correlation-analysis";

// Note: Full integration tests for Tauri invoke functions would require
// mocking @tauri-apps/api/core. These tests focus on the utility functions.

describe("Correlation Analysis Utilities", () => {
  describe("formatCorrelation", () => {
    it("should format positive correlations with + sign", () => {
      expect(formatCorrelation(0.5)).toBe("+0.500");
      expect(formatCorrelation(0.123)).toBe("+0.123");
    });

    it("should format negative correlations without extra sign", () => {
      expect(formatCorrelation(-0.5)).toBe("-0.500");
      expect(formatCorrelation(-0.789)).toBe("-0.789");
    });

    it("should format zero correctly", () => {
      expect(formatCorrelation(0)).toBe("+0.000");
    });
  });

  describe("formatPValue", () => {
    it("should format very small p-values", () => {
      expect(formatPValue(0.0001)).toBe("< 0.001");
      expect(formatPValue(0.0005)).toBe("< 0.001");
    });

    it("should format small p-values", () => {
      expect(formatPValue(0.005)).toBe("< 0.01");
      expect(formatPValue(0.009)).toBe("< 0.01");
    });

    it("should format moderate p-values", () => {
      expect(formatPValue(0.03)).toBe("< 0.05");
      expect(formatPValue(0.049)).toBe("< 0.05");
    });

    it("should format large p-values with precision", () => {
      expect(formatPValue(0.1)).toBe("0.100");
      expect(formatPValue(0.567)).toBe("0.567");
    });
  });

  describe("interpretCorrelation", () => {
    it("should identify strong correlations", () => {
      expect(interpretCorrelation(0.8)).toBe("strong");
      expect(interpretCorrelation(-0.75)).toBe("strong");
    });

    it("should identify moderate correlations", () => {
      expect(interpretCorrelation(0.5)).toBe("moderate");
      expect(interpretCorrelation(-0.45)).toBe("moderate");
    });

    it("should identify weak correlations", () => {
      expect(interpretCorrelation(0.25)).toBe("weak");
      expect(interpretCorrelation(-0.3)).toBe("weak");
    });

    it("should identify negligible correlations", () => {
      expect(interpretCorrelation(0.1)).toBe("negligible");
      expect(interpretCorrelation(-0.05)).toBe("negligible");
    });
  });

  describe("isSignificant", () => {
    it("should identify significant p-values at default alpha", () => {
      expect(isSignificant(0.01)).toBe(true);
      expect(isSignificant(0.049)).toBe(true);
    });

    it("should identify non-significant p-values at default alpha", () => {
      expect(isSignificant(0.05)).toBe(false);
      expect(isSignificant(0.1)).toBe(false);
    });

    it("should respect custom alpha levels", () => {
      expect(isSignificant(0.05, 0.1)).toBe(true);
      expect(isSignificant(0.02, 0.01)).toBe(false);
    });
  });

  describe("getDayName", () => {
    it("should return correct day names", () => {
      expect(getDayName(0)).toBe("Sunday");
      expect(getDayName(1)).toBe("Monday");
      expect(getDayName(6)).toBe("Saturday");
    });

    it("should handle invalid day numbers", () => {
      expect(getDayName(7)).toBe("Unknown");
      expect(getDayName(-1)).toBe("Unknown");
    });
  });

  describe("getTimeBucketName", () => {
    it("should identify morning hours", () => {
      expect(getTimeBucketName(6)).toBe("Morning");
      expect(getTimeBucketName(9)).toBe("Morning");
      expect(getTimeBucketName(11)).toBe("Morning");
    });

    it("should identify afternoon hours", () => {
      expect(getTimeBucketName(12)).toBe("Afternoon");
      expect(getTimeBucketName(15)).toBe("Afternoon");
      expect(getTimeBucketName(17)).toBe("Afternoon");
    });

    it("should identify evening hours", () => {
      expect(getTimeBucketName(18)).toBe("Evening");
      expect(getTimeBucketName(20)).toBe("Evening");
      expect(getTimeBucketName(21)).toBe("Evening");
    });

    it("should identify night hours", () => {
      expect(getTimeBucketName(22)).toBe("Night");
      expect(getTimeBucketName(0)).toBe("Night");
      expect(getTimeBucketName(5)).toBe("Night");
    });
  });

  describe("getCorrelationColorClass", () => {
    it("should return green for strong positive correlations", () => {
      expect(getCorrelationColorClass(0.8)).toBe("text-green-600");
    });

    it("should return red for strong negative correlations", () => {
      expect(getCorrelationColorClass(-0.8)).toBe("text-red-600");
    });

    it("should return moderate colors for moderate correlations", () => {
      expect(getCorrelationColorClass(0.5)).toBe("text-green-500");
      expect(getCorrelationColorClass(-0.5)).toBe("text-red-500");
    });

    it("should return weak colors for weak correlations", () => {
      expect(getCorrelationColorClass(0.25)).toBe("text-green-400");
      expect(getCorrelationColorClass(-0.25)).toBe("text-red-400");
    });

    it("should return gray for negligible correlations", () => {
      expect(getCorrelationColorClass(0.1)).toBe("text-gray-500");
      expect(getCorrelationColorClass(-0.05)).toBe("text-gray-500");
    });
  });

  describe("getSuccessRateColorClass", () => {
    it("should return green for high success rates", () => {
      expect(getSuccessRateColorClass(0.9)).toBe("text-green-600 dark:text-green-400");
      expect(getSuccessRateColorClass(0.8)).toBe("text-green-600 dark:text-green-400");
    });

    it("should return yellow for moderate success rates", () => {
      expect(getSuccessRateColorClass(0.7)).toBe("text-yellow-600 dark:text-yellow-400");
      expect(getSuccessRateColorClass(0.6)).toBe("text-yellow-600 dark:text-yellow-400");
    });

    it("should return orange for low-moderate success rates", () => {
      expect(getSuccessRateColorClass(0.5)).toBe("text-orange-600 dark:text-orange-400");
      expect(getSuccessRateColorClass(0.4)).toBe("text-orange-600 dark:text-orange-400");
    });

    it("should return red for low success rates", () => {
      expect(getSuccessRateColorClass(0.3)).toBe("text-red-600 dark:text-red-400");
      expect(getSuccessRateColorClass(0.1)).toBe("text-red-600 dark:text-red-400");
    });
  });
});
