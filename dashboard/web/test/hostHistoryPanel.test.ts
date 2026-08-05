import { describe, expect, it } from "vitest";
import { HOST_HISTORY_WINDOWS, HostHistoryPanel } from "../src/hostHistoryPanel.js";
import type { FetchLike } from "../src/historyClient.js";
import type { HistoryQueryResult, HistoryRecord, TelemetryRecord } from "../src/types.js";

const NOW = () => new Date("2026-08-01T12:00:00Z");

function healthRecord(overrides: {
  emittedAt: string;
  id: number;
  cpuIdleFraction?: number;
  worktreeRootFreeGb?: number;
  hostId?: string;
}): HistoryRecord {
  const record: Record<string, unknown> = { kind: "host.health" };
  if (overrides.cpuIdleFraction !== undefined) record.cpu_idle_fraction = overrides.cpuIdleFraction;
  if (overrides.worktreeRootFreeGb !== undefined) record.worktree_root_free_gb = overrides.worktreeRootFreeGb;

  return {
    id: overrides.id,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: overrides.hostId ?? "host-a",
    kind: "host.health",
    ingestedAt: overrides.emittedAt,
    record: record as unknown as TelemetryRecord,
  };
}

function sweepOutcome(overrides: { emittedAt: string; id: number; sweepId: string; result: string }): HistoryRecord {
  return {
    id: overrides.id,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: "host-a",
    kind: "sweep.outcome",
    sweepId: overrides.sweepId,
    ingestedAt: overrides.emittedAt,
    record: { kind: "sweep.outcome", sweep_id: overrides.sweepId, result: overrides.result } as unknown as TelemetryRecord,
  };
}

/** A single-page `FetchLike` stub, recording every URL it saw. */
function stubFetch(records: HistoryRecord[]): { fetchImpl: FetchLike; calls: string[] } {
  const calls: string[] = [];
  const page: HistoryQueryResult = { records, nextCursor: null };
  const fetchImpl: FetchLike = async (url: string) => {
    calls.push(url);
    return { ok: true, status: 200, async json() { return page; } };
  };
  return { fetchImpl, calls };
}

/** A two-page stub, so a test can assert `fetchAllHistory`'s pagination loop
 * (nextCursor) is actually exercised rather than assumed. */
function stubPaginatedFetch(pages: HistoryQueryResult[]): { fetchImpl: FetchLike; calls: string[] } {
  const calls: string[] = [];
  const fetchImpl: FetchLike = async (url: string) => {
    calls.push(url);
    const parsed = new URL(url, "http://example.test");
    const cursor = parsed.searchParams.get("cursor");
    const page = cursor === null ? pages[0]! : pages[Number(cursor)]!;
    return { ok: true, status: 200, async json() { return page; } };
  };
  return { fetchImpl, calls };
}

describe("HostHistoryPanel", () => {
  it("mounts its chrome (title, window buttons, three chart slots) synchronously", () => {
    const container = document.createElement("div");
    const { fetchImpl } = stubFetch([]);
    new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    expect(container.querySelector('[data-testid="host-history-panel"]')).not.toBeNull();
    for (const window of HOST_HISTORY_WINDOWS) {
      expect(container.querySelector(`[data-testid="history-window-${window}"]`)).not.toBeNull();
    }
    expect(container.querySelector('[data-testid="history-chart-cpu"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="history-chart-disk"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="history-chart-throughput"]')).not.toBeNull();
  });

  it("fetches host-scoped history filtered by the current window on refresh", async () => {
    const { fetchImpl, calls } = stubFetch([healthRecord({ emittedAt: "2026-08-01T10:00:00Z", id: 1 })]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "fleet-mac-1", container, fetchImpl, now: NOW });

    await panel.refresh();

    expect(calls).toHaveLength(1);
    const url = new URL(calls[0]!, "http://example.test");
    expect(url.pathname).toBe("/api/history");
    expect(url.searchParams.get("host")).toBe("fleet-mac-1");
    // Default window is 24h.
    expect(url.searchParams.get("since")).toBe("2026-07-31T12:00:00.000Z");
  });

  it("points at /public/history instead with no other code change", async () => {
    const { fetchImpl, calls } = stubFetch([]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/public/history", hostId: "host-a", container, fetchImpl, now: NOW });
    await panel.refresh();
    expect(calls[0]).toContain("/public/history");
  });

  it("renders CPU-idle, worktree-free, and throughput charts once history has data", async () => {
    const records = [
      healthRecord({ emittedAt: "2026-08-01T09:00:00Z", id: 1, cpuIdleFraction: 0.8, worktreeRootFreeGb: 200 }),
      healthRecord({ emittedAt: "2026-08-01T10:00:00Z", id: 2, cpuIdleFraction: 0.7, worktreeRootFreeGb: 190 }),
      sweepOutcome({ emittedAt: "2026-08-01T10:30:00Z", id: 3, sweepId: "s1", result: "success" }),
    ];
    const { fetchImpl } = stubFetch(records);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    await panel.refresh();

    expect(container.querySelector('[data-testid="history-empty"]')?.hasAttribute("hidden")).toBe(true);
    expect(container.querySelectorAll('[data-testid="history-chart-cpu"] circle').length).toBe(2);
    expect(container.querySelectorAll('[data-testid="history-chart-disk"] circle').length).toBe(2);
    expect(container.querySelectorAll('[data-testid="history-chart-throughput"] rect').length).toBeGreaterThan(0);
  });

  // Acceptance criterion (#5355): a host with no host.health history renders
  // the empty-state notice, not a broken chart.
  it("renders the empty-state notice for a host with no host.health history in the window", async () => {
    const { fetchImpl } = stubFetch([]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    await panel.refresh();

    const notice = container.querySelector('[data-testid="history-empty"]');
    expect(notice?.hasAttribute("hidden")).toBe(false);
    expect(notice?.textContent).toContain("no host.health history");
    // Chart slots exist in the DOM (constructed once, up front) but are
    // hidden, not left rendering a broken/empty chart in view.
    expect(container.querySelector(".history-panel__charts")?.hasAttribute("hidden")).toBe(true);
  });

  it("also treats sweep-only history (no host.health record at all) as the empty state", async () => {
    const { fetchImpl } = stubFetch([
      sweepOutcome({ emittedAt: "2026-08-01T10:00:00Z", id: 1, sweepId: "s1", result: "success" }),
    ]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    await panel.refresh();

    expect(container.querySelector('[data-testid="history-empty"]')?.hasAttribute("hidden")).toBe(false);
  });

  // Acceptance criterion (#5355): a window wider than one page of history
  // paginates via nextCursor rather than silently truncating at 500 records.
  it("paginates through nextCursor when the window's history spans multiple pages", async () => {
    const page0: HistoryQueryResult = {
      records: [healthRecord({ emittedAt: "2026-08-01T08:00:00Z", id: 1, cpuIdleFraction: 0.9 })],
      nextCursor: 1,
    };
    const page1: HistoryQueryResult = {
      records: [healthRecord({ emittedAt: "2026-08-01T09:00:00Z", id: 2, cpuIdleFraction: 0.8 })],
      nextCursor: null,
    };
    const { fetchImpl, calls } = stubPaginatedFetch([page0, page1]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    await panel.refresh();

    expect(calls).toHaveLength(2);
    expect(container.querySelectorAll('[data-testid="history-chart-cpu"] circle').length).toBe(2);
  });

  it("switches window on refresh(window), re-fetching with the new since bound", async () => {
    const { fetchImpl, calls } = stubFetch([healthRecord({ emittedAt: "2026-08-01T10:00:00Z", id: 1, cpuIdleFraction: 0.5 })]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    await panel.refresh("7d");

    expect(panel.getWindow()).toBe("7d");
    const url = new URL(calls[0]!, "http://example.test");
    expect(url.searchParams.get("since")).toBe("2026-07-25T12:00:00.000Z");
  });

  it("marks the active window button with aria-pressed", async () => {
    const { fetchImpl } = stubFetch([]);
    const container = document.createElement("div");
    const panel = new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    await panel.refresh("30d");

    expect(container.querySelector('[data-testid="history-window-30d"]')?.getAttribute("aria-pressed")).toBe("true");
    expect(container.querySelector('[data-testid="history-window-24h"]')?.getAttribute("aria-pressed")).toBe("false");
  });

  it("clicking a window button triggers a refresh with that window", async () => {
    const { fetchImpl, calls } = stubFetch([]);
    const container = document.createElement("div");
    new HostHistoryPanel({ basePath: "/api/history", hostId: "host-a", container, fetchImpl, now: NOW });

    container.querySelector<HTMLButtonElement>('[data-testid="history-window-7d"]')!.click();
    await Promise.resolve();
    await Promise.resolve();

    expect(calls).toHaveLength(1);
    const url = new URL(calls[0]!, "http://example.test");
    expect(url.searchParams.get("since")).toBe("2026-07-25T12:00:00.000Z");
  });
});
