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
  PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID,
  SWEEP_ONLY_HOST_ID,
  multiHostSnapshot,
  persistentRoleTickFailureFixture,
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
    expect(fieldValue(rendered, "Build commit")).toBe("8c16fb5b");
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

  it("renders GB only, with no fabricated percentage, for a free-but-no-total record (#5356)", () => {
    // HEALTHY_HOST_ID's fixture carries worktree_root_free_gb but no
    // worktree_root_total_gb — the pre-#5356 shape, and the shape a daemon
    // whose total probe failed independently would still send. Pins that no
    // percentage is invented for it.
    const rendered = detail(HEALTHY_HOST_ID);
    expect(fieldValue(rendered, "Worktree root free")).toBe("200 GB");
  });

  it("renders a percentage when both free and total are present (#5356)", () => {
    // PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID's fixture carries both: 300 GB
    // free of 1500 GB total → 80% used.
    const rendered = detail(PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID);
    expect(fieldValue(rendered, "Worktree root free")).toBe("300 GB (80% used)");
  });

  it("renders the build identity as unknown for a record from a pre-#4956 daemon", () => {
    // The degraded fixture carries no `build_commit`/`built_at` — the panel
    // must say so rather than invent a commit or a build time (#4956).
    const rendered = detail(DEGRADED_HOST_ID);
    expect(fieldValue(rendered, "Build commit")).toBe(UNKNOWN);
    expect(fieldValue(rendered, "Built at")).toBe(UNKNOWN);
  });

  it("renders the role-tick health summary, ok for the healthy fixture (#5022)", () => {
    const rendered = detail(HEALTHY_HOST_ID);
    expect(fieldValue(rendered, "Role ticks")).toBe("12/12 ticks ok");
  });

  it("names a persistent role-tick failure in the health panel (#5022)", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: {
              record: { kind: "host.health", roles: persistentRoleTickFailureFixture() },
              updatedAt: "2026-07-30T12:09:00Z",
            },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const rendered = hostDetailView(built.hosts[0]!, NOW);
    expect(fieldValue(rendered, "Role ticks")).toBe(
      "1/3 ticks ok; 1 persistent failure(s): judge @ loom",
    );
    expect(rendered.querySelector('[data-testid="status-badge"]')?.getAttribute("data-status")).toBe(
      "degraded",
    );
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

describe("hostDetailView — history section (#5355)", () => {
  it("renders a loading placeholder when no historySection is injected", () => {
    const rendered = detail(HEALTHY_HOST_ID);
    const history = rendered.querySelector('[data-testid="host-history-panel"]');
    expect(history).not.toBeNull();
    expect(history?.querySelector('[data-testid="history-loading"]')).not.toBeNull();
  });

  it("places the injected historySection node in the history slot instead of the placeholder", () => {
    const injected = document.createElement("div");
    injected.dataset.testid = "host-history-panel";
    injected.dataset.testMarker = "injected";

    const built = buildFleetView(parseFleetSnapshot(multiHostSnapshot()), NOW);
    const rendered = hostDetailView(findHost(built, HEALTHY_HOST_ID)!, NOW, injected);

    const history = rendered.querySelector('[data-testid="host-history-panel"]');
    expect(history).toBe(injected);
    expect(history?.querySelector('[data-testid="history-loading"]')).toBeNull();
  });
});
