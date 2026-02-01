import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

vi.mock("../agent-metrics", () => ({
  getAgentMetrics: vi.fn().mockResolvedValue({
    prompt_count: 0,
    total_tokens: 0,
    total_cost: 0,
    success_rate: 0,
    prs_created: 0,
    issues_closed: 0,
  }),
  getVelocitySummary: vi.fn().mockResolvedValue({
    issues_closed: 0,
    issues_trend: "stable",
    prs_merged: 0,
    prs_trend: "stable",
    total_cost_usd: 0,
    avg_cycle_time_hours: null,
    cycle_time_trend: "stable",
  }),
}));

import {
  aggregateInputStats,
  collectAnalyticsData,
  type InputEntry,
  parseInputJSONL,
} from "./file-collector";

describe("parseInputJSONL", () => {
  it("parses valid JSONL lines", () => {
    const content = [
      '{"timestamp":"2026-01-01T00:00:00Z","type":"keystroke","length":1,"preview":"a","terminalId":"t-1"}',
      '{"timestamp":"2026-01-01T00:01:00Z","type":"command","length":5,"preview":"build","terminalId":"t-2"}',
    ].join("\n");

    const entries = parseInputJSONL(content);
    expect(entries).toHaveLength(2);
    expect(entries[0].type).toBe("keystroke");
    expect(entries[0].length).toBe(1);
    expect(entries[1].type).toBe("command");
    expect(entries[1].terminalId).toBe("t-2");
  });

  it("skips malformed JSON lines", () => {
    const content = [
      '{"timestamp":"2026-01-01T00:00:00Z","type":"keystroke","length":1,"preview":"a","terminalId":"t-1"}',
      "not valid json",
      '{"timestamp":"2026-01-01T00:02:00Z","type":"paste","length":100,"preview":"code","terminalId":"t-1"}',
    ].join("\n");

    const entries = parseInputJSONL(content);
    expect(entries).toHaveLength(2);
    expect(entries[0].type).toBe("keystroke");
    expect(entries[1].type).toBe("paste");
  });

  it("skips empty lines", () => {
    const content =
      "\n\n" + '{"timestamp":"t","type":"enter","length":0,"preview":"","terminalId":"x"}' + "\n\n";
    const entries = parseInputJSONL(content);
    expect(entries).toHaveLength(1);
  });

  it("returns empty array for empty content", () => {
    expect(parseInputJSONL("")).toEqual([]);
    expect(parseInputJSONL("\n\n\n")).toEqual([]);
  });

  it("skips lines missing required fields", () => {
    const content = [
      '{"timestamp":"t","type":"keystroke"}', // missing length
      '{"type":"command","length":5}', // missing timestamp
      '{"timestamp":"t","length":5}', // missing type
      '{"timestamp":"t","type":"paste","length":10,"preview":"ok","terminalId":"x"}', // valid
    ].join("\n");

    const entries = parseInputJSONL(content);
    expect(entries).toHaveLength(1);
    expect(entries[0].type).toBe("paste");
  });

  it("handles missing optional fields gracefully", () => {
    const content = '{"timestamp":"t","type":"keystroke","length":1}';
    const entries = parseInputJSONL(content);
    expect(entries).toHaveLength(1);
    expect(entries[0].preview).toBe("");
    expect(entries[0].terminalId).toBe("");
  });
});

describe("aggregateInputStats", () => {
  it("counts entries by type", () => {
    const entries: InputEntry[] = [
      { timestamp: "t", type: "keystroke", length: 1, preview: "", terminalId: "" },
      { timestamp: "t", type: "keystroke", length: 2, preview: "", terminalId: "" },
      { timestamp: "t", type: "command", length: 5, preview: "", terminalId: "" },
      { timestamp: "t", type: "paste", length: 100, preview: "", terminalId: "" },
      { timestamp: "t", type: "enter", length: 0, preview: "", terminalId: "" },
    ];

    const stats = aggregateInputStats(entries);
    expect(stats.totalEntries).toBe(5);
    expect(stats.keystrokes).toBe(2);
    expect(stats.commands).toBe(1);
    expect(stats.pastes).toBe(1);
    expect(stats.enters).toBe(1);
    expect(stats.totalCharacters).toBe(108);
  });

  it("returns zeros for empty entries", () => {
    const stats = aggregateInputStats([]);
    expect(stats.totalEntries).toBe(0);
    expect(stats.keystrokes).toBe(0);
    expect(stats.commands).toBe(0);
    expect(stats.pastes).toBe(0);
    expect(stats.enters).toBe(0);
    expect(stats.totalCharacters).toBe(0);
  });
});

describe("collectAnalyticsData", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Default: read_text_file fails (no files available)
    mockInvoke.mockRejectedValue(new Error("file not found"));
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it("returns a complete AnalyticsData object", async () => {
    const data = await collectAnalyticsData("/workspace");
    expect(data.collectedAt).toBeDefined();
    expect(data.todayMetrics).toBeDefined();
    expect(data.weekMetrics).toBeDefined();
    expect(data.velocity).toBeDefined();
    expect(data.inputStats).toBeDefined();
    expect(data.gitStats).toBeDefined();
  });

  it("returns empty input stats when log file is missing", async () => {
    const data = await collectAnalyticsData("/workspace");
    expect(data.inputStats.totalEntries).toBe(0);
    expect(data.inputStats.keystrokes).toBe(0);
  });

  it("returns empty git stats when daemon state is missing", async () => {
    const data = await collectAnalyticsData("/workspace");
    expect(data.gitStats.commitsToday).toBe(0);
    expect(data.gitStats.activeBranches).toBe(0);
  });

  it("parses input log when available", async () => {
    const logContent = [
      '{"timestamp":"t","type":"keystroke","length":1,"preview":"a","terminalId":"t-1"}',
      '{"timestamp":"t","type":"command","length":5,"preview":"build","terminalId":"t-1"}',
    ].join("\n");

    mockInvoke.mockImplementation((cmd: string, payload: Record<string, string>) => {
      if (cmd === "read_text_file" && payload.path?.includes("input/")) {
        return Promise.resolve(logContent);
      }
      return Promise.reject(new Error("not found"));
    });

    const data = await collectAnalyticsData("/workspace");
    expect(data.inputStats.totalEntries).toBe(2);
    expect(data.inputStats.keystrokes).toBe(1);
    expect(data.inputStats.commands).toBe(1);
  });

  it("parses daemon state for git stats", async () => {
    const daemonState = JSON.stringify({
      total_prs_merged: 5,
      pipeline_state: {
        building: ["#1", "#2"],
        review_requested: ["PR #3"],
        ready_to_merge: ["PR #4"],
      },
    });

    mockInvoke.mockImplementation((cmd: string, payload: Record<string, string>) => {
      if (cmd === "read_text_file" && payload.path?.includes("daemon-state.json")) {
        return Promise.resolve(daemonState);
      }
      return Promise.reject(new Error("not found"));
    });

    const data = await collectAnalyticsData("/workspace");
    expect(data.gitStats.commitsToday).toBe(5);
    expect(data.gitStats.activeBranches).toBe(2);
    expect(data.gitStats.filesChanged).toBe(4); // building + review + ready
  });

  it("collectedAt is a valid ISO timestamp", async () => {
    const data = await collectAnalyticsData("/workspace");
    const parsed = new Date(data.collectedAt);
    expect(parsed.getTime()).not.toBeNaN();
  });
});
