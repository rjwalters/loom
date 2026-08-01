/**
 * Fixture builders for the token/cost analytics suite (issue #4752).
 *
 * These produce `GET /api/history` envelopes in exactly the shape
 * `../../docs/query-api.md` documents, with record payloads matching
 * `.loom/docs/telemetry-schema.md` — so a test that passes here is a test
 * against the documented contract, not against the parser's convenience.
 *
 * Every builder takes epoch ms and serializes to RFC 3339, because the wire
 * format is a string and the parser's `Date.parse` handling is part of what is
 * under test.
 */

import type { HistoryEnvelope } from "../src/analytics/types.js";

/** A convenient, readable base instant: 2026-07-30T12:00:00Z. */
export const T0 = Date.parse("2026-07-30T12:00:00Z");

export const MINUTE = 60 * 1000;
export const HOUR = 60 * MINUTE;

let nextId = 1;

/** Reset the monotonic envelope id counter between tests that assert on ids. */
export function resetIds(): void {
  nextId = 1;
}

/**
 * Index into an array, asserting the element exists.
 *
 * `noUncheckedIndexedAccess` is on, so `items[0]` is `T | undefined`.
 * Assertions that merely *read* a field use optional chaining (the convention
 * the sibling suites use — see `test/timelineBuilder.test.ts`); this helper is
 * for the handful of places where the element is a function argument or an
 * assignment target, where a `?.` would quietly change what the test asserts.
 */
export function at<T>(items: readonly T[], index: number): T {
  const value = items[index];
  if (value === undefined) {
    throw new Error(`expected an element at index ${index}, but the array has ${items.length}`);
  }
  return value;
}

export interface AccountFixture {
  account: string;
  rank?: number;
  usage?: number;
  resetAt?: number;
  exhausted?: boolean;
}

/** One `tokens.snapshot` history envelope. `usage`/`rank`/`resetAt` are omitted
 * from the payload when undefined — mirroring the daemon's "omit when unknown"
 * contract, which is what makes "unknown != zero" testable. */
export function tokensSnapshot(
  at: number,
  accounts: readonly AccountFixture[],
  hostId = "host-a",
): HistoryEnvelope {
  return {
    id: nextId++,
    emittedAt: new Date(at).toISOString(),
    hostId,
    kind: "tokens.snapshot",
    repo: null,
    visibility: "private",
    issue: null,
    sweepId: null,
    record: {
      kind: "tokens.snapshot",
      captured_at: new Date(at).toISOString(),
      accounts: accounts.map((account) => ({
        account: account.account,
        ...(account.rank !== undefined && { rank: account.rank }),
        ...(account.usage !== undefined && { usage_fraction: account.usage }),
        ...(account.resetAt !== undefined && {
          limit_window_reset_at: new Date(account.resetAt).toISOString(),
        }),
        exhausted: account.exhausted ?? false,
      })),
    },
  };
}

export interface PoolFixture {
  accountCount: number;
  exhaustedCount?: number;
  meanUsage?: number;
  maxUsage?: number;
  nextResetAt?: number;
}

/**
 * One `tokens.snapshot` history envelope in the `/public/history` aggregate
 * shape (`../../docs/query-api.md`'s "GET /api/history / /public/history"
 * section) — no `accounts[]` at all, only the pool-wide summary
 * `../src/redaction.ts`'s `deriveTokenPoolAggregate` computes. `meanUsage` /
 * `maxUsage` / `nextResetAt` are omitted (serialized as `null`, matching the
 * backend's own "measured but none reported" contract) when undefined, the
 * aggregate counterpart of `tokensSnapshot`'s "omit when unknown" convention.
 */
export function poolTokensSnapshot(at: number, pool: PoolFixture, hostId = "host-a"): HistoryEnvelope {
  return {
    id: nextId++,
    emittedAt: new Date(at).toISOString(),
    hostId,
    kind: "tokens.snapshot",
    repo: null,
    visibility: "private",
    issue: null,
    sweepId: null,
    record: {
      kind: "tokens.snapshot",
      captured_at: new Date(at).toISOString(),
      account_count: pool.accountCount,
      exhausted_count: pool.exhaustedCount ?? 0,
      mean_usage_fraction: pool.meanUsage ?? null,
      max_usage_fraction: pool.maxUsage ?? null,
      next_limit_window_reset_at: pool.nextResetAt !== undefined ? new Date(pool.nextResetAt).toISOString() : null,
    },
  };
}

export interface SweepFixture {
  sweepId: string;
  repo: string;
  startedAt: number;
  completedAt?: number;
  model?: string;
  issue?: number;
  hostId?: string;
}

/** The `sweep.started` (+ optional `sweep.completed`) envelopes for one sweep. */
export function sweepRecords(sweep: SweepFixture): HistoryEnvelope[] {
  const hostId = sweep.hostId ?? "host-a";
  const base = {
    repo: sweep.repo,
    visibility: "public" as const,
    issue: sweep.issue ?? null,
    sweepId: sweep.sweepId,
  };

  const records: HistoryEnvelope[] = [
    {
      id: nextId++,
      emittedAt: new Date(sweep.startedAt).toISOString(),
      hostId,
      kind: "sweep.started",
      ...base,
      record: {
        kind: "sweep.started",
        repo: sweep.repo,
        visibility: "public",
        ...(sweep.issue !== undefined && { issue: sweep.issue }),
        sweep_id: sweep.sweepId,
        started_at: new Date(sweep.startedAt).toISOString(),
        ...(sweep.model !== undefined && { model: sweep.model }),
      },
    },
  ];

  if (sweep.completedAt !== undefined) {
    records.push({
      id: nextId++,
      emittedAt: new Date(sweep.completedAt).toISOString(),
      hostId,
      kind: "sweep.completed",
      ...base,
      record: {
        kind: "sweep.completed",
        repo: sweep.repo,
        visibility: "public",
        ...(sweep.issue !== undefined && { issue: sweep.issue }),
        sweep_id: sweep.sweepId,
        completed_at: new Date(sweep.completedAt).toISOString(),
        result: "success",
      },
    });
  }

  return records;
}

/** Emulate the API's newest-first ordering, which every consumer must tolerate. */
export function newestFirst(records: readonly HistoryEnvelope[]): HistoryEnvelope[] {
  return [...records].sort((a, b) => b.id - a.id);
}
