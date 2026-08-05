import { describe, expect, it } from "vitest";

import { buildFleetView, findHost } from "../src/fleet";
import { parseFleetSnapshot } from "../src/parse";
import { UNKNOWN } from "../src/format";
import { daemonIdentityText, fleetOverviewView, hostCard, protectionBadge, statusBadge } from "../src/views/fleetOverview";
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
  persistentRoleTickFailureFixture,
  unprotectedHostProtectionFixture,
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
    // 5, not 6: SWEEP_ONLY_HOST_ID has no health/tokens entry — it is known
    // only from activeSweeps (status "unknown") — so it is excluded from the
    // headline "N hosts" count even though it still gets a card (#5101).
    expect(summary?.textContent).toContain("5 hosts");
    expect(summary?.textContent).toContain("3 active sweeps");
    expect(summary?.textContent).toContain("2 need");
  });

  // #5101: the headline "N hosts" count must not include a host known only
  // from activeSweeps, even though its card (and its sweeps) still render.
  it("excludes a sweep-only host from the headline host count, but still renders its card", () => {
    const rendered = fleetOverviewView(view(), NOW);
    const summary = rendered.querySelector('[data-testid="fleet-summary"]');
    expect(summary?.textContent).toContain("5 hosts");
    expect(summary?.textContent).not.toContain("6 hosts");

    const sweepOnlyCard = rendered.querySelector(`[data-host="${SWEEP_ONLY_HOST_ID}"]`);
    expect(sweepOnlyCard).not.toBeNull();
    expect(sweepOnlyCard?.querySelectorAll(".card__sweep")).toHaveLength(1);
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
    expect(fieldValue(card, "Daemon")).toBe("0.16.0 @ 8c16fb5b, built 6h 0m ago");
    expect(fieldValue(card, "Uptime")).toBe("1d 0h");
    expect(fieldValue(card, "CPUs")).toBe("28");
    expect(fieldValue(card, "CPU idle")).toBe("83%");
    expect(fieldValue(card, "Load/core")).toBe("0.51");
    expect(fieldValue(card, "Worktree free")).toBe("200 GB");
  });

  it("distinguishes two same-version hosts by their build commit (#4956)", () => {
    // The whole point of #4956: `daemon_version` is identical on both hosts
    // (0.16.0), so the card must carry something that is NOT.
    const healthy = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    const degraded = hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW);
    expect(fieldValue(healthy, "Daemon")).toContain("8c16fb5b");
    expect(fieldValue(healthy, "Daemon")).not.toBe(fieldValue(degraded, "Daemon"));
  });

  it("falls back to the bare version for a record from a pre-#4956 daemon", () => {
    // No `build_commit` / `built_at` on the wire — render exactly what the
    // pre-#4956 card rendered, never a fabricated commit or build age.
    const card = hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW);
    expect(fieldValue(card, "Daemon")).toBe("0.16.0");
  });

  it("renders unmeasured health fields as unknown rather than zero", () => {
    const card = hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW);
    expect(fieldValue(card, "CPU idle")).toBe(UNKNOWN);
    expect(fieldValue(card, "Load/core")).toBe(UNKNOWN);
    expect(fieldValue(card, "Worktree free")).toBe(UNKNOWN);
    // …while the fields that *were* measured still render.
    expect(fieldValue(card, "CPUs")).toBe("8");
  });

  it("renders a percentage when both free and total disk are known (#5356)", () => {
    // PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID's fixture: 300 GB free of 1500 GB
    // total → 80% used. HEALTHY_HOST_ID above stays the free-only regression
    // pin ("200 GB", no percentage) for the pre-#5356 shape.
    const card = hostCard(findHost(view(), PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID)!, NOW);
    expect(fieldValue(card, "Worktree free")).toBe("300 GB (80% used)");
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

  it("names the specific reason in the degraded badge's tooltip, not a generic 'Degraded' (#4975)", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: {
              record: { kind: "host.health", dispatch_halted: true, halt_reason: "host-distress breaker" },
              updatedAt: isoMinutesBefore(1),
            },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const card = hostCard(findHost(built, "h")!, NOW);
    const badge = card.querySelector('[data-testid="status-badge"]');
    expect(badge?.getAttribute("data-status")).toBe("degraded");
    expect(badge?.getAttribute("title")).toBe("dispatch halted: host-distress breaker");
  });

  it("falls back to a generic tooltip when a degraded host has no specific reason recorded", () => {
    // Defensive-only path: buildHostView always sets a reason today, but the
    // badge itself must never render an empty/undefined title.
    const badge = statusBadge("degraded");
    expect(badge.getAttribute("title")).toBeTruthy();
    expect(badge.getAttribute("title")).not.toBe("");
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

  // #4976: the "Repositories" section, below "Active sweeps".
  it("lists the host's managed repositories, with an in-flight count for the busy one", () => {
    const card = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    expect(fieldValue(card, "Repositories")).toBe("3");
    const repoRows = [...card.querySelectorAll(".card__repo")];
    expect(repoRows).toHaveLength(3);
    // Both live sweeps target "rjwalters/loom" — its row must show ×2.
    const loomRow = repoRows.find((row) => row.textContent?.includes("rjwalters/loom"));
    expect(loomRow?.querySelector(".chip")?.textContent).toBe("×2");
    // The two idle, registered-but-never-dispatched-into repos still appear
    // — an idle repo is not the same as an unregistered one (#4976's whole
    // point) — but carry no in-flight chip.
    const idleRow = repoRows.find((row) => row.textContent?.includes("2AMLogic/gf180-pll"));
    expect(idleRow?.querySelector(".chip")).toBeNull();
  });

  // #5022: compact per-host role-tick indicator.
  it("shows the compact role-tick indicator at a glance", () => {
    expect(fieldValue(hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW), "Roles")).toBe("ok");
  });

  it("shows unknown when the host has never reported roles", () => {
    expect(fieldValue(hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW), "Roles")).toBe(UNKNOWN);
  });

  it("badges a host with a persistent role-tick failure as degraded, distinguishable from a healthy host", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: {
              record: { kind: "host.health", roles: persistentRoleTickFailureFixture() },
              updatedAt: isoMinutesBefore(1),
            },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const card = hostCard(findHost(built, "h")!, NOW);
    expect(fieldValue(card, "Roles")).toBe("1 failing");
    const badge = card.querySelector('[data-testid="status-badge"]');
    expect(badge?.getAttribute("data-status")).toBe("degraded");
    expect(badge?.getAttribute("title")).toBe("role tick(s) persistently failing: judge @ loom");
  });

  it("says 'none' for a host with no registered repos", () => {
    const card = hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW);
    expect(fieldValue(card, "Repositories")).toBe("none");
    expect(card.querySelector('[data-testid="card-repos"]')).toBeNull();
  });

  // #5352: per-host watchdog/crash-protection state.
  describe("watchdog/crash-protection state (#5352)", () => {
    it("shows the full protection text for a protected host, with no warning badge", () => {
      const card = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
      expect(fieldValue(card, "Protection")).toBe("protected");
      expect(card.querySelector('[data-testid="protection-badge"]')).toBeNull();
    });

    it("shows 'not reported' and no warning badge for a host that has never reported protection", () => {
      // DEGRADED_HOST_ID's fixture predates #5352 — must not read as
      // unprotected just because the field is absent.
      const card = hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW);
      expect(fieldValue(card, "Protection")).toBe("not reported");
      expect(card.querySelector('[data-testid="protection-badge"]')).toBeNull();
    });

    it("badges an unprotected host with a distinct warning indicator", () => {
      const built = buildFleetView(
        parseFleetSnapshot({
          hosts: {
            h: {
              health: {
                record: { kind: "host.health", protection: unprotectedHostProtectionFixture() },
                updatedAt: isoMinutesBefore(1),
              },
            },
          },
          activeSweeps: [],
        }),
        NOW,
      );
      const card = hostCard(findHost(built, "h")!, NOW);
      expect(fieldValue(card, "Protection")).toBe("watchdog job not provisioned");
      const badge = card.querySelector('[data-testid="protection-badge"]');
      expect(badge?.textContent).toBe("Unprotected");
      expect(badge?.getAttribute("title")).toBe("watchdog job not provisioned");
    });

    it("protectionBadge renders null for the protected and unknown cases", () => {
      expect(protectionBadge({ state: "protected", watchdog_provisioned: true })).toBeNull();
      expect(protectionBadge({ state: "unknown" })).toBeNull();
      expect(protectionBadge(undefined)).toBeNull();
    });
  });

  it("collapses a private, redacted repo entry to a count instead of naming it", () => {
    // IDLE_HOST_ID's fixture stands in for the unauthenticated, redacted wire
    // shape: two `managed_repos` entries carry `visibility: "private"` with
    // no `slug` at all (`dashboard/src/redaction.ts`'s `redactManagedRepos`).
    const card = hostCard(findHost(view(), IDLE_HOST_ID)!, NOW);
    expect(fieldValue(card, "Repositories")).toBe("3");
    const repoRows = [...card.querySelectorAll(".card__repo")];
    // One named public repo, plus ONE collapsed "+2 private" row — not two
    // separate unnamed rows.
    expect(repoRows).toHaveLength(2);
    expect(repoRows.some((row) => row.textContent?.includes("rjwalters/loom"))).toBe(true);
    const privateRow = card.querySelector(".card__repo--private");
    expect(privateRow?.textContent).toContain("2 private");
  });

  // #4868: the card used to stop at three and append "+N more", which on a
  // working fleet hid most of what the overview exists to show.
  it("renders every in-flight sweep rather than truncating", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {},
        activeSweeps: Array.from({ length: 14 }, (_, index) => ({
          hostId: "busy",
          sweepId: `sweep-${index}`,
          issue: 100 + index,
          startedAt: isoMinutesBefore(20 - index),
        })),
      }),
      NOW,
    );
    const card = hostCard(built.hosts[0]!, NOW);

    expect(card.querySelectorAll(".card__sweep")).toHaveLength(14);
    expect(card.querySelector(".card__sweep--more")).toBeNull();
    // The count in the fields block and the number of rows must agree — they
    // came from the same array, and a reader will compare them.
    expect(fieldValue(card, "Active sweeps")).toBe("14");
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

describe("daemonIdentityText (#4956)", () => {
  it("joins version, commit, and build age", () => {
    expect(
      daemonIdentityText({ daemon_version: "0.17.0", build_commit: "8c16fb5b", built_at: isoMinutesBefore(360) }, NOW),
    ).toBe("0.17.0 @ 8c16fb5b, built 6h 0m ago");
  });

  it("drops the 'unknown' commit sentinel rather than showing it as a SHA", () => {
    // `build.rs` stamps the literal "unknown" when the build host had no git.
    expect(daemonIdentityText({ daemon_version: "0.17.0", build_commit: "unknown" }, NOW)).toBe("0.17.0");
  });

  it("omits the age clause when the build time is absent or unparseable", () => {
    expect(daemonIdentityText({ daemon_version: "0.17.0", build_commit: "8c16fb5b" }, NOW)).toBe("0.17.0 @ 8c16fb5b");
    expect(daemonIdentityText({ daemon_version: "0.17.0", build_commit: "8c16fb5b", built_at: "not-a-date" }, NOW)).toBe(
      "0.17.0 @ 8c16fb5b",
    );
  });

  it("renders an entirely empty record as unknown, never a fabricated identity", () => {
    expect(daemonIdentityText({}, NOW)).toBe(UNKNOWN);
  });
});
