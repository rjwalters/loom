import { describe, expect, it } from "vitest";

import { buildFleetView, findHost } from "../src/fleet";
import { parseFleetSnapshot } from "../src/parse";
import { UNKNOWN } from "../src/format";
import { fleetOverviewView, hostCard } from "../src/views/fleetOverview";
import {
  DEGRADED_HOST_ID,
  HEALTHY_HOST_ID,
  IDLE_HOST_ID,
  NOW,
  PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID,
  STALE_HOST_ID,
  SWEEP_ONLY_HOST_ID,
  isoMinutesBefore,
  multiHostSnapshot,
} from "./fixtures";

const view = () => buildFleetView(parseFleetSnapshot(multiHostSnapshot()), NOW);

function fieldValue(card: HTMLElement, label: string): string | undefined {
  const labels = [...card.querySelectorAll(".field__label")];
  const match = labels.find((node) => node.textContent === label);
  return match?.nextElementSibling?.textContent ?? undefined;
}

describe("fleetOverviewView", () => {
  it("renders one card per host, including hosts known only from sweeps", () => {
    const rendered = fleetOverviewView(view(), NOW);
    const cards = [...rendered.querySelectorAll('[data-testid="host-card"]')];
    expect(cards.map((card) => card.getAttribute("data-host"))).toEqual([
      STALE_HOST_ID,
      DEGRADED_HOST_ID,
      SWEEP_ONLY_HOST_ID,
      HEALTHY_HOST_ID,
      IDLE_HOST_ID,
      PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID,
    ]);
  });

  it("summarizes the fleet above the grid", () => {
    const summary = fleetOverviewView(view(), NOW).querySelector('[data-testid="fleet-summary"]');
    expect(summary?.textContent).toContain("6 hosts");
    expect(summary?.textContent).toContain("3 active sweeps");
    expect(summary?.textContent).toContain("2 need");
  });

  it("shows the empty-fleet state, not an error, when no host has reported", () => {
    const rendered = fleetOverviewView(buildFleetView({ hosts: {}, activeSweeps: [] }, NOW), NOW);
    expect(rendered.getAttribute("data-testid")).toBe("empty-fleet");
    expect(rendered.textContent).toContain("No hosts are reporting yet");
  });
});

describe("hostCard", () => {
  it("shows the whole host.health field set at a glance", () => {
    const card = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    expect(fieldValue(card, "Daemon")).toBe("0.16.0");
    expect(fieldValue(card, "Uptime")).toBe("1d 0h");
    expect(fieldValue(card, "CPUs")).toBe("28");
    expect(fieldValue(card, "CPU idle")).toBe("83%");
    expect(fieldValue(card, "Load/core")).toBe("0.51");
    expect(fieldValue(card, "Worktree free")).toBe("200 GB");
  });

  it("renders unmeasured health fields as unknown rather than zero", () => {
    const card = hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW);
    expect(fieldValue(card, "CPU idle")).toBe(UNKNOWN);
    expect(fieldValue(card, "Load/core")).toBe(UNKNOWN);
    expect(fieldValue(card, "Worktree free")).toBe(UNKNOWN);
    // …while the fields that *were* measured still render.
    expect(fieldValue(card, "CPUs")).toBe("8");
  });

  it("summarizes the token pool", () => {
    expect(fieldValue(hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW), "Token pool")).toBe(
      "0/2 exhausted · peak 42%",
    );
    expect(fieldValue(hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW), "Token pool")).toBe(
      "1/2 exhausted · peak 100%",
    );
  });

  it("renders the token pool as unknown when the host has never reported one", () => {
    expect(fieldValue(hostCard(findHost(view(), SWEEP_ONLY_HOST_ID)!, NOW), "Token pool")).toBe(UNKNOWN);
  });

  it("links to the host's drill-down with an encoded id", () => {
    const built = buildFleetView(
      parseFleetSnapshot({ hosts: { "host/with space": { health: { record: {}, updatedAt: isoMinutesBefore(1) } } }, activeSweeps: [] }),
      NOW,
    );
    const card = hostCard(built.hosts[0]!, NOW);
    expect(card.querySelector(".card__title")?.getAttribute("href")).toBe("#/hosts/host%2Fwith%20space");
  });

  it("badges each host status", () => {
    const built = view();
    const badge = (hostId: string) =>
      hostCard(findHost(built, hostId)!, NOW).querySelector('[data-testid="status-badge"]')?.getAttribute("data-status");
    expect(badge(HEALTHY_HOST_ID)).toBe("ok");
    expect(badge(DEGRADED_HOST_ID)).toBe("degraded");
    expect(badge(STALE_HOST_ID)).toBe("stale");
    expect(badge(SWEEP_ONLY_HOST_ID)).toBe("unknown");
  });

  it("lists live sweeps and says 'none' for an idle host", () => {
    const busy = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    expect(fieldValue(busy, "Active sweeps")).toBe("2");
    expect(busy.querySelectorAll(".card__sweep")).toHaveLength(2);
    expect(busy.textContent).toContain("#4703");
    // A sweep that has not reported a phase yet is labelled, not blank.
    expect(busy.textContent).toContain("starting");

    const idle = hostCard(findHost(view(), IDLE_HOST_ID)!, NOW);
    expect(fieldValue(idle, "Active sweeps")).toBe("none");
    expect(idle.querySelector('[data-testid="card-sweeps"]')).toBeNull();
  });

  it("truncates a long sweep list with a +N more row", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {},
        activeSweeps: Array.from({ length: 5 }, (_, index) => ({
          hostId: "busy",
          sweepId: `sweep-${index}`,
          issue: 100 + index,
          startedAt: isoMinutesBefore(10 - index),
        })),
      }),
      NOW,
    );
    const card = hostCard(built.hosts[0]!, NOW);
    expect(card.querySelectorAll(".card__sweep")).toHaveLength(4);
    expect(card.querySelector(".card__sweep--more")?.textContent).toBe("+2 more");
  });

  it("says 'Never reported' instead of a fabricated timestamp", () => {
    const card = hostCard(findHost(view(), SWEEP_ONLY_HOST_ID)!, NOW);
    expect(card.querySelector(".card__subtitle")?.textContent).toBe("Never reported");
  });

  it("escapes remote-supplied text instead of interpolating markup", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {},
        activeSweeps: [{ hostId: "<img src=x onerror=alert(1)>", sweepId: "s", repo: "<script>bad()</script>" }],
      }),
      NOW,
    );
    const card = hostCard(built.hosts[0]!, NOW);
    expect(card.querySelector("img")).toBeNull();
    expect(card.querySelector("script")).toBeNull();
    expect(card.textContent).toContain("<img src=x onerror=alert(1)>");
  });
});
