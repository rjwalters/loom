/**
 * Tests for the Success Prediction Module
 */

import { describe, expect, it } from "vitest";
import {
  calculateTotalImprovement,
  describeProbability,
  formatContribution,
  formatProbability,
  getConfidenceColorClass,
  getDirectionIcon,
  getProbabilityColorClass,
  type ModelStats,
  type PredictionFactor,
  type PromptAlternative,
  shouldRetrain,
  summarizeKeyFactors,
} from "./prediction";

describe("Prediction Module", () => {
  describe("formatProbability", () => {
    it("should format probability as percentage", () => {
      expect(formatProbability(0.5)).toBe("50.0%");
      expect(formatProbability(0.756)).toBe("75.6%");
      expect(formatProbability(1.0)).toBe("100.0%");
      expect(formatProbability(0.0)).toBe("0.0%");
    });
  });

  describe("describeProbability", () => {
    it("should describe high probability", () => {
      expect(describeProbability(0.85)).toBe("Very likely to succeed");
      expect(describeProbability(0.9)).toBe("Very likely to succeed");
    });

    it("should describe moderate probability", () => {
      expect(describeProbability(0.65)).toBe("Likely to succeed");
      expect(describeProbability(0.45)).toBe("Moderate chance of success");
    });

    it("should describe low probability", () => {
      expect(describeProbability(0.25)).toBe("May need improvement");
      expect(describeProbability(0.1)).toBe("Low chance of success");
    });
  });

  describe("getProbabilityColorClass", () => {
    it("should return green for high probability", () => {
      expect(getProbabilityColorClass(0.85)).toContain("green-600");
      expect(getProbabilityColorClass(0.65)).toContain("green-500");
    });

    it("should return yellow for moderate probability", () => {
      expect(getProbabilityColorClass(0.45)).toContain("yellow-600");
    });

    it("should return orange/red for low probability", () => {
      expect(getProbabilityColorClass(0.25)).toContain("orange-600");
      expect(getProbabilityColorClass(0.1)).toContain("red-600");
    });
  });

  describe("getConfidenceColorClass", () => {
    it("should return blue for high confidence", () => {
      expect(getConfidenceColorClass(0.9)).toContain("blue-600");
      expect(getConfidenceColorClass(0.6)).toContain("blue-500");
    });

    it("should return gray for low confidence", () => {
      expect(getConfidenceColorClass(0.3)).toContain("gray-500");
    });
  });

  describe("formatContribution", () => {
    it("should format positive contribution with plus sign", () => {
      expect(formatContribution(0.15)).toBe("+15.0%");
      expect(formatContribution(0.05)).toBe("+5.0%");
    });

    it("should format negative contribution", () => {
      expect(formatContribution(-0.1)).toBe("-10.0%");
      expect(formatContribution(-0.25)).toBe("-25.0%");
    });

    it("should format zero contribution", () => {
      expect(formatContribution(0)).toBe("+0.0%");
    });
  });

  describe("getDirectionIcon", () => {
    it("should return + for positive direction", () => {
      expect(getDirectionIcon("positive")).toBe("+");
    });

    it("should return - for negative direction", () => {
      expect(getDirectionIcon("negative")).toBe("-");
    });
  });

  describe("shouldRetrain", () => {
    it("should return true if model is not trained", () => {
      const stats: ModelStats = {
        is_trained: false,
        samples_count: 100,
        last_trained: null,
        accuracy: null,
        coefficients: null,
      };
      expect(shouldRetrain(stats)).toBe(true);
    });

    it("should return true if accuracy is low", () => {
      const stats: ModelStats = {
        is_trained: true,
        samples_count: 100,
        last_trained: new Date().toISOString(),
        accuracy: 0.5,
        coefficients: null,
      };
      expect(shouldRetrain(stats)).toBe(true);
    });

    it("should return true if model is old", () => {
      const oldDate = new Date();
      oldDate.setDate(oldDate.getDate() - 10);
      const stats: ModelStats = {
        is_trained: true,
        samples_count: 100,
        last_trained: oldDate.toISOString(),
        accuracy: 0.8,
        coefficients: null,
      };
      expect(shouldRetrain(stats)).toBe(true);
    });

    it("should return false for recently trained accurate model", () => {
      const stats: ModelStats = {
        is_trained: true,
        samples_count: 100,
        last_trained: new Date().toISOString(),
        accuracy: 0.8,
        coefficients: null,
      };
      expect(shouldRetrain(stats)).toBe(false);
    });
  });

  describe("summarizeKeyFactors", () => {
    it("should return message for empty factors", () => {
      expect(summarizeKeyFactors([])).toBe("No significant factors identified");
    });

    it("should summarize positive factors", () => {
      const factors: PredictionFactor[] = [
        {
          name: "code_block",
          contribution: 0.2,
          direction: "positive",
          explanation: "Has code block",
        },
        {
          name: "file_refs",
          contribution: 0.15,
          direction: "positive",
          explanation: "Has file refs",
        },
      ];
      const summary = summarizeKeyFactors(factors);
      expect(summary).toContain("Helpful");
      expect(summary).toContain("code block");
      expect(summary).toContain("file refs");
    });

    it("should summarize negative factors", () => {
      const factors: PredictionFactor[] = [
        {
          name: "question_count",
          contribution: -0.1,
          direction: "negative",
          explanation: "Too many questions",
        },
      ];
      const summary = summarizeKeyFactors(factors);
      expect(summary).toContain("Could improve");
      expect(summary).toContain("question count");
    });

    it("should summarize mixed factors", () => {
      const factors: PredictionFactor[] = [
        {
          name: "code_block",
          contribution: 0.2,
          direction: "positive",
          explanation: "Has code block",
        },
        {
          name: "question_count",
          contribution: -0.1,
          direction: "negative",
          explanation: "Too many questions",
        },
      ];
      const summary = summarizeKeyFactors(factors);
      expect(summary).toContain("Helpful");
      expect(summary).toContain("Could improve");
    });
  });

  describe("calculateTotalImprovement", () => {
    it("should return 0 for empty alternatives", () => {
      expect(calculateTotalImprovement([])).toBe(0);
    });

    it("should calculate improvement for single alternative", () => {
      const alternatives: PromptAlternative[] = [
        {
          suggestion: "Add file refs",
          predicted_improvement: 0.1,
          reason: "Better specificity",
        },
      ];
      expect(calculateTotalImprovement(alternatives)).toBeCloseTo(0.1, 2);
    });

    it("should apply diminishing returns for multiple alternatives", () => {
      const alternatives: PromptAlternative[] = [
        {
          suggestion: "Add file refs",
          predicted_improvement: 0.1,
          reason: "Better specificity",
        },
        {
          suggestion: "Add code block",
          predicted_improvement: 0.1,
          reason: "Clearer examples",
        },
        {
          suggestion: "Add verbs",
          predicted_improvement: 0.1,
          reason: "Clearer intent",
        },
      ];
      const total = calculateTotalImprovement(alternatives);
      // Should be less than 0.3 due to diminishing returns
      expect(total).toBeLessThan(0.3);
      expect(total).toBeGreaterThan(0.1);
    });
  });
});
