import { beforeEach, describe, expect, it } from "vitest";

import { buildBurnCurves } from "../src/analytics/burn.js";
import { parseTokenSamples } from "../src/analytics/parse.js";
import { HOUR, MINUTE, T0, at, newestFirst, resetIds, tokensSnapshot } from "./analyticsFixtures.js";

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

  it("keeps identically-named accounts on different hosts as separate curves", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }], "host-a"),
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.9 }], "host-b"),
      ]),
    );
    expect(curves).toHaveLength(2);
    expect(curves.map((curve) => `${curve.hostId}:${curve.points[0]?.usageFraction}`)).toEqual([
      "host-a:0.1",
      "host-b:0.9",
    ]);
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
