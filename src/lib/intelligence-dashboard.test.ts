/**
 * Tests for Intelligence Dashboard
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as agentMetrics from "./agent-metrics";
import {
  closeIntelligenceDashboard,
  isDashboardVisible,
  showIntelligenceDashboard,
} from "./intelligence-dashboard";
import * as promptOptimization from "./prompt-optimization";
import { AppState, setAppState } from "./state";

// Mock localStorage before any imports that might use it
const localStorageMock = {
  getItem: vi.fn(),
  setItem: vi.fn(),
  removeItem: vi.fn(),
  clear: vi.fn(),
  length: 0,
  key: vi.fn(),
};
Object.defineProperty(globalThis, "localStorage", {
  value: localStorageMock,
  writable: true,
});

// Mock dependencies
vi.mock("./agent-metrics", () => ({
  getAgentMetrics: vi.fn(),
  getMetricsByRole: vi.fn(),
  getVelocitySummary: vi.fn(),
  formatNumber: vi.fn((n) => n.toString()),
  formatTokens: vi.fn((n) => n.toString()),
  formatCurrency: vi.fn((n) => `$${n.toFixed(2)}`),
  formatPercent: vi.fn((n) => `${(n * 100).toFixed(1)}%`),
  formatCycleTime: vi.fn((n) => (n ? `${n}h` : "-")),
  formatChangePercent: vi.fn((n) => (n >= 0 ? `+${n.toFixed(1)}%` : `${n.toFixed(1)}%`)),
  getRoleDisplayName: vi.fn((r) => r.charAt(0).toUpperCase() + r.slice(1)),
  getSuccessRateColor: vi.fn(() => "text-green-600"),
  getTrendColor: vi.fn(() => "text-green-600"),
  getTrendIcon: vi.fn((t) =>
    t === "improving" ? "\u2191" : t === "declining" ? "\u2193" : "\u2192"
  ),
}));

vi.mock("./prompt-optimization", () => ({
  getOptimizationStats: vi.fn(),
  getOptimizationTypeName: vi.fn((type: string) => {
    const names: Record<string, string> = {
      length: "Length Adjustment",
      specificity: "Specificity Enhancement",
      structure: "Structure Improvement",
      pattern: "Pattern Matching",
    };
    return names[type] ?? type;
  }),
}));

vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      error: vi.fn(),
      warn: vi.fn(),
      debug: vi.fn(),
    }),
  },
}));

describe("Intelligence Dashboard", () => {
  let mockState: AppState;

  const mockMetrics = {
    prompt_count: 150,
    total_tokens: 250000,
    total_cost: 12.5,
    success_rate: 0.85,
    prs_created: 8,
    issues_closed: 5,
  };

  const mockRoleMetrics = [
    { role: "builder", prompt_count: 80, total_tokens: 150000, total_cost: 7.5, success_rate: 0.9 },
    { role: "judge", prompt_count: 40, total_tokens: 60000, total_cost: 3.0, success_rate: 0.95 },
    { role: "curator", prompt_count: 30, total_tokens: 40000, total_cost: 2.0, success_rate: 0.75 },
  ];

  const mockVelocity = {
    issues_closed: 5,
    prs_merged: 4,
    avg_cycle_time_hours: 2.5,
    total_prompts: 150,
    total_cost_usd: 12.5,
    prev_issues_closed: 3,
    prev_prs_merged: 3,
    prev_avg_cycle_time_hours: 3.0,
    issues_trend: "improving" as const,
    prs_trend: "improving" as const,
    cycle_time_trend: "improving" as const,
  };

  beforeEach(() => {
    // Set up DOM
    document.body.innerHTML = '<div id="app"></div>';

    // Create mock state
    mockState = new AppState();
    mockState.workspace.setWorkspace("/test/workspace");
    setAppState(mockState);

    // Set up mocks
    vi.mocked(agentMetrics.getAgentMetrics).mockResolvedValue(mockMetrics);
    vi.mocked(agentMetrics.getMetricsByRole).mockResolvedValue(mockRoleMetrics);
    vi.mocked(agentMetrics.getVelocitySummary).mockResolvedValue(mockVelocity);
    vi.mocked(promptOptimization.getOptimizationStats).mockResolvedValue({
      total_suggestions: 0,
      accepted_suggestions: 0,
      rejected_suggestions: 0,
      pending_suggestions: 0,
      acceptance_rate: 0,
      avg_improvement_when_accepted: 0,
      suggestions_by_type: [],
    });
  });

  afterEach(() => {
    // Clean up any open modals
    closeIntelligenceDashboard();
    vi.clearAllMocks();
    vi.useRealTimers();
  });

  describe("showIntelligenceDashboard", () => {
    it("should open the dashboard modal", async () => {
      await showIntelligenceDashboard(0); // Disable auto-refresh for tests

      expect(isDashboardVisible()).toBe(true);
      expect(document.getElementById("intelligence-dashboard-modal")).toBeTruthy();
    });

    it("should display today's activity summary", async () => {
      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("Today's Activity");
      expect(modal?.textContent).toContain("Prompts");
      expect(modal?.textContent).toContain("Features");
      expect(modal?.textContent).toContain("Cost");
      expect(modal?.textContent).toContain("Tokens");
    });

    it("should display week-over-week trends", async () => {
      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("Week-over-Week Trends");
      expect(modal?.textContent).toContain("Issues Closed");
      expect(modal?.textContent).toContain("PRs Merged");
      expect(modal?.textContent).toContain("Avg Cycle Time");
    });

    it("should display role performance table", async () => {
      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("Performance by Role");
      expect(modal?.textContent).toContain("Builder");
      expect(modal?.textContent).toContain("Judge");
      expect(modal?.textContent).toContain("Curator");
    });

    it("should display agent status section", async () => {
      // Add some terminals to the state
      mockState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: "running" as any,
        roleConfig: { roleFile: "builder.md" } as any,
      } as any);

      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("Agent Status");
    });

    it("should fetch metrics for today and week", async () => {
      await showIntelligenceDashboard(0);

      expect(agentMetrics.getAgentMetrics).toHaveBeenCalledWith("/test/workspace", "today");
      expect(agentMetrics.getAgentMetrics).toHaveBeenCalledWith("/test/workspace", "week");
    });

    it("should fetch role metrics for the week", async () => {
      await showIntelligenceDashboard(0);

      expect(agentMetrics.getMetricsByRole).toHaveBeenCalledWith("/test/workspace", "week");
    });

    it("should fetch velocity summary", async () => {
      await showIntelligenceDashboard(0);

      expect(agentMetrics.getVelocitySummary).toHaveBeenCalledWith("/test/workspace");
    });

    it("should show error state when no workspace is selected", async () => {
      mockState.workspace.clearWorkspace();

      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("No workspace selected");
    });

    it("should show error state when API call fails", async () => {
      vi.mocked(agentMetrics.getAgentMetrics).mockRejectedValue(new Error("API error"));

      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("Failed to load dashboard");
    });
  });

  describe("closeIntelligenceDashboard", () => {
    it("should close the dashboard modal", async () => {
      await showIntelligenceDashboard(0);
      expect(isDashboardVisible()).toBe(true);

      closeIntelligenceDashboard();
      expect(isDashboardVisible()).toBe(false);
    });

    it("should do nothing if dashboard is not open", () => {
      expect(() => closeIntelligenceDashboard()).not.toThrow();
      expect(isDashboardVisible()).toBe(false);
    });
  });

  describe("isDashboardVisible", () => {
    it("should return false when dashboard is not open", () => {
      expect(isDashboardVisible()).toBe(false);
    });

    it("should return true when dashboard is open", async () => {
      await showIntelligenceDashboard(0);
      expect(isDashboardVisible()).toBe(true);
    });

    it("should return false after dashboard is closed", async () => {
      await showIntelligenceDashboard(0);
      closeIntelligenceDashboard();
      expect(isDashboardVisible()).toBe(false);
    });
  });

  describe("auto-refresh", () => {
    it("should set up auto-refresh interval when enabled", async () => {
      vi.useFakeTimers();

      await showIntelligenceDashboard(5000); // 5 second refresh

      // Clear initial call counts
      vi.clearAllMocks();

      // Fast-forward time
      await vi.advanceTimersByTimeAsync(5000);

      // Should have refreshed
      expect(agentMetrics.getAgentMetrics).toHaveBeenCalled();

      closeIntelligenceDashboard();
    });

    it("should not set up auto-refresh when interval is 0", async () => {
      vi.useFakeTimers();

      await showIntelligenceDashboard(0);

      // Clear initial call counts
      vi.clearAllMocks();

      // Fast-forward time
      await vi.advanceTimersByTimeAsync(60000);

      // Should not have refreshed
      expect(agentMetrics.getAgentMetrics).not.toHaveBeenCalled();

      closeIntelligenceDashboard();
    });

    it("should clean up interval when modal is closed", async () => {
      vi.useFakeTimers();

      await showIntelligenceDashboard(5000);

      // Clear initial call counts
      vi.clearAllMocks();

      // Close the dashboard
      closeIntelligenceDashboard();

      // Fast-forward time
      await vi.advanceTimersByTimeAsync(10000);

      // Should not have refreshed after closing
      expect(agentMetrics.getAgentMetrics).not.toHaveBeenCalled();
    });
  });

  describe("refresh button", () => {
    it("should have a refresh button", async () => {
      await showIntelligenceDashboard(0);

      const refreshBtn = document.getElementById("refresh-dashboard-btn");
      expect(refreshBtn).toBeTruthy();
    });

    it("should refresh data when button is clicked", async () => {
      await showIntelligenceDashboard(0);

      // Clear initial call counts
      vi.clearAllMocks();

      // Click refresh button
      const refreshBtn = document.getElementById("refresh-dashboard-btn");
      refreshBtn?.click();

      // Wait for async refresh
      await vi.waitFor(() => {
        expect(agentMetrics.getAgentMetrics).toHaveBeenCalled();
      });
    });
  });

  describe("empty state handling", () => {
    it("should show empty state when no role metrics", async () => {
      vi.mocked(agentMetrics.getMetricsByRole).mockResolvedValue([]);

      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("No agent activity recorded");
    });

    it("should show empty state when no terminals", async () => {
      await showIntelligenceDashboard(0);

      const modal = document.getElementById("intelligence-dashboard-modal");
      expect(modal?.textContent).toContain("No active terminals");
    });
  });

  describe("closing existing dashboard", () => {
    it("should close existing dashboard when opening a new one", async () => {
      await showIntelligenceDashboard(0);
      // Verify first modal exists
      expect(document.getElementById("intelligence-dashboard-modal")).toBeTruthy();

      await showIntelligenceDashboard(0);

      // Should only have one modal
      const modals = document.querySelectorAll("#intelligence-dashboard-modal");
      expect(modals.length).toBe(1);
    });
  });
});
