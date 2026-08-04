import { describe, expect, it } from "vitest";

import {
  STALE_AFTER_SEC,
  buildFleetView,
  distressReason,
  findHost,
  isHostDistressed,
  isTokenPoolDegraded,
  sortSweeps,
  summarizeTokens,
} from "../src/fleet";
import { parseFleetSnapshot } from "../src/parse";
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
} from "./fixtures";

const view = () => buildFleetView(parseFleetSnapshot(multiHostSnapshot()), NOW);

describe("buildFleetView", () => {
  it("includes a host known only from activeSweeps", () => {
    // The Durable Object creates a `hosts` entry only on host.health /
    // tokens.snapshot, so a host whose first push was sweep.started has live
    // sweeps and no `hosts` key. Keying off `hosts` alone would hide it.
    const host = findHost(view(), SWEEP_ONLY_HOST_ID);
    expect(host).toBeDefined();
    expect(host?.status).toBe("unknown");
    expect(host?.sweeps).toHaveLength(1);
  });

  it("includes an idle host that has zero active sweeps", () => {
    const host = findHost(view(), IDLE_HOST_ID);
    expect(host).toBeDefined();
    expect(host?.sweeps).toEqual([]);
    expect(host?.status).toBe("ok");
  });

  it("classifies host status from report age and token pool availability", () => {
    const built = view();
    expect(findHost(built, HEALTHY_HOST_ID)?.status).toBe("ok");
    expect(findHost(built, DEGRADED_HOST_ID)?.status).toBe("degraded");
    expect(findHost(built, STALE_HOST_ID)?.status).toBe("stale");
    expect(findHost(built, SWEEP_ONLY_HOST_ID)?.status).toBe("unknown");
  });

  it("renders a partially-spent but functioning pool as ok, not degraded (#4864)", () => {
    // 14 accounts, 5 exhausted: routine rotation, not a fault. Regression
    // pin for the "any exhausted account -> degraded" false alarm.
    const host = findHost(view(), PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID);
    expect(host?.tokens).toMatchObject({ total: 14, exhausted: 5 });
    expect(host?.status).toBe("ok");
  });

  it("treats the staleness boundary as strictly greater-than", () => {
    const at = buildFleetView(
      parseFleetSnapshot({
        hosts: { h: { health: { record: {}, updatedAt: isoMinutesBefore(STALE_AFTER_SEC / 60) } } },
        activeSweeps: [],
      }),
      NOW,
    );
    expect(findHost(at, "h")?.status).toBe("ok");

    const past = buildFleetView(
      parseFleetSnapshot({
        hosts: { h: { health: { record: {}, updatedAt: isoMinutesBefore(STALE_AFTER_SEC / 60 + 1) } } },
        activeSweeps: [],
      }),
      NOW,
    );
    expect(findHost(past, "h")?.status).toBe("stale");
  });

  it("uses the newest of health/tokens as the liveness signal", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: { record: {}, updatedAt: isoMinutesBefore(60) },
            tokens: { record: {}, updatedAt: isoMinutesBefore(1) },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    // Health alone would read as stale; the fresher tokens push proves the
    // host is alive.
    expect(findHost(built, "h")?.status).toBe("ok");
  });

  it("orders hosts needing attention first, then by sweep count, then by id", () => {
    const built = view();
    expect(built.hosts.map((host) => host.hostId)).toEqual([
      STALE_HOST_ID,
      DEGRADED_HOST_ID,
      SWEEP_ONLY_HOST_ID,
      HEALTHY_HOST_ID,
      IDLE_HOST_ID,
      PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID,
    ]);
  });

  it("counts sweeps and attention-needing hosts", () => {
    const built = view();
    expect(built.totalSweeps).toBe(3);
    // Only the stale host and the truly-at-the-edge degraded host — not the
    // partially-exhausted-but-healthy one, and not the sweep-only "unknown"
    // host either — see the dedicated #5101 test below.
    expect(built.needsAttention).toBe(2);
  });

  // #5101: the SPA's fleet-overview headline uses `reportingHosts`, not
  // `hosts.length`, so a host known only from activeSweeps ("unknown"
  // status) does not inflate the "N hosts" count — while still remaining in
  // `hosts` (and rendering its own card, per the module doc's union rule).
  it("excludes sweep-only 'unknown' hosts from reportingHosts, but not from hosts", () => {
    const built = view();
    expect(built.hosts).toHaveLength(6);
    expect(built.reportingHosts).toBe(5);
    expect(findHost(built, SWEEP_ONLY_HOST_ID)?.status).toBe("unknown");
  });

  // Pin: needsAttention (stale/degraded only) already excludes "unknown"
  // hosts — STATUS_ORDER and the needsAttention filter both treat "unknown"
  // as its own bucket, distinct from stale/degraded (#5101).
  it("excludes sweep-only 'unknown' hosts from needsAttention", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {},
        activeSweeps: [{ hostId: "sweep-only", sweepId: "s1" }],
      }),
      NOW,
    );
    expect(findHost(built, "sweep-only")?.status).toBe("unknown");
    expect(built.needsAttention).toBe(0);
    expect(built.reportingHosts).toBe(0);
  });

  it("returns an empty view for an empty fleet", () => {
    const built = buildFleetView({ hosts: {}, activeSweeps: [] }, NOW);
    expect(built.hosts).toEqual([]);
    expect(built.reportingHosts).toBe(0);
    expect(built.totalSweeps).toBe(0);
    expect(built.needsAttention).toBe(0);
  });
});

describe("isTokenPoolDegraded", () => {
  const pool = (total: number, exhausted: number) => ({
    accounts: [],
    total,
    exhausted,
    peakUsage: undefined,
    hasAccountDetail: true,
  });

  it("is not degraded when no accounts have been reported yet", () => {
    expect(isTokenPoolDegraded(pool(0, 0))).toBe(false);
  });

  it("is not degraded for a partially-spent pool with plenty of availability", () => {
    expect(isTokenPoolDegraded(pool(14, 5))).toBe(false);
  });

  it("is degraded when zero accounts remain available", () => {
    expect(isTokenPoolDegraded(pool(5, 5))).toBe(true);
  });

  it("is degraded when only one account remains available", () => {
    expect(isTokenPoolDegraded(pool(5, 4))).toBe(true);
  });

  it("is not degraded once two or more accounts remain available", () => {
    expect(isTokenPoolDegraded(pool(5, 3))).toBe(false);
  });

  it("is degraded once exhaustion crosses the 75% threshold, even with 2+ available", () => {
    expect(isTokenPoolDegraded(pool(20, 15))).toBe(true); // 5 available, 75% exhausted
  });
});

describe("distressReason / isHostDistressed (#4975)", () => {
  it("is undefined for a host with no health record at all", () => {
    expect(distressReason(undefined)).toBeUndefined();
    expect(isHostDistressed(undefined)).toBe(false);
  });

  it("is undefined for a merely busy host — high load, but still admitting work", () => {
    // The #4975 AC in one line: busy != degraded. Load well below the
    // breaker's trip threshold, idle comfortably above zero, dispatch not
    // halted.
    const reason = distressReason({ load_per_core: 1.4, cpu_idle_fraction: 0.2, dispatch_halted: false });
    expect(reason).toBeUndefined();
  });

  it("names the breaker's own reason when dispatch is halted", () => {
    const reason = distressReason({
      dispatch_halted: true,
      halt_reason: "load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)",
      load_per_core: 4.24,
      cpu_idle_fraction: 0,
    });
    expect(reason).toBe("dispatch halted: load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)");
  });

  it("still reports halted, generically, when dispatch_halted is true with no reason string", () => {
    expect(distressReason({ dispatch_halted: true })).toBe("dispatch halted");
  });

  it("flags load/core at or above the daemon's own distress threshold even without dispatch_halted", () => {
    // Same-number fallback for a daemon build that predates/disables the
    // dispatch_halted field.
    expect(isHostDistressed({ load_per_core: 2.5 })).toBe(true);
    expect(isHostDistressed({ load_per_core: 2.49 })).toBe(false);
  });

  it("flags CPU idle pinned near zero even without dispatch_halted", () => {
    expect(isHostDistressed({ cpu_idle_fraction: 0 })).toBe(true);
    expect(isHostDistressed({ cpu_idle_fraction: 0.02 })).toBe(true);
    // A real busy host dips well above the near-zero line.
    expect(isHostDistressed({ cpu_idle_fraction: 0.2 })).toBe(false);
  });

  // #5022: role-tick health.
  it("names the failing role(s) when roles has a persistent failure", () => {
    const reason = distressReason({
      roles: { total: 3, ok: 1, persistent: [{ root: "/repos/loom", role: "judge", failures: 2 }] },
    });
    expect(reason).toBe("role tick(s) persistently failing: judge @ loom");
  });

  it("is not distressed when roles reports every tick ok", () => {
    expect(isHostDistressed({ roles: { total: 12, ok: 12, persistent: [] } })).toBe(false);
  });

  it("is not distressed when roles reports zero ticks sampled (role runner idle/disabled)", () => {
    expect(isHostDistressed({ roles: { total: 0, ok: 0, persistent: [] } })).toBe(false);
  });

  it("takes priority over the load/idle heuristic fallbacks, same as dispatch_halted", () => {
    const reason = distressReason({
      roles: { total: 2, ok: 0, persistent: [{ root: "/repos/loom", role: "guide", failures: 2 }] },
      load_per_core: 0.1,
      cpu_idle_fraction: 0.9,
    });
    expect(reason).toBe("role tick(s) persistently failing: guide @ loom");
  });
});

describe("buildFleetView host-distress classification (#4975)", () => {
  const snapshotFor = (health: Record<string, unknown>) =>
    parseFleetSnapshot({
      hosts: { h: { health: { record: { kind: "host.health", ...health }, updatedAt: isoMinutesBefore(1) } } },
      activeSweeps: [],
    });

  it("goes degraded when dispatch is halted, independent of the token pool", () => {
    const built = buildFleetView(
      snapshotFor({ dispatch_halted: true, halt_reason: "host-distress breaker" }),
      NOW,
    );
    const host = findHost(built, "h");
    expect(host?.status).toBe("degraded");
    expect(host?.degradedReason).toBe("dispatch halted: host-distress breaker");
  });

  it("goes degraded when load/core is at the daemon's distress threshold", () => {
    const built = buildFleetView(snapshotFor({ load_per_core: 4.24, cpu_idle_fraction: 0 }), NOW);
    expect(findHost(built, "h")?.status).toBe("degraded");
  });

  it("stays ok for a merely busy host — high-ish load, dispatch still admitting", () => {
    const built = buildFleetView(snapshotFor({ load_per_core: 1.4, cpu_idle_fraction: 0.2 }), NOW);
    const host = findHost(built, "h");
    expect(host?.status).toBe("ok");
    expect(host?.degradedReason).toBeUndefined();
  });

  it("goes degraded when roles reports a persistent tick failure, independent of load/tokens (#5022)", () => {
    const built = buildFleetView(snapshotFor({ roles: persistentRoleTickFailureFixture() }), NOW);
    const host = findHost(built, "h");
    expect(host?.status).toBe("degraded");
    expect(host?.degradedReason).toBe("role tick(s) persistently failing: judge @ loom");
  });

  it("stays ok when roles reports zero ticks sampled (role runner idle/disabled) — not an error state", () => {
    const built = buildFleetView(snapshotFor({ roles: { total: 0, ok: 0, persistent: [] } }), NOW);
    expect(findHost(built, "h")?.status).toBe("ok");
  });

  it("names the token-exhaustion reason when that is the only trigger", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: { record: { kind: "host.health", load_per_core: 0.3, cpu_idle_fraction: 0.7 }, updatedAt: isoMinutesBefore(1) },
            tokens: {
              record: { kind: "tokens.snapshot", accounts: [{ account: "a", exhausted: true }] },
              updatedAt: isoMinutesBefore(1),
            },
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const host = findHost(built, "h");
    expect(host?.status).toBe("degraded");
    expect(host?.degradedReason).toBe("token pool at or near exhaustion");
  });
});

describe("summarizeTokens", () => {
  it("summarizes counts and peak usage", () => {
    const built = view();
    const healthy = findHost(built, HEALTHY_HOST_ID);
    expect(healthy?.tokens).toMatchObject({ total: 2, exhausted: 0, peakUsage: 0.42 });
    const degraded = findHost(built, DEGRADED_HOST_ID);
    expect(degraded?.tokens).toMatchObject({ total: 2, exhausted: 1, peakUsage: 1 });
  });

  it("reports peak usage as unknown, not zero, when no account knows it", () => {
    const summary = summarizeTokens({
      tokens: { record: { accounts: [{ account: "a", exhausted: false }] }, updatedAt: "x" },
    });
    expect(summary.peakUsage).toBeUndefined();
    expect(summary.total).toBe(1);
  });

  it("handles a host with no tokens record at all", () => {
    const summary = summarizeTokens({});
    // `hasAccountDetail: true` with an empty pool: nothing is being withheld,
    // this host simply has not reported a snapshot.
    expect(summary).toEqual({ accounts: [], total: 0, exhausted: 0, peakUsage: undefined, hasAccountDetail: true });
  });

  it("reads the public aggregate when per-account rows were withheld", () => {
    const summary = summarizeTokens({
      tokens: {
        record: {
          kind: "tokens.snapshot",
          account_count: 13,
          exhausted_count: 5,
          mean_usage_fraction: 0.32,
          max_usage_fraction: 0.91,
        },
        updatedAt: "2026-07-30T12:00:00Z",
      },
    });
    expect(summary).toEqual({
      accounts: [],
      total: 13,
      exhausted: 5,
      peakUsage: 0.91,
      hasAccountDetail: false,
    });
  });

  it("prefers per-account rows over the aggregate when both are present", () => {
    const summary = summarizeTokens({
      tokens: {
        record: {
          kind: "tokens.snapshot",
          accounts: [
            { account: "a", usage_fraction: 0.5, exhausted: false },
            { account: "b", exhausted: true },
          ],
          account_count: 99,
          exhausted_count: 99,
        },
        updatedAt: "2026-07-30T12:00:00Z",
      },
    });
    expect(summary.total).toBe(2);
    expect(summary.exhausted).toBe(1);
    expect(summary.hasAccountDetail).toBe(true);
  });
});

describe("sortSweeps", () => {
  it("puts the longest-running sweep first and sweeps without a start last", () => {
    const sorted = sortSweeps([
      { hostId: "h", sweepId: "c" },
      { hostId: "h", sweepId: "b", startedAt: "2026-07-30T12:00:00Z" },
      { hostId: "h", sweepId: "a", startedAt: "2026-07-30T11:00:00Z" },
    ]);
    expect(sorted.map((sweep) => sweep.sweepId)).toEqual(["a", "b", "c"]);
  });

  it("is stable for identical start times", () => {
    const sorted = sortSweeps([
      { hostId: "h", sweepId: "z", startedAt: "2026-07-30T12:00:00Z" },
      { hostId: "h", sweepId: "a", startedAt: "2026-07-30T12:00:00Z" },
    ]);
    expect(sorted.map((sweep) => sweep.sweepId)).toEqual(["a", "z"]);
  });
});
