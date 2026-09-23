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

  // #5642: the sweep count alone reads as "the fleet is idle" whenever every
  // running agent is doing role work (Curator/Champion/Judge/Doctor role-runner
  // ticks) rather than a sweep, since role ticks never post to activeSweeps.
  // The headline must say so, and fold in the fleet-wide role-tick count
  // (only HEALTHY_HOST_ID reports `roles: { total: 12, ok: 12 }` in this
  // fixture) so a reader gets both halves of the answer without drilling in.
  it("qualifies the sweep count and folds in the fleet-wide role-tick total", () => {
    const summary = fleetOverviewView(view(), NOW).querySelector('[data-testid="fleet-summary"]');
    expect(summary?.textContent).toContain("3 active sweeps (excludes role ticks)");
    expect(summary?.textContent).toContain("12/12 role ticks ok");
  });

  it("omits the role-tick clause entirely when no host has reported health.roles", () => {
    const built = buildFleetView(
      { hosts: { h: { health: { record: { kind: "host.health" }, updatedAt: NOW.toISOString() } } }, activeSweeps: [] },
      NOW,
    );
    const summary = fleetOverviewView(built, NOW).querySelector('[data-testid="fleet-summary"]');
    expect(summary?.textContent).toContain("0 active sweeps (excludes role ticks)");
    expect(summary?.textContent).not.toContain("role ticks ok");
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

  it("summarizes the token pool per provider, untagged rows reading as Claude", () => {
    const healthy = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    const labels = [...healthy.querySelectorAll('[data-testid="token-pool-label"]')];
    expect(labels.map((node) => node.getAttribute("data-provider"))).toEqual(["claude"]);
    expect(labels[0]!.textContent).toBe("Claudepool");
    expect(labels[0]!.querySelector('[data-testid="provider-mark"] img')?.getAttribute("src")).toBe("/icons/claude.svg");
    expect(healthy.querySelector('[data-testid="token-pool-value"][data-provider="claude"]')?.textContent).toBe(
      "0/2 exhausted · peak 42%",
    );
    expect(
      hostCard(findHost(view(), DEGRADED_HOST_ID)!, NOW).querySelector('[data-testid="token-pool-value"]')?.textContent,
    ).toBe("1/2 exhausted · peak 100%");
  });

  it("renders one token-pool row per provider so each pool's availability reads on its own", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: { record: { kind: "host.health", uptime_sec: 10 }, updatedAt: isoMinutesBefore(1) },
            tokens: {
              record: {
                kind: "tokens.snapshot",
                accounts: [
                  { account: "agent-1", provider: "claude", usage_fraction: 1, exhausted: true },
                  { account: "agent-2", provider: "claude", usage_fraction: 1, exhausted: true },
                  { account: "cx-1", provider: "codex", exhausted: false },
                  { account: "cx-2", provider: "codex", exhausted: false },
                  { account: "cx-3", provider: "codex", exhausted: true },
                ],
              },
              updatedAt: isoMinutesBefore(1),
            },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const card = hostCard(findHost(built, "h")!, NOW);
    const values = [...card.querySelectorAll('[data-testid="token-pool-value"]')];
    expect(values.map((node) => [node.getAttribute("data-provider"), node.textContent])).toEqual([
      ["claude", "2/2 exhausted · peak 100%"],
      // Codex accounts report no usage fraction: no "peak 0%" fabricated.
      ["codex", "1/3 exhausted"],
    ]);
    // Codex has no icon on the homepage feed either — a text mark.
    const codexLabel = card.querySelector('[data-testid="token-pool-label"][data-provider="codex"]')!;
    expect(codexLabel.querySelector("img")).toBeNull();
    expect(codexLabel.textContent).toBe("Codexpool");
    // The degraded badge names the spent provider, not a blended pool.
    expect(card.querySelector('[data-testid="status-badge"]')?.getAttribute("title")).toBe(
      "claude token pool at or near exhaustion",
    );
  });

  it("marks each sweep with the agent working it, and nothing when the runtime is unknown", () => {
    const busy = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    const rows = [...busy.querySelectorAll(".card__sweep")];
    const claude = rows[0]!.querySelector('[data-testid="provider-mark"]')!;
    expect(claude.getAttribute("data-provider")).toBe("claude");
    expect(claude.querySelector("img")?.getAttribute("alt")).toBe("Claude");
    const codex = rows[1]!.querySelector('[data-testid="provider-mark"]')!;
    expect(codex.getAttribute("data-provider")).toBe("codex");
    expect(codex.textContent).toBe("Codex");
    // SWEEP_ONLY_HOST_ID's sweep carries no runtime — no mark, never a guess.
    const unknown = hostCard(findHost(view(), SWEEP_ONLY_HOST_ID)!, NOW);
    expect(unknown.querySelector('.card__sweep [data-testid="provider-mark"]')).toBeNull();
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

  it("lists live sweeps and qualifies 'none' for an idle host (#5642)", () => {
    const busy = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    expect(fieldValue(busy, "Active sweeps")).toBe("2");
    expect(busy.querySelectorAll(".card__sweep")).toHaveLength(2);
    expect(busy.textContent).toContain("#4703");
    // A sweep that has not reported a phase yet is labelled, not blank.
    expect(busy.textContent).toContain("starting");

    // "none" alone would read as "this host is idle" — but role ticks never
    // post to activeSweeps, so a host doing nothing but role work can
    // legitimately show zero sweeps while still being busy (#5642).
    const idle = hostCard(findHost(view(), IDLE_HOST_ID)!, NOW);
    expect(fieldValue(idle, "Active sweeps")).toBe("none (excludes role ticks)");
    expect(idle.querySelector('[data-testid="card-sweeps"]')).toBeNull();
  });

  it("links each sweep to its forge work and each repo slug to its forge page", () => {
    const busy = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    const rows = [...busy.querySelectorAll(".card__sweep")];
    // sweep-issue-4703-0 is in `builder` → the pushed feature branch.
    const builderLabel = rows[0]!.querySelector(".card__sweep-label")!;
    expect(builderLabel.tagName).toBe("A");
    expect(builderLabel.getAttribute("href")).toBe("https://github.com/rjwalters/loom/tree/feature/issue-4703");
    expect(builderLabel.getAttribute("target")).toBe("_blank");
    expect(builderLabel.getAttribute("rel")).toBe("noopener noreferrer");
    // sweep-issue-4749-0 has reported no phase yet → no branch exists, so
    // the issue is the only honest destination.
    const startingLabel = rows[1]!.querySelector(".card__sweep-label")!;
    expect(startingLabel.getAttribute("href")).toBe("https://github.com/rjwalters/loom/issues/4749");
    // The trailing repo slug on a sweep row goes to the repo itself.
    const sweepRepo = rows[0]!.querySelector(".card__sweep-repo")!;
    expect(sweepRepo.tagName).toBe("A");
    expect(sweepRepo.getAttribute("href")).toBe("https://github.com/rjwalters/loom");

    // Active and idle roster rows alike link to the repo page.
    const activeRepo = busy.querySelector('[data-testid="card-repos"] .card__repo-label')!;
    expect(activeRepo.tagName).toBe("A");
    expect(activeRepo.getAttribute("href")).toBe("https://github.com/rjwalters/loom");
    const idleRepos = [...busy.querySelectorAll('[data-testid="card-repos-idle"] .card__repo-label')];
    expect(idleRepos.map((node) => node.getAttribute("href"))).toEqual([
      "https://github.com/2AMLogic/gf180-pll",
      "https://github.com/2AMLogic/gf180-trng",
    ]);
  });

  it("renders a sweep with no linkable target as plain text, not a dead link", () => {
    // SWEEP_ONLY_HOST_ID's sweep has no issue number: nothing to link.
    const card = hostCard(findHost(view(), SWEEP_ONLY_HOST_ID)!, NOW);
    const label = card.querySelector(".card__sweep-label")!;
    expect(label.tagName).toBe("SPAN");
    expect(label.hasAttribute("href")).toBe(false);
  });

  // #4976: the "Repositories" section, below "Active sweeps". #7662 split it
  // into an always-visible active tier and a closed-by-default idle
  // <details> so a long idle roster doesn't bury the busy rows.
  it("lists the host's managed repositories, with an in-flight count for the busy one", () => {
    const card = hostCard(findHost(view(), HEALTHY_HOST_ID)!, NOW);
    expect(fieldValue(card, "Repositories")).toBe("3");

    // Active tier: only "rjwalters/loom" has in-flight sweeps, and its row
    // shows the ×2 chip. It is the only entry in the visible list.
    const visibleRows = [...card.querySelector('[data-testid="card-repos"]')!.querySelectorAll(".card__repo")];
    expect(visibleRows).toHaveLength(1);
    expect(visibleRows[0]?.textContent).toContain("rjwalters/loom");
    expect(visibleRows[0]?.querySelector(".chip")?.textContent).toBe("×2");

    // Idle tier: the two registered-but-never-dispatched-into repos live
    // inside the closed-by-default <details> — an idle repo is not the same
    // as an unregistered one (#4976's whole point) — with no in-flight chip.
    const details = card.querySelector('[data-testid="card-repos-idle"]') as HTMLDetailsElement | null;
    expect(details).not.toBeNull();
    expect(details?.open).toBe(false);
    expect(details?.querySelector("summary")?.textContent).toBe("2 idle repositories");
    const idleRows = [...details!.querySelectorAll(".card__repo")];
    expect(idleRows).toHaveLength(2);
    const idleRow = idleRows.find((row) => row.textContent?.includes("2AMLogic/gf180-pll"));
    expect(idleRow).toBeDefined();
    expect(idleRow?.querySelector(".chip")).toBeNull();
  });

  // #7662: the split's two edge cases.
  describe("active/idle repo roster split (#7662)", () => {
    it("shows no visible list and a summary that stands in for the whole roster when the host has zero sweeps", () => {
      const card = hostCard(findHost(view(), IDLE_HOST_ID)!, NOW);
      expect(fieldValue(card, "Repositories")).toBe("3");
      expect(card.querySelector('[data-testid="card-repos"]')).toBeNull();
      const details = card.querySelector('[data-testid="card-repos-idle"]');
      expect(details).not.toBeNull();
      // 1 named idle repo ("rjwalters/loom") + 2 redacted private entries —
      // the same 3 the "Repositories" field counts.
      expect(details?.querySelector("summary")?.textContent).toBe("1 idle repository, 2 private");
    });

    it("renders no <details> at all when every named repo is active", () => {
      const built = buildFleetView(
        parseFleetSnapshot({
          hosts: {
            h: {
              health: {
                record: {
                  kind: "host.health",
                  managed_repos: [{ slug: "a/one", visibility: "public" }, { slug: "a/two", visibility: "public" }],
                },
                updatedAt: isoMinutesBefore(1),
              },
            },
          },
          activeSweeps: [
            { hostId: "h", sweepId: "s1", repo: "a/one" },
            { hostId: "h", sweepId: "s2", repo: "a/two" },
          ],
        }),
        NOW,
      );
      const card = hostCard(findHost(built, "h")!, NOW);
      expect(card.querySelector('[data-testid="card-repos-idle"]')).toBeNull();
      const visibleRows = [...card.querySelector('[data-testid="card-repos"]')!.querySelectorAll(".card__repo")];
      expect(visibleRows).toHaveLength(2);
    });

    it("sorts the active tier by in-flight count desc, then slug", () => {
      const built = buildFleetView(
        parseFleetSnapshot({
          hosts: {
            h: {
              health: {
                record: {
                  kind: "host.health",
                  managed_repos: [
                    { slug: "z/low", visibility: "public" },
                    { slug: "a/high", visibility: "public" },
                    { slug: "m/idle", visibility: "public" },
                  ],
                },
                updatedAt: isoMinutesBefore(1),
              },
            },
          },
          activeSweeps: [
            { hostId: "h", sweepId: "s1", repo: "z/low" },
            { hostId: "h", sweepId: "s2", repo: "a/high" },
            { hostId: "h", sweepId: "s3", repo: "a/high" },
            { hostId: "h", sweepId: "s4", repo: "a/high" },
          ],
        }),
        NOW,
      );
      const card = hostCard(findHost(built, "h")!, NOW);
      const visibleRows = [...card.querySelector('[data-testid="card-repos"]')!.querySelectorAll(".card__repo-label")];
      expect(visibleRows.map((row) => row.textContent)).toEqual(["a/high", "z/low"]);
      const details = card.querySelector('[data-testid="card-repos-idle"]');
      expect(details?.querySelector("summary")?.textContent).toBe("1 idle repository");
      expect(details?.textContent).toContain("m/idle");
    });

    it("remembers the idle roster's open/closed state per host in localStorage", () => {
      window.localStorage.clear();
      const host = findHost(view(), HEALTHY_HOST_ID)!;
      const first = hostCard(host, NOW);
      const details = first.querySelector('[data-testid="card-repos-idle"]') as HTMLDetailsElement;
      expect(details.open).toBe(false);

      details.open = true;
      details.dispatchEvent(new Event("toggle"));

      const second = hostCard(host, NOW);
      const detailsAgain = second.querySelector('[data-testid="card-repos-idle"]') as HTMLDetailsElement;
      expect(detailsAgain.open).toBe(true);

      window.localStorage.clear();
    });
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


it("renders resolved Z.ai provider/model with OpenCode as launch runtime context", () => {
  const host = findHost(view(), HEALTHY_HOST_ID)!;
  host.sweeps[0] = { ...host.sweeps[0]!, runtime: "opencode", provider: "zai-coding-plan", model: "glm-5.3" };
  const row = hostCard(host, NOW).querySelector(".card__sweep")!;
  expect(row.textContent).toContain("Z.ai");
  expect(row.textContent).toContain("glm-5.3");
  const mark = row.querySelector<HTMLElement>('[data-testid="provider-mark"]')!;
  expect(mark.dataset.provider).toBe("zai-coding-plan");
  expect(mark.title).toContain("Runtime: OpenCode");
  expect(mark.title).toContain("Sweep launch");
});
