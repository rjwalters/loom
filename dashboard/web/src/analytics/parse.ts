/**
 * Narrowing layer: raw `GET /api/history` / `GET /public/history` JSON → the
 * domain shapes in `types.ts` (Epic #4702, Phase 3, issue #4752; the
 * `/public/history` aggregate shape added in issue #4847).
 *
 * Every function here is total: it never throws on malformed input, it drops
 * what it cannot understand. That is a deliberate contract, not laziness —
 * the telemetry schema is additive and multi-version by design (hosts on
 * different daemon builds report into the same backend), so a single
 * wrong-typed field from one host must not blank the whole dashboard.
 *
 * The one rule every narrowing function obeys: **an absent or unparseable
 * measurement stays absent.** It is never coerced to `0`, because `0` means
 * "measured, and it is zero" everywhere downstream (a `usage_fraction` of 0 is
 * a fresh limit window; an unknown one must not be drawn as one).
 *
 * `tokens.snapshot` has two disjoint wire shapes depending on which route
 * served it (`../../docs/query-api.md`): `/api/history` sends per-account
 * rows (`parseTokenSample`), `/public/history` sends the non-identifying
 * pool aggregate (`parsePoolSample`). A page is never a mix of the two, but
 * both parsers are safe to run over any page regardless — each recognizes
 * only the shape it owns and returns `undefined` for the other.
 */

import type {
  AccountReading,
  HistoryEnvelope,
  PoolSample,
  SweepWindow,
  TokenPoolAggregatePayload,
  TokenSample,
  TokensSnapshotPayload,
} from "./types.js";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Parse an RFC 3339 datetime to epoch ms, or `undefined` if absent/invalid. */
export function parseTimestamp(value: unknown): number | undefined {
  if (typeof value !== "string" || value === "") return undefined;
  const ms = Date.parse(value);
  return Number.isNaN(ms) ? undefined : ms;
}

function parseFiniteNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/** Narrow one `tokens.snapshot` history record. Returns `undefined` when the
 * record has no usable timestamp or no account array at all. */
export function parseTokenSample(envelope: HistoryEnvelope): TokenSample | undefined {
  if (envelope.kind !== "tokens.snapshot") return undefined;
  const payload = envelope.record as TokensSnapshotPayload;
  // `captured_at` is the daemon's own clock; `emittedAt` is the envelope's.
  // Prefer the former (it is when the pool was actually sampled) and fall back
  // to the latter so a snapshot missing `captured_at` still plots.
  const at = parseTimestamp(payload?.captured_at) ?? parseTimestamp(envelope.emittedAt);
  if (at === undefined) return undefined;
  const rawAccounts = Array.isArray(payload?.accounts) ? payload.accounts : [];

  const accounts: AccountReading[] = [];
  for (const raw of rawAccounts) {
    if (!isRecord(raw)) continue;
    const account = typeof raw.account === "string" && raw.account !== "" ? raw.account : undefined;
    if (account === undefined) continue; // an unnamed account cannot be a series key
    const usageFraction = parseFiniteNumber(raw.usage_fraction);
    accounts.push({
      account,
      rank: parseFiniteNumber(raw.rank),
      usageFraction,
      limitWindowResetAt: parseTimestamp(raw.limit_window_reset_at),
      // `exhausted` is documented as always present; a missing/wrong-typed one
      // is read as "not exhausted" so a partial record never invents an alarm.
      exhausted: raw.exhausted === true,
    });
  }
  return { hostId: envelope.hostId, at, accounts };
}

/** Narrow every `tokens.snapshot` in a history page, oldest-first.
 *
 * `GET /api/history` returns newest-first (`id` descending); every consumer in
 * this directory wants chronological order, so sorting here means no caller
 * has to remember. Ties on `at` are broken by envelope `id` so the ordering is
 * total and stable even for two snapshots that share a `captured_at` second. */
export function parseTokenSamples(envelopes: readonly HistoryEnvelope[]): TokenSample[] {
  const withOrder: Array<{ sample: TokenSample; id: number }> = [];
  for (const envelope of envelopes) {
    const sample = parseTokenSample(envelope);
    if (sample) withOrder.push({ sample, id: envelope.id });
  }
  withOrder.sort((a, b) => a.sample.at - b.sample.at || a.id - b.id);
  return withOrder.map((entry) => entry.sample);
}

/**
 * Narrow one `tokens.snapshot` history record shaped as
 * {@link TokenPoolAggregatePayload} — the `/public/history` form
 * (`../../docs/query-api.md`), with no per-account detail at all (issue
 * #4847).
 *
 * Returns `undefined` when the record has no usable timestamp, **or when it
 * is not the aggregate shape** (no numeric `account_count`) — that is
 * exactly what an authenticated `accounts[]`-carrying record looks like, and
 * `parseTokenSample` above owns that shape. The two parsers are total and
 * disjoint on purpose: a caller can run both over the same page and each
 * only claims the records it understands, never throwing on the other's.
 */
export function parsePoolSample(envelope: HistoryEnvelope): PoolSample | undefined {
  if (envelope.kind !== "tokens.snapshot") return undefined;
  const payload = envelope.record as TokenPoolAggregatePayload;
  const at = parseTimestamp(payload?.captured_at) ?? parseTimestamp(envelope.emittedAt);
  if (at === undefined) return undefined;

  const accountCount = parseFiniteNumber(payload?.account_count);
  if (accountCount === undefined) return undefined;

  return {
    hostId: envelope.hostId,
    at,
    accountCount,
    // `exhausted_count` is a count the backend always computes (defaulting to
    // 0), never an unmeasured probe — unlike the usage fractions below, an
    // absent/malformed value here reads as 0, not "unknown".
    exhaustedCount: parseFiniteNumber(payload?.exhausted_count) ?? 0,
    meanUsageFraction: parseFiniteNumber(payload?.mean_usage_fraction),
    maxUsageFraction: parseFiniteNumber(payload?.max_usage_fraction),
    nextLimitWindowResetAt: parseTimestamp(payload?.next_limit_window_reset_at),
  };
}

/** Narrow every `tokens.snapshot` in a history page as pool aggregates,
 * oldest-first — the pool-level counterpart of {@link parseTokenSamples}. */
export function parsePoolSamples(envelopes: readonly HistoryEnvelope[]): PoolSample[] {
  const withOrder: Array<{ sample: PoolSample; id: number }> = [];
  for (const envelope of envelopes) {
    const sample = parsePoolSample(envelope);
    if (sample) withOrder.push({ sample, id: envelope.id });
  }
  withOrder.sort((a, b) => a.sample.at - b.sample.at || a.id - b.id);
  return withOrder.map((entry) => entry.sample);
}

/**
 * Reconstruct sweep windows from `sweep.started` / `sweep.completed` /
 * `sweep.outcome` records.
 *
 * Keyed by `hostId` + `sweep_id`: a sweep id embeds the issue number
 * (`sweep-issue-4703-0`) and is unique per host, but two hosts could in
 * principle mint the same id, and attribution must never fuse two hosts'
 * sweeps into one window.
 *
 * A record whose `repo` is absent or `null` is skipped — that is exactly what
 * a redacted `/public/history` response looks like for a private repo, and
 * attributing usage to a repo you are not allowed to see is the failure mode
 * this drop prevents. (The view is authenticated-only anyway; this is
 * defense-in-depth in the parse layer.)
 *
 * **Two passes, deliberately.** `GET /api/history` returns records
 * newest-first, so a sweep's `sweep.completed` arrives *before* its
 * `sweep.started`. A single-pass fold would drop every terminal record as "no
 * matching start" and leave every window open — which `attribution.ts` would
 * then cap at hours long and let absorb the fleet's usage. Starts are
 * collected first and terminal records applied second, so the result is
 * identical for any input ordering.
 */
export function parseSweepWindows(envelopes: readonly HistoryEnvelope[]): SweepWindow[] {
  const windows = new Map<string, SweepWindow>();

  // Pass 1 — `sweep.started` defines the window and its repo/model/issue.
  for (const envelope of envelopes) {
    if (envelope.kind !== "sweep.started") continue;
    const payload = isRecord(envelope.record) ? envelope.record : {};
    const sweepId = readSweepId(envelope, payload);
    const repo = readRepo(envelope, payload);
    if (sweepId === undefined || repo === undefined) continue;

    const startedAt = parseTimestamp(payload.started_at) ?? parseTimestamp(envelope.emittedAt);
    if (startedAt === undefined) continue;

    const key = `${envelope.hostId} ${sweepId}`;
    const existing = windows.get(key);
    windows.set(key, {
      hostId: envelope.hostId,
      sweepId,
      repo,
      model: typeof payload.model === "string" && payload.model !== "" ? payload.model : existing?.model,
      issue: parseFiniteNumber(payload.issue) ?? parseFiniteNumber(envelope.issue) ?? existing?.issue,
      // A duplicate start (a retried sweep reusing its id) keeps the earliest
      // instant, so the window still covers everything that sweep could burn.
      startedAt: existing === undefined ? startedAt : Math.min(existing.startedAt, startedAt),
      endedAt: existing?.endedAt,
    });
  }

  // Pass 2 — terminal records close a window pass 1 opened. A terminal record
  // with no matching start (the start fell outside the queried range) cannot
  // define a window on its own: there is no start instant to attribute from,
  // and inventing one would invent usage.
  for (const envelope of envelopes) {
    if (envelope.kind !== "sweep.completed" && envelope.kind !== "sweep.outcome") continue;
    const payload = isRecord(envelope.record) ? envelope.record : {};
    const sweepId = readSweepId(envelope, payload);
    if (sweepId === undefined) continue;

    const existing = windows.get(`${envelope.hostId} ${sweepId}`);
    if (!existing) continue;

    // `sweep.outcome` carries no completion instant of its own (see the schema
    // doc), so its envelope `emittedAt` is the best available proxy.
    const endedAt =
      envelope.kind === "sweep.completed"
        ? (parseTimestamp(payload.completed_at) ?? parseTimestamp(envelope.emittedAt))
        : parseTimestamp(envelope.emittedAt);
    if (endedAt !== undefined) {
      const clamped = Math.max(endedAt, existing.startedAt);
      existing.endedAt = existing.endedAt === undefined ? clamped : Math.max(existing.endedAt, clamped);
    }
    if (existing.model === undefined && typeof payload.model === "string" && payload.model !== "") {
      existing.model = payload.model;
    }
  }

  return [...windows.values()].sort((a, b) => a.startedAt - b.startedAt);
}

function readSweepId(envelope: HistoryEnvelope, payload: Record<string, unknown>): string | undefined {
  if (typeof payload.sweep_id === "string" && payload.sweep_id !== "") return payload.sweep_id;
  if (typeof envelope.sweepId === "string" && envelope.sweepId !== "") return envelope.sweepId;
  return undefined;
}

function readRepo(envelope: HistoryEnvelope, payload: Record<string, unknown>): string | undefined {
  if (typeof payload.repo === "string" && payload.repo !== "") return payload.repo;
  if (typeof envelope.repo === "string" && envelope.repo !== "") return envelope.repo;
  return undefined;
}
