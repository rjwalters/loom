import { describe, expect, it } from "vitest";

import {
  UNKNOWN,
  formatAbsolute,
  formatCount,
  formatCountdown,
  formatDuration,
  formatGigabytes,
  formatPercent,
  formatRatio,
  formatRelative,
  formatText,
  roleFailureLabel,
  roleTickCompactText,
  roleTickSummaryText,
  secondsSince,
} from "../src/format";
import { NOW, isoMinutesBefore } from "./fixtures";

describe("unknown is never zero", () => {
  // `.loom/docs/telemetry-schema.md`: "A consumer MUST treat an absent
  // measurement as unknown, never as zero/full." Every formatter is checked
  // here because a regression in any one of them invents a fleet alarm.
  it("renders every absent measurement as the unknown marker", () => {
    expect(formatPercent(undefined)).toBe(UNKNOWN);
    expect(formatRatio(undefined)).toBe(UNKNOWN);
    expect(formatGigabytes(undefined)).toBe(UNKNOWN);
    expect(formatDuration(undefined)).toBe(UNKNOWN);
    expect(formatCount(undefined)).toBe(UNKNOWN);
    expect(formatText(undefined)).toBe(UNKNOWN);
    expect(formatRelative(undefined)).toBe(UNKNOWN);
    expect(formatAbsolute(undefined)).toBe(UNKNOWN);
  });

  it("still renders a genuine zero as zero", () => {
    expect(formatPercent(0)).toBe("0%");
    expect(formatRatio(0)).toBe("0.00");
    expect(formatGigabytes(0)).toBe("0.0 GB");
    expect(formatDuration(0)).toBe("0s");
    expect(formatCount(0)).toBe("0");
  });
});

describe("formatPercent", () => {
  it("scales a fraction", () => {
    expect(formatPercent(0.83)).toBe("83%");
    expect(formatPercent(0.834, 1)).toBe("83.4%");
    expect(formatPercent(1)).toBe("100%");
  });
});

describe("formatGigabytes", () => {
  it("keeps a decimal below 10 GB and rounds above", () => {
    expect(formatGigabytes(0.4)).toBe("0.4 GB");
    expect(formatGigabytes(9.9)).toBe("9.9 GB");
    expect(formatGigabytes(200)).toBe("200 GB");
  });
});

describe("formatDuration", () => {
  it("uses coarse units", () => {
    expect(formatDuration(86_400)).toBe("1d 0h");
    expect(formatDuration(90_000)).toBe("1d 1h");
    expect(formatDuration(3_600)).toBe("1h 0m");
    expect(formatDuration(125)).toBe("2m 5s");
    expect(formatDuration(9)).toBe("9s");
  });

  it("treats a negative duration as unknown", () => {
    expect(formatDuration(-5)).toBe(UNKNOWN);
  });
});

describe("secondsSince / formatRelative", () => {
  it("measures against the injected clock", () => {
    expect(secondsSince(isoMinutesBefore(5), NOW)).toBe(300);
    expect(formatRelative(isoMinutesBefore(5), NOW)).toBe("5m 0s ago");
  });

  it("returns unknown for an unparseable timestamp", () => {
    expect(secondsSince("not-a-date", NOW)).toBeUndefined();
    expect(formatRelative("not-a-date", NOW)).toBe(UNKNOWN);
  });

  it("clamps a future timestamp from a skewed host clock", () => {
    expect(formatRelative(isoMinutesBefore(-30), NOW)).toBe("just now");
  });
});

describe("formatCountdown", () => {
  it("counts down to a future reset window instead of clamping it", () => {
    expect(formatCountdown("2026-07-30T18:00:00Z", NOW)).toBe("in 5h 50m");
  });

  it("reports a past window as elapsed", () => {
    expect(formatCountdown(isoMinutesBefore(5), NOW)).toBe("5m 0s ago");
  });

  it("returns unknown when the daemon did not report the window", () => {
    expect(formatCountdown(undefined, NOW)).toBe(UNKNOWN);
  });
});

describe("formatAbsolute", () => {
  // An explicit zone throughout: the default resolves to the machine's zone,
  // which would make these assertions pass locally and fail in CI.
  it("renders a readable timestamp in the given zone, with the zone named", () => {
    expect(formatAbsolute("2026-07-30T12:00:00Z", "UTC")).toBe("2026-07-30 12:00:00 UTC");
  });

  it("renders the same instant as local wall-clock elsewhere", () => {
    // 12:00Z is 05:00 PDT the same day.
    expect(formatAbsolute("2026-07-30T12:00:00Z", "America/Los_Angeles")).toBe(
      "2026-07-30 05:00:00 PDT",
    );
    // 23:30Z is 16:30 PDT the same day — the #4857 case, where UTC would
    // have shown a different date than the chart bucket.
    expect(formatAbsolute("2026-07-31T23:30:00Z", "America/Los_Angeles")).toBe(
      "2026-07-31 16:30:00 PDT",
    );
  });

  it("renders local midnight as 00:00, never 24:00", () => {
    expect(formatAbsolute("2026-08-01T07:00:00Z", "America/Los_Angeles")).toBe(
      "2026-08-01 00:00:00 PDT",
    );
  });

  it("returns unknown for garbage", () => {
    expect(formatAbsolute("tuesday")).toBe(UNKNOWN);
  });
});

describe("roleFailureLabel (#5022)", () => {
  it("joins the role and the workspace root's basename, mirroring the daemon's RoleFailure::label()", () => {
    expect(roleFailureLabel({ role: "judge", root: "/repos/loom" })).toBe("judge @ loom");
  });

  it("falls back to unknown for a missing role or root, never an empty string", () => {
    expect(roleFailureLabel({ root: "/repos/loom" })).toBe(`${UNKNOWN} @ loom`);
    expect(roleFailureLabel({ role: "judge" })).toBe(`judge @ ${UNKNOWN}`);
    expect(roleFailureLabel({})).toBe(`${UNKNOWN} @ ${UNKNOWN}`);
  });
});

describe("roleTickSummaryText / roleTickCompactText (#5022)", () => {
  it("renders unknown when the daemon has not reported roles at all", () => {
    expect(roleTickSummaryText(undefined)).toBe(UNKNOWN);
    expect(roleTickCompactText(undefined)).toBe(UNKNOWN);
  });

  it("reports 'no role ticks' for a genuine total: 0, not an error state", () => {
    const roles = { total: 0, ok: 0, persistent: [] };
    expect(roleTickSummaryText(roles)).toBe("no role ticks");
    expect(roleTickCompactText(roles)).toBe("no role ticks");
  });

  it("reports every-tick-ok with no alarming detail", () => {
    const roles = { total: 12, ok: 12, persistent: [] };
    expect(roleTickSummaryText(roles)).toBe("12/12 ticks ok");
    expect(roleTickCompactText(roles)).toBe("ok");
  });

  it("names the failing role(s) in the full summary, and just a count in the compact one", () => {
    const roles = {
      total: 3,
      ok: 1,
      persistent: [{ root: "/repos/loom", role: "judge", failures: 2 }],
    };
    expect(roleTickSummaryText(roles)).toBe(
      "1/3 ticks ok; 1 persistent failure(s): judge @ loom",
    );
    expect(roleTickCompactText(roles)).toBe("1 failing");
  });
});
