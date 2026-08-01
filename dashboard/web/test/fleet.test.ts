import { describe, expect, it } from "vitest";

import {
  STALE_AFTER_SEC,
  buildFleetView,
  findHost,
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
    // partially-exhausted-but-healthy one.
    expect(built.needsAttention).toBe(2);
  });

  it("returns an empty view for an empty fleet", () => {
    const built = buildFleetView({ hosts: {}, activeSweeps: [] }, NOW);
    expect(built.hosts).toEqual([]);
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
