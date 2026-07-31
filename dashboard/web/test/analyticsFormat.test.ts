import { describe, expect, it } from "vitest";

import {
  UNKNOWN,
  formatDuration,
  formatPercent,
  formatRatePerHour,
  formatRelative,
} from "../src/analytics/format.js";

describe("format helpers", () => {
  it("renders an unknown value as an em-dash, never as zero", () => {
    expect(formatPercent(undefined)).toBe(UNKNOWN);
    expect(formatRatePerHour(undefined)).toBe(UNKNOWN);
    expect(formatDuration(undefined)).toBe(UNKNOWN);
    expect(formatRelative(undefined)).toBe(UNKNOWN);
    // ...while a genuine zero still renders as zero.
    expect(formatPercent(0)).toBe("0.0%");
    expect(formatRatePerHour(0)).toBe("0.0%/h");
  });

  it("formats fractions and rates", () => {
    expect(formatPercent(0.4237)).toBe("42.4%");
    expect(formatPercent(0.4237, 2)).toBe("42.37%");
    expect(formatRatePerHour(0.125)).toBe("12.5%/h");
  });

  it("formats durations coarsely", () => {
    expect(formatDuration(45)).toBe("45s");
    expect(formatDuration(600)).toBe("10m");
    expect(formatDuration(3 * 3600 + 300)).toBe("3h 5m");
    expect(formatDuration(2 * 86400 + 3600)).toBe("2d 1h");
  });

  it("signs relative durations", () => {
    expect(formatRelative(2400)).toBe("in 40m");
    expect(formatRelative(-2400)).toBe("40m ago");
    expect(formatRelative(5)).toBe("now");
  });
});
