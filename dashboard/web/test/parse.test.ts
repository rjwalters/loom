import { describe, expect, it } from "vitest";

import {
  parseFleetSnapshot,
  parseHostProtection,
  parseMissingHost,
  parseRoleTickHealth,
} from "../src/parse";
import {
  HEALTHY_HOST_ID,
  MISSING_HOST_ID,
  UNPROVISIONED_HOST_ID,
  multiHostSnapshot,
  persistentRoleTickFailureFixture,
  rosterMissingSnapshot,
  unprotectedHostProtectionFixture,
} from "./fixtures";

describe("parseFleetSnapshot", () => {
  it("narrows a well-formed multi-host snapshot", () => {
    const snapshot = parseFleetSnapshot(multiHostSnapshot());
    expect(Object.keys(snapshot.hosts)).toHaveLength(5);
    expect(snapshot.activeSweeps).toHaveLength(3);
    expect(snapshot.hosts[HEALTHY_HOST_ID]?.health?.record.cpu_idle_fraction).toBe(0.83);
    expect(snapshot.hosts[HEALTHY_HOST_ID]?.tokens?.record.accounts).toHaveLength(2);
  });

  it("degrades a non-object body to an empty snapshot instead of throwing", () => {
    for (const body of [null, undefined, 42, "nope", [1, 2, 3]]) {
      const snapshot = parseFleetSnapshot(body);
      expect(snapshot).toEqual({ hosts: {}, activeSweeps: [], activeCompute: [] });
    }
  });

  it("degrades wrong-typed sub-trees independently", () => {
    const snapshot = parseFleetSnapshot({ hosts: "not-an-object", activeSweeps: { nope: true } });
    expect(snapshot.hosts).toEqual({});
    expect(snapshot.activeSweeps).toEqual([]);
  });

  it("drops an absent measurement rather than coercing it to zero", () => {
    const snapshot = parseFleetSnapshot({
      hosts: { h: { health: { record: { kind: "host.health" }, updatedAt: "2026-07-30T12:00:00Z" } } },
      activeSweeps: [],
    });
    const health = snapshot.hosts.h?.health?.record ?? {};
    expect("cpu_idle_fraction" in health).toBe(false);
    expect(health.cpu_idle_fraction).toBeUndefined();
  });

  it("drops a wrong-typed measurement rather than rendering it", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { cpu_idle_fraction: "0.5", uptime_sec: null, logical_cpus: Number.NaN },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const health = snapshot.hosts.h?.health?.record ?? {};
    expect(health.cpu_idle_fraction).toBeUndefined();
    expect(health.uptime_sec).toBeUndefined();
    expect(health.logical_cpus).toBeUndefined();
  });

  it("preserves a genuine zero", () => {
    const snapshot = parseFleetSnapshot({
      hosts: { h: { health: { record: { cpu_idle_fraction: 0 }, updatedAt: "x" } } },
      activeSweeps: [],
    });
    expect(snapshot.hosts.h?.health?.record.cpu_idle_fraction).toBe(0);
  });

  it("narrows dispatch-attention state (#4975)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { kind: "host.health", dispatch_halted: true, halt_reason: "host-distress breaker" },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const health = snapshot.hosts.h?.health?.record ?? {};
    expect(health.dispatch_halted).toBe(true);
    expect(health.halt_reason).toBe("host-distress breaker");
  });

  it("narrows worktree_root_total_gb alongside worktree_root_free_gb (#5356)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { kind: "host.health", worktree_root_free_gb: 200, worktree_root_total_gb: 1000 },
            updatedAt: "2026-08-04T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const health = snapshot.hosts.h?.health?.record ?? {};
    expect(health.worktree_root_free_gb).toBe(200);
    expect(health.worktree_root_total_gb).toBe(1000);
  });

  it("drops worktree_root_total_gb when absent — a free-but-no-total record must not fabricate one (#5356)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { kind: "host.health", worktree_root_free_gb: 200 },
            updatedAt: "2026-08-04T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const health = snapshot.hosts.h?.health?.record ?? {};
    expect(health.worktree_root_free_gb).toBe(200);
    expect("worktree_root_total_gb" in health).toBe(false);
    expect(health.worktree_root_total_gb).toBeUndefined();
  });

  it("drops dispatch_halted/halt_reason absent from a pre-#4975 daemon's record, never coercing to false", () => {
    const snapshot = parseFleetSnapshot({
      hosts: { h: { health: { record: { kind: "host.health" }, updatedAt: "2026-07-30T12:00:00Z" } } },
      activeSweeps: [],
    });
    const health = snapshot.hosts.h?.health?.record ?? {};
    expect("dispatch_halted" in health).toBe(false);
    expect("halt_reason" in health).toBe(false);
  });

  it("defaults any visibility that is not exactly \"public\" to private", () => {
    const base = { hostId: "h", sweepId: "s", startedAt: "2026-07-30T12:00:00Z" };
    for (const visibility of [undefined, null, "private", "internal", "PUBLIC", 1, {}, ["public"]]) {
      const snapshot = parseFleetSnapshot({ hosts: {}, activeSweeps: [{ ...base, visibility }] });
      expect(snapshot.activeSweeps[0]?.visibility).toBe("private");
    }
    const snapshot = parseFleetSnapshot({ hosts: {}, activeSweeps: [{ ...base, visibility: "public" }] });
    expect(snapshot.activeSweeps[0]?.visibility).toBe("public");
  });

  it("drops an unaddressable sweep (no hostId or no sweepId)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {},
      activeSweeps: [{ sweepId: "s" }, { hostId: "h" }, {}, null, "x", { hostId: "h", sweepId: "s" }],
    });
    expect(snapshot.activeSweeps).toHaveLength(1);
    expect(snapshot.activeSweeps[0]?.sweepId).toBe("s");
  });

  it("ignores unknown additive fields from a newer daemon", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { daemon_version: "99.0.0", gpu_count: 4, some_future_thing: { a: 1 } },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [{ hostId: "h", sweepId: "s", runtime: "codex", sandbox: "firecracker" }],
    });
    expect(snapshot.hosts.h?.health?.record.daemon_version).toBe("99.0.0");
    expect(snapshot.hosts.h?.health?.record).not.toHaveProperty("gpu_count");
    // `runtime` is a known field now; `sandbox` stands in as the unknown one.
    expect(snapshot.activeSweeps[0]?.runtime).toBe("codex");
    expect(snapshot.activeSweeps[0]).not.toHaveProperty("sandbox");
  });

  it("narrows host.health build identity, dropping wrong-typed values (#4956)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        good: {
          health: {
            record: { daemon_version: "0.17.0", build_commit: "8c16fb5b", built_at: "2026-08-02T03:09:51Z" },
            updatedAt: "2026-08-02T12:00:00Z",
          },
        },
        bad: {
          health: {
            record: { daemon_version: "0.17.0", build_commit: 12345, built_at: "" },
            updatedAt: "2026-08-02T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    expect(snapshot.hosts.good?.health?.record.build_commit).toBe("8c16fb5b");
    expect(snapshot.hosts.good?.health?.record.built_at).toBe("2026-08-02T03:09:51Z");
    // A wrong-typed / empty value is dropped, not coerced to a rendered "12345".
    expect(snapshot.hosts.bad?.health?.record).not.toHaveProperty("build_commit");
    expect(snapshot.hosts.bad?.health?.record).not.toHaveProperty("built_at");
  });

  it("narrows host.health's managed_repos roster (#4976)", () => {
    const snapshot = parseFleetSnapshot(multiHostSnapshot());
    const repos = snapshot.hosts[HEALTHY_HOST_ID]?.health?.record.managed_repos;
    expect(repos).toEqual([
      { slug: "rjwalters/loom", visibility: "public" },
      { slug: "2AMLogic/gf180-pll", visibility: "private" },
      { slug: "2AMLogic/gf180-trng", visibility: "private" },
    ]);
  });

  it("narrows a redacted managed_repos entry (slug stripped, visibility kept)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { managed_repos: [{ visibility: "private" }, { slug: "owner/repo", visibility: "public" }] },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const repos = snapshot.hosts.h?.health?.record.managed_repos ?? [];
    expect(repos).toEqual([{ visibility: "private" }, { slug: "owner/repo", visibility: "public" }]);
    expect("slug" in repos[0]!).toBe(false);
  });

  it("degrades a wrong-typed managed_repos to absent rather than throwing", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { managed_repos: "not-an-array" },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    expect(snapshot.hosts.h?.health?.record.managed_repos).toBeUndefined();
  });

  it("drops a malformed managed_repos row rather than the whole roster", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: {
              managed_repos: [null, "nope", { slug: 42 }, { slug: "owner/repo", visibility: "public" }],
            },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    // `null`/`"nope"` are dropped entirely (not objects); `{ slug: 42 }` keeps
    // its place as a slugless (private-by-default) row rather than vanishing.
    expect(snapshot.hosts.h?.health?.record.managed_repos).toEqual([
      { visibility: "private" },
      { slug: "owner/repo", visibility: "public" },
    ]);
  });

  // -------------------------------------------------------------------------
  // host.health.roles — role-tick health (#5022)
  // -------------------------------------------------------------------------

  it("narrows host.health's roles summary, including a persistent failure", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { kind: "host.health", roles: persistentRoleTickFailureFixture() },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const roles = snapshot.hosts.h?.health?.record.roles;
    expect(roles?.total).toBe(5);
    expect(roles?.ok).toBe(1);
    expect(roles?.persistent).toEqual([
      {
        root: "/repos/loom",
        role: "judge",
        failures: 4,
        last_at: "2026-07-30T12:09:00.000Z",
        detail: "no-token-pool",
      },
    ]);
  });

  it("preserves a genuine total: 0 for roles (role runner idle/disabled), never coercing it to unknown", () => {
    const parsed = parseRoleTickHealth({ total: 0, ok: 0, persistent: [] });
    expect(parsed?.total).toBe(0);
    expect("total" in (parsed ?? {})).toBe(true);
  });

  it("drops roles entirely when absent — distinct from a genuine zero (a pre-#5022 daemon)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: { h: { health: { record: { kind: "host.health" }, updatedAt: "2026-07-30T12:00:00Z" } } },
      activeSweeps: [],
    });
    expect("roles" in (snapshot.hosts.h?.health?.record ?? {})).toBe(false);
  });

  it("drops a malformed roles.persistent entry rather than the whole roster", () => {
    const parsed = parseRoleTickHealth({
      total: 2,
      ok: 1,
      persistent: [null, "nope", { role: "judge" }],
    });
    // `{ role: "judge" }` (no root/failures/last_at) keeps its place with
    // just the field it reported; `null`/`"nope"` are dropped entirely.
    expect(parsed?.persistent).toEqual([{ role: "judge" }]);
  });

  it("degrades a wrong-typed roles value to absent rather than throwing", () => {
    expect(parseRoleTickHealth("not-an-object")).toBeUndefined();
    expect(parseRoleTickHealth(42)).toBeUndefined();
    expect(parseRoleTickHealth(null)).toBeUndefined();
  });

  // -------------------------------------------------------------------------
  // host.health.protection — watchdog/crash-protection state (#5352)
  // -------------------------------------------------------------------------

  it("narrows host.health's protection summary for the protected case", () => {
    const snapshot = parseFleetSnapshot(multiHostSnapshot());
    const protection = snapshot.hosts[HEALTHY_HOST_ID]?.health?.record.protection;
    expect(protection?.state).toBe("protected");
    expect(protection?.watchdog_provisioned).toBe(true);
  });

  it("narrows host.health's protection summary for the unprotected case", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        h: {
          health: {
            record: { kind: "host.health", protection: unprotectedHostProtectionFixture() },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    });
    const protection = snapshot.hosts.h?.health?.record.protection;
    expect(protection?.state).toBe("watchdog-not-provisioned");
    expect(protection?.watchdog_provisioned).toBe(false);
  });

  it("drops watchdog_provisioned when the probe could not answer (state: unknown)", () => {
    const parsed = parseHostProtection({ state: "unknown" });
    expect(parsed?.state).toBe("unknown");
    expect("watchdog_provisioned" in (parsed ?? {})).toBe(false);
  });

  it("drops protection entirely when absent — distinct from a genuine 'unknown' verdict (a pre-#5352 daemon)", () => {
    const snapshot = parseFleetSnapshot({
      hosts: { h: { health: { record: { kind: "host.health" }, updatedAt: "2026-07-30T12:00:00Z" } } },
      activeSweeps: [],
    });
    expect("protection" in (snapshot.hosts.h?.health?.record ?? {})).toBe(false);
  });

  it("degrades a wrong-typed protection value to absent rather than throwing", () => {
    expect(parseHostProtection("not-an-object")).toBeUndefined();
    expect(parseHostProtection(42)).toBeUndefined();
    expect(parseHostProtection(null)).toBeUndefined();
  });
});


it("keeps unknown launch identity strings and rejects malformed or blank identity", () => {
  const snapshot = parseFleetSnapshot({ hosts: {}, activeSweeps: [
    { hostId: "host-a", sweepId: "future", runtime: "next-runtime", provider: "next-provider", model: "next-model" },
    { hostId: "host-a", sweepId: "legacy", runtime: " ", provider: {}, model: 9 },
  ] });
  expect(snapshot.activeSweeps[0]).toMatchObject({ runtime: "next-runtime", provider: "next-provider", model: "next-model" });
  for (const key of ["runtime", "provider", "model"]) expect(snapshot.activeSweeps[1]).not.toHaveProperty(key);
});

// ---------------------------------------------------------------------------
// Expected-host roster (#8792 backend → #8804 SPA)
// ---------------------------------------------------------------------------

describe("parseFleetSnapshot — missingHosts (#8804)", () => {
  it("narrows a well-formed missingHosts array", () => {
    const snapshot = parseFleetSnapshot(rosterMissingSnapshot());
    expect(snapshot.missingHosts).toEqual([
      { hostId: MISSING_HOST_ID, state: "missing" },
      { hostId: UNPROVISIONED_HOST_ID, state: "unprovisioned" },
    ]);
  });

  it("leaves the key off entirely when the backend sends no missingHosts", () => {
    // A pre-#8792 backend, or a deployment with no EXPECTED_HOSTS roster.
    // Absent, not `[]`: "no roster configured" and "roster configured, every
    // host reporting" must stay distinguishable.
    const snapshot = parseFleetSnapshot(multiHostSnapshot());
    expect("missingHosts" in snapshot).toBe(false);
    expect(snapshot.missingHosts).toBeUndefined();
  });

  it("degrades a wrong-typed missingHosts sub-tree to absent instead of throwing", () => {
    for (const missingHosts of [null, 42, "nope", { hostId: "h" }]) {
      const snapshot = parseFleetSnapshot({ hosts: {}, activeSweeps: [], missingHosts });
      expect(snapshot.missingHosts).toBeUndefined();
      // One bad sub-tree must not blank the rest of the snapshot.
      expect(snapshot.hosts).toEqual({});
      expect(snapshot.activeSweeps).toEqual([]);
    }
  });

  it("keeps an empty missingHosts array the backend actually sent", () => {
    const snapshot = parseFleetSnapshot({ hosts: {}, activeSweeps: [], missingHosts: [] });
    expect(snapshot.missingHosts).toEqual([]);
  });

  it("drops malformed entries but keeps the well-formed ones around them", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {},
      activeSweeps: [],
      missingHosts: [
        { hostId: "good-1", state: "missing" },
        // Unaddressable: no id to key or link a card by.
        { state: "missing" },
        { hostId: "", state: "unprovisioned" },
        { hostId: 42, state: "missing" },
        // Unknown/wrong-typed state: the two known states prescribe opposite
        // operator actions, so guessing one would be worse than dropping.
        { hostId: "bad-state", state: "decommissioned" },
        { hostId: "bad-state-2", state: 7 },
        { hostId: "bad-state-3" },
        "not-an-object",
        null,
        { hostId: "good-2", state: "unprovisioned" },
      ],
    });
    expect(snapshot.missingHosts).toEqual([
      { hostId: "good-1", state: "missing" },
      { hostId: "good-2", state: "unprovisioned" },
    ]);
  });

  it("narrows one entry at a time through parseMissingHost", () => {
    expect(parseMissingHost({ hostId: "h", state: "missing" })).toEqual({ hostId: "h", state: "missing" });
    expect(parseMissingHost({ hostId: "h", state: "unprovisioned" })).toEqual({
      hostId: "h",
      state: "unprovisioned",
    });
    for (const bad of [undefined, null, 42, "h", [], { hostId: "h" }, { state: "missing" }]) {
      expect(parseMissingHost(bad)).toBeUndefined();
    }
  });

  it("ignores additive keys on a missingHosts entry rather than failing", () => {
    // Same "additive fields are tolerated" rule the rest of this module
    // follows: an unrecognized *key* is ignored, unlike an unrecognized
    // `state` *value*, which changes what the entry means.
    const snapshot = parseFleetSnapshot({
      hosts: {},
      activeSweeps: [],
      missingHosts: [{ hostId: "h", state: "missing", expectedSince: "2026-09-01T00:00:00Z" }],
    });
    expect(snapshot.missingHosts).toEqual([{ hostId: "h", state: "missing" }]);
  });
});
