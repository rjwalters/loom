/**
 * The one display timezone every time-rendering surface in this app resolves
 * through — chart buckets, timestamp tooltips, and the refresh status line.
 *
 * ## Why this is configured rather than taken from the browser
 *
 * For *formatting* a timestamp, the viewer's own zone is the obvious answer.
 * For **bucketing** it is not: `bucketKey` decides which calendar day a sweep
 * belongs to, so a browser-derived zone would give two viewers different
 * daily counts for identical data, and would move the operator's own "today"
 * when they travel. Which day a sweep happened on is a property of the fleet,
 * not of whoever is looking at it.
 *
 * So the deployment picks the zone (`DISPLAY_TIMEZONE`, a Worker `[vars]`
 * setting injected into the page next to the auth state), and the browser's
 * zone is only the fallback when it is unset. That also keeps the reference
 * deployment's city out of this repo, which ships a deploy-to-your-own-account
 * template.
 *
 * ## Resolution order
 *
 * 1. `DISPLAY_TIMEZONE`, injected by `../../src/index.ts`'s `handleRoot`
 * 2. the viewer's browser zone
 * 3. `UTC`
 *
 * Every step is validated and falls through on failure. A typo'd IANA name in
 * a deploy's config must not blank the dashboard — `Intl` throws a
 * `RangeError` on an unknown zone, and an uncaught one inside `bucketKey`
 * would take out every chart on the page.
 */

/** Last-resort zone. Also what a non-Intl environment gets. */
export const FALLBACK_TIME_ZONE = "UTC";

/** The global the Worker injects the page's config into (shared with the
 * auth state — see `api.ts`). */
const INJECTED_STATE_GLOBAL = "__LOOM_FLEET__";

/** Whether `Intl` accepts `zone` as an IANA timezone name. */
export function isValidTimeZone(zone: string | undefined): zone is string {
  if (!zone) return false;
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: zone });
    return true;
  } catch {
    return false;
  }
}

function injectedTimeZone(scope: typeof globalThis): string | undefined {
  const raw = (scope as Record<string, unknown>)[INJECTED_STATE_GLOBAL];
  if (typeof raw !== "object" || raw === null) return undefined;
  const zone = (raw as { timeZone?: unknown }).timeZone;
  return typeof zone === "string" ? zone : undefined;
}

function browserTimeZone(): string | undefined {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {
    return undefined;
  }
}

/**
 * Resolve the display timezone. Always returns a zone `Intl` accepts, so
 * callers never need their own try/catch.
 */
export function displayTimeZone(scope: typeof globalThis = globalThis): string {
  const configured = injectedTimeZone(scope);
  if (isValidTimeZone(configured)) return configured;

  const browser = browserTimeZone();
  if (isValidTimeZone(browser)) return browser;

  return FALLBACK_TIME_ZONE;
}

/** A date's civil (wall-clock) year/month/day in `zone`. This is the whole
 * trick behind zone-correct bucketing: `Intl` is the only thing that knows
 * the offset in effect at that instant, DST included, so the civil date is
 * read out of it rather than computed from an offset. */
export interface CivilDate {
  year: number;
  month: number;
  day: number;
}

/**
 * The civil date `instant` falls on in `zone`.
 *
 * Uses `formatToParts` rather than parsing a formatted string: a locale's
 * output order is not guaranteed, and `en-CA`'s happens-to-be-ISO format is
 * an implementation detail worth not depending on.
 */
export function civilDateIn(instant: Date, zone: string): CivilDate {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: zone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).formatToParts(instant);

  const value = (type: string): number => {
    const part = parts.find((p) => p.type === type);
    return part ? Number(part.value) : Number.NaN;
  };

  return { year: value("year"), month: value("month"), day: value("day") };
}

/** `YYYY-MM-DD` for a civil date. */
export function formatCivilDate({ year, month, day }: CivilDate): string {
  return `${String(year).padStart(4, "0")}-${String(month).padStart(2, "0")}-${String(day).padStart(2, "0")}`;
}

/**
 * A short zone label (`PDT`, `UTC`) for appending to a rendered timestamp, so
 * a value pasted into a bug report is not ambiguous.
 *
 * Falls back to the zone's own name when the runtime cannot produce a short
 * name — a label is a nicety, never a reason to fail a render.
 */
export function timeZoneAbbreviation(zone: string, at: Date = new Date()): string {
  try {
    const parts = new Intl.DateTimeFormat("en-US", { timeZone: zone, timeZoneName: "short" }).formatToParts(at);
    return parts.find((p) => p.type === "timeZoneName")?.value ?? zone;
  } catch {
    return zone;
  }
}
