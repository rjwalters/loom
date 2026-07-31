/**
 * Time-bucketing helper shared by the outcomes-over-time and success-rate
 * charts (issue #4751). Buckets are computed in UTC so the same input
 * produces the same bucket key regardless of the caller's local timezone —
 * `emittedAt` is always an RFC 3339 UTC timestamp (see
 * `.loom/docs/telemetry-schema.md`).
 */

export type BucketGranularity = "daily" | "weekly";

/** `YYYY-MM-DD` for `daily` (the UTC calendar day `emittedAt` falls in), or
 * the `YYYY-MM-DD` of that UTC week's Monday for `weekly` (ISO-8601 week
 * start) — a stable, sortable string key either way. */
export function bucketKey(emittedAt: string, granularity: BucketGranularity): string {
  const date = new Date(emittedAt);
  if (Number.isNaN(date.getTime())) {
    throw new Error(`bucketKey: invalid emittedAt timestamp: ${emittedAt}`);
  }

  if (granularity === "daily") {
    return isoDateUtc(date);
  }

  // Weekly: normalize to that week's Monday (ISO-8601 week start). UTC day
  // 0 = Sunday ... 6 = Saturday; shift so Monday = 0.
  const dayOfWeek = (date.getUTCDay() + 6) % 7;
  const monday = new Date(
    Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate() - dayOfWeek),
  );
  return isoDateUtc(monday);
}

function isoDateUtc(date: Date): string {
  const year = date.getUTCFullYear();
  const month = String(date.getUTCMonth() + 1).padStart(2, "0");
  const day = String(date.getUTCDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
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
): Map<string, T[]> {
  const buckets = new Map<string, T[]>();
  for (const item of items) {
    const key = bucketKey(getTimestamp(item), granularity);
    const existing = buckets.get(key);
    if (existing) {
      existing.push(item);
    } else {
      buckets.set(key, [item]);
    }
  }
  return new Map([...buckets.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}
