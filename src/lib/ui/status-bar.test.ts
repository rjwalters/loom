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
import { renderStatusBar, stopStatusRefresh } from "./status-bar";

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

describe("renderStatusBar", () => {
  let leftContainer: HTMLElement;
  let rightContainer: HTMLElement;

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();

    const statusBar = document.createElement("div");
    statusBar.id = "status-bar";

    leftContainer = document.createElement("div");
    leftContainer.id = "status-left";
    statusBar.appendChild(leftContainer);

    rightContainer = document.createElement("div");
    rightContainer.id = "status-right";
    statusBar.appendChild(rightContainer);

    document.body.appendChild(statusBar);
  });

  afterEach(() => {
    stopStatusRefresh();
    vi.useRealTimers();
    document.body.innerHTML = "";
  });

  it("renders idle state indicator", async () => {
    await renderStatusBar(null, "idle");
    expect(leftContainer.innerHTML).toContain("Idle");
    expect(leftContainer.innerHTML).toContain("bg-green-500");
  });

  it("renders working state with pulse animation", async () => {
    await renderStatusBar(null, "working");
    expect(leftContainer.innerHTML).toContain("Working");
    expect(leftContainer.innerHTML).toContain("animate-pulse");
  });

  it("renders error state indicator", async () => {
    await renderStatusBar(null, "error");
    expect(leftContainer.innerHTML).toContain("Error");
    expect(leftContainer.innerHTML).toContain("bg-red-500");
  });

  it("renders stopped state indicator", async () => {
    await renderStatusBar(null, "stopped");
    expect(leftContainer.innerHTML).toContain("Stopped");
    expect(leftContainer.innerHTML).toContain("bg-gray-400");
  });

  it("clears right container when no workspace", async () => {
    await renderStatusBar(null, "idle");
    expect(rightContainer.innerHTML).toBe("");
  });

  it("renders metrics summary with workspace", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderStatusBar("/workspace", "working");

    expect(rightContainer.innerHTML).toContain("12 prompts");
    expect(rightContainer.innerHTML).toContain("5 issues");
    expect(rightContainer.innerHTML).toContain("3 PRs");
    expect(rightContainer.innerHTML).toContain("$1.50");
  });

  it("renders 'No activity today' when metrics are zero", async () => {
    mockCollectAnalyticsData.mockResolvedValue(
      makeMockData({
        todayMetrics: {
          prompt_count: 0,
          total_tokens: 0,
          total_cost: 0,
          success_rate: 0,
          prs_created: 0,
          issues_closed: 0,
        },
      })
    );
    await renderStatusBar("/workspace", "idle");
    expect(rightContainer.innerHTML).toContain("No activity today");
  });

  it("clears right container on collector error", async () => {
    mockCollectAnalyticsData.mockRejectedValue(new Error("fail"));
    await renderStatusBar("/workspace", "working");
    expect(rightContainer.innerHTML).toBe("");
  });

  it("does nothing if containers are missing", async () => {
    document.body.innerHTML = "";
    // Should not throw
    await renderStatusBar("/workspace", "working");
  });

  it("auto-refreshes metrics after interval", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderStatusBar("/workspace", "working");

    expect(mockCollectAnalyticsData).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(30000);
    expect(mockCollectAnalyticsData).toHaveBeenCalledTimes(2);
  });

  it("stopStatusRefresh prevents further refreshes", async () => {
    mockCollectAnalyticsData.mockResolvedValue(makeMockData());
    await renderStatusBar("/workspace", "working");

    stopStatusRefresh();

    await vi.advanceTimersByTimeAsync(60000);
    expect(mockCollectAnalyticsData).toHaveBeenCalledTimes(1);
  });
});
