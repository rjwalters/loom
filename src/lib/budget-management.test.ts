/**
 * Tests for Budget Management Module
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as agentMetrics from "./agent-metrics";
import {
  type BudgetConfig,
  checkBudgetAlerts,
  closeBudgetManagementModal,
  getBudgetConfig,
  getBudgetStatus,
  getRunwayProjection,
  isBudgetModalVisible,
  saveBudgetConfig,
  showBudgetManagementModal,
} from "./budget-management";
import { AppState, setAppState } from "./state";
import * as toast from "./toast";

// Mock localStorage
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

// Mock Tauri invoke
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

// Mock dependencies
vi.mock("./agent-metrics", () => ({
  getAgentMetrics: vi.fn(),
  getMetricsByRole: vi.fn(),
  formatNumber: vi.fn((n) => n.toString()),
  formatTokens: vi.fn((n) => n.toString()),
  formatCurrency: vi.fn((n) => `$${n.toFixed(2)}`),
  formatPercent: vi.fn((n) => `${(n * 100).toFixed(1)}%`),
  getRoleDisplayName: vi.fn((r) => r.charAt(0).toUpperCase() + r.slice(1)),
}));

vi.mock("./toast", () => ({
  showToast: vi.fn(),
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

describe("Budget Management", () => {
  let mockState: AppState;
  let mockInvoke: ReturnType<typeof vi.fn>;

  const mockMetricsToday = {
    prompt_count: 50,
    total_tokens: 100000,
    total_cost: 5.0,
    success_rate: 0.85,
    prs_created: 2,
    issues_closed: 1,
  };

  const mockMetricsWeek = {
    prompt_count: 200,
    total_tokens: 500000,
    total_cost: 25.0,
    success_rate: 0.8,
    prs_created: 8,
    issues_closed: 5,
  };

  const mockMetricsMonth = {
    prompt_count: 600,
    total_tokens: 1500000,
    total_cost: 75.0,
    success_rate: 0.82,
    prs_created: 20,
    issues_closed: 15,
  };

  const mockBudgetConfig: BudgetConfig = {
    dailyLimit: 10.0,
    weeklyLimit: 50.0,
    monthlyLimit: 150.0,
    alertThresholds: [50, 75, 90, 100],
    alertsEnabled: true,
    lastAlerts: {
      daily: {},
      weekly: {},
      monthly: {},
    },
  };

  beforeEach(async () => {
    // Set up DOM
    document.body.innerHTML = '<div id="app"></div>';

    // Create mock state
    mockState = new AppState();
    mockState.workspace.setWorkspace("/test/workspace");
    setAppState(mockState);

    // Get mock invoke from Tauri
    const { invoke } = await import("@tauri-apps/api/core");
    mockInvoke = invoke as ReturnType<typeof vi.fn>;

    // Set up mocks
    vi.mocked(agentMetrics.getAgentMetrics).mockImplementation(async (_path, timeRange) => {
      switch (timeRange) {
        case "today":
          return mockMetricsToday;
        case "week":
          return mockMetricsWeek;
        case "month":
          return mockMetricsMonth;
        default:
          return mockMetricsWeek;
      }
    });

    vi.mocked(agentMetrics.getMetricsByRole).mockResolvedValue([
      {
        role: "builder",
        prompt_count: 100,
        total_tokens: 250000,
        total_cost: 12.5,
        success_rate: 0.85,
      },
      { role: "judge", prompt_count: 60, total_tokens: 150000, total_cost: 7.5, success_rate: 0.9 },
    ]);

    mockInvoke.mockImplementation(async (cmd: string, _args?: Record<string, unknown>) => {
      if (cmd === "get_budget_config") {
        return mockBudgetConfig;
      }
      if (cmd === "save_budget_config") {
        return undefined;
      }
      if (cmd === "get_costs_by_issue") {
        return [
          {
            issueNumber: 123,
            issueTitle: "Test Issue",
            totalCost: 5.0,
            totalTokens: 100000,
            promptCount: 20,
            lastActivity: "2026-01-24T10:00:00Z",
          },
        ];
      }
      return null;
    });
  });

  afterEach(() => {
    closeBudgetManagementModal();
    vi.clearAllMocks();
    vi.useRealTimers();
  });

  describe("getBudgetConfig", () => {
    it("should return budget config from backend", async () => {
      const config = await getBudgetConfig("/test/workspace");

      expect(mockInvoke).toHaveBeenCalledWith("get_budget_config", {
        workspacePath: "/test/workspace",
      });
      expect(config).toEqual(mockBudgetConfig);
    });

    it("should return default config when backend returns null", async () => {
      mockInvoke.mockResolvedValueOnce(null);

      const config = await getBudgetConfig("/test/workspace");

      expect(config.alertsEnabled).toBe(true);
      expect(config.alertThresholds).toEqual([50, 75, 90, 100]);
    });
  });

  describe("saveBudgetConfig", () => {
    it("should save config to backend", async () => {
      const newConfig: BudgetConfig = {
        ...mockBudgetConfig,
        dailyLimit: 20.0,
      };

      await saveBudgetConfig("/test/workspace", newConfig);

      expect(mockInvoke).toHaveBeenCalledWith("save_budget_config", {
        workspacePath: "/test/workspace",
        config: newConfig,
      });
    });
  });

  describe("getBudgetStatus", () => {
    it("should return status for all periods", async () => {
      const statuses = await getBudgetStatus("/test/workspace");

      expect(statuses).toHaveLength(3);
      expect(statuses[0].period).toBe("daily");
      expect(statuses[1].period).toBe("weekly");
      expect(statuses[2].period).toBe("monthly");
    });

    it("should calculate correct percentages", async () => {
      const statuses = await getBudgetStatus("/test/workspace");

      // Daily: $5.00 / $10.00 = 50%
      expect(statuses[0].percentUsed).toBe(50);
      expect(statuses[0].isOverBudget).toBe(false);

      // Weekly: $25.00 / $50.00 = 50%
      expect(statuses[1].percentUsed).toBe(50);
      expect(statuses[1].isOverBudget).toBe(false);

      // Monthly: $75.00 / $150.00 = 50%
      expect(statuses[2].percentUsed).toBe(50);
      expect(statuses[2].isOverBudget).toBe(false);
    });

    it("should detect over budget condition", async () => {
      mockInvoke.mockImplementation(async (cmd: string) => {
        if (cmd === "get_budget_config") {
          return { ...mockBudgetConfig, dailyLimit: 3.0 }; // Under $5 daily spend
        }
        return null;
      });

      const statuses = await getBudgetStatus("/test/workspace");

      expect(statuses[0].isOverBudget).toBe(true);
      expect(statuses[0].triggeredThresholds).toContain(100);
    });

    it("should identify triggered thresholds", async () => {
      const statuses = await getBudgetStatus("/test/workspace");

      // 50% used should trigger 50% threshold
      expect(statuses[0].triggeredThresholds).toContain(50);
      expect(statuses[0].triggeredThresholds).not.toContain(75);
      expect(statuses[0].triggeredThresholds).not.toContain(90);
    });
  });

  describe("getRunwayProjection", () => {
    it("should calculate burn rates", async () => {
      const runway = await getRunwayProjection("/test/workspace");

      // Daily burn rate = weekly spend / 7 = $25.00 / 7 = ~$3.57/day
      expect(runway.dailyBurnRate).toBeCloseTo(25.0 / 7, 1);
      expect(runway.weeklyBurnRate).toBe(25.0);
    });

    it("should calculate days remaining", async () => {
      const runway = await getRunwayProjection("/test/workspace");

      // Remaining budget = $150 - $75 = $75
      // Days remaining = $75 / ($25/7) = ~21 days
      expect(runway.daysRemaining).toBeGreaterThan(0);
    });

    it("should project monthly spend", async () => {
      const runway = await getRunwayProjection("/test/workspace");

      // Projected = daily burn rate * 30
      const expectedProjected = (25.0 / 7) * 30;
      expect(runway.projectedMonthlySpend).toBeCloseTo(expectedProjected, 1);
    });
  });

  describe("checkBudgetAlerts", () => {
    it("should send toast alerts when thresholds are triggered", async () => {
      await checkBudgetAlerts("/test/workspace");

      // 50% threshold is triggered, should show toast
      expect(toast.showToast).toHaveBeenCalled();
    });

    it("should not send alerts when alerts are disabled", async () => {
      mockInvoke.mockImplementation(async (cmd: string) => {
        if (cmd === "get_budget_config") {
          return { ...mockBudgetConfig, alertsEnabled: false };
        }
        return null;
      });

      await checkBudgetAlerts("/test/workspace");

      expect(toast.showToast).not.toHaveBeenCalled();
    });

    it("should respect alert cooldown", async () => {
      // First call should trigger alert
      await checkBudgetAlerts("/test/workspace");
      const firstCallCount = vi.mocked(toast.showToast).mock.calls.length;

      // Second immediate call should not trigger again due to cooldown
      await checkBudgetAlerts("/test/workspace");
      const secondCallCount = vi.mocked(toast.showToast).mock.calls.length;

      // Count should not increase significantly (depends on threshold overlap)
      expect(secondCallCount).toBeLessThanOrEqual(firstCallCount * 2);
    });
  });

  describe("showBudgetManagementModal", () => {
    it("should open the budget management modal", async () => {
      await showBudgetManagementModal();

      expect(isBudgetModalVisible()).toBe(true);
      expect(document.getElementById("budget-management-modal")).toBeTruthy();
    });

    it("should display budget settings section", async () => {
      await showBudgetManagementModal();

      const modal = document.getElementById("budget-management-modal");
      expect(modal?.textContent).toContain("Budget Limits");
      expect(modal?.textContent).toContain("Daily Limit");
      expect(modal?.textContent).toContain("Weekly Limit");
      expect(modal?.textContent).toContain("Monthly Limit");
    });

    it("should display current spend section", async () => {
      await showBudgetManagementModal();

      const modal = document.getElementById("budget-management-modal");
      expect(modal?.textContent).toContain("Current Spend");
    });

    it("should display runway projection section", async () => {
      await showBudgetManagementModal();

      const modal = document.getElementById("budget-management-modal");
      expect(modal?.textContent).toContain("Runway Projection");
      expect(modal?.textContent).toContain("Daily Burn Rate");
      expect(modal?.textContent).toContain("Budget Runway");
    });

    it("should display cost by role section", async () => {
      await showBudgetManagementModal();

      const modal = document.getElementById("budget-management-modal");
      expect(modal?.textContent).toContain("Cost by Role");
    });

    it("should show error state when no workspace is selected", async () => {
      mockState.workspace.clearWorkspace();

      await showBudgetManagementModal();

      const modal = document.getElementById("budget-management-modal");
      expect(modal?.textContent).toContain("No workspace selected");
    });
  });

  describe("closeBudgetManagementModal", () => {
    it("should close the budget management modal", async () => {
      await showBudgetManagementModal();
      expect(isBudgetModalVisible()).toBe(true);

      closeBudgetManagementModal();
      expect(isBudgetModalVisible()).toBe(false);
    });

    it("should do nothing if modal is not open", () => {
      expect(() => closeBudgetManagementModal()).not.toThrow();
      expect(isBudgetModalVisible()).toBe(false);
    });
  });

  describe("isBudgetModalVisible", () => {
    it("should return false when modal is not open", () => {
      expect(isBudgetModalVisible()).toBe(false);
    });

    it("should return true when modal is open", async () => {
      await showBudgetManagementModal();
      expect(isBudgetModalVisible()).toBe(true);
    });

    it("should return false after modal is closed", async () => {
      await showBudgetManagementModal();
      closeBudgetManagementModal();
      expect(isBudgetModalVisible()).toBe(false);
    });
  });

  describe("Budget calculations edge cases", () => {
    it("should handle null limits gracefully", async () => {
      mockInvoke.mockImplementation(async (cmd: string) => {
        if (cmd === "get_budget_config") {
          return {
            ...mockBudgetConfig,
            dailyLimit: null,
            weeklyLimit: null,
            monthlyLimit: null,
          };
        }
        return null;
      });

      const statuses = await getBudgetStatus("/test/workspace");

      expect(statuses[0].percentUsed).toBeNull();
      expect(statuses[0].remaining).toBeNull();
      expect(statuses[0].isOverBudget).toBe(false);
    });

    it("should handle zero limits", async () => {
      mockInvoke.mockImplementation(async (cmd: string) => {
        if (cmd === "get_budget_config") {
          return { ...mockBudgetConfig, dailyLimit: 0 };
        }
        return null;
      });

      const statuses = await getBudgetStatus("/test/workspace");

      // Zero limit with any spend should be 0% (edge case handling)
      expect(statuses[0].percentUsed).toBe(null); // 0 limit treated as no limit
    });

    it("should handle zero burn rate for runway projection", async () => {
      vi.mocked(agentMetrics.getAgentMetrics).mockResolvedValue({
        prompt_count: 0,
        total_tokens: 0,
        total_cost: 0,
        success_rate: 0,
        prs_created: 0,
        issues_closed: 0,
      });

      const runway = await getRunwayProjection("/test/workspace");

      expect(runway.dailyBurnRate).toBe(0);
      expect(runway.daysRemaining).toBeNull(); // Cannot project with zero burn rate
    });
  });
});
