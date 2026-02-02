import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockRejectedValue(new Error("not available")),
}));

const mockCollectAnalyticsData = vi.fn();
vi.mock("../analytics/file-collector", () => ({
  collectAnalyticsData: (...args: unknown[]) => mockCollectAnalyticsData(...args),
}));

vi.mock("../logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      warn: vi.fn(),
      error: vi.fn(),
    }),
  },
}));

import type { AnalyticsData } from "../analytics/file-collector";
import { renderDashboardView, stopAutoRefresh } from "./dashboard-view";

function makeMockData(overrides?: Partial<AnalyticsData>): AnalyticsData {
  return {
    todayMetrics: {
      prompt_count: 12,
      total_tokens: 5000,
      total_cost: 1.5,
      success_rate: 0.85,
      prs_created: 3,
      issues_closed: 5,
    },
    weekMetrics: {
      prompt_count: 80,
      total_tokens: 45000,
      total_cost: 12.5,
      success_rate: 0.9,
      prs_created: 15,
      issues_closed: 20,
    },
    velocity: {
      issues_closed: 20,
      issues_trend: "improving" as const,
      prs_merged: 15,
      prs_trend: "stable" as const,
      total_cost_usd: 12.5,
      avg_cycle_time_hours: 2.5,
      cycle_time_trend: "improving" as const,
      total_prompts: 80,
      prev_issues_closed: 15,
      prev_prs_merged: 12,
      prev_avg_cycle_time_hours: 3.0,
    },
    inputStats: {
      totalEntries: 50,
      keystrokes: 30,
      commands: 10,
      pastes: 5,
      enters: 5,
      totalCharacters: 2000,
    },
    gitStats: {
      commitsToday: 4,
      filesChanged: 8,
      insertions: 200,
      deletions: 50,
      activeBranches: 3,
    },
    collectedAt: "2026-01-01T12:00:00Z",
    ...overrides,
  };
}

describe("renderDashboardView", () => {
  let container: HTMLElement;

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    container = document.createElement("div");
    container.id = "analytics-view";
    document.body.appendChild(container);
  });

  afterEach(() => {
    stopAutoRefresh();
    vi.useRealTimers();
    document.body.innerHTML = "";
  });

  it("renders empty state when workspacePath is null", async () => {
    await renderDashboardView(null);
    expect(container.innerHTML).toContain("Start a session to see analytics");
  });

  it("renders dashboard with data when workspace is provided", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderDashboardView("/workspace");

    expect(container.innerHTML).toContain("Analytics");
    expect(container.innerHTML).toContain("Session Summary");
    expect(container.innerHTML).toContain("Activity");
    expect(container.innerHTML).toContain("Pipeline");
    expect(container.innerHTML).toContain("Cost");
  });

  it("renders metric values from data", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderDashboardView("/workspace");

    // Prompt count
    expect(container.innerHTML).toContain("12");
    // Issues closed
    expect(container.innerHTML).toContain("5");
    // PRs created
    expect(container.innerHTML).toContain("3");
    // Success rate
    expect(container.innerHTML).toContain("85.0%");
  });

  it("renders error state when collector fails", async () => {
    mockCollectAnalyticsData.mockRejectedValue(new Error("fail"));
    await renderDashboardView("/workspace");

    expect(container.innerHTML).toContain("Failed to load analytics");
    expect(container.innerHTML).toContain("retry on next refresh");
  });

  it("does nothing if container element is missing", async () => {
    document.body.innerHTML = "";
    await renderDashboardView("/workspace");
    // No error thrown
  });

  it("auto-refreshes after interval", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderDashboardView("/workspace");

    expect(mockCollectAnalyticsData).toHaveBeenCalledTimes(1);

    // Advance past refresh interval (30s)
    mockCollectAnalyticsData.mockResolvedValue(
      makeMockData({ collectedAt: "2026-01-01T12:00:30Z" })
    );
    await vi.advanceTimersByTimeAsync(30000);

    expect(mockCollectAnalyticsData).toHaveBeenCalledTimes(2);
  });

  it("stopAutoRefresh prevents further refreshes", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderDashboardView("/workspace");

    stopAutoRefresh();

    await vi.advanceTimersByTimeAsync(60000);
    // Only the initial call, no refresh calls
    expect(mockCollectAnalyticsData).toHaveBeenCalledTimes(1);
  });

  it("renders input stats section", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderDashboardView("/workspace");

    expect(container.innerHTML).toContain("Commands");
    expect(container.innerHTML).toContain("Keystrokes");
    expect(container.innerHTML).toContain("Pastes");
  });

  it("renders cost section", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderDashboardView("/workspace");

    expect(container.innerHTML).toContain("$1.50");
    expect(container.innerHTML).toContain("$12.50");
  });
});
