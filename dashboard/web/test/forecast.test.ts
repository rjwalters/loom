import { beforeEach, describe, expect, it } from "vitest";

import { buildBurnCurves } from "../src/analytics/burn.js";
import { forecastAccount, forecastAccounts } from "../src/analytics/forecast.js";
import { parseTokenSamples } from "../src/analytics/parse.js";
import type { HistoryEnvelope } from "../src/analytics/types.js";
import { HOUR, MINUTE, T0, resetIds, tokensSnapshot } from "./analyticsFixtures.js";

beforeEach(resetIds);

function forecastOf(records: readonly HistoryEnvelope[], now: number, account = "agent-1") {
  const curves = buildBurnCurves(parseTokenSamples(records));
  const curve = curves.find((candidate) => candidate.account === account);
  if (!curve) throw new Error(`no curve for ${account}`);
  return forecastAccount(curve, { now });
}

describe("forecastAccount", () => {
  it("projects exhaustion before the window resets", () => {
    // 0.2 → 0.4 over an hour is 0.2/h; from 0.2 that reaches 1.0 four hours
    // after T0, while the window does not reset until T0+10h.
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.2, resetAt: T0 + 10 * HOUR }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.4, resetAt: T0 + 10 * HOUR }]),
      ],
      T0 + HOUR,
    );

    expect(forecast.status).toBe("projected-exhaustion");
    expect(forecast.slopePerHour).toBeCloseTo(0.2, 10);
    // Instants derived from a division are compared to the millisecond within
    // half a second, not bit-exactly.
    expect(forecast.projectedExhaustionAt).toBeCloseTo(T0 + 4 * HOUR, -3);
    expect(forecast.secondsUntilExhaustion).toBe(3 * 3600);
    expect(forecast.limitWindowResetAt).toBe(T0 + 10 * HOUR);
    expect(forecast.marginSec).toBe(6 * 3600);
    expect(forecast.latestUsageFraction).toBe(0.4);
    expect(forecast.sampleCount).toBe(2);
  });

  it("reports resets-first when the window rolls over before the projection lands", () => {
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.2, resetAt: T0 + 2 * HOUR }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.4, resetAt: T0 + 2 * HOUR }]),
      ],
      T0 + HOUR,
    );

    expect(forecast.status).toBe("resets-first");
    expect(forecast.projectedExhaustionAt).toBeCloseTo(T0 + 4 * HOUR, -3);
    // Negative margin: the window resets two hours *before* the projection.
    expect(forecast.marginSec).toBe(-2 * 3600);
  });

  it("reports a measured exhaustion as exhausted regardless of the trend", () => {
    // The classic "approaching, then crossing" series: a gentle climb that
    // ends with the daemon reporting the account exhausted.
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.5, resetAt: T0 + 8 * HOUR }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.7, resetAt: T0 + 8 * HOUR }]),
        tokensSnapshot(T0 + 2 * HOUR, [{ account: "agent-1", usage: 0.9, resetAt: T0 + 8 * HOUR }]),
        tokensSnapshot(T0 + 3 * HOUR, [
          { account: "agent-1", usage: 1, resetAt: T0 + 8 * HOUR, exhausted: true },
        ]),
      ],
      T0 + 3 * HOUR,
    );

    expect(forecast.status).toBe("exhausted");
    expect(forecast.limitWindowResetAt).toBe(T0 + 8 * HOUR);
    expect(forecast.secondsUntilReset).toBe(5 * 3600);
  });

  it("crosses from resets-first to projected-exhaustion as the burn accelerates", () => {
    const window = { resetAt: T0 + 6 * HOUR };
    const slow: HistoryEnvelope[] = [
      tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1, ...window }]),
      tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.2, ...window }]),
    ];
    expect(forecastOf(slow, T0 + HOUR).status).toBe("resets-first");

    const accelerated: HistoryEnvelope[] = [
      ...slow,
      tokensSnapshot(T0 + 2 * HOUR, [{ account: "agent-1", usage: 0.55, ...window }]),
    ];
    expect(forecastOf(accelerated, T0 + 2 * HOUR).status).toBe("projected-exhaustion");
  });

  it("fits only the live segment, not across a limit-window rollover", () => {
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1, resetAt: T0 + 2 * HOUR }]),
        tokensSnapshot(T0 + 2 * HOUR, [{ account: "agent-1", usage: 0.9, resetAt: T0 + 2 * HOUR }]),
        tokensSnapshot(T0 + 150 * MINUTE, [{ account: "agent-1", usage: 0, resetAt: T0 + 26 * HOUR }]),
        tokensSnapshot(T0 + 210 * MINUTE, [{ account: "agent-1", usage: 0.1, resetAt: T0 + 26 * HOUR }]),
      ],
      T0 + 210 * MINUTE,
    );

    // Pre-rollover burn was 0.4/h; the live window's is 0.1/h. Fitting the
    // whole series would have predicted exhaustion hours too early.
    expect(forecast.sampleCount).toBe(2);
    expect(forecast.slopePerHour).toBeCloseTo(0.1, 10);
    expect(forecast.projectedExhaustionAt).toBeCloseTo(T0 + 150 * MINUTE + 10 * HOUR, -3);
    expect(forecast.limitWindowResetAt).toBe(T0 + 26 * HOUR);
    // 12.5h to exhaustion vs. a 26h window: the fresh window still runs dry
    // first, but ~13.5h later than the pre-rollover trend would have said.
    expect(forecast.status).toBe("projected-exhaustion");
    expect(forecast.marginSec).toBeCloseTo(13.5 * 3600, -1);
  });

  it("averages noise rather than tracking the last two points", () => {
    // A jittery but clearly 0.1/h series; a last-two-points model would read
    // the final flat step as 0.0/h and report "Idle".
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.0 }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.11 }]),
        tokensSnapshot(T0 + 2 * HOUR, [{ account: "agent-1", usage: 0.19 }]),
        tokensSnapshot(T0 + 3 * HOUR, [{ account: "agent-1", usage: 0.3 }]),
        tokensSnapshot(T0 + 4 * HOUR, [{ account: "agent-1", usage: 0.3 }]),
      ],
      T0 + 4 * HOUR,
    );

    expect(forecast.status).toBe("projected-exhaustion");
    expect(forecast.slopePerHour).toBeGreaterThan(0.06);
    expect(forecast.slopePerHour).toBeLessThan(0.09);
  });

  it("reports flat when usage is not moving", () => {
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.3, resetAt: T0 + 5 * HOUR }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.3, resetAt: T0 + 5 * HOUR }]),
      ],
      T0 + HOUR,
    );

    expect(forecast.status).toBe("flat");
    expect(forecast.slopePerHour).toBe(0);
    expect(forecast.projectedExhaustionAt).toBeUndefined();
    expect(forecast.secondsUntilReset).toBe(4 * 3600);
  });

  it("reports insufficient-data from a single point", () => {
    const forecast = forecastOf([tokensSnapshot(T0, [{ account: "agent-1", usage: 0.3 }])], T0);
    expect(forecast.status).toBe("insufficient-data");
    expect(forecast.sampleCount).toBe(1);
    expect(forecast.slopePerHour).toBeUndefined();
  });

  it("reports insufficient-data when usage was never measured", () => {
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1" }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1" }]),
      ],
      T0 + HOUR,
    );
    expect(forecast.status).toBe("insufficient-data");
    expect(forecast.sampleCount).toBe(0);
    expect(forecast.latestUsageFraction).toBeUndefined();
  });

  it("never projects exhaustion into the past", () => {
    // A steep run that the fitted line puts past 1.0 already; the projection
    // clamps to `now` instead of reporting a negative ETA.
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.5 }]),
        tokensSnapshot(T0 + 10 * MINUTE, [{ account: "agent-1", usage: 0.99 }]),
      ],
      T0 + 2 * HOUR,
    );

    expect(forecast.projectedExhaustionAt).toBe(T0 + 2 * HOUR);
    expect(forecast.secondsUntilExhaustion).toBe(0);
  });

  it("omits the margin when the window reset is unknown", () => {
    const forecast = forecastOf(
      [
        tokensSnapshot(T0, [{ account: "agent-1", usage: 0.2 }]),
        tokensSnapshot(T0 + HOUR, [{ account: "agent-1", usage: 0.4 }]),
      ],
      T0 + HOUR,
    );

    expect(forecast.status).toBe("projected-exhaustion");
    expect(forecast.marginSec).toBeUndefined();
    expect(forecast.secondsUntilReset).toBeUndefined();
  });
});

describe("forecastAccounts", () => {
  it("preserves curve ordering and forecasts each account independently", () => {
    const curves = buildBurnCurves(
      parseTokenSamples([
        tokensSnapshot(T0, [
          { account: "agent-1", rank: 0, usage: 0.2, resetAt: T0 + 10 * HOUR },
          { account: "agent-2", rank: 1, usage: 0.98, resetAt: T0 + 10 * HOUR },
        ]),
        tokensSnapshot(T0 + HOUR, [
          { account: "agent-1", rank: 0, usage: 0.25, resetAt: T0 + 10 * HOUR },
          { account: "agent-2", rank: 1, usage: 1, resetAt: T0 + 10 * HOUR, exhausted: true },
        ]),
      ]),
    );

    const forecasts = forecastAccounts(curves, { now: T0 + HOUR });
    expect(forecasts.map((forecast) => forecast.account)).toEqual(["agent-1", "agent-2"]);
    expect(forecasts.map((forecast) => forecast.status)).toEqual(["resets-first", "exhausted"]);
  });
});
