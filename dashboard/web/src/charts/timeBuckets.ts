/**
 * Time-bucketing helper shared by the outcomes-over-time and success-rate
 * charts (issue #4751).
 *
 * Buckets are computed in the **display timezone** (`../timezone.ts`), not
 * UTC. `emittedAt` is always an RFC 3339 UTC instant (see
 * `.loom/docs/telemetry-schema.md`) — that is the wire format and it does not
 * change; what changes is which calendar day that instant is *attributed* to.
 *
 * Bucketing in UTC put every sweep between 17:00 and 23:59 US-Pacific into
 * the next day's bar, so a Pacific fleet's "daily" chart was cut at 5pm local
 * rather than at midnight (issue #4857). The success-rate trend is derived
 * from these same buckets, so it inherited the same skew.
 *
 * The zone is a property of the *deployment*, not of the viewer's browser —
 * see `../timezone.ts`'s module doc for why. Within one deployment the old
 * guarantee still holds: the same input produces the same bucket key for
 * every viewer.
 */

import { civilDateIn, displayTimeZone, formatCivilDate } from "../timezone";

export type BucketGranularity = "daily" | "weekly";

/** `YYYY-MM-DD` for `daily` (the calendar day `emittedAt` falls in, in the
 * display timezone), or the `YYYY-MM-DD` of that week's Monday for `weekly`
 * (ISO-8601 week start) — a stable, sortable string key either way. */
export function bucketKey(
  emittedAt: string,
  granularity: BucketGranularity,
  zone: string = displayTimeZone(),
): string {
  const date = new Date(emittedAt);
  if (Number.isNaN(date.getTime())) {
    throw new Error(`bucketKey: invalid emittedAt timestamp: ${emittedAt}`);
  }

  const civil = civilDateIn(date, zone);
  if (granularity === "daily") {
    return formatCivilDate(civil);
  }

  // Weekly: normalize to that week's Monday (ISO-8601 week start).
  //
  // The arithmetic runs on the *civil* date lifted into UTC — a pure calendar
  // value with no zone of its own. That is what keeps DST out of it: a week
  // containing a transition still has seven civil days, whereas subtracting
  // 24h-per-day from the original instant would land an hour off and, near
  // midnight, on the wrong date entirely.
  const civilUtc = new Date(Date.UTC(civil.year, civil.month - 1, civil.day));
  const dayOfWeek = (civilUtc.getUTCDay() + 6) % 7; // Mon = 0 … Sun = 6
  const monday = new Date(Date.UTC(civil.year, civil.month - 1, civil.day - dayOfWeek));

  return formatCivilDate({
    year: monday.getUTCFullYear(),
    month: monday.getUTCMonth() + 1,
    day: monday.getUTCDate(),
  });
}

/** Group items into buckets keyed by `bucketKey(getTimestamp(item),
 * granularity)`, returning buckets in ascending (chronological) key order.
 * Buckets with no items in the input are never synthesized — callers who
 * need a dense/zero-filled time axis (e.g. for a chart with no gaps) should
 * fill missing keys themselves once they know the desired date range. */
export function groupByTimeBucket<T>(
  items: T[],
  getTimestamp: (item: T) => string,
  granularity: BucketGranularity,
  zone: string = displayTimeZone(),
): Map<string, T[]> {
  const buckets = new Map<string, T[]>();
  for (const item of items) {
    const key = bucketKey(getTimestamp(item), granularity, zone);
    const existing = buckets.get(key);
    if (existing) {
      existing.push(item);
    } else {
      buckets.set(key, [item]);
    }
  }
  return new Map([...buckets.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}
