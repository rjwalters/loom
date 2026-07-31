// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { HistoricalChartsPanel } from "../src/historicalChartsPanel.js";
import type { FetchLike } from "../src/historyClient.js";
import type { HistoryQueryResult, HistoryRecord } from "../src/types.js";
import { makeCompletedSweepPair, resetFixtureIds } from "./fixtures.js";

/** A `FetchLike` stub serving one fixed page, recording every URL it saw. */
function stubFetch(records: HistoryRecord[]): { fetchImpl: FetchLike; calls: string[] } {
  const calls: string[] = [];
  const page: HistoryQueryResult = { records, nextCursor: null };
  const fetchImpl: FetchLike = async (url: string) => {
    calls.push(url);
    return { ok: true, status: 200, async json() { return page; } };
  };
  return { fetchImpl, calls };
}

describe("HistoricalChartsPanel", () => {
  it("fetches once and renders all three charts from the result", async () => {
    resetFixtureIds();
    const records = makeCompletedSweepPair({
      sweepId: "s1",
      emittedAt: "2026-07-28T10:00:00Z",
      result: "success",
      model: "opus",
      totalDurationSec: 100,
      phaseDurations: [{ phase: "builder", duration_sec: 60 }],
    });
    const { fetchImpl, calls } = stubFetch(records);

    const outcomesContainer = document.createElement("div");
    const successRateContainer = document.createElement("div");
    const durationsContainer = document.createElement("div");

    const panel = new HistoricalChartsPanel({
      basePath: "/api/history",
      outcomesContainer,
      successRateContainer,
      durationsContainer,
      fetchImpl,
    });

    await panel.refresh();

    expect(calls).toHaveLength(1);
    expect(calls[0]).toContain("/api/history");
    expect(outcomesContainer.querySelectorAll("rect").length).toBeGreaterThan(0);
    expect(successRateContainer.querySelectorAll("circle").length).toBeGreaterThan(0);
    expect(durationsContainer.querySelectorAll("rect").length).toBeGreaterThan(0);
  });

  it("points at /public/history instead with no other code change", async () => {
    resetFixtureIds();
    const { fetchImpl, calls } = stubFetch([]);
    const panel = new HistoricalChartsPanel({
      basePath: "/public/history",
      outcomesContainer: document.createElement("div"),
      successRateContainer: document.createElement("div"),
      durationsContainer: document.createElement("div"),
      fetchImpl,
    });

    await panel.refresh();

    expect(calls[0]).toContain("/public/history");
  });

  it("merges a refresh-time filter into the last-applied filter and re-fetches with it", async () => {
    resetFixtureIds();
    const { fetchImpl, calls } = stubFetch([]);
    const panel = new HistoricalChartsPanel({
      basePath: "/api/history",
      outcomesContainer: document.createElement("div"),
      successRateContainer: document.createElement("div"),
      durationsContainer: document.createElement("div"),
      filter: { repo: "rjwalters/loom" },
      fetchImpl,
    });

    await panel.refresh({ model: "opus" });

    expect(panel.getFilter()).toEqual({ repo: "rjwalters/loom", model: "opus" });
    const url = new URL(calls[0]!, "http://example.test");
    expect(url.searchParams.get("repo")).toBe("rjwalters/loom");
    expect(url.searchParams.get("model")).toBe("opus");
  });
});
