import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  computeTokenAnalytics,
  mountTokenAnalytics,
  renderTokenAnalytics,
  REQUIRED_SURFACE,
} from "../src/analytics/render.js";
import { currentSurface } from "../src/analytics/bootstrap.js";
import type { HistoryEnvelope } from "../src/analytics/types.js";
import { HOUR, MINUTE, T0, at, newestFirst, resetIds, sweepRecords, tokensSnapshot } from "./analyticsFixtures.js";

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

describe("public-exposure decision", () => {
  it("defaults to the authenticated surface", () => {
    expect(REQUIRED_SURFACE).toBe("authenticated");
  });

  it("renders nothing but a withheld notice on the public surface", () => {
    const container = mount();
    const rendered = renderTokenAnalytics(container, computeTokenAnalytics(fixturePage(), { now: NOW }), {
      surface: "public",
      now: NOW,
    });

    expect(rendered).toBe(false);
    expect(container.querySelector('[data-testid="analytics-withheld"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="burn-curves"]')).toBeNull();
    expect(container.querySelector('[data-testid="forecasts"]')).toBeNull();
    expect(container.querySelector('[data-testid="attribution"]')).toBeNull();

    // Neither account identifiers nor repo names reach the DOM.
    const text = container.textContent ?? "";
    expect(text).not.toContain("agent-1");
    expect(text).not.toContain("agent-2");
    expect(text).not.toContain("rjwalters/loom");
    expect(text).not.toContain("rjwalters/anvil");
  });

  it("does not even fetch history on the public surface", async () => {
    const container = mount();
    const fetchImpl = fetchReturning(fixturePage());

    const analytics = await mountTokenAnalytics(container, { surface: "public", now: NOW, fetchImpl });

    expect(analytics).toBeUndefined();
    expect(fetchImpl).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="analytics-withheld"]')).not.toBeNull();
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
