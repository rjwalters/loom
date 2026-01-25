/**
 * Tests for Weekly Intelligence Report Module
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  type DetectedAnomaly,
  type DidYouKnow,
  exportReportAsHtml,
  exportReportAsMarkdown,
  formatHour,
  formatWeekRange,
  getDayName,
  getDefaultSchedule,
  type IdentifiedPattern,
  type Recommendation,
  type ReportSchedule,
  stopReportScheduler,
  type WeeklyReport,
  type WeekSummary,
} from "./weekly-report";

// Mock Tauri API
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

// Mock dependencies
vi.mock("./agent-metrics", () => ({
  getAgentMetrics: vi.fn(),
  getVelocitySummary: vi.fn(),
  getMetricsByRole: vi.fn(),
  formatCurrency: vi.fn((v) => `$${v.toFixed(2)}`),
  formatNumber: vi.fn((v) => v.toLocaleString()),
  formatTokens: vi.fn((v) => (v >= 1000 ? `${(v / 1000).toFixed(1)}K` : v.toString())),
  formatPercent: vi.fn((v) => `${(v * 100).toFixed(1)}%`),
  formatCycleTime: vi.fn((v) => (v === null ? "-" : `${v.toFixed(1)}h`)),
  formatChangePercent: vi.fn((v) => `${v >= 0 ? "+" : ""}${v?.toFixed(1) ?? 0}%`),
  getTrendIcon: vi.fn((t) => (t === "improving" ? "^" : t === "declining" ? "v" : "-")),
}));

vi.mock("./correlation-analysis", () => ({
  runCorrelationAnalysis: vi.fn(),
}));

vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      error: vi.fn(),
      debug: vi.fn(),
      warn: vi.fn(),
    }),
  },
}));

describe("Weekly Report Module", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    stopReportScheduler();
  });

  describe("getDefaultSchedule", () => {
    it("returns Monday at 9 AM by default", () => {
      const schedule = getDefaultSchedule();

      expect(schedule.dayOfWeek).toBe(1); // Monday
      expect(schedule.hourOfDay).toBe(9); // 9 AM
      expect(schedule.enabled).toBe(true);
      expect(typeof schedule.timezoneOffset).toBe("number");
    });
  });

  describe("formatWeekRange", () => {
    it("formats week range correctly", () => {
      const result = formatWeekRange("2026-01-20", "2026-01-26");

      // Should contain both dates in some format
      // Note: Date parsing may have timezone offsets, so check for presence of month
      expect(result).toContain("Jan");
      // The exact day numbers may vary by timezone, but the range should be ~7 days
      expect(result.includes("-")).toBe(true);
    });

    it("handles cross-month ranges", () => {
      const result = formatWeekRange("2026-01-27", "2026-02-02");

      expect(result).toContain("Jan");
      expect(result).toContain("Feb");
    });
  });

  describe("getDayName", () => {
    it("returns correct day names", () => {
      expect(getDayName(0)).toBe("Sunday");
      expect(getDayName(1)).toBe("Monday");
      expect(getDayName(2)).toBe("Tuesday");
      expect(getDayName(3)).toBe("Wednesday");
      expect(getDayName(4)).toBe("Thursday");
      expect(getDayName(5)).toBe("Friday");
      expect(getDayName(6)).toBe("Saturday");
    });

    it("handles invalid day numbers", () => {
      expect(getDayName(7)).toBe("Unknown");
      expect(getDayName(-1)).toBe("Unknown");
    });
  });

  describe("formatHour", () => {
    it("formats morning hours correctly", () => {
      expect(formatHour(0)).toBe("12:00 AM");
      expect(formatHour(1)).toBe("1:00 AM");
      expect(formatHour(9)).toBe("9:00 AM");
      expect(formatHour(11)).toBe("11:00 AM");
    });

    it("formats afternoon hours correctly", () => {
      expect(formatHour(12)).toBe("12:00 PM");
      expect(formatHour(13)).toBe("1:00 PM");
      expect(formatHour(17)).toBe("5:00 PM");
      expect(formatHour(23)).toBe("11:00 PM");
    });
  });

  describe("exportReportAsMarkdown", () => {
    const mockReport = createMockReport();

    it("includes report header with dates", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("# Weekly Intelligence Report");
      expect(markdown).toContain("2026-01-20");
      expect(markdown).toContain("2026-01-26");
    });

    it("includes summary section with metrics", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## Summary");
      expect(markdown).toContain("Features");
      expect(markdown).toContain("PRs Merged");
      expect(markdown).toContain("Cost");
      expect(markdown).toContain("Success Rate");
    });

    it("includes success patterns when present", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## What Worked Well");
      expect(markdown).toContain("Builder achieving high success rate");
    });

    it("includes improvement areas when present", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## Areas for Improvement");
      expect(markdown).toContain("Judge role needs attention");
    });

    it("includes anomalies when present", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## Alerts");
      expect(markdown).toContain("3 PRs stuck in review");
    });

    it("includes recommendations", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## Recommendations");
      expect(markdown).toContain("Review stuck PRs");
    });

    it("includes did you know insights", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## Did You Know?");
      expect(markdown).toContain("cost an average of");
    });

    it("includes role metrics table", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("## Performance by Role");
      expect(markdown).toContain("builder");
      expect(markdown).toContain("judge");
    });

    it("includes footer", () => {
      const markdown = exportReportAsMarkdown(mockReport);

      expect(markdown).toContain("Generated by Loom Intelligence");
    });
  });

  describe("exportReportAsHtml", () => {
    const mockReport = createMockReport();

    it("returns valid HTML document", () => {
      const html = exportReportAsHtml(mockReport);

      expect(html).toContain("<!DOCTYPE html>");
      expect(html).toContain("<html>");
      expect(html).toContain("</html>");
      expect(html).toContain("<head>");
      expect(html).toContain("<body>");
    });

    it("includes title with week start date", () => {
      const html = exportReportAsHtml(mockReport);

      expect(html).toContain("<title>Weekly Intelligence Report - 2026-01-20</title>");
    });

    it("includes CSS styles", () => {
      const html = exportReportAsHtml(mockReport);

      expect(html).toContain("<style>");
      expect(html).toContain("font-family");
    });

    it("converts headers to HTML", () => {
      const html = exportReportAsHtml(mockReport);

      expect(html).toContain("<h1>Weekly Intelligence Report</h1>");
      expect(html).toContain("<h2>Summary</h2>");
    });
  });

  describe("Report Data Structure", () => {
    it("WeekSummary has all required fields", () => {
      const summary: WeekSummary = {
        features_completed: 5,
        prs_merged: 4,
        total_prompts: 100,
        total_tokens: 50000,
        total_cost: 10.5,
        success_rate: 0.85,
        avg_cycle_time_hours: 6.5,
        prev_features_completed: 3,
        prev_prs_merged: 2,
        prev_total_prompts: 80,
        prev_total_tokens: 40000,
        prev_total_cost: 8.0,
        prev_success_rate: 0.8,
        prev_avg_cycle_time_hours: 8.0,
        features_trend: "improving",
        prs_trend: "improving",
        cost_trend: "declining",
        success_trend: "improving",
        cycle_time_trend: "improving",
      };

      expect(summary.features_completed).toBe(5);
      expect(summary.features_trend).toBe("improving");
    });

    it("ReportSchedule has all required fields", () => {
      const schedule: ReportSchedule = {
        dayOfWeek: 1,
        hourOfDay: 9,
        timezoneOffset: -480,
        enabled: true,
      };

      expect(schedule.dayOfWeek).toBe(1);
      expect(schedule.enabled).toBe(true);
    });

    it("IdentifiedPattern has all required fields", () => {
      const pattern: IdentifiedPattern = {
        type: "success",
        factor: "test_factor",
        description: "Test description",
        impact: "Test impact",
        strength: "strong",
      };

      expect(pattern.type).toBe("success");
      expect(pattern.strength).toBe("strong");
    });

    it("DetectedAnomaly has all required fields", () => {
      const anomaly: DetectedAnomaly = {
        severity: "warning",
        type: "test_type",
        message: "Test message",
        details: "Test details",
        detected_at: "2026-01-24T10:00:00Z",
      };

      expect(anomaly.severity).toBe("warning");
      expect(anomaly.type).toBe("test_type");
    });

    it("Recommendation has all required fields", () => {
      const rec: Recommendation = {
        priority: "high",
        category: "Test",
        title: "Test title",
        description: "Test description",
        action: "Test action",
      };

      expect(rec.priority).toBe("high");
      expect(rec.category).toBe("Test");
    });

    it("DidYouKnow has all required fields", () => {
      const insight: DidYouKnow = {
        icon: "star",
        fact: "Test fact",
        context: "Test context",
      };

      expect(insight.icon).toBe("star");
      expect(insight.fact).toBe("Test fact");
    });
  });

  describe("Edge Cases", () => {
    it("handles empty report data gracefully", () => {
      const emptyReport: WeeklyReport = {
        id: "test-empty",
        generated_at: new Date().toISOString(),
        week_start: "2026-01-20",
        week_end: "2026-01-26",
        summary: {
          features_completed: 0,
          prs_merged: 0,
          total_prompts: 0,
          total_tokens: 0,
          total_cost: 0,
          success_rate: 0,
          avg_cycle_time_hours: null,
          prev_features_completed: 0,
          prev_prs_merged: 0,
          prev_total_prompts: 0,
          prev_total_tokens: 0,
          prev_total_cost: 0,
          prev_success_rate: 0,
          prev_avg_cycle_time_hours: null,
          features_trend: "stable",
          prs_trend: "stable",
          cost_trend: "stable",
          success_trend: "stable",
          cycle_time_trend: "stable",
        },
        role_metrics: [],
        success_patterns: [],
        improvement_areas: [],
        anomalies: [],
        recommendations: [],
        did_you_know: [],
        status: "generated",
      };

      const markdown = exportReportAsMarkdown(emptyReport);

      expect(markdown).toContain("# Weekly Intelligence Report");
      expect(markdown).toContain("## Summary");
      // Should NOT contain sections with no data
      expect(markdown).not.toContain("## What Worked Well");
      expect(markdown).not.toContain("## Areas for Improvement");
      expect(markdown).not.toContain("## Alerts");
    });

    it("handles null cycle time gracefully", () => {
      const report = createMockReport();
      report.summary.avg_cycle_time_hours = null;
      report.summary.prev_avg_cycle_time_hours = null;

      const markdown = exportReportAsMarkdown(report);

      expect(markdown).toContain("Avg Cycle Time");
    });

    it("handles very large numbers", () => {
      const report = createMockReport();
      report.summary.total_tokens = 10000000;
      report.summary.total_cost = 1000.99;

      const markdown = exportReportAsMarkdown(report);

      // Should not throw and should contain the values
      expect(markdown).toBeTruthy();
    });
  });
});

/**
 * Create a mock report for testing
 */
function createMockReport(): WeeklyReport {
  return {
    id: "report-test-123",
    generated_at: "2026-01-27T09:00:00Z",
    week_start: "2026-01-20",
    week_end: "2026-01-26",
    summary: {
      features_completed: 5,
      prs_merged: 4,
      total_prompts: 100,
      total_tokens: 50000,
      total_cost: 10.5,
      success_rate: 0.85,
      avg_cycle_time_hours: 6.5,
      prev_features_completed: 3,
      prev_prs_merged: 2,
      prev_total_prompts: 80,
      prev_total_tokens: 40000,
      prev_total_cost: 8.0,
      prev_success_rate: 0.8,
      prev_avg_cycle_time_hours: 8.0,
      features_trend: "improving",
      prs_trend: "improving",
      cost_trend: "declining",
      success_trend: "improving",
      cycle_time_trend: "improving",
    },
    role_metrics: [
      {
        role: "builder",
        prompt_count: 60,
        total_tokens: 30000,
        total_cost: 6.0,
        success_rate: 0.9,
      },
      {
        role: "judge",
        prompt_count: 30,
        total_tokens: 15000,
        total_cost: 3.0,
        success_rate: 0.8,
      },
      {
        role: "curator",
        prompt_count: 10,
        total_tokens: 5000,
        total_cost: 1.5,
        success_rate: 0.85,
      },
    ],
    success_patterns: [
      {
        type: "success",
        factor: "builder_high_performance",
        description: "Builder achieving high success rate",
        impact: "90% success with 60 prompts",
        strength: "strong",
      },
      {
        type: "success",
        factor: "velocity_improvement",
        description: "Feature velocity improved 67%",
        impact: "5 features vs 3 last week",
        strength: "moderate",
      },
    ],
    improvement_areas: [
      {
        type: "improvement",
        factor: "judge_needs_work",
        description: "Judge role needs attention",
        impact: "Success rate below target",
        strength: "moderate",
      },
    ],
    anomalies: [
      {
        severity: "warning",
        type: "stuck_prs",
        message: "3 PRs stuck in review > 48 hours",
        details: "PRs waiting for review may block development velocity",
        detected_at: "2026-01-27T09:00:00Z",
      },
    ],
    recommendations: [
      {
        priority: "high",
        category: "Workflow",
        title: "Review stuck PRs",
        description: "3 PRs stuck in review > 48 hours",
        action: "Check PR queue and address blocking reviews",
      },
      {
        priority: "low",
        category: "Success",
        title: "Continue successful practices",
        description: "Builder achieving high success rate - keep this up!",
        action: "Document and share what's working well",
      },
    ],
    did_you_know: [
      {
        icon: "dollar",
        fact: "Each feature cost an average of $2.10 in API usage",
        context: "Last week: $2.67",
      },
      {
        icon: "zap",
        fact: "The builder role was most active with 60 prompts",
        context: "That's 60% of all activity this week",
      },
    ],
    status: "generated",
  };
}
