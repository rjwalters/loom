/**
 * Read-side query API + live tail (Epic #4702, Phase 2 — issue #4726).
 *
 * Builds on the D1 history store and `FleetState` Durable Object #4725
 * maintains (`src/index.ts` / `src/fleetState.ts`) — this module adds no new
 * storage, only read paths over what already exists:
 *
 *   - `queryHistory` — filterable, paginated queries over the `records`
 *     table (host/repo/model/result/time-range), the data source for the
 *     Phase-3 dashboard's historical charts and token/cost analytics.
 *   - `createLiveTailStream` — an SSE stream of newly-ingested records,
 *     framed the same way the frozen daemon `sweep.*` SSE topics are
 *     (`data: {"topic": ..., "event": {...}}\n\n`, see
 *     `loom-daemon/src/serve.rs`'s `sse_frame`) — extended here with a
 *     `hostId` on every event, since this is a multi-host fleet tail rather
 *     than a single daemon's own bus.
 *
 * **Unclassified surface, by design**: everything here returns full record
 * detail regardless of the stored `visibility` tag. Visibility-based
 * redaction (public vs. authenticated views) is issue #4727's job, as a
 * wrapper in front of this API — this module deliberately does not
 * implement it, matching the issue's stated scope boundary.
 */

// ---------------------------------------------------------------------------
// History query (`GET /api/history`)
// ---------------------------------------------------------------------------

/** Parsed, validated filter for a `queryHistory` call. `limit` always has a
 * value (defaulted); every other field is present only when the caller
 * supplied it. */
export interface HistoryFilter {
  host?: string;
  repo?: string;
  kind?: string;
  model?: string;
  result?: string;
  /** Inclusive lower bound on `emitted_at` (RFC 3339). */
  since?: string;
  /** Exclusive upper bound on `emitted_at` (RFC 3339). */
  until?: string;
  limit: number;
  /** Keyset pagination cursor — the `id` of the last record from the
   * previous page; this page returns rows with `id` strictly less than it
   * (rows are always ordered newest-first, by `id` descending). */
  cursor?: number;
}

/** A decoded `records` row, camelCased and with `payload` parsed back into
 * `record` — matches the convention `fleetState.ts`'s `FleetSnapshot` JSON
 * already uses (camelCase), not the snake_case ingest wire format. */
export interface HistoryRecord {
  id: number;
  schemaVersion: number;
  emittedAt: string;
  hostId: string;
  kind: string;
  repo: string | null;
  visibility: string;
  issue: number | null;
  sweepId: string | null;
  ingestedAt: string;
  record: Record<string, unknown>;
}

export interface HistoryQueryResult {
  records: HistoryRecord[];
  /** The `cursor` value to pass for the next page, or `null` when this page
   * reached the end of the matching result set. */
  nextCursor: number | null;
}

export interface HistoryQueryError {
  error: string;
}

/** Default/maximum page size for `GET /api/history`. A caller may request
 * fewer via `?limit=`, never more than `MAX_HISTORY_LIMIT` — an unbounded
 * `limit` would let one request force a full-table-scan-sized response. */
export const DEFAULT_HISTORY_LIMIT = 50;
export const MAX_HISTORY_LIMIT = 500;

/** Parse+validate `GET /api/history`'s query-string params into a
 * `HistoryFilter`, or a `{ error }` describing the first invalid param. Every
 * filter param is optional; an absent one simply omits that predicate. */
export function parseHistoryQuery(params: URLSearchParams): HistoryFilter | HistoryQueryError {
  const filter: HistoryFilter = { limit: DEFAULT_HISTORY_LIMIT };

  const host = params.get("host");
  if (host) filter.host = host;

  const repo = params.get("repo");
  if (repo) filter.repo = repo;

  const kind = params.get("kind");
  if (kind) filter.kind = kind;

  const model = params.get("model");
  if (model) filter.model = model;

  const result = params.get("result");
  if (result) filter.result = result;

  const since = params.get("since");
  if (since !== null) {
    if (Number.isNaN(Date.parse(since))) return { error: "since must be an RFC 3339 datetime" };
    filter.since = since;
  }

  const until = params.get("until");
  if (until !== null) {
    if (Number.isNaN(Date.parse(until))) return { error: "until must be an RFC 3339 datetime" };
    filter.until = until;
  }

  const limitParam = params.get("limit");
  if (limitParam !== null) {
    const parsed = Number.parseInt(limitParam, 10);
    if (!Number.isInteger(parsed) || parsed <= 0) return { error: "limit must be a positive integer" };
    filter.limit = Math.min(parsed, MAX_HISTORY_LIMIT);
  }

  const cursorParam = params.get("cursor");
  if (cursorParam !== null) {
    const parsed = Number.parseInt(cursorParam, 10);
    if (!Number.isInteger(parsed) || parsed <= 0) return { error: "cursor must be a positive integer record id" };
    filter.cursor = parsed;
  }

  return filter;
}

/** Raw shape of one `records` row as D1 returns it (snake_case column
 * names) — see `migrations/0001_init.sql`. */
interface RawRecordRow {
  id: number;
  schema_version: number;
  emitted_at: string;
  host_id: string;
  kind: string;
  repo: string | null;
  visibility: string;
  issue: number | null;
  sweep_id: string | null;
  payload: string;
  ingested_at: string;
}

function rowToHistoryRecord(row: RawRecordRow): HistoryRecord {
  let record: Record<string, unknown>;
  try {
    record = JSON.parse(row.payload) as Record<string, unknown>;
  } catch {
    // The `payload` column is only ever written by `handleIngest` (always
    // `JSON.stringify` of a validated object) — this branch should be
    // unreachable, but a query surface must never 500 on a bad row.
    record = {};
  }
  return {
    id: row.id,
    schemaVersion: row.schema_version,
    emittedAt: row.emitted_at,
    hostId: row.host_id,
    kind: row.kind,
    repo: row.repo,
    visibility: row.visibility,
    issue: row.issue,
    sweepId: row.sweep_id,
    ingestedAt: row.ingested_at,
    record,
  };
}

/**
 * Run a filtered, paginated query over the `records` table.
 *
 * `model`/`result` are not indexed columns (they live inside the
 * free-form `payload` JSON — see `.loom/docs/telemetry-schema.md`'s
 * `sweep.started`/`sweep.completed`/`sweep.outcome` shapes) so those two
 * predicates use D1's `json_extract` (SQLite's JSON1 extension, which D1
 * supports) rather than a dedicated column. `host`/`repo`/the time range use
 * the indexed columns `idx_records_host_time` / `idx_records_repo_time` /
 * `idx_records_ingested_at` migration already provides.
 *
 * Pagination is keyset-based on `id` (rows are always returned newest-first):
 * pass the previous page's `nextCursor` back as `cursor` to continue. This is
 * O(1) per page (no `OFFSET` scan) and stable under concurrent inserts (a new
 * row never shifts an already-issued cursor's meaning).
 */
export async function queryHistory(db: D1Database, filter: HistoryFilter): Promise<HistoryQueryResult> {
  const clauses: string[] = [];
  const bindings: unknown[] = [];

  if (filter.host) {
    clauses.push("host_id = ?");
    bindings.push(filter.host);
  }
  if (filter.repo) {
    clauses.push("repo = ?");
    bindings.push(filter.repo);
  }
  if (filter.kind) {
    clauses.push("kind = ?");
    bindings.push(filter.kind);
  }
  if (filter.model) {
    clauses.push("json_extract(payload, '$.model') = ?");
    bindings.push(filter.model);
  }
  if (filter.result) {
    clauses.push("json_extract(payload, '$.result') = ?");
    bindings.push(filter.result);
  }
  if (filter.since) {
    clauses.push("emitted_at >= ?");
    bindings.push(filter.since);
  }
  if (filter.until) {
    clauses.push("emitted_at < ?");
    bindings.push(filter.until);
  }
  if (filter.cursor !== undefined) {
    clauses.push("id < ?");
    bindings.push(filter.cursor);
  }

  const where = clauses.length > 0 ? `WHERE ${clauses.join(" AND ")}` : "";
  // Fetch one extra row beyond the page size — its presence (or absence)
  // tells us whether a next page exists without a separate COUNT query.
  const limitPlusOne = filter.limit + 1;
  const { results } = await db
    .prepare(`SELECT * FROM records ${where} ORDER BY id DESC LIMIT ?`)
    .bind(...bindings, limitPlusOne)
    .all<RawRecordRow>();

  const hasMore = results.length > filter.limit;
  const page = hasMore ? results.slice(0, filter.limit) : results;
  const records = page.map(rowToHistoryRecord);
  const lastRecord = records[records.length - 1];
  const nextCursor = hasMore && lastRecord ? lastRecord.id : null;

  return { records, nextCursor };
}

// ---------------------------------------------------------------------------
// Elastic compute spend (`GET /api/spend`) — Issue #8306, Phase 3 of #8257
// ---------------------------------------------------------------------------

/** The record kind this aggregation reads. Named rather than inlined because
 * three separate SQL predicates below depend on it matching the partial index
 * `migrations/0003_ephemeral_compute.sql` creates. */
export const EPHEMERAL_COMPUTE_KIND = "ephemeral_compute";

/** Parsed, validated filter for a {@link queryElasticSpend} call. Every field
 * is optional — an unfiltered call sums every `ephemeral_compute` completion
 * record in the table. */
export interface ElasticSpendFilter {
  /** Inclusive lower bound on `emitted_at` (RFC 3339). */
  since?: string;
  /** Exclusive upper bound on `emitted_at` (RFC 3339). */
  until?: string;
  /** Only the named emitting host (the synthetic elastic-fleet `host_id` — see
   * `defaults/docs/observability.md` §5d). */
  host?: string;
}

/** One UTC day's spend within the queried window. Only days that actually had
 * a completed job appear — a gap is a day with no spend, and the renderer
 * decides whether to draw it as a zero or skip it (a zero here would be
 * indistinguishable from "a job that cost nothing"). */
export interface ElasticSpendDay {
  /** UTC calendar day, `YYYY-MM-DD`. */
  day: string;
  costUsd: number;
  jobCount: number;
}

/** Aggregate `ephemeral_compute` spend over a time window. */
export interface ElasticSpendSummary {
  /** Echo of the requested window, so a renderer can label the period without
   * re-deriving it from its own request. `null` for an open-ended bound. */
  since: string | null;
  until: string | null;
  /** Summed `estimated_cost_usd` across every completed job in the window. */
  totalCostUsd: number;
  /** Completed jobs in the window — i.e. records carrying a numeric
   * `estimated_cost_usd`. A launch record has none (the job has not finished,
   * so there is no cost yet) and is deliberately excluded: counting it would
   * inflate the job count with rows contributing `0` to the total. Jobs still
   * running are the "running now" panel's subject, sourced from the Durable
   * Object rather than from here. */
  jobCount: number;
  /** Summed `wall_clock_sec` across those jobs, or `null` when none reported
   * one — never a fabricated `0` (the "unknown != zero" contract every other
   * surface in this backend follows). */
  totalWallClockSec: number | null;
  /** The largest single-day `costUsd` in the window, or `null` when the window
   * had no spend at all. This is the number an operator compares against a
   * standing per-day spot ceiling — a window total cannot answer "did any day
   * breach the cap", and an average hides exactly the day that did. */
  peakDailyCostUsd: number | null;
  /** Per-UTC-day breakdown, oldest first. */
  days: ElasticSpendDay[];
}

/** Parse+validate `GET /api/spend`'s query-string params, or a `{ error }`
 * describing the first invalid one. Mirrors {@link parseHistoryQuery}'s
 * contract exactly (same param names, same RFC 3339 validation) so the two
 * read surfaces stay one idiom rather than two. */
export function parseElasticSpendQuery(
  params: URLSearchParams,
): ElasticSpendFilter | HistoryQueryError {
  const filter: ElasticSpendFilter = {};

  const host = params.get("host");
  if (host) filter.host = host;

  const since = params.get("since");
  if (since !== null) {
    if (Number.isNaN(Date.parse(since))) return { error: "since must be an RFC 3339 datetime" };
    filter.since = since;
  }

  const until = params.get("until");
  if (until !== null) {
    if (Number.isNaN(Date.parse(until))) return { error: "until must be an RFC 3339 datetime" };
    filter.until = until;
  }

  return filter;
}

/** Round a dollar figure to 4 decimal places. Summing IEEE-754 floats in
 * SQLite produces trailing noise (`3.6899999999999995`) that would render as
 * a nonsense precision; 4 places keeps sub-cent resolution — spot pricing is
 * quoted per-hour to 4-6 places, so a short job's real cost can be a fraction
 * of a cent — while dropping the artifact. */
function roundUsd(value: number): number {
  return Math.round(value * 10_000) / 10_000;
}

/** Raw shape of one grouped row from the aggregation below. SQLite's `SUM`
 * returns `NULL` for an empty group, which cannot happen here (a group exists
 * only because a row matched) but is typed honestly anyway. */
interface RawSpendDayRow {
  day: string | null;
  cost_usd: number | null;
  job_count: number | null;
  wall_clock_sec: number | null;
  wall_clock_reported: number | null;
}

/**
 * Sum `ephemeral_compute` spend over a window, bucketed by UTC day.
 *
 * **Which rows count.** Exactly the rows whose payload carries a *numeric*
 * `estimated_cost_usd` — enforced with `json_type(...) IN ('integer','real')`
 * rather than a bare `IS NOT NULL`, because `json_extract` happily returns a
 * string for a malformed payload and SQLite's `SUM` silently coerces one to
 * `0`, which would understate the total rather than fail visibly. Per
 * `migrations/0003_ephemeral_compute.sql`, a job emits two records — a launch
 * record (no cost yet) and a completion record (cost, wall clock, `ended_at`)
 * — so this predicate is also what makes the aggregation count each job once.
 *
 * **Why `emitted_at`, not `ended_at`.** `emitted_at` is the envelope field the
 * `(kind, emitted_at)` index that same migration adds is built on, and it is
 * the field `since`/`until` filter on everywhere else in this module, so day
 * bucketing and window filtering agree by construction. A completion record is
 * emitted at completion, so the two are the same instant in practice.
 *
 * **UTC day bucketing** is `substr(emitted_at, 1, 10)` — the daemon and every
 * other emitter write RFC 3339 with a `Z` offset (see
 * `.loom/docs/telemetry-schema.md`), so the leading 10 characters *are* the
 * UTC calendar day. A row written with a non-`Z` offset would bucket by its
 * local day; that is a deliberate accepted approximation rather than a
 * `datetime(...)` conversion, because converting would make the expression
 * non-sargable and force a full scan of the kind's rows.
 */
export async function queryElasticSpend(
  db: D1Database,
  filter: ElasticSpendFilter = {},
): Promise<ElasticSpendSummary> {
  const clauses: string[] = [
    "kind = ?",
    "json_type(payload, '$.estimated_cost_usd') IN ('integer', 'real')",
  ];
  const bindings: unknown[] = [EPHEMERAL_COMPUTE_KIND];

  if (filter.host) {
    clauses.push("host_id = ?");
    bindings.push(filter.host);
  }
  if (filter.since) {
    clauses.push("emitted_at >= ?");
    bindings.push(filter.since);
  }
  if (filter.until) {
    clauses.push("emitted_at < ?");
    bindings.push(filter.until);
  }

  const { results } = await db
    .prepare(
      `SELECT substr(emitted_at, 1, 10) AS day,
              SUM(json_extract(payload, '$.estimated_cost_usd')) AS cost_usd,
              COUNT(*) AS job_count,
              SUM(CASE WHEN json_type(payload, '$.wall_clock_sec') IN ('integer', 'real')
                       THEN json_extract(payload, '$.wall_clock_sec') ELSE 0 END) AS wall_clock_sec,
              SUM(CASE WHEN json_type(payload, '$.wall_clock_sec') IN ('integer', 'real')
                       THEN 1 ELSE 0 END) AS wall_clock_reported
         FROM records
        WHERE ${clauses.join(" AND ")}
        GROUP BY day
        ORDER BY day ASC`,
    )
    .bind(...bindings)
    .all<RawSpendDayRow>();

  const days: ElasticSpendDay[] = [];
  let totalCostUsd = 0;
  let jobCount = 0;
  let totalWallClockSec = 0;
  let wallClockReported = 0;

  for (const row of results) {
    const costUsd = typeof row.cost_usd === "number" ? row.cost_usd : 0;
    const dayJobCount = typeof row.job_count === "number" ? row.job_count : 0;
    totalCostUsd += costUsd;
    jobCount += dayJobCount;
    totalWallClockSec += typeof row.wall_clock_sec === "number" ? row.wall_clock_sec : 0;
    wallClockReported += typeof row.wall_clock_reported === "number" ? row.wall_clock_reported : 0;
    days.push({ day: row.day ?? "", costUsd: roundUsd(costUsd), jobCount: dayJobCount });
  }

  return {
    since: filter.since ?? null,
    until: filter.until ?? null,
    totalCostUsd: roundUsd(totalCostUsd),
    jobCount,
    // `0` here would claim every job ran instantaneously; absence of the field
    // on every row means the duration is unknown, not zero.
    totalWallClockSec: wallClockReported > 0 ? totalWallClockSec : null,
    peakDailyCostUsd: days.length > 0 ? roundUsd(Math.max(...days.map((day) => day.costUsd))) : null,
    days,
  };
}

// ---------------------------------------------------------------------------
// Live tail (`GET /api/events`)
// ---------------------------------------------------------------------------

/** Optional filters for a live-tail connection — a subset of
 * `HistoryFilter`'s predicates (the ones cheap to apply per-poll against the
 * indexed columns; `model`/`result` filtering on a live tail is left to the
 * client for now, same as the reference daemon SSE bridge does no
 * server-side payload filtering beyond topic prefix). */
export interface LiveTailFilter {
  host?: string;
  repo?: string;
}

export interface LiveTailOptions {
  /** How often to poll D1 for rows ingested since the last poll. */
  pollIntervalMs?: number;
  /** How often to emit a `: keepalive` comment when no new rows arrive —
   * mirrors `loom-daemon serve`'s `SSE_HEARTBEAT_INTERVAL` so intermediaries
   * never reap an idle connection. */
  heartbeatIntervalMs?: number;
  /** Aborted when the client disconnects (`Request.signal`) — stops the
   * poll loop and closes the stream. */
  signal?: AbortSignal;
}

/** Reconnect delay advertised via the SSE `retry:` field — same value the
 * daemon's `/api/events` bridge uses (`loom-daemon/src/serve.rs`). */
export const LIVE_TAIL_RETRY_MS = 3_000;

/** Default poll interval — the "low latency" AC without needing a
 * Durable-Object-side socket fan-out: at this cadence a newly-ingested
 * record reaches connected clients within ~1s. */
export const LIVE_TAIL_DEFAULT_POLL_INTERVAL_MS = 1_000;

/** Default heartbeat cadence when no new rows are flowing. */
export const LIVE_TAIL_DEFAULT_HEARTBEAT_INTERVAL_MS = 15_000;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Render one `records` row as an SSE frame, in the same
 * `data: {"topic": ..., "event": {...}}\n\n` shape
 * `loom-daemon/src/serve.rs`'s `sse_frame` emits for the frozen `sweep.*`
 * topics — `topic` is the record's own `kind` (already exactly `sweep.
 * started`/`sweep.phase`/`sweep.completed` for the topics that overlap the
 * frozen taxonomy), and `event` carries the multi-host extension
 * (`hostId`) alongside the envelope fields. */
function sseFrameForRecord(row: HistoryRecord): string {
  const payload = {
    topic: row.kind,
    event: {
      hostId: row.hostId,
      emittedAt: row.emittedAt,
      schemaVersion: row.schemaVersion,
      record: row.record,
    },
  };
  return `data: ${JSON.stringify(payload)}\n\n`;
}

/**
 * Build an SSE `ReadableStream` that tails newly-ingested `records` rows.
 *
 * Implementation note: rather than a Durable-Object-side socket registry
 * (which would need the WebSocket Hibernation API to survive DO eviction
 * across a long-lived connection), this tails D1 directly via a short poll
 * loop scoped to *this* stream's lifetime — simpler, and D1's `id` is
 * already a strictly-increasing cursor with an index
 * (`idx_records_ingested_at` for the underlying insert order), so the query
 * cost per poll is a small indexed range scan, not a table scan. Only rows
 * inserted **after** the stream opens are delivered — replaying prior
 * history is `GET /api/history`'s job, not this one's.
 */
export function createLiveTailStream(
  db: D1Database,
  filter: LiveTailFilter,
  options: LiveTailOptions = {},
): ReadableStream<Uint8Array> {
  const pollIntervalMs = options.pollIntervalMs ?? LIVE_TAIL_DEFAULT_POLL_INTERVAL_MS;
  const heartbeatIntervalMs = options.heartbeatIntervalMs ?? LIVE_TAIL_DEFAULT_HEARTBEAT_INTERVAL_MS;
  const encoder = new TextEncoder();
  // Shared mutable state between `start` (the polling loop) and `cancel`
  // (invoked when the client disconnects) — `cancel` cannot reach into
  // `start`'s local scope directly, so both close over this instead.
  const state = { closed: false };

  return new ReadableStream<Uint8Array>({
    async start(controller) {
      const onAbort = () => {
        state.closed = true;
      };
      options.signal?.addEventListener("abort", onAbort);
      // A function, not a re-narrowed inline expression: TS's control-flow
      // narrowing otherwise treats `options.signal?.aborted` as unable to
      // change back to `true` once compared against inside the loop below,
      // even though the underlying `AbortSignal` is genuinely mutable.
      const isAborted = (): boolean => options.signal?.aborted === true;

      const enqueue = (chunk: string): boolean => {
        try {
          controller.enqueue(encoder.encode(chunk));
          return true;
        } catch {
          // The consumer has gone away and the controller can no longer
          // accept data — stop the loop rather than throwing out of `start`.
          return false;
        }
      };

      enqueue(`retry: ${LIVE_TAIL_RETRY_MS}\n: connected to loom fleet telemetry live tail\n\n`);

      const maxRow = await db.prepare("SELECT MAX(id) AS maxId FROM records").first<{ maxId: number | null }>();
      let lastId = maxRow?.maxId ?? 0;
      let lastActivityAt = Date.now();

      while (!state.closed && !isAborted()) {
        await sleep(pollIntervalMs);
        if (state.closed || isAborted()) break;

        const clauses = ["id > ?"];
        const bindings: unknown[] = [lastId];
        if (filter.host) {
          clauses.push("host_id = ?");
          bindings.push(filter.host);
        }
        if (filter.repo) {
          clauses.push("repo = ?");
          bindings.push(filter.repo);
        }

        const { results } = await db
          .prepare(`SELECT * FROM records WHERE ${clauses.join(" AND ")} ORDER BY id ASC LIMIT 200`)
          .bind(...bindings)
          .all<RawRecordRow>();

        if (results.length > 0) {
          for (const row of results) {
            if (!enqueue(sseFrameForRecord(rowToHistoryRecord(row)))) {
              state.closed = true;
              break;
            }
          }
          const lastRow = results[results.length - 1];
          if (lastRow) lastId = lastRow.id;
          lastActivityAt = Date.now();
        } else if (Date.now() - lastActivityAt >= heartbeatIntervalMs) {
          if (!enqueue(": keepalive\n\n")) state.closed = true;
          lastActivityAt = Date.now();
        }
      }

      options.signal?.removeEventListener("abort", onAbort);
      try {
        controller.close();
      } catch {
        // Already closed (e.g. the consumer cancelled first) — nothing to do.
      }
    },
    cancel() {
      state.closed = true;
    },
  });
}
