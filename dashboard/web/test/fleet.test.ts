import { describe, expect, it } from "vitest";

import {
  STALE_AFTER_SEC,
  attributeComputeJobs,
  buildFleetView,
  buildHostView,
  degradedProviders,
  distressReason,
  ROLE_FAILURE_DEGRADED_MIN,
  ROLE_FAILURE_RECENT_SEC,
  sustainedRoleFailures,
  throttleReason,
  findHost,
  isHostDistressed,
  isRosterMissingStatus,
  isTokenPoolDegraded,
  noCaptainReporting,
  singletonsArmedOnNonCaptain,
  sortSweeps,
  summarizeTokens,
} from "../src/fleet";
import { parseFleetSnapshot } from "../src/parse";
import {
  DEGRADED_HOST_ID,
  HEALTHY_HOST_ID,
  IDLE_HOST_ID,
  MISSING_HOST_ID,
  NOW,
  PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID,
  STALE_HOST_ID,
  SWEEP_ONLY_HOST_ID,
  UNPROVISIONED_HOST_ID,
  isoMinutesBefore,
  multiHostSnapshot,
  persistentRoleTickFailureFixture,
  rosterMissingSnapshot,
} from "./fixtures";
import type { ActiveComputeJob } from "../src/types";

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

  // #5642: `roleTicks` sums `health.roles` across every host that reports it
  // — only HEALTHY_HOST_ID does in this fixture (`{ total: 12, ok: 12 }`),
  // so the fleet-wide aggregate equals that one host's numbers.
  it("aggregates role-tick totals across reporting hosts", () => {
    const built = view();
    expect(built.roleTicks).toEqual({ total: 12, ok: 12 });
  });

  it("reports roleTicks as undefined when no host has sent health.roles", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: { h: { health: { record: { kind: "host.health" }, updatedAt: isoMinutesBefore(1) } } },
        activeSweeps: [],
      }),
      NOW,
    );
    expect(built.roleTicks).toBeUndefined();
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
    expect(built.roleTicks).toBeUndefined();
  });
});

describe("isTokenPoolDegraded", () => {
  const pool = (total: number, exhausted: number) => ({
    accounts: [],
    total,
    exhausted,
    peakUsage: undefined,
    hasAccountDetail: true,
    providers: [],
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

  it("judges each provider pool on its own — a spent Claude pool is degraded however idle Codex is", () => {
    const summary = {
      ...pool(8, 4),
      providers: [
        { provider: "claude", total: 4, exhausted: 4, peakUsage: 1 },
        { provider: "codex", total: 4, exhausted: 0, peakUsage: undefined },
      ],
    };
    // Pool-wide this is 4/8 — comfortably fine — which is exactly the
    // blended figure that would have hidden the Claude outage.
    expect(isTokenPoolDegraded(summary)).toBe(true);
    expect(degradedProviders(summary)).toEqual(["claude"]);
  });

  it("does not treat a single-account provider as perpetually degraded", () => {
    const summary = {
      ...pool(3, 0),
      providers: [
        { provider: "claude", total: 2, exhausted: 0, peakUsage: 0.2 },
        { provider: "codex", total: 1, exhausted: 0, peakUsage: undefined },
      ],
    };
    expect(isTokenPoolDegraded(summary)).toBe(false);
    // …until that one account is actually spent.
    summary.providers[1]!.exhausted = 1;
    expect(degradedProviders(summary)).toEqual(["codex"]);
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

  it("does NOT treat the host-distress breaker's own halt as distress — that is throttling (#8832)", () => {
    const record = {
      dispatch_halted: true,
      halt_reason: "load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)",
      load_per_core: 4.24,
      cpu_idle_fraction: 0,
    };
    expect(distressReason(record)).toBeUndefined();
    expect(throttleReason(record)).toBe("dispatch paused: load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)");
    expect(throttleReason({ dispatch_halted: true })).toBe("dispatch paused");
    expect(throttleReason({ dispatch_halted: false })).toBeUndefined();
  });

  it("names the admission brake's foreign-load halt as distress (#8478, #8832)", () => {
    const halt = "admission brake STARVING for 900s with 0 sweeps in flight (≥ this host's starvationWarnSecs 300); dispatch is suppressed by load Loom does not own (#8478)";
    expect(distressReason({ dispatch_halted: true, halt_reason: halt })).toBe(`dispatch halted: ${halt}`);
  });

  it("flags load/core at or above the daemon's own distress threshold only for a daemon that sends no dispatch_halted", () => {
    // Same-number fallback for a daemon build that predates the field.
    expect(isHostDistressed({ load_per_core: 2.5 })).toBe(true);
    expect(isHostDistressed({ load_per_core: 2.49 })).toBe(false);
    // #8832: when the daemon reports its own sustained verdict, one hot
    // sample is not a second opinion.
    expect(isHostDistressed({ load_per_core: 3.9, dispatch_halted: false })).toBe(false);
    expect(isHostDistressed({ cpu_idle_fraction: 0, dispatch_halted: false })).toBe(false);
  });

  it("flags CPU idle pinned near zero even without dispatch_halted", () => {
    expect(isHostDistressed({ cpu_idle_fraction: 0 })).toBe(true);
    expect(isHostDistressed({ cpu_idle_fraction: 0.02 })).toBe(true);
    // A real busy host dips well above the near-zero line.
    expect(isHostDistressed({ cpu_idle_fraction: 0.2 })).toBe(false);
  });

  // #5022: role-tick health.
  it("names the failing role(s) when a persistent failure is repeated and recent", () => {
    const reason = distressReason(
      { roles: { total: 5, ok: 1, persistent: [{ root: "/repos/loom", role: "judge", failures: 3, last_at: isoMinutesBefore(5) }] } },
      NOW,
    );
    expect(reason).toBe("role tick(s) persistently failing: judge @ loom");
  });

  it("ignores a persistent pair below the repeat threshold — one or two failed ticks are a blip (#8832)", () => {
    const roles = { total: 3, ok: 1, persistent: [{ root: "/repos/loom", role: "judge", failures: ROLE_FAILURE_DEGRADED_MIN - 1, last_at: isoMinutesBefore(1) }] };
    expect(distressReason({ roles }, NOW)).toBeUndefined();
  });

  it("ignores a persistent pair whose latest tick is older than the recency window (#8832)", () => {
    const stale = new Date(NOW.getTime() - (ROLE_FAILURE_RECENT_SEC + 60) * 1000).toISOString();
    const roles = { total: 9, ok: 0, persistent: [{ root: "/repos/anvil", role: "doctor", failures: 9, last_at: stale }] };
    expect(distressReason({ roles }, NOW)).toBeUndefined();
    expect(sustainedRoleFailures({ roles }, NOW)).toEqual([]);
  });

  it("is not distressed when roles reports every tick ok", () => {
    expect(isHostDistressed({ roles: { total: 12, ok: 12, persistent: [] } })).toBe(false);
  });

  it("is not distressed when roles reports zero ticks sampled (role runner idle/disabled)", () => {
    expect(isHostDistressed({ roles: { total: 0, ok: 0, persistent: [] } })).toBe(false);
  });

  it("takes priority over the load/idle heuristic fallbacks, same as dispatch_halted", () => {
    const reason = distressReason(
      {
        roles: { total: 3, ok: 0, persistent: [{ root: "/repos/loom", role: "guide", failures: 3, last_at: isoMinutesBefore(2) }] },
        load_per_core: 0.1,
        cpu_idle_fraction: 0.9,
      },
      NOW,
    );
    expect(reason).toBe("role tick(s) persistently failing: guide @ loom");
  });
});

describe("singletonsArmedOnNonCaptain (#8848)", () => {
  it("is empty with no health record at all", () => {
    expect(singletonsArmedOnNonCaptain(undefined)).toEqual([]);
  });

  it("is empty when this host IS the captain, even with jobs armed", () => {
    expect(singletonsArmedOnNonCaptain({ is_captain: true, armed_singleton_jobs: ["edge-queue-pull"] })).toEqual([]);
  });

  it("is empty when nothing is armed here, captain or not", () => {
    expect(singletonsArmedOnNonCaptain({ is_captain: false })).toEqual([]);
    expect(singletonsArmedOnNonCaptain({ is_captain: false, armed_singleton_jobs: [] })).toEqual([]);
  });

  it("flags a job armed on a non-captain host — the anomaly this exists to catch", () => {
    expect(singletonsArmedOnNonCaptain({ is_captain: false, armed_singleton_jobs: ["edge-queue-pull"] })).toEqual([
      "edge-queue-pull",
    ]);
  });

  it("flags a job armed when is_captain is not even reported (undefined != true)", () => {
    expect(singletonsArmedOnNonCaptain({ armed_singleton_jobs: ["edge-queue-pull"] })).toEqual(["edge-queue-pull"]);
  });
});

describe("noCaptainReporting (#8848)", () => {
  const hostWith = (isCaptain: boolean | undefined) =>
    buildHostView(
      "h",
      { health: { record: { kind: "host.health", is_captain: isCaptain }, updatedAt: NOW.toISOString() } },
      [],
      NOW,
    );

  it("is false for an empty fleet", () => {
    expect(noCaptainReporting([])).toBe(false);
  });

  it("is false when no host reports is_captain at all — the overwhelmingly common, opted-out fleet", () => {
    expect(noCaptainReporting([hostWith(undefined), hostWith(undefined)])).toBe(false);
  });

  it("is false once at least one host reports is_captain: true", () => {
    expect(noCaptainReporting([hostWith(false), hostWith(true), hostWith(undefined)])).toBe(false);
  });

  it("is true when the fleet participates (some host reports is_captain) but none is true — typo'd/decommissioned captain id", () => {
    expect(noCaptainReporting([hostWith(false), hostWith(false)])).toBe(true);
  });

  it("is true even when only one host in a larger fleet participates and it is false", () => {
    expect(noCaptainReporting([hostWith(undefined), hostWith(false)])).toBe(true);
  });
});

describe("buildFleetView host-distress classification (#4975)", () => {
  const snapshotFor = (health: Record<string, unknown>) =>
    parseFleetSnapshot({
      hosts: { h: { health: { record: { kind: "host.health", ...health }, updatedAt: isoMinutesBefore(1) } } },
      activeSweeps: [],
    });

  it("goes throttled — not degraded, not needing attention — when the host breaker pauses dispatch (#8832)", () => {
    const built = buildFleetView(
      snapshotFor({ dispatch_halted: true, halt_reason: "host-distress breaker", load_per_core: 3.9 }),
      NOW,
    );
    const host = findHost(built, "h");
    expect(host?.status).toBe("throttled");
    expect(host?.degradedReason).toBe("dispatch paused: host-distress breaker");
    expect(built.needsAttention).toBe(0);
  });

  it("stays ok for a single failed role tick — the 2026-09-24 always-degraded case (#8832)", () => {
    const built = buildFleetView(
      snapshotFor({
        dispatch_halted: false,
        roles: { total: 400, ok: 399, persistent: [{ root: "/repos/loom", role: "judge", failures: 1, last_at: isoMinutesBefore(3) }] },
      }),
      NOW,
    );
    expect(findHost(built, "h")?.status).toBe("ok");
    expect(built.needsAttention).toBe(0);
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

  it("goes degraded when roles reports a repeated, recent tick failure, independent of load/tokens (#5022, #8832)", () => {
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
    // Rows with no `provider` are the Claude pool, and the reason says so.
    expect(host?.degradedReason).toBe("claude token pool exhausted — nothing left to dispatch on");
  });
});

describe("buildFleetView token pool — near-empty is throttled, empty is degraded (#8832)", () => {
  const withAccounts = (accounts: unknown[]) =>
    parseFleetSnapshot({
      hosts: {
        h: {
          health: { record: { kind: "host.health", dispatch_halted: false }, updatedAt: isoMinutesBefore(1) },
          tokens: { record: { kind: "tokens.snapshot", accounts }, updatedAt: isoMinutesBefore(1) },
        },
      },
      activeSweeps: [],
    });

  it("goes throttled, not degraded, when one account is left to rotate onto", () => {
    const built = buildFleetView(
      withAccounts([{ account: "a", exhausted: true }, { account: "b", exhausted: false }]),
      NOW,
    );
    const host = findHost(built, "h");
    expect(host?.status).toBe("throttled");
    expect(host?.degradedReason).toBe("claude token pool running low");
    expect(built.needsAttention).toBe(0);
  });

  it("goes degraded once every account in a provider is spent", () => {
    const built = buildFleetView(
      withAccounts([{ account: "a", exhausted: true }, { account: "b", exhausted: true }]),
      NOW,
    );
    expect(findHost(built, "h")?.status).toBe("degraded");
    expect(built.needsAttention).toBe(1);
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
    expect(summary).toEqual({
      accounts: [],
      total: 0,
      exhausted: 0,
      peakUsage: undefined,
      hasAccountDetail: true,
      providers: [],
    });
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
      // A backend that predates the per-provider aggregate: the pool was
      // the Claude pool, so it becomes one Claude slice rather than none.
      providers: [{ provider: "claude", total: 13, exhausted: 5, peakUsage: 0.91 }],
    });
  });

  it("reads the public per-provider aggregate when the backend sends one", () => {
    const summary = summarizeTokens({
      tokens: {
        record: {
          kind: "tokens.snapshot",
          account_count: 5,
          exhausted_count: 3,
          max_usage_fraction: 1,
          providers: [
            { provider: "claude", account_count: 2, exhausted_count: 1, max_usage_fraction: 1 },
            { provider: "codex", account_count: 3, exhausted_count: 2, max_usage_fraction: null },
            // A slice too malformed to name is dropped, not rendered under a
            // fabricated provider.
            { account_count: 9 },
          ],
        },
        updatedAt: "2026-07-30T12:00:00Z",
      },
    });
    expect(summary.hasAccountDetail).toBe(false);
    expect(summary.providers).toEqual([
      { provider: "claude", total: 2, exhausted: 1, peakUsage: 1 },
      { provider: "codex", total: 3, exhausted: 2, peakUsage: undefined },
    ]);
  });

  it("splits per-account rows by provider, folding untagged rows into claude", () => {
    const summary = summarizeTokens({
      tokens: {
        record: {
          kind: "tokens.snapshot",
          accounts: [
            { account: "agent-1", usage_fraction: 0.5, exhausted: false },
            { account: "agent-2", provider: "claude", usage_fraction: 0.9, exhausted: true },
            { account: "cx-1", provider: "codex", exhausted: false },
            { account: "cx-2", provider: "codex", exhausted: true },
            { account: "cx-3", provider: "codex", exhausted: true },
          ],
        },
        updatedAt: "2026-07-30T12:00:00Z",
      },
    });
    expect(summary.total).toBe(5);
    expect(summary.exhausted).toBe(3);
    expect(summary.providers).toEqual([
      { provider: "claude", total: 2, exhausted: 1, peakUsage: 0.9 },
      { provider: "codex", total: 3, exhausted: 2, peakUsage: undefined },
    ]);
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

// ---------------------------------------------------------------------------
// Expected-host roster (#8792 backend → #8804 SPA)
// ---------------------------------------------------------------------------

describe("buildFleetView — missingHosts (#8804)", () => {
  const roster = () => buildFleetView(parseFleetSnapshot(rosterMissingSnapshot()), NOW);

  it("adds roster hosts that never reported to the host set", () => {
    const built = roster();
    // 6 telemetry-derived hosts (5 reporting + 1 sweep-only), plus the two
    // roster hosts that have no `hosts` entry at all.
    expect(built.hosts).toHaveLength(8);
    expect(findHost(built, MISSING_HOST_ID)?.status).toBe("missing");
    expect(findHost(built, UNPROVISIONED_HOST_ID)?.status).toBe("unprovisioned");
  });

  it("counts roster hosts separately from reporting hosts", () => {
    const built = roster();
    expect(built.reportingHosts).toBe(5);
    expect(built.missingHosts).toBe(1);
    expect(built.unprovisionedHosts).toBe(1);
  });

  it("counts a missing host as needing attention, but not an unprovisioned one", () => {
    // `missing` means an enrolled host is silent — an incident.
    // `unprovisioned` means it was never enrolled — a to-do, not an outage,
    // and flagging it would make "needs attention" permanently non-zero for
    // any roster listing a host someone plans to add.
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {},
        activeSweeps: [],
        missingHosts: [
          { hostId: "m", state: "missing" },
          { hostId: "u", state: "unprovisioned" },
        ],
      }),
      NOW,
    );
    expect(built.needsAttention).toBe(1);
    expect(built.reportingHosts).toBe(0);
  });

  it("sorts missing first and unprovisioned above the data-less unknown bucket", () => {
    const built = roster();
    const order = built.hosts.map((host) => host.hostId);
    expect(order[0]).toBe(MISSING_HOST_ID);
    expect(order.indexOf(UNPROVISIONED_HOST_ID)).toBeLessThan(order.indexOf(SWEEP_ONLY_HOST_ID));
    // The four pre-existing statuses keep their relative order.
    expect(order.indexOf(STALE_HOST_ID)).toBeLessThan(order.indexOf(DEGRADED_HOST_ID));
    expect(order.indexOf(DEGRADED_HOST_ID)).toBeLessThan(order.indexOf(SWEEP_ONLY_HOST_ID));
    expect(order.indexOf(SWEEP_ONLY_HOST_ID)).toBeLessThan(order.indexOf(HEALTHY_HOST_ID));
  });

  it("keeps the sweeps of a roster host that pushed sweep records but never health", () => {
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {},
        activeSweeps: [{ hostId: "m", sweepId: "s1", phase: "builder" }],
        missingHosts: [{ hostId: "m", state: "missing" }],
      }),
      NOW,
    );
    const host = findHost(built, "m");
    expect(host?.status).toBe("missing");
    expect(host?.sweeps).toHaveLength(1);
  });

  it("never overrides live telemetry with a stale roster classification", () => {
    // A host with a real health record is not "never reported", whatever a
    // hand-built or racing snapshot's missingHosts claims — a card saying
    // both "last report 2m ago" and "never reported" would be incoherent.
    const built = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: { health: { record: { kind: "host.health" }, updatedAt: isoMinutesBefore(2) } },
          t: { tokens: { record: { kind: "tokens.snapshot", accounts: [] }, updatedAt: isoMinutesBefore(2) } },
        },
        activeSweeps: [],
        missingHosts: [
          { hostId: "h", state: "missing" },
          { hostId: "t", state: "missing" },
        ],
      }),
      NOW,
    );
    expect(findHost(built, "h")?.status).toBe("ok");
    expect(findHost(built, "t")?.status).toBe("ok");
    expect(built.missingHosts).toBe(0);
    expect(built.reportingHosts).toBe(2);
  });

  it("leaves every count untouched when the payload carries no missingHosts", () => {
    const withRoster = roster();
    const without = view();
    expect(without.missingHosts).toBe(0);
    expect(without.unprovisionedHosts).toBe(0);
    expect(without.hosts).toHaveLength(6);
    expect(without.reportingHosts).toBe(withRoster.reportingHosts);
    expect(without.needsAttention).toBe(2);
  });

  it("classifies roster state through buildHostView's own parameter", () => {
    expect(buildHostView("h", {}, [], NOW, "missing").status).toBe("missing");
    expect(buildHostView("h", {}, [], NOW, "unprovisioned").status).toBe("unprovisioned");
    expect(buildHostView("h", {}, [], NOW).status).toBe("unknown");
    expect(isRosterMissingStatus("missing")).toBe(true);
    expect(isRosterMissingStatus("unprovisioned")).toBe(true);
    for (const status of ["ok", "degraded", "stale", "unknown"] as const) {
      expect(isRosterMissingStatus(status)).toBe(false);
    }
  });
});

/**
 * Issue #8835 — joining live compute jobs to the sweep that submitted them.
 *
 * The load-bearing property here is not the nesting, it is the *partition*:
 * every job lands in exactly one of `bySweep`/`unattributed`, so a job that
 * cannot be attributed — the shape an orphaned, still-billing instance takes —
 * is guaranteed to stay visible in the fleet-level "running compute" list.
 */
describe("attributeComputeJobs (#8835)", () => {
  const sweep = (sweepId: string, hostId = "host-a") => ({
    hostId,
    sweepId,
    startedAt: "2026-09-19T12:00:00Z",
  });
  const job = (jobId: string, overrides: Partial<ActiveComputeJob> = {}): ActiveComputeJob => ({
    hostId: "2am-elastic",
    jobId,
    instanceType: "c7i.4xlarge",
    spot: true,
    startedAt: "2026-09-19T12:00:00Z",
    ...overrides,
  });

  it("nests a job under the live sweep its sweepId names", () => {
    const { bySweep, unattributed } = attributeComputeJobs(
      [job("job-1", { sweepId: "sweep-issue-8835-1" })],
      [sweep("sweep-issue-8835-1")],
    );
    expect(bySweep.get("sweep-issue-8835-1")?.map((entry) => entry.jobId)).toEqual(["job-1"]);
    expect(unattributed).toEqual([]);
  });

  it("joins on sweepId ALONE — a matching hostId is not an attribution", () => {
    // The submitter's `hostId` is its ingest identity (one synthetic id for a
    // whole hostless elastic fleet), so a hostId match says nothing about
    // which sweep is paying. Attributing on it would be confidently wrong.
    const { bySweep, unattributed } = attributeComputeJobs(
      [job("job-1", { hostId: "host-a" })],
      [sweep("sweep-issue-8835-1", "host-a")],
    );
    expect(bySweep.size).toBe(0);
    expect(unattributed.map((entry) => entry.jobId)).toEqual(["job-1"]);
  });

  it("leaves a job whose sweep is not live in the unattributed list", () => {
    const { bySweep, unattributed } = attributeComputeJobs(
      [job("job-gone", { sweepId: "sweep-finished-9" })],
      [sweep("sweep-issue-8835-1")],
    );
    expect(bySweep.size).toBe(0);
    expect(unattributed.map((entry) => entry.jobId)).toEqual(["job-gone"]);
  });

  it("leaves a job with no sweepId at all in the unattributed list", () => {
    // A pre-#8835 emitter, or a submission from outside any sweep.
    const { unattributed } = attributeComputeJobs(
      [job("job-bare"), job("job-empty", { sweepId: "" })],
      [sweep("sweep-issue-8835-1")],
    );
    expect(unattributed.map((entry) => entry.jobId)).toEqual(["job-bare", "job-empty"]);
  });

  it("partitions without dropping anything, and preserves the incoming order", () => {
    const jobs = [
      job("a", { sweepId: "s1" }),
      job("b"),
      job("c", { sweepId: "s1" }),
      job("d", { sweepId: "s-dead" }),
      job("e", { sweepId: "s2" }),
    ];
    const { bySweep, unattributed } = attributeComputeJobs(jobs, [sweep("s1"), sweep("s2")]);
    expect(bySweep.get("s1")?.map((entry) => entry.jobId)).toEqual(["a", "c"]);
    expect(bySweep.get("s2")?.map((entry) => entry.jobId)).toEqual(["e"]);
    expect(unattributed.map((entry) => entry.jobId)).toEqual(["b", "d"]);
    const total = [...bySweep.values()].reduce((sum, list) => sum + list.length, 0) + unattributed.length;
    expect(total).toBe(jobs.length);
  });
});

describe("buildFleetView — compute attribution (#8835)", () => {
  const snapshot = (activeSweeps: unknown[], activeCompute: unknown[]) => ({
    hosts: { "host-a": { health: { record: { kind: "host.health" }, updatedAt: NOW.toISOString() } } },
    activeSweeps,
    activeCompute,
  });
  const liveSweep = {
    hostId: "host-a",
    sweepId: "sweep-issue-8835-1",
    issue: 8835,
    phase: "builder",
    startedAt: "2026-09-19T12:00:00Z",
  };
  const computeJob = {
    hostId: "2am-elastic",
    jobId: "job-1",
    instanceType: "c7i.4xlarge",
    spot: true,
    startedAt: "2026-09-19T12:00:00Z",
  };

  it("hangs the job off its sweep's own host view, keyed by sweepId", () => {
    const view = buildFleetView(
      parseFleetSnapshot(snapshot([liveSweep], [{ ...computeJob, sweepId: liveSweep.sweepId }])),
      NOW,
    );
    const host = findHost(view, "host-a")!;
    expect([...host.computeBySweep.keys()]).toEqual([liveSweep.sweepId]);
    expect(host.computeBySweep.get(liveSweep.sweepId)?.map((entry) => entry.jobId)).toEqual(["job-1"]);
    // Nested, so not also listed flat.
    expect(view.unattributedCompute).toEqual([]);
    // …but still counted fleet-wide: nesting must not make the fleet look
    // like it is running less compute than it is.
    expect(view.activeCompute).toHaveLength(1);
  });

  it("keeps an unmatched job — and its leaked flag — in the flat running-compute list", () => {
    const view = buildFleetView(
      parseFleetSnapshot(
        snapshot([liveSweep], [{ ...computeJob, sweepId: "sweep-long-finished", leaked: true }]),
      ),
      NOW,
    );
    expect(findHost(view, "host-a")!.computeBySweep.size).toBe(0);
    expect(view.unattributedCompute.map((entry) => entry.jobId)).toEqual(["job-1"]);
    expect(view.unattributedCompute[0]?.leaked).toBe(true);
    expect(view.leakedCompute).toBe(1);
  });

  it("counts a nested leaked job in leakedCompute too, so nesting cannot hide a leak", () => {
    const view = buildFleetView(
      parseFleetSnapshot(snapshot([liveSweep], [{ ...computeJob, sweepId: liveSweep.sweepId, leaked: true }])),
      NOW,
    );
    expect(view.leakedCompute).toBe(1);
  });

  it("retires the nested job when its completion record removes it from the snapshot", () => {
    // The backend deletes the `compute:<jobId>` entry on the completion
    // record (`../../src/fleetState.ts`), so "retired" reaches the UI as an
    // absent entry — the sweep simply has no subprocesses again.
    const view = buildFleetView(parseFleetSnapshot(snapshot([liveSweep], [])), NOW);
    expect(findHost(view, "host-a")!.computeBySweep.size).toBe(0);
    expect(view.activeCompute).toEqual([]);
    expect(view.unattributedCompute).toEqual([]);
  });

  it("leaves every host's map empty on a snapshot whose emitters do not stamp sweepId", () => {
    const view = buildFleetView(parseFleetSnapshot(snapshot([liveSweep], [computeJob])), NOW);
    expect(view.hosts.every((host) => host.computeBySweep.size === 0)).toBe(true);
    expect(view.unattributedCompute).toHaveLength(1);
  });
});
