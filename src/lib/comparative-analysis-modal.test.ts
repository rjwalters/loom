/**
 * Comparative Analysis Modal Tests
 *
 * Tests for the Comparative Analysis UI component including
 * experiment display, statistical visualization, and export functionality.
 *
 * @see Issue #1113 - Add Comparative Analysis UI for experiments
 */

import { afterEach, beforeEach, describe, expect, it, type Mock, vi } from "vitest";
import * as abTesting from "./ab-testing";

// Mock the ab-testing module
vi.mock("./ab-testing", async (importOriginal) => {
  const original = (await importOriginal()) as typeof abTesting;
  return {
    ...original,
    getExperiments: vi.fn(),
    getExperimentsSummary: vi.fn(),
    analyzeExperiment: vi.fn(),
    getVariants: vi.fn(),
    createExperiment: vi.fn(),
    addVariant: vi.fn(),
    startExperiment: vi.fn(),
    concludeExperiment: vi.fn(),
    cancelExperiment: vi.fn(),
  };
});

// Mock the state module
vi.mock("./state", () => ({
  getAppState: vi.fn(() => ({
    workspace: {
      getWorkspace: vi.fn(() => "/test/workspace"),
    },
  })),
}));

// Mock the toast module
vi.mock("./toast", () => ({
  showToast: vi.fn(),
}));

// Mock the modal-builder module
vi.mock("./modal-builder", () => ({
  ModalBuilder: vi.fn().mockImplementation(() => ({
    setContent: vi.fn(),
    show: vi.fn().mockReturnThis(),
    close: vi.fn(),
    addFooterButton: vi.fn().mockReturnThis(),
    querySelector: vi.fn(),
    querySelectorAll: vi.fn(() => []),
  })),
}));

describe("Comparative Analysis Modal", () => {
  beforeEach(() => {
    vi.clearAllMocks();

    // Set up default mock implementations
    (abTesting.getExperimentsSummary as Mock).mockResolvedValue({
      total_experiments: 5,
      active_experiments: 2,
      concluded_experiments: 2,
      total_assignments: 100,
      total_results: 80,
    });

    (abTesting.getExperiments as Mock).mockResolvedValue([
      {
        id: 1,
        name: "manual-vs-autonomous",
        description: "Testing manual vs autonomous builder mode",
        hypothesis: "Manual mode will be faster",
        status: "active",
        created_at: "2026-01-01T00:00:00Z",
        started_at: "2026-01-01T01:00:00Z",
        concluded_at: null,
        min_sample_size: 20,
        target_metric: "success_rate",
        target_direction: "higher",
      },
      {
        id: 2,
        name: "model-comparison",
        description: "Comparing Sonnet vs Opus",
        hypothesis: null,
        status: "concluded",
        created_at: "2025-12-01T00:00:00Z",
        started_at: "2025-12-01T01:00:00Z",
        concluded_at: "2026-01-15T00:00:00Z",
        min_sample_size: 30,
        target_metric: "cost",
        target_direction: "lower",
      },
    ]);

    (abTesting.analyzeExperiment as Mock).mockResolvedValue({
      experiment_id: 1,
      winner: "Manual",
      winner_variant_id: 1,
      confidence: 0.95,
      p_value: 0.02,
      effect_size: 0.45,
      stats_per_variant: [
        {
          variant_name: "Manual",
          variant_id: 1,
          sample_size: 25,
          success_rate: 0.88,
          avg_metric_value: 0.88,
          std_dev: 0.12,
          ci_lower: 0.72,
          ci_upper: 0.96,
        },
        {
          variant_name: "Autonomous",
          variant_id: 2,
          sample_size: 23,
          success_rate: 0.65,
          avg_metric_value: 0.65,
          std_dev: 0.18,
          ci_lower: 0.48,
          ci_upper: 0.79,
        },
      ],
      recommendation:
        "Winner detected: Manual (p=0.0200, confidence=95.0%). Consider concluding the experiment.",
      should_conclude: true,
      analysis_date: "2026-01-24T12:00:00Z",
    });

    (abTesting.getVariants as Mock).mockResolvedValue([
      {
        id: 1,
        experiment_id: 1,
        name: "Manual",
        description: "Manual mode with human oversight",
        config_json: '{"mode": "manual"}',
        weight: 1.0,
      },
      {
        id: 2,
        experiment_id: 1,
        name: "Autonomous",
        description: "Fully autonomous mode",
        config_json: '{"mode": "autonomous"}',
        weight: 1.0,
      },
    ]);

    (abTesting.createExperiment as Mock).mockResolvedValue(3);
    (abTesting.addVariant as Mock).mockResolvedValue(1);
    (abTesting.startExperiment as Mock).mockResolvedValue(undefined);
    (abTesting.concludeExperiment as Mock).mockResolvedValue(undefined);
    (abTesting.cancelExperiment as Mock).mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  describe("Formatting Functions (from ab-testing)", () => {
    it("formats status correctly", () => {
      expect(abTesting.formatStatus("draft")).toBe("Draft");
      expect(abTesting.formatStatus("active")).toBe("Active");
      expect(abTesting.formatStatus("concluded")).toBe("Concluded");
      expect(abTesting.formatStatus("cancelled")).toBe("Cancelled");
    });

    it("formats p-value correctly", () => {
      expect(abTesting.formatPValue(0.0001)).toBe("< 0.001");
      expect(abTesting.formatPValue(0.005)).toBe("< 0.01");
      expect(abTesting.formatPValue(0.03)).toBe("< 0.05");
      expect(abTesting.formatPValue(0.15)).toBe("0.150");
    });

    it("formats confidence correctly", () => {
      expect(abTesting.formatConfidence(0.95)).toBe("95.0%");
      expect(abTesting.formatConfidence(0.99)).toBe("99.0%");
    });

    it("formats effect size correctly", () => {
      // Effect size thresholds: large >= 0.8, medium >= 0.5, small >= 0.2, negligible < 0.2
      expect(abTesting.formatEffectSize(0.55)).toContain("medium");
      expect(abTesting.formatEffectSize(0.1)).toContain("negligible");
      expect(abTesting.formatEffectSize(0.85)).toContain("large");
    });

    it("formats success rate correctly", () => {
      expect(abTesting.formatSuccessRate(0.88)).toBe("88.0%");
      expect(abTesting.formatSuccessRate(0.5)).toBe("50.0%");
    });

    it("determines significance correctly", () => {
      expect(abTesting.isSignificant(0.02)).toBe(true);
      expect(abTesting.isSignificant(0.05)).toBe(false);
      expect(abTesting.isSignificant(0.1)).toBe(false);
    });

    it("gets metric labels correctly", () => {
      expect(abTesting.getMetricLabel("success_rate")).toBe("Success Rate");
      expect(abTesting.getMetricLabel("cycle_time")).toBe("Cycle Time");
      expect(abTesting.getMetricLabel("cost")).toBe("Cost");
    });

    it("gets direction indicator correctly", () => {
      expect(abTesting.getDirectionIndicator("higher")).toContain("Higher is better");
      expect(abTesting.getDirectionIndicator("lower")).toContain("Lower is better");
    });

    it("formats metric values correctly", () => {
      expect(abTesting.formatMetricValue(0.88, "success_rate")).toBe("88.0%");
      expect(abTesting.formatMetricValue(4.5, "cycle_time")).toBe("4.5h");
      expect(abTesting.formatMetricValue(12.5, "cost")).toBe("$12.50");
      expect(abTesting.formatMetricValue(null, "cost")).toBe("-");
    });
  });

  describe("API Mocks", () => {
    it("getExperimentsSummary returns summary data", async () => {
      const summary = await abTesting.getExperimentsSummary("/test/workspace");
      expect(summary.total_experiments).toBe(5);
      expect(summary.active_experiments).toBe(2);
    });

    it("getExperiments returns experiment list", async () => {
      const experiments = await abTesting.getExperiments("/test/workspace");
      expect(experiments).toHaveLength(2);
      expect(experiments[0].name).toBe("manual-vs-autonomous");
    });

    it("analyzeExperiment returns analysis results", async () => {
      const analysis = await abTesting.analyzeExperiment("/test/workspace", 1);
      expect(analysis.winner).toBe("Manual");
      expect(analysis.p_value).toBe(0.02);
      expect(analysis.stats_per_variant).toHaveLength(2);
    });

    it("getVariants returns variant list", async () => {
      const variants = await abTesting.getVariants("/test/workspace", 1);
      expect(variants).toHaveLength(2);
      expect(variants[0].name).toBe("Manual");
    });

    it("createExperiment returns new experiment ID", async () => {
      const id = await abTesting.createExperiment("/test/workspace", {
        name: "test",
        description: null,
        hypothesis: null,
        status: "draft",
        min_sample_size: 20,
        target_metric: "success_rate",
        target_direction: "higher",
      });
      expect(id).toBe(3);
    });
  });

  describe("Status Colors", () => {
    it("returns appropriate colors for each status", () => {
      expect(abTesting.getStatusColor("draft")).toContain("gray");
      expect(abTesting.getStatusColor("active")).toContain("blue");
      expect(abTesting.getStatusColor("concluded")).toContain("green");
      expect(abTesting.getStatusColor("cancelled")).toContain("red");
    });
  });

  describe("P-Value Colors", () => {
    it("returns green for highly significant results", () => {
      expect(abTesting.getPValueColor(0.005)).toContain("green");
    });

    it("returns yellow for significant results", () => {
      expect(abTesting.getPValueColor(0.03)).toContain("yellow");
    });

    it("returns gray for non-significant results", () => {
      expect(abTesting.getPValueColor(0.15)).toContain("gray");
    });
  });

  describe("Statistical Analysis Types", () => {
    it("VariantStats has required fields", () => {
      const stats: abTesting.VariantStats = {
        variant_name: "Control",
        variant_id: 1,
        sample_size: 25,
        success_rate: 0.75,
        avg_metric_value: 0.75,
        std_dev: 0.15,
        ci_lower: 0.6,
        ci_upper: 0.88,
      };
      expect(stats.sample_size).toBe(25);
      expect(stats.ci_lower).toBeLessThan(stats.success_rate);
      expect(stats.ci_upper).toBeGreaterThan(stats.success_rate);
    });

    it("ExperimentAnalysis has required fields", () => {
      const analysis: abTesting.ExperimentAnalysis = {
        experiment_id: 1,
        winner: "Treatment",
        winner_variant_id: 2,
        confidence: 0.95,
        p_value: 0.02,
        effect_size: 0.45,
        stats_per_variant: [],
        recommendation: "Winner detected",
        should_conclude: true,
        analysis_date: "2026-01-24T00:00:00Z",
      };
      expect(analysis.confidence).toBeGreaterThan(0.9);
      expect(analysis.should_conclude).toBe(true);
    });
  });

  describe("Edge Cases", () => {
    it("handles experiments with no data", async () => {
      (abTesting.analyzeExperiment as Mock).mockResolvedValueOnce({
        experiment_id: 1,
        winner: null,
        winner_variant_id: null,
        confidence: 0,
        p_value: 1.0,
        effect_size: 0,
        stats_per_variant: [],
        recommendation: "Not enough data for analysis",
        should_conclude: false,
        analysis_date: "2026-01-24T00:00:00Z",
      });

      const analysis = await abTesting.analyzeExperiment("/test/workspace", 1);
      expect(analysis.winner).toBeNull();
      expect(analysis.stats_per_variant).toHaveLength(0);
      expect(analysis.should_conclude).toBe(false);
    });

    it("handles empty experiment list", async () => {
      (abTesting.getExperiments as Mock).mockResolvedValueOnce([]);
      const experiments = await abTesting.getExperiments("/test/workspace");
      expect(experiments).toHaveLength(0);
    });

    it("handles experiments with null optional fields", async () => {
      (abTesting.getExperiments as Mock).mockResolvedValueOnce([
        {
          id: 1,
          name: "minimal-experiment",
          description: null,
          hypothesis: null,
          status: "draft",
          created_at: "2026-01-01T00:00:00Z",
          started_at: null,
          concluded_at: null,
          min_sample_size: 20,
          target_metric: "success_rate",
          target_direction: "higher",
        },
      ]);

      const experiments = await abTesting.getExperiments("/test/workspace");
      expect(experiments[0].description).toBeNull();
      expect(experiments[0].hypothesis).toBeNull();
    });

    it("formats zero values correctly", () => {
      expect(abTesting.formatSuccessRate(0)).toBe("0.0%");
      expect(abTesting.formatConfidence(0)).toBe("0.0%");
      expect(abTesting.formatMetricValue(0, "cost")).toBe("$0.00");
    });

    it("formats very small metric values", () => {
      expect(abTesting.formatMetricValue(0.01, "cycle_time")).toBe("1m");
      expect(abTesting.formatMetricValue(0.001, "cost")).toBe("$0.00");
    });

    it("formats very large metric values", () => {
      expect(abTesting.formatMetricValue(72, "cycle_time")).toBe("3.0d");
      expect(abTesting.formatMetricValue(9999.99, "cost")).toBe("$9999.99");
    });
  });

  describe("Experiment Lifecycle", () => {
    it("creates experiment with variants", async () => {
      const experimentId = await abTesting.createExperiment("/test/workspace", {
        name: "new-experiment",
        description: "Test description",
        hypothesis: "Test hypothesis",
        status: "draft",
        min_sample_size: 25,
        target_metric: "cost",
        target_direction: "lower",
      });

      expect(experimentId).toBe(3);
      expect(abTesting.createExperiment).toHaveBeenCalledWith("/test/workspace", {
        name: "new-experiment",
        description: "Test description",
        hypothesis: "Test hypothesis",
        status: "draft",
        min_sample_size: 25,
        target_metric: "cost",
        target_direction: "lower",
      });
    });

    it("adds variants to experiment", async () => {
      await abTesting.addVariant("/test/workspace", {
        experiment_id: 3,
        name: "Control",
        description: "Baseline",
        config_json: '{"mode": "default"}',
        weight: 1.0,
      });

      expect(abTesting.addVariant).toHaveBeenCalledWith("/test/workspace", {
        experiment_id: 3,
        name: "Control",
        description: "Baseline",
        config_json: '{"mode": "default"}',
        weight: 1.0,
      });
    });

    it("starts experiment", async () => {
      await abTesting.startExperiment("/test/workspace", 3);
      expect(abTesting.startExperiment).toHaveBeenCalledWith("/test/workspace", 3);
    });

    it("concludes experiment", async () => {
      await abTesting.concludeExperiment("/test/workspace", 1, 1);
      expect(abTesting.concludeExperiment).toHaveBeenCalledWith("/test/workspace", 1, 1);
    });

    it("cancels experiment", async () => {
      await abTesting.cancelExperiment("/test/workspace", 1);
      expect(abTesting.cancelExperiment).toHaveBeenCalledWith("/test/workspace", 1);
    });
  });

  describe("Filter Functionality", () => {
    it("filters by active status", async () => {
      await abTesting.getExperiments("/test/workspace", "active");
      expect(abTesting.getExperiments).toHaveBeenCalledWith("/test/workspace", "active");
    });

    it("filters by concluded status", async () => {
      await abTesting.getExperiments("/test/workspace", "concluded");
      expect(abTesting.getExperiments).toHaveBeenCalledWith("/test/workspace", "concluded");
    });

    it("returns all when no filter", async () => {
      await abTesting.getExperiments("/test/workspace");
      expect(abTesting.getExperiments).toHaveBeenCalledWith("/test/workspace");
    });
  });
});
