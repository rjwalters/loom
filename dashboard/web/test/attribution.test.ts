import { beforeEach, describe, expect, it } from "vitest";

import { attributeUsageToRepos } from "../src/analytics/attribution.js";
import { parseSweepWindows, parseTokenSamples } from "../src/analytics/parse.js";
import type { HistoryEnvelope } from "../src/analytics/types.js";
import { HOUR, MINUTE, T0, at, newestFirst, resetIds, sweepRecords, tokensSnapshot } from "./analyticsFixtures.js";

beforeEach(resetIds);

/** Run the full join over a mixed history page, as the view does. */
function attribute(records: readonly HistoryEnvelope[], options: Parameters<typeof attributeUsageToRepos>[2] = {}) {
  const page = newestFirst(records);
  return attributeUsageToRepos(parseTokenSamples(page), parseSweepWindows(page), { now: T0 + 24 * HOUR, ...options });
}

describe("attributeUsageToRepos — the join", () => {
  it("attributes an interval's delta to the one sweep covering it", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      ...sweepRecords({
        sweepId: "sweep-issue-1-0",
        repo: "rjwalters/loom",
        startedAt: T0 - 5 * MINUTE,
        completedAt: T0 + 15 * MINUTE,
        model: "opus",
      }),
    ]);

    expect(result.repos).toHaveLength(1);
    expect(result.repos[0]?.repo).toBe("rjwalters/loom");
    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.repos[0]?.share).toBeCloseTo(1, 10);
    expect(result.repos[0]?.sweepCount).toBe(1);
    expect(result.repos[0]?.byModel).toEqual([{ name: "opus", usage: expect.closeTo(0.1, 10) }]);
    expect(result.repos[0]?.byAccount).toEqual([{ name: "agent-1", usage: expect.closeTo(0.1, 10) }]);
    expect(result.unattributedUsage).toBe(0);
    expect(result.attributedIntervals).toBe(1);
  });

  it("splits an interval evenly between two fully-overlapping sweeps", () => {
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
        ...sweepRecords({
          sweepId: "s-alpha",
          repo: "org/alpha",
          startedAt: T0 - MINUTE,
          completedAt: T0 + 11 * MINUTE,
        }),
        ...sweepRecords({
          sweepId: "s-beta",
          repo: "org/beta",
          startedAt: T0 - MINUTE,
          completedAt: T0 + 11 * MINUTE,
        }),
      ],
      { edgeToleranceMs: 0 },
    );

    expect(result.repos.map((repo) => repo.repo).sort()).toEqual(["org/alpha", "org/beta"]);
    for (const repo of result.repos) expect(repo.usage).toBeCloseTo(0.05, 10);
    expect(result.totalUsage).toBeCloseTo(0.1, 10);
  });

  it("splits in proportion to overlap duration for partially-overlapping sweeps", () => {
    // Interval is 10 minutes. `long` covers all 10; `short` covers the last 5.
    // Overlap-weighted split is therefore 10:5 → two thirds / one third.
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.3 }]),
        ...sweepRecords({
          sweepId: "s-long",
          repo: "org/long",
          startedAt: T0 - HOUR,
          completedAt: T0 + HOUR,
        }),
        ...sweepRecords({
          sweepId: "s-short",
          repo: "org/short",
          startedAt: T0 + 5 * MINUTE,
          completedAt: T0 + 10 * MINUTE,
        }),
      ],
      { edgeToleranceMs: 0 },
    );

    const byRepo = new Map(result.repos.map((repo) => [repo.repo, repo.usage]));
    expect(byRepo.get("org/long")).toBeCloseTo(0.2, 10);
    expect(byRepo.get("org/short")).toBeCloseTo(0.1, 10);
    expect(result.unattributedUsage).toBe(0);
  });

  it("attributes only the covered share of a partially-covered interval", () => {
    // The sweep runs for 2 of the interval's 10 minutes. Usage is assumed
    // uniform, so it claims a fifth — not all of it merely for being the only
    // sweep in sight.
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.5 }]),
        ...sweepRecords({
          sweepId: "s-brief",
          repo: "org/brief",
          startedAt: T0,
          completedAt: T0 + 2 * MINUTE,
        }),
      ],
      { edgeToleranceMs: 0 },
    );

    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.unattributedUsage).toBeCloseTo(0.4, 10);
    expect(result.totalUsage).toBeCloseTo(0.5, 10);
  });

  it("counts overlapping sweeps' shared seconds once when measuring coverage", () => {
    // Two sweeps each cover the same half of the interval. Coverage is 50%,
    // not 100% — the union, not the sum — and the covered half is then split
    // evenly between them.
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.4 }]),
        ...sweepRecords({ sweepId: "s-1", repo: "org/one", startedAt: T0, completedAt: T0 + 5 * MINUTE }),
        ...sweepRecords({ sweepId: "s-2", repo: "org/two", startedAt: T0, completedAt: T0 + 5 * MINUTE }),
      ],
      { edgeToleranceMs: 0 },
    );

    for (const repo of result.repos) expect(repo.usage).toBeCloseTo(0.1, 10);
    expect(result.unattributedUsage).toBeCloseTo(0.2, 10);
  });

  it("reports usage with no overlapping sweep as unattributed, not redistributed", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      // A sweep that ran well after the observed interval — a cron or manual
      // session burned the tokens in between.
      ...sweepRecords({
        sweepId: "s-later",
        repo: "org/later",
        startedAt: T0 + 30 * MINUTE,
        completedAt: T0 + 40 * MINUTE,
      }),
    ]);

    expect(result.repos).toEqual([]);
    expect(result.unattributedUsage).toBeCloseTo(0.1, 10);
    expect(result.totalUsage).toBeCloseTo(0.1, 10);
    expect(result.attributedIntervals).toBe(0);
  });

  it("mixes attributed and unattributed intervals in one run", () => {
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
        tokensSnapshot(T0 + 20 * MINUTE, [{ account: "agent-1", usage: 0.5 }]),
        ...sweepRecords({
          sweepId: "s-first",
          repo: "org/first",
          startedAt: T0,
          completedAt: T0 + 10 * MINUTE,
        }),
      ],
      { edgeToleranceMs: 0 },
    );

    expect(result.repos).toHaveLength(1);
    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.repos[0]?.share).toBeCloseTo(0.25, 10);
    expect(result.unattributedUsage).toBeCloseTo(0.3, 10);
  });

  it("never attributes across hosts", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }], "host-a"),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }], "host-a"),
      ...sweepRecords({
        sweepId: "s-elsewhere",
        repo: "org/elsewhere",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 11 * MINUTE,
        hostId: "host-b",
      }),
    ]);

    expect(result.repos).toEqual([]);
    expect(result.unattributedUsage).toBeCloseTo(0.1, 10);
  });

  it("keeps per-host usage on its own host when both are busy", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }], "host-a"),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }], "host-a"),
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.4 }], "host-b"),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.7 }], "host-b"),
      ...sweepRecords({
        sweepId: "s-a",
        repo: "org/on-a",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 11 * MINUTE,
        hostId: "host-a",
      }),
      ...sweepRecords({
        sweepId: "s-b",
        repo: "org/on-b",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 11 * MINUTE,
        hostId: "host-b",
      }),
    ]);

    const byRepo = new Map(result.repos.map((repo) => [repo.repo, repo.usage]));
    expect(byRepo.get("org/on-a")).toBeCloseTo(0.1, 10);
    expect(byRepo.get("org/on-b")).toBeCloseTo(0.3, 10);
  });
});

describe("attributeUsageToRepos — tolerances", () => {
  it("claims an interval a sweep starts just after, within the edge tolerance", () => {
    const records = [
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      // Starts 30s after the interval closes — clock skew between the pool
      // sampler and the sweep, not a genuinely later sweep.
      ...sweepRecords({
        sweepId: "s-skewed",
        repo: "org/skewed",
        startedAt: T0 + 10 * MINUTE + 30 * 1000,
        completedAt: T0 + 20 * MINUTE,
      }),
    ];

    expect(attribute(records).repos.map((repo) => repo.repo)).toEqual(["org/skewed"]);
    expect(attribute(records, { edgeToleranceMs: 0 }).repos).toEqual([]);
  });

  it("drops an interval that straddles a telemetry gap", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 5 * HOUR, [{ account: "agent-1", usage: 0.6 }]),
      ...sweepRecords({
        sweepId: "s-long",
        repo: "org/long",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 6 * HOUR,
      }),
    ]);

    expect(result.droppedIntervals).toBe(1);
    expect(result.repos).toEqual([]);
    expect(result.totalUsage).toBe(0);
  });

  it("attributes across a wide gap when the caller widens the tolerance", () => {
    const records = [
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 5 * HOUR, [{ account: "agent-1", usage: 0.6 }]),
      ...sweepRecords({
        sweepId: "s-long",
        repo: "org/long",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 6 * HOUR,
      }),
    ];

    const result = attribute(records, { maxSampleGapMs: 6 * HOUR });
    expect(result.droppedIntervals).toBe(0);
    expect(result.repos[0]?.usage).toBeCloseTo(0.5, 10);
  });

  it("caps an in-flight sweep so it cannot absorb the whole day", () => {
    const records = [
      // Inside the 6h cap.
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      // Well past it — the sweep never emitted a terminal record.
      tokensSnapshot(T0 + 8 * HOUR, [{ account: "agent-1", usage: 0.3 }]),
      tokensSnapshot(T0 + 8 * HOUR + 10 * MINUTE, [{ account: "agent-1", usage: 0.45 }]),
      ...sweepRecords({ sweepId: "s-stuck", repo: "org/stuck", startedAt: T0 - MINUTE }),
    ];

    const result = attribute(records);
    expect(result.repos[0]?.repo).toBe("org/stuck");
    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.unattributedUsage).toBeCloseTo(0.15, 10);
  });

  it("lets a still-open sweep claim right up to now", () => {
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
        ...sweepRecords({ sweepId: "s-running", repo: "org/running", startedAt: T0 - MINUTE }),
      ],
      { now: T0 + 20 * MINUTE },
    );

    expect(result.repos[0]?.repo).toBe("org/running");
    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
  });
});

describe("attributeUsageToRepos — usage semantics", () => {
  it("treats a usage drop as a limit-window rollover, never as negative usage", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.9 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.05 }]),
      tokensSnapshot(T0 + 20 * MINUTE, [{ account: "agent-1", usage: 0.15 }]),
      ...sweepRecords({
        sweepId: "s-1",
        repo: "org/one",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 30 * MINUTE,
      }),
    ]);

    expect(result.rolloverIntervals).toBe(1);
    // Only the post-rollover climb (0.05 → 0.15) is attributable.
    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.totalUsage).toBeCloseTo(0.1, 10);
  });

  it("sums several accounts' deltas and breaks them down exactly", () => {
    const result = attribute([
      tokensSnapshot(T0, [
        { account: "agent-1", usage: 0.1 },
        { account: "agent-2", usage: 0.5 },
      ]),
      tokensSnapshot(T0 + 10 * MINUTE, [
        { account: "agent-1", usage: 0.2 },
        { account: "agent-2", usage: 0.55 },
      ]),
      ...sweepRecords({
        sweepId: "s-1",
        repo: "org/one",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 11 * MINUTE,
      }),
    ]);

    expect(result.repos[0]?.usage).toBeCloseTo(0.15, 10);
    expect(result.repos[0]?.byAccount.map((entry) => entry.name)).toEqual(["agent-1", "agent-2"]);
    expect(result.repos[0]?.byAccount[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.repos[0]?.byAccount[1]?.usage).toBeCloseTo(0.05, 10);
  });

  it("ignores an account whose usage is unknown on either side of an interval", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }, { account: "agent-2" }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }, { account: "agent-2", exhausted: true }]),
      ...sweepRecords({
        sweepId: "s-1",
        repo: "org/one",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 11 * MINUTE,
      }),
    ]);

    expect(result.repos[0]?.usage).toBeCloseTo(0.1, 10);
    expect(result.repos[0]?.byAccount.map((entry) => entry.name)).toEqual(["agent-1"]);
  });

  it("carries model through as a breakdown, inheriting the overlap split", () => {
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
        ...sweepRecords({
          sweepId: "s-opus",
          repo: "org/one",
          startedAt: T0 - MINUTE,
          completedAt: T0 + 11 * MINUTE,
          model: "opus",
        }),
        ...sweepRecords({
          sweepId: "s-sonnet",
          repo: "org/one",
          startedAt: T0 - MINUTE,
          completedAt: T0 + 11 * MINUTE,
          model: "sonnet",
        }),
      ],
      { edgeToleranceMs: 0 },
    );

    expect(result.repos).toHaveLength(1);
    expect(result.repos[0]?.sweepCount).toBe(2);
    expect(result.repos[0]?.byModel.map((entry) => entry.name).sort()).toEqual(["opus", "sonnet"]);
    for (const entry of at(result.repos, 0).byModel) expect(entry.usage).toBeCloseTo(0.1, 10);
  });

  it("labels a sweep with no reported model as unknown rather than dropping it", () => {
    const result = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
      ...sweepRecords({
        sweepId: "s-1",
        repo: "org/one",
        startedAt: T0 - MINUTE,
        completedAt: T0 + 11 * MINUTE,
      }),
    ]);

    expect(result.repos[0]?.byModel).toEqual([{ name: "unknown", usage: expect.closeTo(0.2, 10) }]);
  });

  it("ranks repos by attributed usage, descending", () => {
    const result = attribute(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.3 }]),
        tokensSnapshot(T0 + 20 * MINUTE, [{ account: "agent-1", usage: 0.4 }]),
        ...sweepRecords({
          sweepId: "s-big",
          repo: "org/big",
          startedAt: T0,
          completedAt: T0 + 10 * MINUTE,
        }),
        ...sweepRecords({
          sweepId: "s-small",
          repo: "org/small",
          startedAt: T0 + 10 * MINUTE,
          completedAt: T0 + 20 * MINUTE,
        }),
      ],
      { edgeToleranceMs: 0 },
    );

    expect(result.repos.map((repo) => repo.repo)).toEqual(["org/big", "org/small"]);
    expect(result.repos[0]?.share).toBeCloseTo(0.75, 10);
    expect(result.repos[1]?.share).toBeCloseTo(0.25, 10);
  });

  it("reports the analyzed range and an empty result for an empty history", () => {
    const empty = attributeUsageToRepos([], [], { now: T0 });
    expect(empty).toMatchObject({ repos: [], unattributedUsage: 0, totalUsage: 0, droppedIntervals: 0 });
    expect(empty.rangeStart).toBeUndefined();

    const populated = attribute([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.2 }]),
    ]);
    expect(populated.rangeStart).toBe(T0);
    expect(populated.rangeEnd).toBe(T0 + 10 * MINUTE);
  });
});
