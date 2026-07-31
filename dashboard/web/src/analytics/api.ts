/**
 * History fetching for the token/cost analytics view (Epic #4702, Phase 3,
 * issue #4752).
 *
 * ## Why this paginates and filters client-side
 *
 * `GET /api/history` (`../../docs/query-api.md`) supports `host`/`repo`/
 * `model`/`result`/`since`/`until`/`limit`/`cursor` — but **not `kind`**. The
 * analytics need two record families (`tokens.snapshot` and the `sweep.*`
 * lifecycle records), so the client pulls the time range it wants at the
 * maximum page size and partitions by `kind` locally. `maxPages` caps the walk
 * so a very busy fleet degrades to "the most recent N records" (the API
 * returns newest-first) instead of hanging the page.
 *
 * A server-side `kind` filter would be a strict improvement and is the obvious
 * follow-up, but it is a backend change — out of scope for this UI issue.
 *
 * ## `/api` only, never `/public`
 *
 * `API_PREFIX` is a constant, not a parameter. Per this issue's
 * public-exposure decision (see `render.ts` and
 * `../../docs/token-analytics.md`), the token/cost analytics are an
 * authenticated-surface feature: the fetch path is pinned to `/api` so the
 * view cannot be repointed at `/public` by configuration, and `render.ts`
 * independently refuses to render on a public surface. Two enforcement points,
 * because "the API technically returns it" is not the same as "the page should
 * show it".
 */

import type { HistoryEnvelope } from "./types.js";

/** Authenticated surface only — see the module doc. */
const API_PREFIX = "/api";

/** The API's own cap (`docs/query-api.md`: "Default 50, capped at 500"). */
const MAX_PAGE_SIZE = 500;

export interface HistoryPage {
  records: HistoryEnvelope[];
  nextCursor: number | null;
}

export interface FetchHistoryOptions {
  /** Inclusive lower bound on `emitted_at` (epoch ms). */
  since?: number;
  /** Exclusive upper bound on `emitted_at` (epoch ms). */
  until?: number;
  /** Only this host. */
  host?: string;
  /** Page-walk cap. Default 10 pages (up to 5000 records). */
  maxPages?: number;
  /** Injectable for tests; defaults to the global `fetch`. */
  fetchImpl?: typeof fetch;
  signal?: AbortSignal;
}

/**
 * Walk `GET /api/history` newest-first and return every record fetched.
 *
 * Throws on a non-2xx response (the caller renders the message); a malformed
 * body yields an empty page rather than an exception, since one bad page
 * should not discard the pages already collected.
 */
export async function fetchHistory(options: FetchHistoryOptions = {}): Promise<HistoryEnvelope[]> {
  const doFetch = options.fetchImpl ?? globalThis.fetch;
  const maxPages = options.maxPages ?? 10;
  const collected: HistoryEnvelope[] = [];
  let cursor: number | null = null;

  for (let page = 0; page < maxPages; page += 1) {
    const params = new URLSearchParams();
    params.set("limit", String(MAX_PAGE_SIZE));
    if (options.since !== undefined) params.set("since", new Date(options.since).toISOString());
    if (options.until !== undefined) params.set("until", new Date(options.until).toISOString());
    if (options.host !== undefined) params.set("host", options.host);
    if (cursor !== null) params.set("cursor", String(cursor));

    const response = await doFetch(`${API_PREFIX}/history?${params.toString()}`, {
      signal: options.signal,
      headers: { accept: "application/json" },
    });
    if (!response.ok) {
      throw new Error(`GET ${API_PREFIX}/history returned ${response.status}`);
    }

    const parsed = await parsePage(response);
    collected.push(...parsed.records);
    if (parsed.nextCursor === null || parsed.records.length === 0) break;
    cursor = parsed.nextCursor;
  }

  return collected;
}

async function parsePage(response: Response): Promise<HistoryPage> {
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    return { records: [], nextCursor: null };
  }
  if (typeof body !== "object" || body === null) return { records: [], nextCursor: null };

  const raw = (body as { records?: unknown }).records;
  const records: HistoryEnvelope[] = [];
  if (Array.isArray(raw)) {
    for (const entry of raw) {
      const envelope = narrowEnvelope(entry);
      if (envelope) records.push(envelope);
    }
  }
  const nextCursorRaw = (body as { nextCursor?: unknown }).nextCursor;
  const nextCursor = typeof nextCursorRaw === "number" && Number.isFinite(nextCursorRaw) ? nextCursorRaw : null;
  return { records, nextCursor };
}

/** Keep only rows with the three fields every downstream consumer requires
 * (`id`, `hostId`, `kind`); anything else is a row this build cannot place. */
function narrowEnvelope(value: unknown): HistoryEnvelope | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const row = value as Record<string, unknown>;
  if (typeof row.id !== "number" || typeof row.hostId !== "string" || typeof row.kind !== "string") {
    return undefined;
  }
  const record =
    typeof row.record === "object" && row.record !== null && !Array.isArray(row.record)
      ? (row.record as Record<string, unknown>)
      : {};
  return {
    id: row.id,
    emittedAt: typeof row.emittedAt === "string" ? row.emittedAt : "",
    hostId: row.hostId,
    kind: row.kind,
    repo: typeof row.repo === "string" ? row.repo : null,
    visibility: typeof row.visibility === "string" ? row.visibility : null,
    issue: typeof row.issue === "number" ? row.issue : null,
    sweepId: typeof row.sweepId === "string" ? row.sweepId : null,
    record,
  };
}
