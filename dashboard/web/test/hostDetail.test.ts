import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { buildFleetView, findHost } from "../src/fleet";
import { UNKNOWN } from "../src/format";
import { parseFleetSnapshot } from "../src/parse";
import { hostDetailView } from "../src/views/hostDetail";
import {
  DEGRADED_HOST_ID,
  HEALTHY_HOST_ID,
  IDLE_HOST_ID,
  NOW,
  SWEEP_ONLY_HOST_ID,
  multiHostSnapshot,
} from "./fixtures";

// Views call `formatAbsolute()` internally with no zone argument, so they
// resolve through `displayTimeZone()` — which falls back to the *machine's*
// zone. Pin it the way production does (the Worker injects this global) so
// these assertions do not depend on where the test runs.
const injected = globalThis as unknown as Record<string, unknown>;
beforeAll(() => {
  injected.__LOOM_FLEET__ = { authenticated: false, timeZone: "UTC" };
});
afterAll(() => {
  delete injected.__LOOM_FLEET__;
});

const view = () => buildFleetView(parseFleetSnapshot(multiHostSnapshot()), NOW);
const detail = (hostId: string) => hostDetailView(findHost(view(), hostId)!, NOW);

function fieldValue(root: HTMLElement, label: string): string | undefined {
  const labels = [...root.querySelectorAll(".field__label")];
  return labels.find((node) => node.textContent === label)?.nextElementSibling?.textContent ?? undefined;
}

function cells(row: Element): string[] {
  return [...row.querySelectorAll("td")].map((cell) => cell.textContent ?? "");
}

describe("hostDetailView — health panel", () => {
  it("renders every host.health field", () => {
    const rendered = detail(HEALTHY_HOST_ID);
    expect(fieldValue(rendered, "Daemon version")).toBe("0.16.0");
    expect(fieldValue(rendered, "Uptime")).toBe("1d 0h");
    expect(fieldValue(rendered, "Logical CPUs")).toBe("28");
    expect(fieldValue(rendered, "CPU idle")).toBe("83.0%");
    expect(fieldValue(rendered, "Load per core")).toBe("0.51");
    expect(fieldValue(rendered, "Worktree root free")).toBe("200 GB");
  });

  it("renders unmeasured fields as unknown, not zero", () => {
    const rendered = detail(DEGRADED_HOST_ID);
    expect(fieldValue(rendered, "CPU idle")).toBe(UNKNOWN);
    expect(fieldValue(rendered, "Load per core")).toBe(UNKNOWN);
    expect(fieldValue(rendered, "Worktree root free")).toBe(UNKNOWN);
  });

  it("explains a host that has no health record yet", () => {
    const rendered = detail(SWEEP_ONLY_HOST_ID);
    expect(rendered.querySelector('[data-testid="health-missing"]')?.textContent).toContain(
      "has not pushed a host.health record yet",
    );
    expect(rendered.querySelector('[data-testid="tokens-missing"]')?.textContent).toContain(
      "has not pushed a tokens.snapshot record yet",
    );
  });
});

describe("hostDetailView — token panel", () => {
  it("renders one row per account with usage and state", () => {
    const rows = [...detail(HEALTHY_HOST_ID).querySelectorAll('[data-testid="token-account"]')];
    expect(rows).toHaveLength(2);
    // The reset window is in the future — rendered as a countdown, not
    // clamped to "just now" the way a past-event timestamp would be.
    expect(cells(rows[0]!)).toEqual(["agent-1", "0", "42%", "in 5h 50m", "available"]);
    // agent-2 has no limit_window_reset_at — unknown, not a fabricated date.
    expect(cells(rows[1]!)[3]).toBe(UNKNOWN);
  });

  it("marks an exhausted account", () => {
    const rows = [...detail(DEGRADED_HOST_ID).querySelectorAll('[data-testid="token-account"]')];
    expect(rows[0]!.classList.contains("row--exhausted")).toBe(true);
    expect(cells(rows[0]!)[4]).toBe("exhausted");
    // agent-4 reports `exhausted: false` but no usage_fraction.
    expect(cells(rows[1]!)[2]).toBe(UNKNOWN);
    expect(cells(rows[1]!)[4]).toBe("available");
  });

  it("counts down to an exhausted account's reset instead of showing a dash", () => {
    // Issue #4874: the daemon never populated `limit_window_reset_at`, so this
    // column was permanently `—` for exactly the accounts an operator needs it
    // for. With the field fed, an exhausted account answers "when does
    // capacity return?" — a multi-day countdown, not a clamped "just now".
    const rows = [...detail(DEGRADED_HOST_ID).querySelectorAll('[data-testid="token-account"]')];
    expect(cells(rows[0]!)[4]).toBe("exhausted");
    expect(cells(rows[0]!)[3]).toBe("in 2d 14h");
    expect(cells(rows[0]!)[3]).not.toBe(UNKNOWN);
    // The absolute instant is still available as the tooltip for a bug report.
    const resetCell = rows[0]!.querySelectorAll("td")[3];
    expect(resetCell?.getAttribute("title")).toBeTruthy();
  });

  it("clamps the usage meter to the track", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: { h: { tokens: { record: { accounts: [{ account: "a", usage_fraction: 4.2 }] }, updatedAt: "2026-07-30T12:09:00Z" } } },
        activeSweeps: [],
      }),
      NOW,
    );
    const fill = hostDetailView(built.hosts[0]!, NOW).querySelector(".meter__fill");
    expect(fill?.getAttribute("style")).toBe("width:100%");
  });

  it("says per-account rows were withheld rather than claiming the pool is empty", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            tokens: {
              record: { kind: "tokens.snapshot", account_count: 13, exhausted_count: 5, max_usage_fraction: 0.91 },
              updatedAt: "2026-07-30T12:09:00Z",
            },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const rendered = hostDetailView(built.hosts[0]!, NOW);

    const notice = rendered.querySelector('[data-testid="tokens-aggregate-only"]')?.textContent ?? "";
    expect(notice).toContain("not shown in the public view");
    expect(notice).toContain("13 account(s)");
    expect(notice).toContain("5 exhausted");
    // The "no token pool provisioned" wording would be actively wrong here.
    expect(rendered.querySelector('[data-testid="tokens-empty"]')).toBeNull();
  });

  it("distinguishes 'no tokens record' from 'a tokens record with no accounts'", () => {
    expect(detail(IDLE_HOST_ID).querySelector('[data-testid="tokens-empty"]')?.textContent).toContain(
      "no accounts",
    );
    expect(detail(SWEEP_ONLY_HOST_ID).querySelector('[data-testid="tokens-empty"]')).toBeNull();
  });
});

describe("hostDetailView — active sweeps", () => {
  it("renders every activeSweeps field the drill-down promises", () => {
    const rows = [...detail(HEALTHY_HOST_ID).querySelectorAll('[data-testid="sweep-row"]')];
    expect(rows).toHaveLength(2);
    // Longest-running first.
    expect(rows[0]!.getAttribute("data-sweep")).toBe("sweep-issue-4703-0");
    expect(cells(rows[0]!)).toEqual([
      "#4703",
      "rjwalters/loom",
      "builder",
      "opus",
      "high",
      "10m 0s",
      "4m 0s",
    ]);
  });

  it("carries the absolute timestamps in tooltips", () => {
    const row = detail(HEALTHY_HOST_ID).querySelector('[data-testid="sweep-row"]')!;
    const tds = [...row.querySelectorAll("td")];
    expect(tds[5]!.getAttribute("title")).toBe("2026-07-30 12:00:00 UTC");
    expect(tds[6]!.getAttribute("title")).toBe("2026-07-30 12:06:00 UTC");
  });

  it("degrades a partially-reported sweep instead of blanking the row", () => {
    const rows = [...detail(HEALTHY_HOST_ID).querySelectorAll('[data-testid="sweep-row"]')];
    // sweep-issue-4749-0: no phase, no effort, no enteredPhaseAt yet.
    expect(cells(rows[1]!)).toEqual(["#4749", "rjwalters/loom", "starting", "opus", UNKNOWN, "1m 0s", UNKNOWN]);
  });

  it("falls back to the sweep id when the issue number is unknown", () => {
    const rows = [...detail(SWEEP_ONLY_HOST_ID).querySelectorAll('[data-testid="sweep-row"]')];
    expect(cells(rows[0]!)[0]).toBe("sweep-issue-9001-0");
    expect(cells(rows[0]!)[1]).toBe(UNKNOWN);
  });

  it("treats an idle host as a normal state, not an error", () => {
    const rendered = detail(IDLE_HOST_ID);
    expect(rendered.querySelectorAll('[data-testid="sweep-row"]')).toHaveLength(0);
    expect(rendered.querySelector('[data-testid="sweeps-empty"]')?.textContent).toContain(
      "No sweeps are in flight",
    );
    // The health panel is still fully rendered for an idle host.
    expect(fieldValue(rendered, "Logical CPUs")).toBe("12");
  });
});

describe("hostDetailView — chrome", () => {
  it("offers a way back to the overview", () => {
    expect(detail(HEALTHY_HOST_ID).querySelector(".detail__breadcrumb a")?.getAttribute("href")).toBe("#/");
  });

  it("shows the host id and status", () => {
    const rendered = detail(DEGRADED_HOST_ID);
    expect(rendered.getAttribute("data-host")).toBe(DEGRADED_HOST_ID);
    expect(rendered.querySelector(".detail__title")?.textContent).toBe(DEGRADED_HOST_ID);
    expect(rendered.querySelector('[data-testid="status-badge"]')?.getAttribute("data-status")).toBe(
      "degraded",
    );
  });
});
