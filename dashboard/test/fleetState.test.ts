/**
 * Tests for the `FleetState` Durable Object's live-state hygiene.
 *
 * Two independent concerns share this file because they share the module
 * under test:
 *
 *  - **Host staleness / pruning (issue #4957)** — the pure
 *    classification/pruning core `buildSnapshot` delegates to for the
 *    `health:`/`tokens:` prefixes.
 *  - **Leaked `sweep:` entry reconciliation (issue #4955)** — the pure
 *    staleness-bound + per-host reconciliation decisions, plus integration
 *    coverage against the real DO through its `/update` + `/snapshot`
 *    interface.
 *  - **Live `compute:` entries (issue #8305, Phase 2 of #8257)** — the
 *    `ephemeral_compute` launch/completion lifecycle and its own (much
 *    longer) leak-detection bound, same pure-core + DO-integration split.
 *
 * The pure functions are tested directly rather than by spinning up the
 * Durable Object wherever a controlled clock matters, because there is no
 * way to control workerd's own wall clock from a vitest-pool-workers test
 * (`vi.useFakeTimers()` patches Node's `Date`, not the separate workerd
 * isolate the DO actually runs in) — `now`/`nowMs` is threaded through each
 * of them as an explicit parameter for exactly this reason.
 */
import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import {
  classifyAndPruneHosts,
  classifyComputeEntries,
  classifyFreshness,
  filterRevokedHosts,
  isComputeEntryLeaked,
  isSweepEntryStale,
  LIVE_AFTER_SEC,
  OFFLINE_AFTER_SEC,
  PRUNE_AFTER_MS,
  PRUNE_COMPUTE_AFTER_MS,
  selectReconciledAwaySweepKeys,
  selectStaleSweepKeys,
  STALE_COMPUTE_MS,
  STALE_SWEEP_MS,
  type ActiveComputeState,
  type ActiveSweepState,
  type FleetSnapshot,
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

// ---------------------------------------------------------------------------
// filterRevokedHosts (Issue #5078, mechanism 2): a D1-revoked host must never
// keep rendering as live off a `handleRevokeHost` cleanup fetch that failed
// and left stale `health:`/`tokens:` entries behind in the DO.
// ---------------------------------------------------------------------------

describe("filterRevokedHosts", () => {
  function snapshotWith(
    hosts: FleetSnapshot["hosts"],
    activeSweeps: ActiveSweepState[] = [],
    activeCompute: ActiveComputeState[] = [],
  ): FleetSnapshot {
    return { hosts, activeSweeps, activeCompute };
  }

  it("drops a host entry whose id is in the revoked set — the stale-DO-cleanup case", () => {
    // Simulates `handleRevokeHost`'s best-effort DO cleanup having failed:
    // D1 says "revoked", but the DO still has this host's last-known
    // health/tokens entries.
    const snapshot = snapshotWith({
      "host-revoked": { health: { record: { kind: "host.health" }, updatedAt: secondsAgo(60) } },
      "host-live": { health: { record: { kind: "host.health" }, updatedAt: secondsAgo(30) } },
    });
    const filtered = filterRevokedHosts(snapshot, new Set(["host-revoked"]));
    expect(filtered.hosts["host-revoked"]).toBeUndefined();
    expect(filtered.hosts["host-live"]).toBeDefined();
  });

  it("leaves activeSweeps untouched — a mid-run-revoke anomaly must remain visible, not be hidden", () => {
    const sweep = sweepEntry({ hostId: "host-revoked", sweepId: "still-running" });
    const snapshot = snapshotWith(
      { "host-revoked": { health: { record: {}, updatedAt: secondsAgo(60) } } },
      [sweep],
    );
    const filtered = filterRevokedHosts(snapshot, new Set(["host-revoked"]));
    expect(filtered.hosts["host-revoked"]).toBeUndefined();
    expect(filtered.activeSweeps).toEqual([sweep]);
  });

  it("leaves activeCompute untouched — same reasoning as activeSweeps (issue #8305)", () => {
    const job = computeEntry({ hostId: "host-revoked", jobId: "job-still-running" });
    const snapshot = snapshotWith(
      { "host-revoked": { health: { record: {}, updatedAt: secondsAgo(60) } } },
      [],
      [job],
    );
    const filtered = filterRevokedHosts(snapshot, new Set(["host-revoked"]));
    expect(filtered.hosts["host-revoked"]).toBeUndefined();
    expect(filtered.activeCompute).toEqual([job]);
  });

  it("an empty revoked set is a no-op, returning the same snapshot", () => {
    const snapshot = snapshotWith({ "host-a": { health: { record: {}, updatedAt: secondsAgo(10) } } });
    expect(filterRevokedHosts(snapshot, new Set())).toBe(snapshot);
  });

  it("a revoked id absent from the snapshot's hosts is simply a no-op for that id", () => {
    const snapshot = snapshotWith({ "host-a": { health: { record: {}, updatedAt: secondsAgo(10) } } });
    const filtered = filterRevokedHosts(snapshot, new Set(["host-never-existed"]));
    expect(filtered.hosts["host-a"]).toBeDefined();
    expect(Object.keys(filtered.hosts)).toEqual(["host-a"]);
  });
});

// ---------------------------------------------------------------------------
// Pure decision logic (Issue #4955, fix layers 1 + 2) — no Durable Object /
// storage involved, so these run fast and deterministically regardless of
// whatever isolation the DO's own storage gets between tests.
// ---------------------------------------------------------------------------

function sweepEntry(overrides: Partial<ActiveSweepState> = {}): ActiveSweepState {
  return {
    hostId: "host-abc",
    sweepId: "sweep-issue-4703-0",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    updatedAt: "2026-07-30T12:00:00Z",
    ...overrides,
  };
}

describe("isSweepEntryStale", () => {
  it("is not stale exactly at the bound", () => {
    const now = Date.parse("2026-07-30T12:00:00Z") + STALE_SWEEP_MS;
    const entry = sweepEntry({ updatedAt: "2026-07-30T12:00:00Z" });
    expect(isSweepEntryStale(entry, now)).toBe(false);
  });

  it("is stale one millisecond past the bound", () => {
    const now = Date.parse("2026-07-30T12:00:00Z") + STALE_SWEEP_MS + 1;
    const entry = sweepEntry({ updatedAt: "2026-07-30T12:00:00Z" });
    expect(isSweepEntryStale(entry, now)).toBe(true);
  });

  it("a fresh entry is never stale", () => {
    const now = Date.parse("2026-07-30T12:00:00Z") + 60_000; // 1 minute later
    const entry = sweepEntry({ updatedAt: "2026-07-30T12:00:00Z" });
    expect(isSweepEntryStale(entry, now)).toBe(false);
  });

  it("an unparseable updatedAt fails open (never stale)", () => {
    const entry = sweepEntry({ updatedAt: "not-a-timestamp" });
    expect(isSweepEntryStale(entry, Date.now() + 100 * STALE_SWEEP_MS)).toBe(false);
  });
});

describe("selectStaleSweepKeys", () => {
  it("excludes a sweep: entry older than the staleness bound and retains a fresh one", () => {
    const now = Date.parse("2026-07-30T16:00:00Z"); // 4h after the stale entry's updatedAt
    const entries: [string, ActiveSweepState][] = [
      ["sweep:stale-1", sweepEntry({ sweepId: "stale-1", updatedAt: "2026-07-30T11:00:00Z" })],
      ["sweep:fresh-1", sweepEntry({ sweepId: "fresh-1", updatedAt: "2026-07-30T15:55:00Z" })],
    ];
    expect(selectStaleSweepKeys(entries, now)).toEqual(["sweep:stale-1"]);
  });

  it("an empty entry set has nothing stale", () => {
    expect(selectStaleSweepKeys([], Date.now())).toEqual([]);
  });
});

describe("selectReconciledAwaySweepKeys", () => {
  const entries: [string, ActiveSweepState][] = [
    ["sweep:a-1", sweepEntry({ hostId: "host-a", sweepId: "a-1" })],
    ["sweep:a-2", sweepEntry({ hostId: "host-a", sweepId: "a-2" })],
    ["sweep:b-1", sweepEntry({ hostId: "host-b", sweepId: "b-1" })],
  ];

  it("deletes only this host's entries not present in active_sweep_ids", () => {
    const result = selectReconciledAwaySweepKeys(entries, "host-a", ["a-1"]);
    expect(result).toEqual(["sweep:a-2"]);
  });

  it("leaves other hosts' entries untouched even when their sweep ids are absent from the set", () => {
    const result = selectReconciledAwaySweepKeys(entries, "host-a", ["a-1"]);
    expect(result).not.toContain("sweep:b-1");
  });

  it("every id present ⇒ nothing to reconcile away", () => {
    expect(selectReconciledAwaySweepKeys(entries, "host-a", ["a-1", "a-2"])).toEqual([]);
  });

  it("an absent active_sweep_ids field is a no-op (must not wipe live sweeps)", () => {
    expect(selectReconciledAwaySweepKeys(entries, "host-a", undefined)).toEqual([]);
  });

  it("an empty active_sweep_ids array is a no-op (daemon registry not yet authoritative)", () => {
    expect(selectReconciledAwaySweepKeys(entries, "host-a", [])).toEqual([]);
  });

  it("a non-array active_sweep_ids is a no-op rather than a crash", () => {
    expect(selectReconciledAwaySweepKeys(entries, "host-a", "not-an-array")).toEqual([]);
    expect(selectReconciledAwaySweepKeys(entries, "host-a", { a: 1 })).toEqual([]);
    expect(selectReconciledAwaySweepKeys(entries, "host-a", null)).toEqual([]);
  });

  it("non-string elements are dropped from the active set, not treated as valid ids", () => {
    // Every element is malformed ⇒ the decoded active set is empty ⇒ treated
    // exactly like an absent/empty field: a no-op, never a full wipe.
    const result = selectReconciledAwaySweepKeys(entries, "host-a", [42, null, {}]);
    expect(result).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Integration coverage against the real `FleetState` Durable Object, driven
// through its actual `/update` + `/snapshot` HTTP interface (mirrors how
// `src/index.ts` talks to it) — one fresh DO instance per test (a unique
// `idFromName`) so tests never see each other's storage regardless of the
// pool's isolated-storage setting.
// ---------------------------------------------------------------------------

function fleetStateStub(name: string): DurableObjectStub {
  return env.FLEET_STATE.get(env.FLEET_STATE.idFromName(name));
}

async function update(stub: DurableObjectStub, hostId: string, record: Record<string, unknown>): Promise<void> {
  const response = await stub.fetch("https://fleet-state/update", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ hostId, record }),
  });
  expect(response.status).toBe(204);
}

async function snapshot(stub: DurableObjectStub): Promise<FleetSnapshot> {
  const response = await stub.fetch("https://fleet-state/snapshot");
  expect(response.status).toBe(200);
  return response.json();
}

function sweepIds(snap: FleetSnapshot): string[] {
  return snap.activeSweeps.map((s) => s.sweepId).sort();
}

describe("FleetState — sweep runtime", () => {
  it("carries sweep.started's runtime into the live entry and keeps it across sweep.phase", async () => {
    const stub = fleetStateStub("test-runtime-carry");
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-rt-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      started_at: "2026-08-02T00:00:00Z",
      runtime: "codex",
    });
    let snap = await snapshot(stub);
    expect(snap.activeSweeps[0]?.runtime).toBe("codex");

    await update(stub, "host-a", {
      kind: "sweep.phase",
      sweep_id: "sweep-rt-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      phase: "builder",
      entered_at: "2026-08-02T00:05:00Z",
    });
    snap = await snapshot(stub);
    expect(snap.activeSweeps[0]?.phase).toBe("builder");
    expect(snap.activeSweeps[0]?.runtime).toBe("codex");
  });

  it("leaves runtime absent when the daemon did not name one", async () => {
    const stub = fleetStateStub("test-runtime-absent");
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-rt-2",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 2,
      started_at: "2026-08-02T00:00:00Z",
    });
    const snap = await snapshot(stub);
    expect(snap.activeSweeps[0]).not.toHaveProperty("runtime");
  });
});

describe("FleetState — host.health reconciliation (fix layer 2, integration)", () => {
  it("a host.health update with active_sweep_ids removes this host's entries not in the set", async () => {
    const stub = fleetStateStub("test-reconcile-basic");
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-a-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      started_at: "2026-08-02T00:00:00Z",
    });
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-a-2",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 2,
      started_at: "2026-08-02T00:00:00Z",
    });

    let snap = await snapshot(stub);
    expect(sweepIds(snap)).toEqual(["sweep-a-1", "sweep-a-2"]);

    // The daemon's registry only still knows about sweep-a-1 — sweep-a-2's
    // sweep.completed must have been lost (e.g. a daemon restart).
    await update(stub, "host-a", {
      kind: "host.health",
      captured_at: "2026-08-02T00:05:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 10,
      logical_cpus: 8,
      active_sweep_ids: ["sweep-a-1"],
    });

    snap = await snapshot(stub);
    expect(sweepIds(snap)).toEqual(["sweep-a-1"]);
  });

  it("leaves another host's live sweeps untouched", async () => {
    const stub = fleetStateStub("test-reconcile-cross-host");
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-a-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      started_at: "2026-08-02T00:00:00Z",
    });
    await update(stub, "host-b", {
      kind: "sweep.started",
      sweep_id: "sweep-b-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 2,
      started_at: "2026-08-02T00:00:00Z",
    });

    // host-a reports an EMPTY authoritative set — every one of its own
    // sweeps should be reconciled away, but host-b's own untouched sweep
    // must survive since this record says nothing about host-b.
    await update(stub, "host-a", {
      kind: "host.health",
      captured_at: "2026-08-02T00:05:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 10,
      logical_cpus: 8,
      active_sweep_ids: ["some-other-sweep-not-tracked-here"],
    });

    const snap = await snapshot(stub);
    expect(sweepIds(snap)).toEqual(["sweep-b-1"]);
  });

  it("an empty active_sweep_ids does NOT wipe this host's live sweeps", async () => {
    const stub = fleetStateStub("test-reconcile-empty-set-is-noop");
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-a-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      started_at: "2026-08-02T00:00:00Z",
    });

    // Daemon just restarted — its registry has not finished reconstructing
    // yet, so it (correctly) reports no authoritative set at all.
    await update(stub, "host-a", {
      kind: "host.health",
      captured_at: "2026-08-02T00:05:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 1,
      logical_cpus: 8,
      active_sweep_ids: [],
    });

    const snap = await snapshot(stub);
    expect(sweepIds(snap)).toEqual(["sweep-a-1"]);
  });

  it("a host.health update with no active_sweep_ids field at all does NOT wipe live sweeps", async () => {
    const stub = fleetStateStub("test-reconcile-absent-field-is-noop");
    await update(stub, "host-a", {
      kind: "sweep.started",
      sweep_id: "sweep-a-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      started_at: "2026-08-02T00:00:00Z",
    });

    // A pre-#4955 daemon's host.health record — no active_sweep_ids key.
    await update(stub, "host-a", {
      kind: "host.health",
      captured_at: "2026-08-02T00:05:00Z",
      daemon_version: "0.16.0",
      uptime_sec: 10,
      logical_cpus: 8,
    });

    const snap = await snapshot(stub);
    expect(sweepIds(snap)).toEqual(["sweep-a-1"]);
  });
});

// ---------------------------------------------------------------------------
// `ephemeral_compute` live state (Issue #8305, Phase 2 of #8257): the pure
// leak-detection/pruning decisions, then the launch → completion lifecycle
// driven through the real Durable Object.
// ---------------------------------------------------------------------------

function computeEntry(overrides: Partial<ActiveComputeState> = {}): ActiveComputeState {
  return {
    hostId: "host-abc",
    jobId: "job-abc123",
    instanceId: "i-0123456789abcdef0",
    region: "us-east-1",
    instanceType: "c7i.4xlarge",
    spot: true,
    ami: "ami-0123456789abcdef0",
    startedAt: "2026-09-19T12:00:00Z",
    updatedAt: "2026-09-19T12:00:00Z",
    ...overrides,
  };
}

describe("STALE_COMPUTE_MS", () => {
  it("is a compute-appropriate window, deliberately far wider than the sweep bound", () => {
    // AC (Issue #8305): "do NOT reuse STALE_SWEEP_MS's value without checking
    // it against realistic job durations". A `compute:` entry is refreshed by
    // NOTHING after its launch record (unlike a sweep, whose `sweep.phase`
    // records keep `updatedAt` moving), so the bound has to clear the longest
    // plausible job wall-clock outright.
    expect(STALE_COMPUTE_MS).toBeGreaterThan(STALE_SWEEP_MS);
    expect(STALE_COMPUTE_MS).toBe(24 * 60 * 60 * 1000);
    // A leaked entry stays visible (flagged) for a long while before it is
    // deleted outright — being flagged and being pruned are distinct steps.
    expect(PRUNE_COMPUTE_AFTER_MS).toBeGreaterThan(STALE_COMPUTE_MS);
  });
});

describe("isComputeEntryLeaked", () => {
  const launchedAt = "2026-09-19T12:00:00Z";

  it("is not leaked exactly at the bound", () => {
    const now = Date.parse(launchedAt) + STALE_COMPUTE_MS;
    expect(isComputeEntryLeaked(computeEntry({ updatedAt: launchedAt }), now)).toBe(false);
  });

  it("is leaked one millisecond past the bound", () => {
    const now = Date.parse(launchedAt) + STALE_COMPUTE_MS + 1;
    expect(isComputeEntryLeaked(computeEntry({ updatedAt: launchedAt }), now)).toBe(true);
  });

  it("a job well inside the bound is not leaked, even after many hours", () => {
    const now = Date.parse(launchedAt) + 6 * 60 * 60 * 1000;
    expect(isComputeEntryLeaked(computeEntry({ updatedAt: launchedAt }), now)).toBe(false);
  });

  it("an unparseable updatedAt fails open (never flagged as leaked)", () => {
    const entry = computeEntry({ updatedAt: "not-a-timestamp" });
    expect(isComputeEntryLeaked(entry, Date.now() + 100 * STALE_COMPUTE_MS)).toBe(false);
  });
});

describe("classifyComputeEntries", () => {
  const launchedAt = "2026-09-19T12:00:00Z";

  it("returns a fresh entry unflagged", () => {
    const now = Date.parse(launchedAt) + 60_000;
    const { activeCompute, pruneKeys } = classifyComputeEntries(
      [["compute:job-1", computeEntry({ jobId: "job-1", updatedAt: launchedAt })]],
      now,
    );
    expect(pruneKeys).toEqual([]);
    expect(activeCompute).toHaveLength(1);
    expect(activeCompute[0]?.leaked).toBe(false);
    expect(activeCompute[0]?.jobId).toBe("job-1");
  });

  it("flags an entry past the staleness bound as leaked but still returns it", () => {
    const now = Date.parse(launchedAt) + STALE_COMPUTE_MS + 60_000;
    const { activeCompute, pruneKeys } = classifyComputeEntries(
      [["compute:job-leaked", computeEntry({ jobId: "job-leaked", updatedAt: launchedAt })]],
      now,
    );
    // Leak detection (parent AC 4) is a FLAG, not an immediate deletion — a
    // silently-dropped entry would be indistinguishable from a job that
    // closed normally via its completion record.
    expect(pruneKeys).toEqual([]);
    expect(activeCompute).toHaveLength(1);
    expect(activeCompute[0]?.leaked).toBe(true);
  });

  it("prunes (and stops returning) an entry past the prune bound", () => {
    const now = Date.parse(launchedAt) + PRUNE_COMPUTE_AFTER_MS + 1;
    const { activeCompute, pruneKeys } = classifyComputeEntries(
      [
        ["compute:job-ancient", computeEntry({ jobId: "job-ancient", updatedAt: launchedAt })],
        [
          "compute:job-fresh",
          computeEntry({ jobId: "job-fresh", updatedAt: new Date(now - 60_000).toISOString() }),
        ],
      ],
      now,
    );
    expect(pruneKeys).toEqual(["compute:job-ancient"]);
    expect(activeCompute.map((entry) => entry.jobId)).toEqual(["job-fresh"]);
  });

  it("never prunes an entry with an unparseable updatedAt (fail-safe, matches host pruning)", () => {
    const { activeCompute, pruneKeys } = classifyComputeEntries(
      [["compute:job-bad", computeEntry({ jobId: "job-bad", updatedAt: "not-a-timestamp" })]],
      Date.now(),
    );
    expect(pruneKeys).toEqual([]);
    expect(activeCompute).toHaveLength(1);
  });

  it("an empty entry set yields nothing", () => {
    expect(classifyComputeEntries([], Date.now())).toEqual({ activeCompute: [], pruneKeys: [] });
  });
});

describe("FleetState — ephemeral_compute live state (integration)", () => {
  const LAUNCH: Record<string, unknown> = {
    kind: "ephemeral_compute",
    job_id: "job-abc123",
    instance_id: "i-0123456789abcdef0",
    region: "us-east-1",
    instance_type: "c7i.4xlarge",
    spot: true,
    ami: "ami-0123456789abcdef0",
    started_at: "2026-09-19T12:00:00Z",
  };
  const COMPLETION: Record<string, unknown> = {
    ...LAUNCH,
    ended_at: "2026-09-19T12:45:00Z",
    wall_clock_sec: 2700,
    estimated_cost_usd: 1.23,
  };

  it("a launch record (no ended_at) creates a live compute entry", async () => {
    const stub = fleetStateStub("test-compute-launch");
    await update(stub, "host-abc", LAUNCH);

    const snap = await snapshot(stub);
    expect(snap.activeCompute).toHaveLength(1);
    expect(snap.activeCompute[0]).toMatchObject({
      hostId: "host-abc",
      jobId: "job-abc123",
      instanceId: "i-0123456789abcdef0",
      region: "us-east-1",
      instanceType: "c7i.4xlarge",
      spot: true,
      ami: "ami-0123456789abcdef0",
      startedAt: "2026-09-19T12:00:00Z",
      leaked: false,
    });
  });

  it("the matching completion record (ended_at present) removes the live entry", async () => {
    const stub = fleetStateStub("test-compute-complete");
    await update(stub, "host-abc", LAUNCH);
    expect((await snapshot(stub)).activeCompute).toHaveLength(1);

    await update(stub, "host-abc", COMPLETION);
    expect((await snapshot(stub)).activeCompute).toEqual([]);
  });

  it("a re-sent launch record for the same job_id does not duplicate the entry", async () => {
    const stub = fleetStateStub("test-compute-retry");
    await update(stub, "host-abc", LAUNCH);
    const first = (await snapshot(stub)).activeCompute[0];

    // Emitter retry / at-least-once delivery: same job, second launch record.
    await update(stub, "host-abc", { ...LAUNCH, instance_id: "i-retried0000000000" });
    const after = await snapshot(stub);

    expect(after.activeCompute).toHaveLength(1);
    // First-seen `updatedAt` wins: a re-sending emitter must not be able to
    // postpone leak detection indefinitely by refreshing the entry's clock.
    expect(after.activeCompute[0]?.updatedAt).toBe(first?.updatedAt);
    // …while the rest of the payload is refreshed from the newer record.
    expect(after.activeCompute[0]?.instanceId).toBe("i-retried0000000000");
  });

  it("a completion record with no matching launch is a silent no-op, not an error", async () => {
    const stub = fleetStateStub("test-compute-orphan-completion");
    // Host restarted mid-job: the launch record never reached the DO (D1 still
    // has both records — the DO is live state only, never history).
    await update(stub, "host-abc", COMPLETION);

    expect((await snapshot(stub)).activeCompute).toEqual([]);
  });

  it("two different jobs are tracked as two independent live entries", async () => {
    const stub = fleetStateStub("test-compute-two-jobs");
    await update(stub, "host-abc", LAUNCH);
    await update(stub, "host-def", { ...LAUNCH, job_id: "job-second" });

    const snap = await snapshot(stub);
    expect(snap.activeCompute.map((entry) => entry.jobId).sort()).toEqual(["job-abc123", "job-second"]);

    // Completing one leaves the other running.
    await update(stub, "host-abc", COMPLETION);
    const after = await snapshot(stub);
    expect(after.activeCompute.map((entry) => entry.jobId)).toEqual(["job-second"]);
  });

  it("a record with no usable job_id is ignored rather than stored under a bogus key", async () => {
    const stub = fleetStateStub("test-compute-missing-job-id");
    await update(stub, "host-abc", { kind: "ephemeral_compute", region: "us-east-1" });
    await update(stub, "host-abc", { kind: "ephemeral_compute", job_id: 42 });

    expect((await snapshot(stub)).activeCompute).toEqual([]);
  });

  it("compute entries and sweep entries stay in their own snapshot lists", async () => {
    const stub = fleetStateStub("test-compute-vs-sweeps");
    await update(stub, "host-abc", {
      kind: "sweep.started",
      sweep_id: "sweep-a-1",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 1,
      started_at: "2026-09-19T12:00:00Z",
    });
    await update(stub, "host-abc", LAUNCH);

    const snap = await snapshot(stub);
    expect(sweepIds(snap)).toEqual(["sweep-a-1"]);
    expect(snap.activeCompute.map((entry) => entry.jobId)).toEqual(["job-abc123"]);

    // A host.health reconciliation pass (#4955) is about sweeps only — it must
    // never reconcile away a live compute entry.
    await update(stub, "host-abc", {
      kind: "host.health",
      captured_at: "2026-09-19T12:05:00Z",
      daemon_version: "0.19.0",
      uptime_sec: 10,
      logical_cpus: 8,
      active_sweep_ids: ["sweep-a-1"],
    });
    expect((await snapshot(stub)).activeCompute).toHaveLength(1);
  });
});
