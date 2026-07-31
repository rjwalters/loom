/**
 * `GET /api/history` / `GET /public/history` client, with keyset-cursor
 * pagination baked in — see `dashboard/docs/query-api.md`'s "`GET
 * /api/history`" section for the wire contract this mirrors.
 *
 * **One client, two routes, no duplication** (issue #4751's last acceptance
 * criterion): every function here takes the route's `basePath` as a plain
 * string argument (`"/api/history"` or `"/public/history"`, or a fully
 * qualified URL if the dashboard is served from a different origin than the
 * Worker) — nothing here special-cases which one it is. The two routes
 * return the exact same page/pagination shape; `/public/history` merely
 * returns redacted `record` payloads for `visibility: "private"` rows (see
 * `types.ts`'s `HistoryRecord` doc), which every downstream chart transform
 * already tolerates.
 */

import type { HistoryQueryResult } from "./types.js";

/** Every filter `GET /api/history` accepts, matching
 * `dashboard/src/query.ts`'s `HistoryFilter` param-for-param (host, repo,
 * model, result, since, until, limit) — see `dashboard/docs/query-api.md`.
 * `cursor` is deliberately omitted here: pagination is `fetchAllHistory`'s
 * job below, not something a chart-data caller should manage by hand. */
export interface HistoryQueryFilter {
  host?: string;
  repo?: string;
  model?: string;
  result?: string;
  /** RFC 3339 datetime — inclusive lower bound on `emittedAt`. */
  since?: string;
  /** RFC 3339 datetime — exclusive upper bound on `emittedAt`. */
  until?: string;
  /** Page size passed to the API (server caps at 500 regardless). Defaults
   * to the server's own default (50) when omitted. Callers paging via
   * `fetchAllHistory` generally want this at or near the 500 cap to
   * minimize round trips. */
  limit?: number;
}

/** Minimal shape of the subset of the `fetch` API this client needs — lets
 * tests substitute a stub without pulling in a DOM/undici `Response`
 * implementation, and lets a consumer pass any `fetch`-compatible
 * implementation (native, a polyfill, an instrumented wrapper, ...). */
export type FetchLike = (input: string, init?: { signal?: AbortSignal }) => Promise<{
  ok: boolean;
  status: number;
  json(): Promise<unknown>;
}>;

export class HistoryApiError extends Error {
  constructor(
    message: string,
    public readonly status: number,
  ) {
    super(message);
    this.name = "HistoryApiError";
  }
}

function buildUrl(basePath: string, filter: HistoryQueryFilter, cursor: number | undefined): string {
  const params = new URLSearchParams();
  if (filter.host) params.set("host", filter.host);
  if (filter.repo) params.set("repo", filter.repo);
  if (filter.model) params.set("model", filter.model);
  if (filter.result) params.set("result", filter.result);
  if (filter.since) params.set("since", filter.since);
  if (filter.until) params.set("until", filter.until);
  if (filter.limit !== undefined) params.set("limit", String(filter.limit));
  if (cursor !== undefined) params.set("cursor", String(cursor));

  const query = params.toString();
  return query ? `${basePath}?${query}` : basePath;
}

/** Fetch a single page. Thin wrapper — most callers want `fetchAllHistory`
 * below instead; this is exposed for callers that want to manage
 * pagination themselves (e.g. infinite-scroll UIs). */
export async function fetchHistoryPage(
  basePath: string,
  filter: HistoryQueryFilter,
  cursor?: number,
  fetchImpl: FetchLike = fetch as unknown as FetchLike,
  signal?: AbortSignal,
): Promise<HistoryQueryResult> {
  const url = buildUrl(basePath, filter, cursor);
  const res = await fetchImpl(url, { signal });
  const body = (await res.json()) as HistoryQueryResult | { error: string };
  if (!res.ok) {
    const message = "error" in body ? body.error : `history query failed with status ${res.status}`;
    throw new HistoryApiError(message, res.status);
  }
  return body as HistoryQueryResult;
}

export interface FetchAllHistoryOptions {
  fetchImpl?: FetchLike;
  signal?: AbortSignal;
  /** Safety cap on the number of pages fetched, so a server bug that never
   * emits a `null` `nextCursor` cannot spin this loop forever. 10,000 pages
   * at the 500-row max page size is 5,000,000 records — comfortably beyond
   * any real chart's date range. */
  maxPages?: number;
}

const DEFAULT_MAX_PAGES = 10_000;

/**
 * Page through `GET /api/history` (or `/public/history`) via `nextCursor`
 * until the result set is exhausted, accumulating every record along the
 * way. This is the pagination loop issue #4751's acceptance criteria call
 * for: chart datasets must not assume a single unpaginated response covers
 * the full requested range, since the API caps `limit` at 500.
 *
 * Ordering is preserved exactly as the API returns it (newest-first, by
 * `id` descending) — each page's records are appended in the order received,
 * and pages are fetched in strict `nextCursor` sequence, so no record is
 * ever dropped or duplicated across the boundary between two pages.
 */
export async function fetchAllHistory(
  basePath: string,
  filter: HistoryQueryFilter,
  options: FetchAllHistoryOptions = {},
): Promise<HistoryQueryResult["records"]> {
  const { fetchImpl = fetch as unknown as FetchLike, signal, maxPages = DEFAULT_MAX_PAGES } = options;

  const records: HistoryQueryResult["records"] = [];
  let cursor: number | undefined;
  let pages = 0;

  for (;;) {
    const page = await fetchHistoryPage(basePath, filter, cursor, fetchImpl, signal);
    records.push(...page.records);
    pages += 1;

    if (page.nextCursor === null) break;
    if (pages >= maxPages) {
      throw new Error(
        `fetchAllHistory: exceeded maxPages=${maxPages} without reaching the end of the result set ` +
          `(server never returned a null nextCursor) — aborting to avoid an unbounded fetch loop`,
      );
    }
    cursor = page.nextCursor;
  }

  return records;
}
