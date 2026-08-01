import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  computeTokenAnalytics,
  DEFAULT_SURFACE,
  mountTokenAnalytics,
  renderTokenAnalytics,
} from "../src/analytics/render.js";
import { currentSurface } from "../src/analytics/bootstrap.js";
import type { HistoryEnvelope } from "../src/analytics/types.js";
import {
  HOUR,
  MINUTE,
  T0,
  at,
  newestFirst,
  poolTokensSnapshot,
  resetIds,
  sweepRecords,
  tokensSnapshot,
} from "./analyticsFixtures.js";

beforeEach(() => {
  resetIds();
  document.body.replaceChildren();
});

const NOW = T0 + 20 * MINUTE;

/** A realistic mixed page: a healthy account, an exhausted one, two repos. */
function fixturePage(): HistoryEnvelope[] {
  return newestFirst([
    tokensSnapshot(T0, [
      { account: "agent-1", rank: 0, usage: 0.2, resetAt: T0 + 8 * HOUR },
      { account: "agent-2", rank: 1, usage: 0.95, resetAt: T0 + 8 * HOUR },
    ]),
    tokensSnapshot(T0 + 10 * MINUTE, [
      { account: "agent-1", rank: 0, usage: 0.21, resetAt: T0 + 8 * HOUR },
      { account: "agent-2", rank: 1, usage: 1, resetAt: T0 + 8 * HOUR, exhausted: true },
    ]),
    tokensSnapshot(T0 + 20 * MINUTE, [
      { account: "agent-1", rank: 0, usage: 0.22, resetAt: T0 + 8 * HOUR },
      { account: "agent-2", rank: 1, usage: 1, resetAt: T0 + 8 * HOUR, exhausted: true },
    ]),
    ...sweepRecords({
      sweepId: "sweep-issue-1-0",
      repo: "rjwalters/loom",
      startedAt: T0,
      completedAt: T0 + 10 * MINUTE,
      model: "opus",
      issue: 1,
    }),
    ...sweepRecords({
      sweepId: "sweep-issue-2-0",
      repo: "rjwalters/anvil",
      startedAt: T0 + 10 * MINUTE,
      completedAt: T0 + 20 * MINUTE,
      model: "sonnet",
      issue: 2,
    }),
  ]);
}

function mount(): HTMLElement {
  const container = document.createElement("div");
  document.body.appendChild(container);
  return container;
}

function fetchReturning(records: HistoryEnvelope[]): typeof fetch {
  return vi.fn(async () =>
    ({
      ok: true,
      status: 200,
      json: async () => ({ records, nextCursor: null }),
    }) as unknown as Response,
  ) as unknown as typeof fetch;
}

describe("computeTokenAnalytics", () => {
  it("produces curves, forecasts and attribution from one history page", () => {
    const analytics = computeTokenAnalytics(fixturePage(), { now: NOW });

    expect(analytics.curves.map((curve) => curve.account)).toEqual(["agent-1", "agent-2"]);
    expect(analytics.forecasts.map((forecast) => forecast.status)).toEqual(["resets-first", "exhausted"]);
    expect(analytics.attribution.repos.map((repo) => repo.repo).sort()).toEqual([
      "rjwalters/anvil",
      "rjwalters/loom",
    ]);
    expect(analytics.sweeps).toHaveLength(2);
    // The per-account page carries none of the aggregate shape.
    expect(analytics.poolSamples).toEqual([]);
    expect(analytics.poolCurves).toEqual([]);
    expect(analytics.poolHealth).toEqual([]);
  });

  it("produces pool curves and health from a /public/history-shaped page, and no per-account curves", () => {
    const records = newestFirst([
      poolTokensSnapshot(T0, { accountCount: 4, exhaustedCount: 1, meanUsage: 0.3, maxUsage: 0.6 }),
      poolTokensSnapshot(T0 + 10 * MINUTE, { accountCount: 4, exhaustedCount: 2, meanUsage: 0.4, maxUsage: 0.8 }),
    ]);
    const analytics = computeTokenAnalytics(records, { now: NOW });

    expect(analytics.poolCurves.map((curve) => curve.hostId)).toEqual(["host-a"]);
    expect(analytics.poolHealth).toEqual([
      { hostId: "host-a", accountCount: 4, exhaustedCount: 2, exhaustedFraction: 0.5, maxUsageFraction: 0.8, meanUsageFraction: 0.4 },
    ]);
    expect(analytics.curves).toEqual([]);
  });
});

describe("renderTokenAnalytics (authenticated surface)", () => {
  it("renders all three analytics blocks", () => {
    const container = mount();
    const rendered = renderTokenAnalytics(container, computeTokenAnalytics(fixturePage(), { now: NOW }), {
      surface: "authenticated",
      now: NOW,
    });

    expect(rendered).toBe(true);
    expect(container.querySelector('[data-testid="burn-curves"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="forecasts"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="attribution"]')).not.toBeNull();
  });

  it("draws one sparkline polyline per burn segment", () => {
    const container = mount();
    const records = newestFirst([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.8 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.95 }]),
      // Rollover: a second segment, drawn as a break rather than a plunge.
      tokensSnapshot(T0 + 20 * MINUTE, [{ account: "agent-1", usage: 0.05 }]),
      tokensSnapshot(T0 + 30 * MINUTE, [{ account: "agent-1", usage: 0.1 }]),
    ]);
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { now: NOW });

    expect(container.querySelectorAll(".sparkline__line")).toHaveLength(2);
  });

  it("flags an exhausted account distinctly from a healthy one", () => {
    const container = mount();
    renderTokenAnalytics(container, computeTokenAnalytics(fixturePage(), { now: NOW }), { now: NOW });

    const healthy = container.querySelector('.burn-card[data-account="agent-1"]');
    const exhausted = container.querySelector('.burn-card[data-account="agent-2"]');

    expect(healthy?.getAttribute("data-exhausted")).toBe("false");
    expect(healthy?.classList.contains("burn-card--exhausted")).toBe(false);
    expect(healthy?.querySelector('[data-testid="exhausted-badge"]')).toBeNull();

    expect(exhausted?.getAttribute("data-exhausted")).toBe("true");
    expect(exhausted?.classList.contains("burn-card--exhausted")).toBe(true);
    expect(exhausted?.querySelector('[data-testid="exhausted-badge"]')?.textContent).toBe("EXHAUSTED");

    // ...and again in the forecast table, by status rather than by colour alone.
    expect(container.querySelector('[data-testid="status-agent-2"]')?.textContent).toBe("Exhausted");
    expect(container.querySelector('tr[data-account="agent-2"]')?.classList.contains("row--at-risk")).toBe(true);
    expect(container.querySelector('tr[data-account="agent-1"]')?.classList.contains("row--at-risk")).toBe(false);
  });

  it("marks an account that was exhausted and has since rolled over as recovered", () => {
    const container = mount();
    const records = [
      tokensSnapshot(T0, [{ account: "agent-1", usage: 1, exhausted: true }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.05 }]),
    ];
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { now: NOW });

    const card = container.querySelector(".burn-card");
    expect(card?.classList.contains("burn-card--exhausted")).toBe(false);
    expect(card?.querySelector(".badge--recovered")?.textContent).toBe("recovered");
  });

  it("lists each repo's attributed usage and an unattributed row", () => {
    const container = mount();
    // One sweep, then an idle stretch that still burns tokens.
    const records = [
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      tokensSnapshot(T0 + 20 * MINUTE, [{ account: "agent-1", usage: 0.3 }]),
      ...sweepRecords({
        sweepId: "s-1",
        repo: "rjwalters/loom",
        startedAt: T0,
        completedAt: T0 + 10 * MINUTE,
        model: "opus",
      }),
    ];
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { now: NOW });

    expect(container.querySelector('tr[data-repo="rjwalters/loom"] .cell--repo')?.textContent).toBe(
      "rjwalters/loom",
    );
    expect(container.querySelector('tr[data-repo="(unattributed)"]')).not.toBeNull();
  });

  it("renders remote strings as text, never as markup", () => {
    const container = mount();
    const records = [
      tokensSnapshot(T0, [{ account: "<img src=x onerror=alert(1)>", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "<img src=x onerror=alert(1)>", usage: 0.2 }]),
      ...sweepRecords({
        sweepId: "s-1",
        repo: "<script>evil()</script>",
        startedAt: T0,
        completedAt: T0 + 10 * MINUTE,
      }),
    ];
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { now: NOW });

    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("script")).toBeNull();
    expect(container.textContent).toContain("<script>evil()</script>");
  });

  it("renders an empty-state note instead of empty tables", () => {
    const container = mount();
    renderTokenAnalytics(container, computeTokenAnalytics([], { now: NOW }), { now: NOW });

    expect(container.querySelector('[data-testid="burn-curves"]')?.textContent).toContain(
      "No tokens.snapshot history in range.",
    );
    expect(container.querySelector('[data-testid="attribution"]')?.textContent).toContain("No usage observed");
  });
});

/** The wire shape `/public/history` actually returns for `tokens.snapshot` —
 * the non-identifying pool aggregate, no `accounts[]` anywhere — plus the
 * sweep records a public visitor would also see. Mirrors `fixturePage()`'s
 * two-account, two-repo story so the two surfaces are easy to compare. */
function publicFixturePage(): HistoryEnvelope[] {
  return newestFirst([
    poolTokensSnapshot(T0, { accountCount: 2, exhaustedCount: 0, meanUsage: 0.575, maxUsage: 0.95, nextResetAt: T0 + 8 * HOUR }),
    poolTokensSnapshot(T0 + 10 * MINUTE, { accountCount: 2, exhaustedCount: 1, meanUsage: 0.605, maxUsage: 1, nextResetAt: T0 + 8 * HOUR }),
    poolTokensSnapshot(T0 + 20 * MINUTE, { accountCount: 2, exhaustedCount: 1, meanUsage: 0.61, maxUsage: 1, nextResetAt: T0 + 8 * HOUR }),
  ]);
}

describe("renderTokenAnalytics (public surface)", () => {
  it("renders the pool-level blocks and the operator-only notice, not the per-account ones", () => {
    const container = mount();
    const rendered = renderTokenAnalytics(container, computeTokenAnalytics(publicFixturePage(), { now: NOW }), {
      surface: "public",
      now: NOW,
    });

    expect(rendered).toBe(true);
    expect(container.querySelector('[data-testid="pool-burn"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="pool-health"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="analytics-operator-only-notice"]')).not.toBeNull();

    expect(container.querySelector('[data-testid="burn-curves"]')).toBeNull();
    expect(container.querySelector('[data-testid="forecasts"]')).toBeNull();
    expect(container.querySelector('[data-testid="attribution"]')).toBeNull();
  });

  it("draws one host card with peak and mean lines from the pool aggregate", () => {
    const container = mount();
    renderTokenAnalytics(container, computeTokenAnalytics(publicFixturePage(), { now: NOW }), {
      surface: "public",
      now: NOW,
    });

    const card = container.querySelector('.burn-card[data-host="host-a"]');
    expect(card).not.toBeNull();
    expect(card?.getAttribute("data-account-count")).toBe("2");
    expect(card?.getAttribute("data-exhausted-count")).toBe("1");
    expect(card?.querySelector('[data-testid="pool-exhausted-badge"]')?.textContent).toContain("1/2");
    // Both lines are drawn: the solid peak line and the dashed mean line.
    expect(card?.querySelectorAll(".sparkline__line")).toHaveLength(2);
    expect(card?.querySelectorAll(".sparkline__line--mean")).toHaveLength(1);
  });

  it("reports pool exhaustion as measured, with no exhaustion ETA anywhere in the DOM", () => {
    const container = mount();
    renderTokenAnalytics(container, computeTokenAnalytics(publicFixturePage(), { now: NOW }), {
      surface: "public",
      now: NOW,
    });

    const row = container.querySelector('[data-testid="pool-health"] tr[data-host="host-a"]');
    expect(row?.textContent).toContain("1 (50.0%)");
    expect(container.querySelector('[data-testid="pool-forecast-decision"]')?.textContent).toContain(
      "No exhaustion timing is projected",
    );
    // The per-account forecast's vocabulary ("Will exhaust", "Resets first",
    // a margin) never appears on the public surface.
    expect(container.textContent).not.toContain("Will exhaust");
    expect(container.textContent).not.toContain("Resets first");
    // "Capacity returns" is populated — it names no account, so it is safe
    // to show even though no per-account ETA is projected.
    const cells = row?.querySelectorAll("td") ?? [];
    expect(cells[cells.length - 1]?.textContent).not.toBe("—");
  });

  it("renders sensibly when exhausted_count is 0", () => {
    const container = mount();
    const records = [poolTokensSnapshot(T0, { accountCount: 6, exhaustedCount: 0, maxUsage: 0.3 })];
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { surface: "public", now: NOW });

    const row = container.querySelector('[data-testid="pool-health"] tr[data-host="host-a"]');
    expect(row?.textContent).toContain("0 (0.0%)");
    expect(row?.classList.contains("row--at-risk")).toBe(false);
  });

  it("renders sensibly when exhausted_count is close to account_count", () => {
    const container = mount();
    const records = [poolTokensSnapshot(T0, { accountCount: 8, exhaustedCount: 7, maxUsage: 1 })];
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { surface: "public", now: NOW });

    const row = container.querySelector('[data-testid="pool-health"] tr[data-host="host-a"]');
    expect(row?.textContent).toContain("7 (87.5%)");
    expect(row?.classList.contains("row--at-risk")).toBe(true);
  });

  it("renders an empty-state note instead of an empty pool panel", () => {
    const container = mount();
    renderTokenAnalytics(container, computeTokenAnalytics([], { now: NOW }), { surface: "public", now: NOW });

    expect(container.querySelector('[data-testid="pool-burn"]')?.textContent).toContain(
      "No tokens.snapshot history in range.",
    );
    expect(container.querySelector('[data-testid="pool-health"]')?.textContent).toContain("No accounts observed");
  });

  // Mirrors `redaction.test.ts`'s "no account identifier survives, at any
  // depth" assertion, but against the rendered public panel's DOM rather
  // than the wire payload.
  it("no account identifier reaches the DOM on the public surface, at any depth", () => {
    const container = mount();
    const records = newestFirst([
      poolTokensSnapshot(T0, { accountCount: 2, exhaustedCount: 1, meanUsage: 0.5, maxUsage: 0.95 }),
      ...sweepRecords({ sweepId: "sweep-issue-1-0", repo: "rjwalters/loom", startedAt: T0, issue: 1 }),
    ]);
    renderTokenAnalytics(container, computeTokenAnalytics(records, { now: NOW }), { surface: "public", now: NOW });

    const serialized = container.innerHTML;
    for (const identifier of ["agent-1", "agent-2", "agent5-2amlogic", "rjwalters/loom", "rjwalters/anvil"]) {
      expect(serialized).not.toContain(identifier);
    }
  });
});

describe("public-exposure decision", () => {
  it("defaults to the authenticated surface", () => {
    expect(DEFAULT_SURFACE).toBe("authenticated");
  });

  it("issues no request to /api/* when mounting on the public surface", async () => {
    const container = mount();
    const fetchImpl = fetchReturning(publicFixturePage());

    const analytics = await mountTokenAnalytics(container, { surface: "public", now: NOW, fetchImpl });

    expect(analytics).toBeDefined();
    const calls = (fetchImpl as unknown as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls).toHaveLength(1);
    const url = String(at(at(calls, 0) as unknown[], 0));
    expect(url.startsWith("/public/history?")).toBe(true);
    expect(url).not.toContain("/api/");
    expect(container.querySelector('[data-testid="pool-burn"]')).not.toBeNull();
  });

  it("derives the surface from the server-injected auth state", () => {
    const scope = (value: unknown) => ({ __LOOM_FLEET__: value }) as unknown as typeof globalThis;

    expect(currentSurface(scope({ authenticated: true }))).toBe("authenticated");
    expect(currentSurface(scope({ authenticated: false }))).toBe("public");
  });

  // The single-URL layout (#4795) is why this is no longer path-derived: `/`
  // serves both audiences, so a pathname cannot identify the viewer. Failing
  // closed matters most for the shapes that are not an explicit `false` — a
  // page that never received the injection must not render the full panel.
  it.each([
    ["flag absent", {}],
    ["malformed", "authenticated"],
    ["null", null],
    ["undefined", undefined],
  ])("falls back to the public surface when the injected state is %s", (_label, value) => {
    const scope = { __LOOM_FLEET__: value } as unknown as typeof globalThis;
    expect(currentSurface(scope)).toBe("public");
  });

  it("falls back to the public surface when nothing was injected at all", () => {
    expect(currentSurface({} as unknown as typeof globalThis)).toBe("public");
  });
});

describe("mountTokenAnalytics (authenticated surface)", () => {
  it("fetches from /api and renders the panel", async () => {
    const container = mount();
    const fetchImpl = fetchReturning(fixturePage());

    const analytics = await mountTokenAnalytics(container, { now: NOW, fetchImpl });

    expect(analytics?.curves).toHaveLength(2);
    const calls = (fetchImpl as unknown as ReturnType<typeof vi.fn>).mock.calls;
    const url = String(at(at(calls, 0) as unknown[], 0));
    expect(url.startsWith("/api/history?")).toBe(true);
    expect(url).not.toContain("/public");
    expect(container.querySelector('[data-testid="attribution"]')).not.toBeNull();
  });

  it("shows an error state instead of throwing when the API fails", async () => {
    const container = mount();
    const fetchImpl = vi.fn(async () => ({ ok: false, status: 503 }) as unknown as Response) as unknown as typeof fetch;

    const analytics = await mountTokenAnalytics(container, { now: NOW, fetchImpl });

    expect(analytics).toBeUndefined();
    expect(container.querySelector(".analytics__error")?.textContent).toContain("503");
  });
});
