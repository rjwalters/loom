/**
 * Wire + domain types for the token/cost analytics view (Epic #4702, Phase 3,
 * issue #4752).
 *
 * **Source of truth is the backend, not this file.** The record payloads below
 * are re-declarations of `.loom/docs/telemetry-schema.md`'s `tokens.snapshot`
 * / `sweep.started` / `sweep.completed` / `sweep.outcome` sections, and the
 * envelope mirrors `../../src/query.ts`'s `HistoryRecord` as documented in
 * `../../docs/query-api.md`. They are re-declared rather than imported across
 * the package boundary because the backend modules compile against
 * `@cloudflare/workers-types` (they reference `D1Database` /
 * `DurableObjectState` globals), which a browser bundle has no business
 * depending on.
 *
 * Everything the daemon may omit is optional here. The telemetry schema is
 * explicitly additive/forward-compatible: an older daemon omits fields a newer
 * one sends, and a newer daemon sends fields this build has never heard of.
 * `parse.ts` narrows the wire JSON field by field and drops anything
 * wrong-typed, so a partial or future record degrades to "unknown" rather than
 * throwing — and every computation in this directory treats an absent field as
 * unknown, never as zero.
 */

// ---------------------------------------------------------------------------
// Wire shapes (`GET /api/history`)
// ---------------------------------------------------------------------------

/** One account inside a `tokens.snapshot`. Only `exhausted` is always sent;
 * `rank` / `usage_fraction` / `limit_window_reset_at` are omitted when the
 * daemon does not know them. */
export interface TokenAccountPayload {
  account?: string;
  rank?: number;
  usage_fraction?: number;
  limit_window_reset_at?: string;
  exhausted?: boolean;
}

/** `tokens.snapshot` — point-in-time view of the multi-account token pool.
 * Host-level: it carries no `repo`, which is the whole reason per-repo
 * attribution needs the join in `attribution.ts`. */
export interface TokensSnapshotPayload {
  kind?: string;
  captured_at?: string;
  accounts?: TokenAccountPayload[];
}

/**
 * The shape `/public/history` sends in place of `TokensSnapshotPayload`'s
 * `accounts` array (`../../src/redaction.ts`'s `deriveTokenPoolAggregate`,
 * documented in `../../docs/query-api.md`'s "GET /api/history /
 * /public/history" section). Non-identifying by construction: it names no
 * account, only how loaded the pool is as a whole (issue #4847).
 */
export interface TokenPoolAggregatePayload {
  kind?: string;
  captured_at?: string;
  account_count?: number;
  exhausted_count?: number;
  /** `null` — never `0` — when no account reported a `usage_fraction`. */
  mean_usage_fraction?: number | null;
  max_usage_fraction?: number | null;
  next_limit_window_reset_at?: string | null;
}

/** The subset of the `HistoryRecord` envelope this view reads. Fields the
 * backend nulls for a redacted record (`repo`, `issue`, `sweepId`) are
 * `null`-able here for exactly that reason. */
export interface HistoryEnvelope {
  id: number;
  emittedAt: string;
  hostId: string;
  kind: string;
  repo?: string | null;
  visibility?: string | null;
  issue?: number | null;
  sweepId?: string | null;
  record: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Domain shapes (post-`parse.ts`)
// ---------------------------------------------------------------------------

/** One `tokens.snapshot` narrowed to what the analytics need: a host, an
 * instant (epoch ms), and the per-account readings at that instant. */
export interface TokenSample {
  hostId: string;
  /** Epoch ms of the record's own `captured_at` (daemon clock), falling back
   * to the envelope's `emittedAt` when `captured_at` is absent/unparseable. */
  at: number;
  accounts: AccountReading[];
}

export interface AccountReading {
  account: string;
  rank?: number;
  /** `usage_fraction` in `[0, 1]`. Absent when the daemon could not measure
   * it — treated as unknown (the sample is skipped for burn/forecast), never
   * as `0`. */
  usageFraction?: number;
  /** Epoch ms of `limit_window_reset_at`, when known. */
  limitWindowResetAt?: number;
  exhausted: boolean;
}

/** One `tokens.snapshot` narrowed from {@link TokenPoolAggregatePayload}: a
 * host, an instant, and the pool-wide stats — no account identity anywhere.
 * The public-surface counterpart of {@link TokenSample} (issue #4847). */
export interface PoolSample {
  hostId: string;
  /** Epoch ms — same fallback rule as {@link TokenSample.at}. */
  at: number;
  accountCount: number;
  exhaustedCount: number;
  /** Mean/peak `usage_fraction` across the accounts that reported one.
   * Absent — never `0` — when none did. */
  meanUsageFraction?: number;
  maxUsageFraction?: number;
  /** Epoch ms of the earliest limit-window reset across the pool, when any
   * account reported one. */
  nextLimitWindowResetAt?: number;
}

/** A sweep's active window, reconstructed from its lifecycle records. This is
 * the only record family that carries `repo`/`model`, so it is the sole
 * bridge between fleet-wide token usage and a repository. */
export interface SweepWindow {
  hostId: string;
  sweepId: string;
  repo: string;
  model?: string;
  issue?: number;
  /** Epoch ms — `sweep.started.started_at`. */
  startedAt: number;
  /** Epoch ms — `sweep.completed.completed_at`, or `undefined` while the
   * sweep is still in flight (see `attribution.ts`'s open-window cap). */
  endedAt?: number;
}
