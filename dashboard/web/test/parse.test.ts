import { describe, expect, it } from "vitest";

import { parseFleetSnapshot } from "../src/parse";
import { HEALTHY_HOST_ID, multiHostSnapshot } from "./fixtures";

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
      expect(snapshot).toEqual({ hosts: {}, activeSweeps: [] });
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
      activeSweeps: [{ hostId: "h", sweepId: "s", runtime: "codex" }],
    });
    expect(snapshot.hosts.h?.health?.record.daemon_version).toBe("99.0.0");
    expect(snapshot.hosts.h?.health?.record).not.toHaveProperty("gpu_count");
    expect(snapshot.activeSweeps[0]).not.toHaveProperty("runtime");
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
});
