import { beforeEach, describe, expect, it } from "vitest";

import { buildBurnCurves, buildPoolBurnCurves } from "../src/analytics/burn.js";
import { parsePoolSamples, parseTokenSamples } from "../src/analytics/parse.js";
import { HOUR, MINUTE, T0, at, newestFirst, poolTokensSnapshot, resetIds, tokensSnapshot } from "./analyticsFixtures.js";

beforeEach(resetIds);

describe("buildBurnCurves", () => {
  it("builds one chronological series per account", () => {
    const curves = buildBurnCurves(
      parseTokenSamples(
        newestFirst([
          tokensSnapshot(T0, [
            { account: "agent-1", rank: 0, usage: 0.1 },
            { account: "agent-2", rank: 1, usage: 0.5 },
          ]),
          tokensSnapshot(T0 + 10 * MINUTE, [
            { account: "agent-1", rank: 0, usage: 0.2 },
            { account: "agent-2", rank: 1, usage: 0.55 },
          ]),
        ]),
      ),
    );

    expect(curves.map((curve) => curve.account)).toEqual(["agent-1", "agent-2"]);
    expect(curves[0]?.points.map((point) => point.usageFraction)).toEqual([0.1, 0.2]);
    expect(curves[0]?.points.map((point) => point.at)).toEqual([T0, T0 + 10 * MINUTE]);
    expect(curves[0]?.segments).toHaveLength(1);
    expect(curves[0]?.currentSegment?.startedBy).toBe("initial");
  });

  it("orders curves by pool rank, then name", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [
          { account: "zeta", rank: 0, usage: 0.1 },
          { account: "alpha", rank: 2, usage: 0.1 },
          { account: "mid", rank: 1, usage: 0.1 },
        ]),
      ]),
    );
    expect(curves.map((curve) => curve.account)).toEqual(["zeta", "mid", "alpha"]);
  });

  // #4898: one account is one curve, however many hosts report it —
  // `usage_fraction` is the account's server-side consumption, so N hosts are
  // one clock read N times, not N clocks.
  it("merges one account reported by several hosts into a single curve", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.5 }], "host-a"),
        tokensSnapshot(T0 + MINUTE, [{ account: "agent-1", usage: 0.5 }], "host-b"),
        tokensSnapshot(T0 + 2 * MINUTE, [{ account: "agent-1", usage: 0.51 }], "host-c"),
      ]),
    );

    expect(curves).toHaveLength(1);
    expect(curves[0]?.account).toBe("agent-1");
    expect(curves[0]?.hostIds).toEqual(["host-a", "host-b", "host-c"]);
    // Agreeing hosts are the normal shared-pool case, not a conflict.
    expect(curves[0]?.divergentHosts).toEqual([]);
    // One continuous series, not three fragments.
    expect(curves[0]?.segments).toHaveLength(1);
    expect(curves[0]?.points).toHaveLength(3);
  });

  // Cross-host wobble (independent probe schedules, unsynchronised clocks)
  // must not read as a limit-window rollover. At the old 1e-9 threshold any
  // such pair shattered the curve into meaningless segments.
  it("does not treat sub-threshold cross-host wobble as a window reset", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.74 }], "host-a"),
        tokensSnapshot(T0 + MINUTE, [{ account: "agent-1", usage: 0.735 }], "host-b"),
        tokensSnapshot(T0 + 2 * MINUTE, [{ account: "agent-1", usage: 0.75 }], "host-a"),
      ]),
    );

    expect(curves).toHaveLength(1);
    expect(curves[0]?.segments).toHaveLength(1);
    expect(curves[0]?.segments[0]?.startedBy).toBe("initial");
  });

  // The old keying assumed this case was universal; it is now detected
  // instead. Two hosts reporting wildly different usage for one name at the
  // same instant are plausibly different upstream accounts.
  it("flags hosts that disagree materially about the same account name", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }], "host-a"),
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.9 }], "host-b"),
      ]),
    );

    expect(curves).toHaveLength(1);
    expect(curves[0]?.divergentHosts).toEqual(["host-a", "host-b"]);
  });

  // A real rollover is a drop of most of the range — still detected, and not
  // confused with the wobble case above.
  it("still detects a genuine window rollover after merging", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.95 }], "host-a"),
        tokensSnapshot(T0 + MINUTE, [{ account: "agent-1", usage: 0.03 }], "host-b"),
      ]),
    );

    expect(curves).toHaveLength(1);
    expect(curves[0]?.segments).toHaveLength(2);
    expect(curves[0]?.segments[1]?.startedBy).toBe("window-reset");
  });

  it("starts a new segment at a limit-window rollover (usage drops)", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.8, resetAt: T0 + HOUR }]),
        tokensSnapshot(T0 + 30 * MINUTE, [{ account: "agent-1", usage: 0.95, resetAt: T0 + HOUR }]),
        tokensSnapshot(T0 + 70 * MINUTE, [{ account: "agent-1", usage: 0.02, resetAt: T0 + 6 * HOUR }]),
        tokensSnapshot(T0 + 90 * MINUTE, [{ account: "agent-1", usage: 0.06, resetAt: T0 + 6 * HOUR }]),
      ]),
    );

    const curve = at(curves, 0);
    expect(curve.segments).toHaveLength(2);
    expect(curve.segments[1]?.startedBy).toBe("window-reset");
    expect(curve.currentSegment?.points.map((point) => point.usageFraction)).toEqual([0.02, 0.06]);
    expect(curve.currentSegment?.limitWindowResetAt).toBe(T0 + 6 * HOUR);
  });

  it("starts a new segment when limit_window_reset_at advances without a usage drop", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.4, resetAt: T0 + HOUR }]),
        tokensSnapshot(T0 + 30 * MINUTE, [{ account: "agent-1", usage: 0.4, resetAt: T0 + 7 * HOUR }]),
      ]),
    );
    expect(curves[0]?.segments.map((segment) => segment.startedBy)).toEqual(["initial", "window-reset"]);
  });

  it("breaks the series across a telemetry gap rather than interpolating it", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
        tokensSnapshot(T0 + 5 * HOUR, [{ account: "agent-1", usage: 0.6 }]),
      ]),
    );
    expect(curves[0]?.segments.map((segment) => segment.startedBy)).toEqual(["initial", "gap"]);
    expect(curves[0]?.currentSegment?.points).toHaveLength(1);
  });

  it("honours a caller-supplied gap tolerance", () => {
    const samples = parseTokenSamples([
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]),
      tokensSnapshot(T0 + 5 * HOUR, [{ account: "agent-1", usage: 0.6 }]),
    ]);
    expect(buildBurnCurves(samples, { maxSampleGapMs: 6 * HOUR })[0]?.segments).toHaveLength(1);
  });

  it("drops unknown usage from the curve but still tracks exhaustion", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-2", usage: 0.9 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-2", exhausted: true }]),
      ]),
    );

    expect(curves[0]?.points).toHaveLength(1);
    expect(curves[0]?.exhausted).toBe(true);
    expect(curves[0]?.everExhausted).toBe(true);
    expect(curves[0]?.latestAt).toBe(T0 + 10 * MINUTE);
  });

  it("distinguishes currently-exhausted from recovered-after-exhaustion", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 1, exhausted: true }]),
        tokensSnapshot(T0 + 30 * MINUTE, [{ account: "agent-1", usage: 0.05 }]),
      ]),
    );
    expect(curves[0]?.exhausted).toBe(false);
    expect(curves[0]?.everExhausted).toBe(true);
  });

  it("clamps an out-of-range usage_fraction into [0, 1]", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([tokensSnapshot(T0, [{ account: "agent-1", usage: 1.4 }])]),
    );
    expect(curves[0]?.points[0]?.usageFraction).toBe(1);
  });
});

describe("buildPoolBurnCurves", () => {
  it("builds one chronological curve per host from the aggregate shape", () => {
    const curves = buildPoolBurnCurves(
      parsePoolSamples(
        newestFirst([
          poolTokensSnapshot(T0, { accountCount: 5, exhaustedCount: 1, meanUsage: 0.2, maxUsage: 0.5 }),
          poolTokensSnapshot(T0 + 10 * MINUTE, { accountCount: 5, exhaustedCount: 1, meanUsage: 0.25, maxUsage: 0.55 }),
        ]),
      ),
    );

    expect(curves.map((curve) => curve.hostId)).toEqual(["host-a"]);
    expect(curves[0]?.points.map((point) => point.meanUsageFraction)).toEqual([0.2, 0.25]);
    expect(curves[0]?.points.map((point) => point.maxUsageFraction)).toEqual([0.5, 0.55]);
    expect(curves[0]?.segments).toHaveLength(1);
    expect(curves[0]?.currentSegment?.startedBy).toBe("initial");
    expect(curves[0]?.accountCount).toBe(5);
    expect(curves[0]?.exhaustedCount).toBe(1);
  });

  it("keeps each host as a separate curve, ordered by hostId", () => {
    const curves = buildPoolBurnCurves(
      parsePoolSamples([
        poolTokensSnapshot(T0, { accountCount: 3, maxUsage: 0.4 }, "host-b"),
        poolTokensSnapshot(T0, { accountCount: 8, maxUsage: 0.9 }, "host-a"),
      ]),
    );
    expect(curves.map((curve) => curve.hostId)).toEqual(["host-a", "host-b"]);
  });

  it("starts a new segment at a rollover (peak usage drops)", () => {
    const curves = buildPoolBurnCurves(
      parsePoolSamples([
        poolTokensSnapshot(T0, { accountCount: 4, maxUsage: 0.8, nextResetAt: T0 + HOUR }),
        poolTokensSnapshot(T0 + 30 * MINUTE, { accountCount: 4, maxUsage: 0.95, nextResetAt: T0 + HOUR }),
        poolTokensSnapshot(T0 + 70 * MINUTE, { accountCount: 4, maxUsage: 0.05, nextResetAt: T0 + 6 * HOUR }),
      ]),
    );

    const curve = at(curves, 0);
    expect(curve.segments).toHaveLength(2);
    expect(curve.segments[1]?.startedBy).toBe("window-reset");
    expect(curve.currentSegment?.points.map((point) => point.maxUsageFraction)).toEqual([0.05]);
  });

  it("breaks the series across a telemetry gap rather than interpolating it", () => {
    const curves = buildPoolBurnCurves(
      parsePoolSamples([
        poolTokensSnapshot(T0, { accountCount: 4, maxUsage: 0.1 }),
        poolTokensSnapshot(T0 + 5 * HOUR, { accountCount: 4, maxUsage: 0.6 }),
      ]),
    );
    expect(curves[0]?.segments.map((segment) => segment.startedBy)).toEqual(["initial", "gap"]);
  });

  it("drops a sample with no usage figure from the plotted points but still tracks the newest account/exhausted counts", () => {
    const curves = buildPoolBurnCurves(
      parsePoolSamples([
        poolTokensSnapshot(T0, { accountCount: 4, exhaustedCount: 0, maxUsage: 0.5 }),
        poolTokensSnapshot(T0 + 10 * MINUTE, { accountCount: 4, exhaustedCount: 2 }),
      ]),
    );

    expect(curves[0]?.points).toHaveLength(1);
    expect(curves[0]?.exhaustedCount).toBe(2);
    expect(curves[0]?.accountCount).toBe(4);
    expect(curves[0]?.latestAt).toBe(T0 + 10 * MINUTE);
  });

  it("honours a caller-supplied gap tolerance", () => {
    const samples = parsePoolSamples([
      poolTokensSnapshot(T0, { accountCount: 2, maxUsage: 0.1 }),
      poolTokensSnapshot(T0 + 5 * HOUR, { accountCount: 2, maxUsage: 0.6 }),
    ]);
    expect(buildPoolBurnCurves(samples, { maxSampleGapMs: 6 * HOUR })[0]?.segments).toHaveLength(1);
  });
});
