import { describe, expect, it } from "vitest";

import {
  FALLBACK_TIME_ZONE,
  civilDateIn,
  displayTimeZone,
  formatCivilDate,
  isValidTimeZone,
  timeZoneAbbreviation,
} from "../src/timezone";

/** A stand-in for `window` carrying whatever the Worker injected. */
function scopeWith(value: unknown): typeof globalThis {
  return { __LOOM_FLEET__: value } as unknown as typeof globalThis;
}

describe("isValidTimeZone", () => {
  it.each(["UTC", "America/Los_Angeles", "Asia/Tokyo", "Europe/London"])("accepts %s", (zone) => {
    expect(isValidTimeZone(zone)).toBe(true);
  });

  it.each([undefined, "", "Not/AZone", "PDT", "America/Nowhere"])("rejects %s", (zone) => {
    expect(isValidTimeZone(zone as string | undefined)).toBe(false);
  });
});

describe("displayTimeZone", () => {
  it("prefers the deployment-injected zone", () => {
    expect(displayTimeZone(scopeWith({ timeZone: "America/Los_Angeles" }))).toBe("America/Los_Angeles");
  });

  it("uses the injected zone regardless of auth state", () => {
    // The zone is deployment config, not identity — an anonymous visitor
    // reads the same charts and must bucket them the same way.
    expect(displayTimeZone(scopeWith({ authenticated: false, timeZone: "Asia/Tokyo" }))).toBe("Asia/Tokyo");
    expect(displayTimeZone(scopeWith({ authenticated: true, timeZone: "Asia/Tokyo" }))).toBe("Asia/Tokyo");
  });

  // A typo'd zone in a deploy's config must not blank every chart on the
  // page — `Intl` throws a RangeError on an unknown zone, and an uncaught one
  // inside `bucketKey` would do exactly that.
  it.each([
    ["an invalid IANA name", { timeZone: "Not/AZone" }],
    ["a non-string", { timeZone: 42 }],
    ["no timeZone key", { authenticated: true }],
    ["null", null],
    ["a non-object", "America/Los_Angeles"],
    ["undefined", undefined],
  ])("falls back when the injected state is %s", (_label, value) => {
    // Falls through to the browser/runtime zone, which is always something
    // Intl accepts — the assertion is that it resolved rather than threw.
    const resolved = displayTimeZone(scopeWith(value));
    expect(isValidTimeZone(resolved)).toBe(true);
  });

  it("resolves to a usable zone when nothing was injected at all", () => {
    expect(isValidTimeZone(displayTimeZone({} as unknown as typeof globalThis))).toBe(true);
  });

  it("exposes UTC as the last-resort fallback", () => {
    expect(FALLBACK_TIME_ZONE).toBe("UTC");
    expect(isValidTimeZone(FALLBACK_TIME_ZONE)).toBe(true);
  });
});

describe("civilDateIn", () => {
  it("reads the wall-clock date in the target zone", () => {
    const instant = new Date("2026-08-01T06:59:59Z"); // 23:59:59 PDT on Jul 31
    expect(civilDateIn(instant, "UTC")).toEqual({ year: 2026, month: 8, day: 1 });
    expect(civilDateIn(instant, "America/Los_Angeles")).toEqual({ year: 2026, month: 7, day: 31 });
  });

  it("handles a zone ahead of UTC", () => {
    const instant = new Date("2026-07-31T16:00:00Z"); // 01:00 Aug 1 in Tokyo
    expect(civilDateIn(instant, "Asia/Tokyo")).toEqual({ year: 2026, month: 8, day: 1 });
  });

  it("crosses a year boundary correctly", () => {
    const instant = new Date("2027-01-01T05:00:00Z"); // 21:00 Dec 31 PST
    expect(civilDateIn(instant, "America/Los_Angeles")).toEqual({ year: 2026, month: 12, day: 31 });
  });
});

describe("formatCivilDate", () => {
  it("zero-pads to YYYY-MM-DD", () => {
    expect(formatCivilDate({ year: 2026, month: 7, day: 4 })).toBe("2026-07-04");
    expect(formatCivilDate({ year: 2026, month: 12, day: 31 })).toBe("2026-12-31");
  });
});

describe("timeZoneAbbreviation", () => {
  it("gives the seasonal abbreviation for the instant", () => {
    // Same zone, two sides of the DST transition.
    expect(timeZoneAbbreviation("America/Los_Angeles", new Date("2026-07-01T12:00:00Z"))).toBe("PDT");
    expect(timeZoneAbbreviation("America/Los_Angeles", new Date("2026-01-01T12:00:00Z"))).toBe("PST");
  });

  it("gives UTC for UTC", () => {
    expect(timeZoneAbbreviation("UTC", new Date("2026-07-01T12:00:00Z"))).toBe("UTC");
  });

  // A label is a nicety; never a reason to throw mid-render.
  it("falls back to the zone name rather than throwing on a bad zone", () => {
    expect(timeZoneAbbreviation("Not/AZone", new Date("2026-07-01T12:00:00Z"))).toBe("Not/AZone");
  });
});
