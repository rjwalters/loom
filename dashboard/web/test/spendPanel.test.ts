/**
 * The "elastic spend this period" panel (Issue #8306, Phase 3 of #8257).
 *
 * The assertions that matter: a period selector that actually re-queries the
 * backend with the right window, a headline total plus a per-day breakdown
 * drawn against the standing daily ceiling, and three states that must never
 * be confused for one another — withheld, a real `$0.00` window, and a
 * breakdown.
 */

import { describe, expect, it, vi } from "vitest";

import {
  DEFAULT_DAILY_CEILING_USD,
  SPEND_WINDOWS,
  SpendPanel,
  ceilingFraction,
  fetchSpend,
  formatUsd,
  isWithheld,
  parseSpendResponse,
  renderSpend,
  spendWindow,
  type SpendResponse,
  type SpendSummary,
} from "../src/spendPanel";
import type { FetchLike } from "../src/historyClient";

const NOW = new Date("2026-09-19T18:00:00Z");
const WEEK = spendWindow("7d");

const SUMMARY: SpendSummary = {
  since: "2026-09-12T18:00:00Z",
  until: "2026-09-19T18:00:00Z",
  totalCostUsd: 137.5,
  jobCount: 9,
  totalWallClockSec: 41_400,
  peakDailyCostUsd: 104.25,
  days: [
    { day: "2026-09-17", costUsd: 33.25, jobCount: 3 },
    { day: "2026-09-18", costUsd: 104.25, jobCount: 6 },
  ],
};

/** A `FetchLike` stub serving one fixed body, recording every URL it saw. */
function stubFetch(body: unknown, ok = true): { fetchImpl: FetchLike; calls: string[] } {
  const calls: string[] = [];
  const fetchImpl: FetchLike = async (url: string) => {
    calls.push(url);
    return { ok, status: ok ? 200 : 500, async json() { return body; } };
  };
  return { fetchImpl, calls };
}

function render(response: SpendResponse, ceiling?: number | null): HTMLElement {
  const container = document.createElement("div");
  renderSpend(container, response, {
    window: WEEK,
    ...(ceiling === undefined ? {} : { dailyCeilingUsd: ceiling }),
  });
  return container;
}

describe("formatUsd", () => {
  it("always shows two decimals, and an em dash for an unknown figure", () => {
    expect(formatUsd(137.5)).toBe("$137.50");
    expect(formatUsd(0)).toBe("$0.00");
    // `null` is "no peak day because there was no spend" — NOT "$0.00", which
    // would claim a day existed and cost nothing.
    expect(formatUsd(null)).toBe("—");
    expect(formatUsd(undefined)).toBe("—");
    expect(formatUsd(Number.NaN)).toBe("—");
  });
});

describe("ceilingFraction", () => {
  it("is the fraction of the ceiling, clamped to [0, 1]", () => {
    expect(ceilingFraction(50, 100)).toBe(0.5);
    // A day that blew through the ceiling still draws a full bar rather than
    // overflowing its track — the `--over-ceiling` flag is what says "over".
    expect(ceilingFraction(250, 100)).toBe(1);
    expect(ceilingFraction(-5, 100)).toBe(0);
  });

  it("is 0 when there is no ceiling to compare against", () => {
    expect(ceilingFraction(50, null)).toBe(0);
    expect(ceilingFraction(50, 0)).toBe(0);
  });
});

describe("spendWindow", () => {
  it("falls back to the default rather than throwing on an unknown id", () => {
    expect(spendWindow("7d").days).toBe(7);
    expect(spendWindow("nonsense").id).toBe("7d");
  });
});

describe("parseSpendResponse", () => {
  it("narrows a full summary body", () => {
    const parsed = parseSpendResponse(SUMMARY);
    expect(isWithheld(parsed)).toBe(false);
    expect(parsed).toMatchObject({ totalCostUsd: 137.5, jobCount: 9, peakDailyCostUsd: 104.25 });
    expect((parsed as SpendSummary).days).toHaveLength(2);
  });

  it("keeps a null wall clock null rather than coercing it to 0", () => {
    const parsed = parseSpendResponse({ ...SUMMARY, totalWallClockSec: null }) as SpendSummary;
    expect(parsed.totalWallClockSec).toBeNull();
  });

  it("recognises the withheld public body", () => {
    const parsed = parseSpendResponse({ since: "s", until: "u", withheld: true });
    expect(isWithheld(parsed)).toBe(true);
  });

  it("degrades an unreadable body to withheld rather than to a fake $0 window", () => {
    for (const body of [null, undefined, 42, "nope"]) {
      expect(isWithheld(parseSpendResponse(body))).toBe(true);
    }
  });

  it("drops a malformed day row instead of rendering it under a blank date", () => {
    const parsed = parseSpendResponse({
      ...SUMMARY,
      days: [{ day: "2026-09-18", costUsd: 1, jobCount: 1 }, { costUsd: 2 }, null, "nope"],
    }) as SpendSummary;
    expect(parsed.days).toEqual([{ day: "2026-09-18", costUsd: 1, jobCount: 1 }]);
  });
});

describe("fetchSpend", () => {
  it("asks for exactly the selected window, as a half-open [since, now) range", async () => {
    const { fetchImpl, calls } = stubFetch(SUMMARY);
    await fetchSpend({ basePath: "/api/spend", window: WEEK, now: NOW, fetchImpl });

    const url = new URL(calls[0]!, "https://example.test");
    expect(url.pathname).toBe("/api/spend");
    expect(url.searchParams.get("until")).toBe("2026-09-19T18:00:00.000Z");
    expect(url.searchParams.get("since")).toBe("2026-09-12T18:00:00.000Z");
  });

  it("throws on a non-2xx so the caller can render the message", async () => {
    const { fetchImpl } = stubFetch({}, false);
    await expect(fetchSpend({ basePath: "/api/spend", window: WEEK, now: NOW, fetchImpl })).rejects.toThrow(
      "returned 500",
    );
  });
});

describe("renderSpend", () => {
  it("shows the headline total, job count, compute time and peak day against the ceiling", () => {
    const container = render(SUMMARY);
    expect(container.querySelector('[data-testid="spend-total"]')?.textContent).toBe("$137.50");
    expect(container.querySelector('[data-testid="spend-peak-day"]')?.textContent).toBe("$104.25 of $100.00");
    expect(container.textContent).toContain("11h 30m");
  });

  it("flags a day that breached the standing ceiling, and does not flag one that did not", () => {
    const container = render(SUMMARY);
    const days = [...container.querySelectorAll<HTMLElement>('[data-testid="spend-day"]')];
    expect(days.map((day) => day.getAttribute("data-day"))).toEqual(["2026-09-17", "2026-09-18"]);
    expect(days[0]!.getAttribute("data-over")).toBe("false");
    expect(days[1]!.getAttribute("data-over")).toBe("true");
    expect(days[1]!.className).toContain("spend-day--over-ceiling");
    // The peak-day field carries the same flag, so the breach is visible
    // without scanning the list.
    expect(container.querySelector('[data-testid="spend-peak-day"]')?.className).toContain("spend--over-ceiling");
  });

  it("draws each bar proportional to the ceiling", () => {
    const container = render({ ...SUMMARY, days: [{ day: "2026-09-18", costUsd: 25, jobCount: 1 }] });
    const fill = container.querySelector<HTMLElement>(".spend-day__fill");
    expect(fill?.getAttribute("style")).toBe("width: 25.0%");
  });

  it("renders a window with no completed jobs as a real $0.00, not a blank panel", () => {
    const container = render({
      since: null,
      until: null,
      totalCostUsd: 0,
      jobCount: 0,
      totalWallClockSec: null,
      peakDailyCostUsd: null,
      days: [],
    });
    // The headline answers the question the reader came with…
    expect(container.querySelector('[data-testid="spend-total"]')?.textContent).toBe("$0.00");
    // …and the note explains why there is no day list below it.
    expect(container.querySelector('[data-testid="spend-empty"]')?.textContent).toContain("No ephemeral compute");
    expect(container.querySelector('[data-testid="spend-days"]')).toBeNull();
    // No spend means no peak day — an em dash, not a $0.00 peak.
    expect(container.querySelector('[data-testid="spend-peak-day"]')?.textContent).toBe("— of $100.00");
  });

  it("renders the withheld public body as a notice, never as $0.00", () => {
    const container = render({ since: null, until: null, withheld: true });
    expect(container.querySelector('[data-testid="spend-withheld"]')).not.toBeNull();
    expect(container.textContent).not.toContain("$0.00");
    expect(container.querySelector('[data-testid="spend-total"]')).toBeNull();
    expect(container.querySelector('[data-testid="spend-days"]')).toBeNull();
  });

  it("omits the ceiling reference entirely when a deployment has no standing budget", () => {
    const container = render(SUMMARY, null);
    expect(container.querySelector('[data-testid="spend-peak-day"]')?.textContent).toBe("$104.25");
    expect(container.textContent).not.toContain("standing daily ceiling");
    // With no ceiling there is nothing to be "over", so no day is flagged.
    const days = [...container.querySelectorAll<HTMLElement>('[data-testid="spend-day"]')];
    expect(days.every((day) => day.getAttribute("data-over") === "false")).toBe(true);
  });

  it("defaults to the reference deployment's standing ceiling", () => {
    expect(DEFAULT_DAILY_CEILING_USD).toBe(100);
  });
});

describe("SpendPanel", () => {
  it("builds the period selector before the first fetch resolves", () => {
    const { fetchImpl } = stubFetch(SUMMARY);
    const container = document.createElement("div");
    new SpendPanel({ basePath: "/api/spend", container, now: () => NOW, fetchImpl });

    const select = container.querySelector<HTMLSelectElement>('[data-testid="spend-period"]');
    expect(select).not.toBeNull();
    expect([...select!.options].map((option) => option.value)).toEqual(SPEND_WINDOWS.map((w) => w.id));
    expect(select!.value).toBe("7d");
  });

  it("re-queries with the newly selected window without rebuilding the selector", async () => {
    const { fetchImpl, calls } = stubFetch(SUMMARY);
    const container = document.createElement("div");
    const panel = new SpendPanel({ basePath: "/api/spend", container, now: () => NOW, fetchImpl });
    await panel.refresh();

    const select = container.querySelector<HTMLSelectElement>('[data-testid="spend-period"]')!;
    select.value = "1d";
    select.dispatchEvent(new Event("change"));
    // The handler is async; let its microtask chain drain.
    await vi.waitFor(() => expect(calls).toHaveLength(2));

    expect(panel.currentWindow.days).toBe(1);
    expect(new URL(calls[1]!, "https://example.test").searchParams.get("since")).toBe("2026-09-18T18:00:00.000Z");
    // The control that triggered the change survives the re-render — replacing
    // it would drop focus mid-interaction.
    expect(container.querySelector('[data-testid="spend-period"]')).toBe(select);
  });

  it("renders into the results region, leaving the header intact", async () => {
    const { fetchImpl } = stubFetch(SUMMARY);
    const container = document.createElement("div");
    const panel = new SpendPanel({ basePath: "/api/spend", container, now: () => NOW, fetchImpl });
    await panel.refresh();

    const results = container.querySelector('[data-testid="spend-results"]');
    expect(results?.querySelector('[data-testid="spend-total"]')?.textContent).toBe("$137.50");
    expect(container.querySelector(".spend__title")?.textContent).toBe("Elastic spend this period");
  });

  it("surfaces a failed period change in the panel rather than as an unhandled rejection", async () => {
    let ok = true;
    const calls: string[] = [];
    const fetchImpl: FetchLike = async (url: string) => {
      calls.push(url);
      return { ok, status: ok ? 200 : 503, async json() { return SUMMARY; } };
    };
    const container = document.createElement("div");
    const panel = new SpendPanel({ basePath: "/api/spend", container, now: () => NOW, fetchImpl });
    await panel.refresh();

    ok = false;
    const select = container.querySelector<HTMLSelectElement>('[data-testid="spend-period"]')!;
    select.value = "30d";
    select.dispatchEvent(new Event("change"));

    await vi.waitFor(() =>
      expect(container.querySelector('[data-testid="spend-error"]')?.textContent).toContain("503"),
    );
  });
});
