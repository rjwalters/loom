import { beforeEach, describe, expect, it } from "vitest";

import {
  parsePoolSample,
  parsePoolSamples,
  parseSweepWindows,
  parseTimestamp,
  parseTokenSample,
  parseTokenSamples,
} from "../src/analytics/parse.js";
import type { HistoryEnvelope } from "../src/analytics/types.js";
import {
  HOUR,
  MINUTE,
  T0,
  at,
  newestFirst,
  poolTokensSnapshot,
  resetIds,
  sweepRecords,
  tokensSnapshot,
} from "./analyticsFixtures.js";

beforeEach(resetIds);

describe("parseTimestamp", () => {
  it("parses RFC 3339 to epoch ms", () => {
    expect(parseTimestamp("2026-07-30T12:00:00Z")).toBe(T0);
  });

  it("returns undefined for absent, empty, or unparseable values", () => {
    expect(parseTimestamp(undefined)).toBeUndefined();
    expect(parseTimestamp("")).toBeUndefined();
    expect(parseTimestamp("not a date")).toBeUndefined();
    expect(parseTimestamp(1234)).toBeUndefined();
  });
});

describe("parseTokenSample", () => {
  it("narrows a documented tokens.snapshot payload", () => {
    const sample = parseTokenSample(
      tokensSnapshot(T0, [
        { account: "agent-1", rank: 0, usage: 0.42, resetAt: T0 + 6 * HOUR },
        { account: "agent-2", exhausted: true },
      ]),
    );

    expect(sample?.at).toBe(T0);
    expect(sample?.hostId).toBe("host-a");
    expect(sample?.accounts).toEqual([
      { account: "agent-1", rank: 0, usageFraction: 0.42, limitWindowResetAt: T0 + 6 * HOUR, exhausted: false },
      { account: "agent-2", rank: undefined, usageFraction: undefined, limitWindowResetAt: undefined, exhausted: true },
    ]);
  });

  it("keeps an unmeasured usage_fraction absent rather than zero", () => {
    const sample = parseTokenSample(tokensSnapshot(T0, [{ account: "agent-2", exhausted: true }]));
    expect(sample?.accounts[0]?.usageFraction).toBeUndefined();
    expect(sample?.accounts[0]?.usageFraction).not.toBe(0);
  });

  it("ignores non-tokens.snapshot kinds", () => {
    expect(
      parseTokenSample(at(sweepRecords({ sweepId: "s1", repo: "o/r", startedAt: T0 }), 0)),
    ).toBeUndefined();
  });

  it("falls back to emittedAt when captured_at is missing", () => {
    const envelope = tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]);
    delete (envelope.record as Record<string, unknown>).captured_at;
    expect(parseTokenSample(envelope)?.at).toBe(T0);
  });

  it("drops accounts with no name and survives a malformed accounts array", () => {
    const envelope: HistoryEnvelope = {
      id: 1,
      emittedAt: new Date(T0).toISOString(),
      hostId: "host-a",
      kind: "tokens.snapshot",
      record: { kind: "tokens.snapshot", captured_at: new Date(T0).toISOString(), accounts: ["nope", { rank: 1 }, null] },
    };
    expect(parseTokenSample(envelope)?.accounts).toEqual([]);
  });
});

describe("parseTokenSamples", () => {
  it("returns chronological order even from the API's newest-first pages", () => {
    const records = newestFirst([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      tokensSnapshot(T0 + 2 * MINUTE, [{ account: "agent-1", usage: 0.3 }]),
    ]);
    expect(parseTokenSamples(records).map((sample) => sample.at)).toEqual([T0, T0 + MINUTE, T0 + 2 * MINUTE]);
  });
});

describe("parsePoolSample", () => {
  it("narrows a documented /public/history aggregate payload", () => {
    const sample = parsePoolSample(
      poolTokensSnapshot(T0, { accountCount: 13, exhaustedCount: 5, meanUsage: 0.3246, maxUsage: 0.91, nextResetAt: T0 + 6 * HOUR }),
    );

    expect(sample).toEqual({
      hostId: "host-a",
      at: T0,
      accountCount: 13,
      exhaustedCount: 5,
      meanUsageFraction: 0.3246,
      maxUsageFraction: 0.91,
      nextLimitWindowResetAt: T0 + 6 * HOUR,
    });
  });

  it("keeps null mean/max/reset absent rather than zero", () => {
    const sample = parsePoolSample(poolTokensSnapshot(T0, { accountCount: 3 }));
    expect(sample?.meanUsageFraction).toBeUndefined();
    expect(sample?.maxUsageFraction).toBeUndefined();
    expect(sample?.nextLimitWindowResetAt).toBeUndefined();
    expect(sample?.exhaustedCount).toBe(0);
  });

  it("ignores the per-account shape — it is not the aggregate one", () => {
    const envelope = tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]);
    expect(parsePoolSample(envelope)).toBeUndefined();
  });

  it("ignores non-tokens.snapshot kinds", () => {
    expect(parsePoolSample(at(sweepRecords({ sweepId: "s1", repo: "o/r", startedAt: T0 }), 0))).toBeUndefined();
  });

  it("falls back to emittedAt when captured_at is missing", () => {
    const envelope = poolTokensSnapshot(T0, { accountCount: 1 });
    delete (envelope.record as Record<string, unknown>).captured_at;
    expect(parsePoolSample(envelope)?.at).toBe(T0);
  });

  it("returns undefined for a malformed account_count", () => {
    const envelope = poolTokensSnapshot(T0, { accountCount: 1 });
    (envelope.record as Record<string, unknown>).account_count = "nope";
    expect(parsePoolSample(envelope)).toBeUndefined();
  });

  it("treats account_count: 0 as a valid, empty aggregate — not malformed", () => {
    const sample = parsePoolSample(poolTokensSnapshot(T0, { accountCount: 0 }));
    expect(sample?.accountCount).toBe(0);
    expect(sample?.exhaustedCount).toBe(0);
  });
});

describe("parsePoolSamples", () => {
  it("returns chronological order even from the API's newest-first pages", () => {
    const records = newestFirst([
      poolTokensSnapshot(T0, { accountCount: 2, meanUsage: 0.1 }),
      poolTokensSnapshot(T0 + MINUTE, { accountCount: 2, meanUsage: 0.2 }),
      poolTokensSnapshot(T0 + 2 * MINUTE, { accountCount: 2, meanUsage: 0.3 }),
    ]);
    expect(parsePoolSamples(records).map((sample) => sample.at)).toEqual([T0, T0 + MINUTE, T0 + 2 * MINUTE]);
  });

  it("does not pick up per-account-shaped records from a mixed page", () => {
    const records = [tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]), poolTokensSnapshot(T0 + MINUTE, { accountCount: 4 })];
    expect(parsePoolSamples(records)).toHaveLength(1);
  });
});

describe("parseSweepWindows", () => {
  it("pairs sweep.started with sweep.completed into one window", () => {
    const windows = parseSweepWindows(
      newestFirst([
        ...sweepRecords({
          sweepId: "sweep-issue-1-0",
          repo: "rjwalters/loom",
          startedAt: T0,
          completedAt: T0 + 30 * MINUTE,
          model: "opus",
          issue: 1,
        }),
      ]),
    );

    expect(windows).toHaveLength(1);
    expect(windows[0]).toMatchObject({
      hostId: "host-a",
      sweepId: "sweep-issue-1-0",
      repo: "rjwalters/loom",
      model: "opus",
      issue: 1,
      startedAt: T0,
      endedAt: T0 + 30 * MINUTE,
    });
  });

  it("leaves an in-flight sweep's window open", () => {
    const windows = parseSweepWindows(sweepRecords({ sweepId: "s1", repo: "o/r", startedAt: T0 }));
    expect(windows[0]?.endedAt).toBeUndefined();
  });

  it("keeps same-id sweeps on different hosts separate", () => {
    const windows = parseSweepWindows([
      ...sweepRecords({ sweepId: "sweep-issue-1-0", repo: "o/a", startedAt: T0, hostId: "host-a" }),
      ...sweepRecords({ sweepId: "sweep-issue-1-0", repo: "o/b", startedAt: T0, hostId: "host-b" }),
    ]);
    expect(windows).toHaveLength(2);
    expect(windows.map((window) => window.hostId).sort()).toEqual(["host-a", "host-b"]);
  });

  it("skips a redacted record whose repo was nulled", () => {
    const started = at(sweepRecords({ sweepId: "s1", repo: "o/r", startedAt: T0 }), 0);
    started.repo = null;
    delete (started.record as Record<string, unknown>).repo;
    expect(parseSweepWindows([started])).toEqual([]);
  });

  it("ignores a terminal record with no matching start", () => {
    const records = sweepRecords({ sweepId: "s1", repo: "o/r", startedAt: T0, completedAt: T0 + MINUTE });
    expect(parseSweepWindows([at(records, 1)])).toEqual([]);
  });
});
