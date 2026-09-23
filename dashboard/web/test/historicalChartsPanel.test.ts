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

    // One query per chart record kind — never an unfiltered crawl of the
    // whole history table (the bug that left the Charts tab blank).
    expect(calls).toHaveLength(2);
    expect(calls[0]).toContain("/api/history");
    expect(outcomesContainer.querySelectorAll("rect").length).toBeGreaterThan(0);
    expect(successRateContainer.querySelectorAll("circle").length).toBeGreaterThan(0);
    expect(durationsContainer.querySelectorAll("rect").length).toBeGreaterThan(0);
  });

  it("asks only for sweep.completed and sweep.outcome, at the page-size cap, over a rolling window", async () => {
    resetFixtureIds();
    const { fetchImpl, calls } = stubFetch([]);
    const panel = new HistoricalChartsPanel({
      basePath: "/public/history",
      outcomesContainer: document.createElement("div"),
      successRateContainer: document.createElement("div"),
      durationsContainer: document.createElement("div"),
      fetchImpl,
      now: () => new Date("2026-09-21T12:00:00Z"),
    });

    await panel.refresh();

    const urls = calls.map((call) => new URL(call, "http://example.test"));
    expect(urls.map((url) => url.searchParams.get("kind")).sort()).toEqual(["sweep.completed", "sweep.outcome"]);
    for (const url of urls) {
      expect(url.searchParams.get("limit")).toBe("500");
      // 30 days before the injected clock.
      expect(url.searchParams.get("since")).toBe("2026-08-22T12:00:00.000Z");
    }
  });

  it("keeps a caller-supplied since instead of the rolling window", async () => {
    resetFixtureIds();
    const { fetchImpl, calls } = stubFetch([]);
    const panel = new HistoricalChartsPanel({
      basePath: "/api/history",
      outcomesContainer: document.createElement("div"),
      successRateContainer: document.createElement("div"),
      durationsContainer: document.createElement("div"),
      filter: { since: "2026-01-01T00:00:00Z" },
      fetchImpl,
    });
    await panel.refresh();
    expect(new URL(calls[0]!, "http://example.test").searchParams.get("since")).toBe("2026-01-01T00:00:00Z");
  });

  it("shows a loading state while fetching and a no-data state for an empty window", async () => {
    resetFixtureIds();
    const outcomesContainer = document.createElement("div");
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const fetchImpl: FetchLike = async () => {
      await gate;
      return { ok: true, status: 200, async json() { return { records: [], nextCursor: null }; } };
    };
    const panel = new HistoricalChartsPanel({
      basePath: "/api/history",
      outcomesContainer,
      successRateContainer: document.createElement("div"),
      durationsContainer: document.createElement("div"),
      fetchImpl,
    });

    const pending = panel.refresh();
    expect(outcomesContainer.querySelector('[data-testid="charts-loading"]')).not.toBeNull();
    release();
    await pending;
    expect(outcomesContainer.querySelector('[data-testid="charts-loading"]')).toBeNull();
    expect(outcomesContainer.querySelector('[data-testid="charts-empty"]')?.textContent).toBe(
      "No completed sweeps in the last 30 days.",
    );
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
