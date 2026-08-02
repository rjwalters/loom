/**
 * Unit tests for the pure staleness-classification/pruning core the
 * `FleetState` Durable Object's `buildSnapshot` delegates to (issue #4957).
 *
 * These test the exported pure functions directly — `classifyFreshness` and
 * `classifyAndPruneHosts` — rather than spinning up the Durable Object,
 * because there is no way to control workerd's own wall clock from a
 * vitest-pool-workers test (`vi.useFakeTimers()` patches Node's `Date`, not
 * the separate workerd isolate the DO actually runs in). `now` is threaded
 * through both functions as an explicit parameter for exactly this reason.
 */
import { describe, expect, it } from "vitest";
import {
  classifyAndPruneHosts,
  classifyFreshness,
  LIVE_AFTER_SEC,
  OFFLINE_AFTER_SEC,
  PRUNE_AFTER_MS,
} from "../src/fleetState";

const NOW = new Date("2026-08-02T12:00:00Z");

function secondsAgo(seconds: number): string {
  return new Date(NOW.getTime() - seconds * 1000).toISOString();
}

describe("classifyFreshness", () => {
  it("classifies an entry updated just now as live", () => {
    expect(classifyFreshness(secondsAgo(0), NOW)).toEqual({ status: "live", ageSeconds: 0 });
  });

  it("classifies an entry at the LIVE boundary as live", () => {
    expect(classifyFreshness(secondsAgo(LIVE_AFTER_SEC), NOW).status).toBe("live");
  });

  it("classifies an entry one second past the LIVE boundary as stale", () => {
    expect(classifyFreshness(secondsAgo(LIVE_AFTER_SEC + 1), NOW).status).toBe("stale");
  });

  it("classifies an entry at the OFFLINE boundary as stale", () => {
    expect(classifyFreshness(secondsAgo(OFFLINE_AFTER_SEC), NOW).status).toBe("stale");
  });

  it("classifies an entry one second past the OFFLINE boundary as offline", () => {
    expect(classifyFreshness(secondsAgo(OFFLINE_AFTER_SEC + 1), NOW).status).toBe("offline");
  });

  it("classifies a very old entry as offline, with the correct age", () => {
    const result = classifyFreshness(secondsAgo(30 * 24 * 60 * 60), NOW);
    expect(result.status).toBe("offline");
    expect(result.ageSeconds).toBe(30 * 24 * 60 * 60);
  });

  it("treats an unparseable updatedAt as offline rather than throwing or reading as fresh", () => {
    expect(classifyFreshness("not-a-timestamp", NOW)).toEqual({
      status: "offline",
      ageSeconds: Number.POSITIVE_INFINITY,
    });
  });
});

describe("classifyAndPruneHosts", () => {
  function entryMap(entries: Record<string, { record: Record<string, unknown>; updatedAt: string }>): Map<
    string,
    { record: Record<string, unknown>; updatedAt: string }
  > {
    return new Map(Object.entries(entries));
  }

  it("classifies a live host.health entry and attaches its freshness", () => {
    const { hosts, pruneKeys } = classifyAndPruneHosts(
      entryMap({ "health:host-a": { record: { kind: "host.health" }, updatedAt: secondsAgo(60) } }),
      entryMap({}),
      NOW,
    );
    expect(pruneKeys).toEqual([]);
    expect(hosts["host-a"]?.health?.freshness).toEqual({ status: "live", ageSeconds: 60 });
    expect(hosts["host-a"]?.health?.record).toEqual({ kind: "host.health" });
  });

  it("classifies a stale tokens.snapshot entry independently of a live health entry for the same host", () => {
    const { hosts } = classifyAndPruneHosts(
      entryMap({ "health:host-a": { record: { kind: "host.health" }, updatedAt: secondsAgo(30) } }),
      entryMap({
        "tokens:host-a": { record: { kind: "tokens.snapshot" }, updatedAt: secondsAgo(OFFLINE_AFTER_SEC - 1) },
      }),
      NOW,
    );
    expect(hosts["host-a"]?.health?.freshness?.status).toBe("live");
    expect(hosts["host-a"]?.tokens?.freshness?.status).toBe("stale");
  });

  it("classifies an offline entry but still returns it (offline != pruned)", () => {
    const { hosts, pruneKeys } = classifyAndPruneHosts(
      entryMap({ "health:host-a": { record: {}, updatedAt: secondsAgo(OFFLINE_AFTER_SEC + 60) } }),
      entryMap({}),
      NOW,
    );
    expect(pruneKeys).toEqual([]);
    expect(hosts["host-a"]?.health?.freshness?.status).toBe("offline");
  });

  it("prunes an entry older than PRUNE_AFTER_MS and excludes it from hosts", () => {
    const ancientUpdatedAt = new Date(NOW.getTime() - PRUNE_AFTER_MS - 1000).toISOString();
    const { hosts, pruneKeys } = classifyAndPruneHosts(
      entryMap({ "health:host-old": { record: {}, updatedAt: ancientUpdatedAt } }),
      entryMap({}),
      NOW,
    );
    expect(hosts["host-old"]).toBeUndefined();
    expect(pruneKeys).toEqual(["health:host-old"]);
  });

  it("does not prune an entry exactly at the prune horizon", () => {
    const boundaryUpdatedAt = new Date(NOW.getTime() - PRUNE_AFTER_MS).toISOString();
    const { hosts, pruneKeys } = classifyAndPruneHosts(
      entryMap({ "health:host-a": { record: {}, updatedAt: boundaryUpdatedAt } }),
      entryMap({}),
      NOW,
    );
    expect(pruneKeys).toEqual([]);
    expect(hosts["host-a"]?.health).toBeDefined();
  });

  it("prunes health and tokens entries independently for a host that is old in one and fresh in the other", () => {
    const ancientUpdatedAt = new Date(NOW.getTime() - PRUNE_AFTER_MS - 1000).toISOString();
    const { hosts, pruneKeys } = classifyAndPruneHosts(
      entryMap({ "health:host-a": { record: {}, updatedAt: ancientUpdatedAt } }),
      entryMap({ "tokens:host-a": { record: {}, updatedAt: secondsAgo(60) } }),
      NOW,
    );
    expect(pruneKeys).toEqual(["health:host-a"]);
    expect(hosts["host-a"]?.health).toBeUndefined();
    expect(hosts["host-a"]?.tokens?.freshness?.status).toBe("live");
  });
});
