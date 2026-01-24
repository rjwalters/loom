/**
 * A/B Testing Module Tests
 *
 * Tests for the A/B testing framework including formatting utilities,
 * type definitions, and mock API interactions.
 */

import { describe, expect, it } from "vitest";
import {
  type Experiment,
  type ExperimentAnalysis,
  type ExperimentStatus,
  formatConfidence,
  formatEffectSize,
  formatMetricValue,
  formatPValue,
  formatStatus,
  formatSuccessRate,
  getDirectionIndicator,
  getMetricLabel,
  getPValueColor,
  getStatusColor,
  isSignificant,
  type TargetDirection,
  type TargetMetric,
  type Variant,
  type VariantStats,
} from "./ab-testing";

describe("A/B Testing Module", () => {
  describe("formatStatus", () => {
    it("formats draft status", () => {
      expect(formatStatus("draft")).toBe("Draft");
    });

    it("formats active status", () => {
      expect(formatStatus("active")).toBe("Active");
    });

    it("formats concluded status", () => {
      expect(formatStatus("concluded")).toBe("Concluded");
    });

    it("formats cancelled status", () => {
      expect(formatStatus("cancelled")).toBe("Cancelled");
    });
  });

  describe("getStatusColor", () => {
    it("returns gray for draft", () => {
      expect(getStatusColor("draft")).toContain("gray");
    });

    it("returns blue for active", () => {
      expect(getStatusColor("active")).toContain("blue");
    });

    it("returns green for concluded", () => {
      expect(getStatusColor("concluded")).toContain("green");
    });

    it("returns red for cancelled", () => {
      expect(getStatusColor("cancelled")).toContain("red");
    });
  });

  describe("formatPValue", () => {
    it("formats very small p-values", () => {
      expect(formatPValue(0.0001)).toBe("< 0.001");
    });

    it("formats small p-values", () => {
      expect(formatPValue(0.005)).toBe("< 0.01");
    });

    it("formats marginal p-values", () => {
      expect(formatPValue(0.03)).toBe("< 0.05");
    });

    it("formats non-significant p-values", () => {
      expect(formatPValue(0.15)).toBe("0.150");
    });

    it("handles exact thresholds", () => {
      // 0.05 is exactly at the threshold, not below it
      expect(formatPValue(0.05)).toBe("0.050");
      expect(formatPValue(0.051)).toBe("0.051");
      // Just below 0.05 should show < 0.05
      expect(formatPValue(0.049)).toBe("< 0.05");
    });
  });

  describe("formatConfidence", () => {
    it("formats confidence as percentage", () => {
      expect(formatConfidence(0.95)).toBe("95.0%");
      expect(formatConfidence(0.8)).toBe("80.0%");
      expect(formatConfidence(0.999)).toBe("99.9%");
    });
  });

  describe("formatEffectSize", () => {
    it("identifies large effect sizes", () => {
      expect(formatEffectSize(0.9)).toContain("large");
      expect(formatEffectSize(-0.85)).toContain("large");
    });

    it("identifies medium effect sizes", () => {
      expect(formatEffectSize(0.6)).toContain("medium");
      expect(formatEffectSize(-0.55)).toContain("medium");
    });

    it("identifies small effect sizes", () => {
      expect(formatEffectSize(0.3)).toContain("small");
      expect(formatEffectSize(-0.25)).toContain("small");
    });

    it("identifies negligible effect sizes", () => {
      expect(formatEffectSize(0.1)).toContain("negligible");
      expect(formatEffectSize(-0.05)).toContain("negligible");
    });

    it("includes sign in output", () => {
      expect(formatEffectSize(0.5)).toMatch(/^\+0\.50/);
      expect(formatEffectSize(-0.5)).toMatch(/^-0\.50/);
    });
  });

  describe("getPValueColor", () => {
    it("returns green for highly significant", () => {
      expect(getPValueColor(0.005)).toContain("green");
    });

    it("returns yellow for significant", () => {
      expect(getPValueColor(0.03)).toContain("yellow");
    });

    it("returns gray for not significant", () => {
      expect(getPValueColor(0.1)).toContain("gray");
    });
  });

  describe("formatSuccessRate", () => {
    it("formats rates as percentages", () => {
      expect(formatSuccessRate(0.75)).toBe("75.0%");
      expect(formatSuccessRate(0.5)).toBe("50.0%");
      expect(formatSuccessRate(1.0)).toBe("100.0%");
      expect(formatSuccessRate(0)).toBe("0.0%");
    });
  });

  describe("formatMetricValue", () => {
    it("formats success_rate as percentage", () => {
      expect(formatMetricValue(0.85, "success_rate")).toBe("85.0%");
    });

    it("formats short cycle_time in minutes", () => {
      expect(formatMetricValue(0.5, "cycle_time")).toBe("30m");
    });

    it("formats medium cycle_time in hours", () => {
      expect(formatMetricValue(4.5, "cycle_time")).toBe("4.5h");
    });

    it("formats long cycle_time in days", () => {
      expect(formatMetricValue(48, "cycle_time")).toBe("2.0d");
    });

    it("formats cost in dollars", () => {
      expect(formatMetricValue(1.5, "cost")).toBe("$1.50");
      expect(formatMetricValue(0.25, "cost")).toBe("$0.25");
    });

    it("handles null values", () => {
      expect(formatMetricValue(null, "success_rate")).toBe("-");
      expect(formatMetricValue(null, "cycle_time")).toBe("-");
      expect(formatMetricValue(null, "cost")).toBe("-");
    });
  });

  describe("isSignificant", () => {
    it("returns true for p-values below default alpha", () => {
      expect(isSignificant(0.01)).toBe(true);
      expect(isSignificant(0.049)).toBe(true);
    });

    it("returns false for p-values at or above default alpha", () => {
      expect(isSignificant(0.05)).toBe(false);
      expect(isSignificant(0.1)).toBe(false);
    });

    it("respects custom alpha level", () => {
      expect(isSignificant(0.08, 0.1)).toBe(true);
      expect(isSignificant(0.03, 0.01)).toBe(false);
    });
  });

  describe("getMetricLabel", () => {
    it("returns human-readable labels", () => {
      expect(getMetricLabel("success_rate")).toBe("Success Rate");
      expect(getMetricLabel("cycle_time")).toBe("Cycle Time");
      expect(getMetricLabel("cost")).toBe("Cost");
    });
  });

  describe("getDirectionIndicator", () => {
    it("indicates higher is better", () => {
      expect(getDirectionIndicator("higher")).toContain("Higher is better");
    });

    it("indicates lower is better", () => {
      expect(getDirectionIndicator("lower")).toContain("Lower is better");
    });
  });

  describe("Type Definitions", () => {
    it("allows valid ExperimentStatus values", () => {
      const statuses: ExperimentStatus[] = ["draft", "active", "concluded", "cancelled"];
      expect(statuses).toHaveLength(4);
    });

    it("allows valid TargetMetric values", () => {
      const metrics: TargetMetric[] = ["success_rate", "cycle_time", "cost"];
      expect(metrics).toHaveLength(3);
    });

    it("allows valid TargetDirection values", () => {
      const directions: TargetDirection[] = ["higher", "lower"];
      expect(directions).toHaveLength(2);
    });

    it("defines complete Experiment structure", () => {
      const experiment: Experiment = {
        id: 1,
        name: "test-experiment",
        description: "A test experiment",
        hypothesis: "Treatment will be better",
        status: "active",
        created_at: "2026-01-24T00:00:00Z",
        started_at: "2026-01-24T01:00:00Z",
        concluded_at: null,
        min_sample_size: 20,
        target_metric: "success_rate",
        target_direction: "higher",
      };
      expect(experiment.name).toBe("test-experiment");
    });

    it("defines complete Variant structure", () => {
      const variant: Variant = {
        id: 1,
        experiment_id: 1,
        name: "Control",
        description: "The control variant",
        config_json: '{"mode": "manual"}',
        weight: 1.0,
      };
      expect(variant.name).toBe("Control");
    });

    it("defines complete VariantStats structure", () => {
      const stats: VariantStats = {
        variant_name: "Control",
        variant_id: 1,
        sample_size: 25,
        success_rate: 0.72,
        avg_metric_value: 4.5,
        std_dev: 1.2,
        ci_lower: 0.52,
        ci_upper: 0.88,
      };
      expect(stats.success_rate).toBe(0.72);
    });

    it("defines complete ExperimentAnalysis structure", () => {
      const analysis: ExperimentAnalysis = {
        experiment_id: 1,
        winner: "Treatment",
        winner_variant_id: 2,
        confidence: 0.95,
        p_value: 0.03,
        effect_size: 0.45,
        stats_per_variant: [],
        recommendation: "Winner detected with 95% confidence",
        should_conclude: true,
        analysis_date: "2026-01-24T12:00:00Z",
      };
      expect(analysis.winner).toBe("Treatment");
    });
  });

  describe("Edge Cases", () => {
    it("handles zero values in formatting", () => {
      expect(formatSuccessRate(0)).toBe("0.0%");
      expect(formatMetricValue(0, "cost")).toBe("$0.00");
    });

    it("handles boundary values for effect size interpretation", () => {
      // Exact boundaries
      expect(formatEffectSize(0.8)).toContain("large");
      expect(formatEffectSize(0.5)).toContain("medium");
      expect(formatEffectSize(0.2)).toContain("small");
    });

    it("handles very small cycle times", () => {
      expect(formatMetricValue(0.1, "cycle_time")).toBe("6m");
    });

    it("handles very large costs", () => {
      expect(formatMetricValue(1000.5, "cost")).toBe("$1000.50");
    });
  });
});
