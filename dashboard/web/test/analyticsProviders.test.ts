import { beforeEach, describe, expect, it } from "vitest";

import { buildBurnCurves } from "../src/analytics/burn.js";
import { forecastAccounts } from "../src/analytics/forecast.js";
import { parseTokenSamples } from "../src/analytics/parse.js";
import { computeTokenAnalytics, renderTokenAnalytics } from "../src/analytics/render.js";
import { HOUR, MINUTE, T0, newestFirst, resetIds, tokensSnapshot } from "./analyticsFixtures.js";

beforeEach(() => {
  resetIds();
  document.body.replaceChildren();
});

/** Two Claude accounts with burn telemetry, two Codex accounts with none —
 * and a Codex account that shares its name with a Claude one. */
function mixedPage() {
  return newestFirst([
    tokensSnapshot(T0, [
      { account: "agent-1", rank: 0, usage: 0.2, resetAt: T0 + 8 * HOUR },
      { account: "robb", provider: "claude", rank: 1, usage: 0.6, resetAt: T0 + 8 * HOUR },
      { account: "robb", provider: "codex" },
      { account: "cx-2", provider: "codex", exhausted: true, resetAt: T0 + 2 * HOUR },
    ]),
    tokensSnapshot(T0 + 10 * MINUTE, [
      { account: "agent-1", rank: 0, usage: 0.25, resetAt: T0 + 8 * HOUR },
      { account: "robb", provider: "claude", rank: 1, usage: 0.65, resetAt: T0 + 8 * HOUR },
      { account: "robb", provider: "codex" },
      { account: "cx-2", provider: "codex", exhausted: true, resetAt: T0 + 2 * HOUR },
    ]),
  ]);
}

describe("burn curves by provider", () => {
  it("keeps a Claude and a Codex account with the same name as two series, Claude group first", () => {
    const curves = buildBurnCurves(parseTokenSamples(mixedPage()));
    expect(curves.map((curve) => `${curve.provider}/${curve.account}`)).toEqual([
      "claude/agent-1",
      "claude/robb",
      "codex/cx-2",
      "codex/robb",
    ]);
    // The untagged row read as Claude; the Codex twin never merged into it.
    expect(curves[1]?.points.map((point) => point.usageFraction)).toEqual([0.6, 0.65]);
    expect(curves[3]?.points).toEqual([]);
    expect(curves[2]?.exhausted).toBe(true);
  });

  it("carries the provider through to the forecast", () => {
    const forecasts = forecastAccounts(buildBurnCurves(parseTokenSamples(mixedPage())), { now: T0 + 20 * MINUTE });
    expect(forecasts.map((forecast) => forecast.provider)).toEqual(["claude", "claude", "codex", "codex"]);
  });
});

describe("token analytics rendering by provider", () => {
  it("renders one burn-curve group per provider with its own availability count and mark", () => {
    const container = document.createElement("div");
    renderTokenAnalytics(container, computeTokenAnalytics(mixedPage(), { now: T0 + 20 * MINUTE }), {
      now: T0 + 20 * MINUTE,
      surface: "authenticated",
    });
    const groups = [...container.querySelectorAll('[data-testid="burn-group"]')];
    expect(groups.map((group) => group.getAttribute("data-provider"))).toEqual(["claude", "codex"]);

    const claude = groups[0]!;
    expect(claude.querySelector(".burn-group__heading img")?.getAttribute("src")).toBe("/icons/claude.svg");
    expect(claude.querySelector(".burn-group__count")?.textContent).toBe("2/2 available");
    expect(claude.querySelectorAll(".burn-card")).toHaveLength(2);

    const codex = groups[1]!;
    expect(codex.querySelector(".burn-group__heading img")).toBeNull();
    expect(codex.querySelector(".burn-group__heading")?.textContent).toContain("Codex");
    expect(codex.querySelector(".burn-group__count")?.textContent).toBe("1/2 available");
  });

  it("gives a no-telemetry account a stated availability instead of an empty sparkline", () => {
    const container = document.createElement("div");
    renderTokenAnalytics(container, computeTokenAnalytics(mixedPage(), { now: T0 + 20 * MINUTE }), {
      now: T0 + 20 * MINUTE,
      surface: "authenticated",
    });
    const held = container.querySelector('.burn-card[data-provider="codex"][data-account="cx-2"]')!;
    expect(held.querySelector("svg")).toBeNull();
    expect(held.querySelector('[data-testid="burn-card-state"]')?.textContent).toContain("Held");
    expect(held.querySelector('[data-testid="exhausted-badge"]')).not.toBeNull();
    const free = container.querySelector('.burn-card[data-provider="codex"][data-account="robb"]')!;
    expect(free.querySelector('[data-testid="burn-card-state"]')?.textContent).toContain("Available");
    // A Claude card with telemetry still draws its sparkline.
    const claude = container.querySelector('.burn-card[data-provider="claude"][data-account="robb"]')!;
    expect(claude.querySelector("svg")).not.toBeNull();
  });

  it("stacks identity and badges so neither can overflow the card, and marks the provider", () => {
    const container = document.createElement("div");
    renderTokenAnalytics(container, computeTokenAnalytics(mixedPage(), { now: T0 + 20 * MINUTE }), {
      now: T0 + 20 * MINUTE,
      surface: "authenticated",
    });
    const card = container.querySelector('.burn-card[data-provider="codex"][data-account="cx-2"]')!;
    const head = card.querySelector(".burn-card__head")!;
    expect(head.querySelector(".burn-card__identity [data-testid='provider-mark']")?.getAttribute("data-provider")).toBe(
      "codex",
    );
    expect(head.querySelector(".burn-card__identity .burn-card__account")?.textContent).toBe("cx-2");
    expect(head.querySelector(".burn-card__badges .badge--exhausted")).not.toBeNull();
  });

  it("marks each forecast row with its provider", () => {
    const container = document.createElement("div");
    renderTokenAnalytics(container, computeTokenAnalytics(mixedPage(), { now: T0 + 20 * MINUTE }), {
      now: T0 + 20 * MINUTE,
      surface: "authenticated",
    });
    const rows = [...container.querySelectorAll('[data-testid="forecasts"] tbody tr')];
    expect(rows.map((row) => row.getAttribute("data-provider"))).toEqual(["claude", "claude", "codex", "codex"]);
    const codexRow = rows[3]!;
    expect(codexRow.querySelector('.cell--account [data-testid="provider-mark"]')?.textContent).toBe("Codex");
    expect(codexRow.querySelector(".cell--account-name")?.textContent).toBe("robb");
    // The table scrolls inside its block rather than widening the page.
    expect(container.querySelector('[data-testid="forecasts"] .table-scroll table')).not.toBeNull();
  });
});
