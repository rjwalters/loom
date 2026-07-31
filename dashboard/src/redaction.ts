/**
 * Visibility-based response redaction — the single policy layer between the
 * unclassified query API (`src/query.ts`, issue #4726) and any client (Epic
 * #4702, Phase 2, issue #4727).
 *
 * **The single enforcement point.** Every `/api/*` (authenticated) and
 * `/public/*` (public) route handler in `src/index.ts` calls into this
 * module as a post-processing step over `query.ts`'s already-unclassified
 * results — `query.ts` itself is intentionally unmodified (still returns
 * full detail for every kind, per its own module doc) so there is exactly
 * one place that decides what a viewer is allowed to see. Nothing else in
 * this backend — the D1 queries, the Durable Object, the live-tail poll
 * loop — implements any redaction of its own.
 *
 * ## How "authenticated" is determined
 *
 * Route-based split (see `docs/query-api.md` and `docs/cloudflare-access.md`
 * for the full rationale): the existing unprefixed `/api/*` routes are the
 * **authenticated** surface — an operator's Cloudflare Access policy is
 * expected to gate everything except the paths `docs/cloudflare-access.md`
 * already documents as Bypass (`/ingest`, `/public`, `/admin/*`). The new
 * `/public/*` routes mirror `/api/*` 1:1 and are always redacted, matching
 * the `/public` path Cloudflare Access's Bypass policy already reserves.
 * `src/index.ts` passes a plain `isAuthenticated: boolean` into every
 * function here based purely on *which route matched* — there is no
 * JWT/header parsing anywhere in this Worker (the epic's explicit
 * constraint: "no auth code in the dashboard itself"). The Worker trusts
 * "this request reached this path" as the authentication signal, exactly as
 * it already trusts a Custom Domain request having traversed Access at all
 * (see `docs/cloudflare-access.md` §5 — even signature verification of the
 * injected `Cf-Access-Jwt-Assertion` header is explicitly deferred, "not
 * implemented today").
 *
 * ## Redaction policy: per-kind field allowlist, not a blocklist
 *
 * For a **private**-visibility record viewed **without** authentication, the
 * policy is an **allowlist** of fields known today to be safe (counts,
 * rates, durations, lifecycle metadata) — never a blocklist of fields known
 * today to be unsafe. This is deliberate defense-in-depth: a blocklist only
 * protects against leak vectors this module's author thought of; an
 * allowlist protects against every leak vector too, including a future
 * schema field (e.g. an issue title, once one exists on the wire) that gets
 * added to `.loom/docs/telemetry-schema.md` without this module being
 * updated in lockstep. An unrecognized `kind` (forward-compatible per the
 * schema doc's `schema_version` contract) gets the most conservative
 * allowlist of all: `kind` only.
 *
 * ## `tokens.snapshot` / `host.health` are host-level, not repo-level
 *
 * These two kinds carry no `repo`/`visibility` field at all — Phase 1's
 * `decodeVisibility` (`src/telemetry.ts`) therefore stores them as
 * `visibility: "private"` by its fail-safe-default rule (a **storage**
 * classification, not a statement that their content is repo-identifying).
 * Every field either kind defines today is host/fleet-operational detail
 * (CPU/uptime/token-pool state) with no reference to a repository, issue,
 * branch, or PR — the four leak vectors this issue's acceptance criteria
 * name. Decision (flagged as open by the Curator, resolved here): these two
 * kinds' allowlists intentionally include every field the schema defines,
 * so they pass through unchanged for both authenticated and public viewers.
 * This is still enforced through the same allowlist mechanism as every other
 * kind (not a special-cased bypass), so a future field added to either kind
 * is dropped by default until this table is deliberately updated — the
 * fail-safe direction never flips silently.
 */

import type { RepoVisibility } from "./telemetry";
import { decodeVisibility } from "./telemetry";
import type { HistoryQueryResult, HistoryRecord } from "./query";
import type { ActiveSweepState, FleetSnapshot } from "./fleetState";

// ---------------------------------------------------------------------------
// Per-kind field allowlist for the nested `record` payload
// ---------------------------------------------------------------------------

/** The nested-payload fields that survive into a public, unauthenticated
 * response for a **private**-visibility record of this `kind`. Every field
 * omitted here is a deliberate redaction, not an oversight — see the module
 * doc for the allowlist-not-blocklist rationale. Keep this table in sync
 * with `.loom/docs/telemetry-schema.md`'s "Record kinds" section: a new
 * field on an existing kind does NOT appear in a public response until it is
 * added here on purpose. */
const RECORD_FIELD_ALLOWLIST: Readonly<Record<string, readonly string[]>> = {
  "sweep.started": ["kind", "started_at", "model", "effort"],
  "sweep.phase": ["kind", "phase", "entered_at"],
  "sweep.completed": ["kind", "completed_at", "result"],
  "sweep.outcome": ["kind", "model", "effort", "config", "phase_durations", "total_duration_sec", "result"],
  // Host-level kinds: no repo/issue/branch/PR reference exists on either —
  // see the module doc's "tokens.snapshot / host.health" section. Every
  // field the schema defines today is listed explicitly (not "pass
  // everything") so the allowlist mechanism stays the single source of
  // truth even for these two kinds.
  "tokens.snapshot": ["kind", "captured_at", "accounts"],
  "host.health": [
    "kind",
    "captured_at",
    "daemon_version",
    "uptime_sec",
    "logical_cpus",
    "cpu_idle_fraction",
    "load_per_core",
    "worktree_root_free_gb",
  ],
};

/** The fail-safe allowlist for a `kind` this table does not (yet) recognize
 * — an unknown, forward-compatible record kind (per the schema doc's
 * `schema_version` contract) reveals only its own `kind` string until this
 * module is updated to classify it deliberately. */
const DEFAULT_ALLOWLIST: readonly string[] = ["kind"];

/** Project `payload` down to the fields `kind`'s allowlist permits. Used for
 * every nested `record` payload this module redacts — history records, live
 * tail SSE frames, and the Durable Object's per-host health/tokens entries
 * alike, so there is exactly one allowlist table for the whole backend. */
export function redactPayload(kind: string, payload: Record<string, unknown>): Record<string, unknown> {
  const allowlist = RECORD_FIELD_ALLOWLIST[kind] ?? DEFAULT_ALLOWLIST;
  const redacted: Record<string, unknown> = {};
  for (const field of allowlist) {
    if (field in payload) {
      redacted[field] = payload[field];
    }
  }
  return redacted;
}

function isPrivateAndUnauthenticated(visibility: RepoVisibility, isAuthenticated: boolean): boolean {
  return !isAuthenticated && visibility === "private";
}

// ---------------------------------------------------------------------------
// `GET /api/history` / `GET /public/history`
// ---------------------------------------------------------------------------

/** Redact one `HistoryRecord` for the given auth state. An authenticated
 * viewer or a `public`-visibility record is returned unchanged (same
 * top-level shape query.ts already produces); a private record viewed
 * without authentication has every repo/issue/sweep-identifying field
 * nulled and its nested `record` payload projected through
 * `redactPayload`. */
export function redactHistoryRecord(record: HistoryRecord, isAuthenticated: boolean): HistoryRecord {
  if (!isPrivateAndUnauthenticated(decodeVisibility(record.visibility), isAuthenticated)) {
    return record;
  }
  return {
    id: record.id,
    schemaVersion: record.schemaVersion,
    emittedAt: record.emittedAt,
    hostId: record.hostId,
    kind: record.kind,
    // The three leak vectors AC2 names by field: repo name, issue number,
    // and the sweep id (which embeds the issue number, e.g.
    // "sweep-issue-4703-0" — see `.loom/docs/telemetry-schema.md`).
    repo: null,
    visibility: record.visibility,
    issue: null,
    sweepId: null,
    ingestedAt: record.ingestedAt,
    record: redactPayload(record.kind, record.record),
  };
}

/** Redact every record in a `queryHistory` result, preserving pagination
 * metadata unchanged (the cursor is an opaque row id, not itself
 * identifying). */
export function redactHistoryQueryResult(
  result: HistoryQueryResult,
  isAuthenticated: boolean,
): HistoryQueryResult {
  return {
    records: result.records.map((record) => redactHistoryRecord(record, isAuthenticated)),
    nextCursor: result.nextCursor,
  };
}

// ---------------------------------------------------------------------------
// `GET /api/fleet-state` / `GET /public/fleet-state`
// ---------------------------------------------------------------------------

/** The shape a redacted `ActiveSweepState` is returned as. `sweepId` is
 * `string` (required) on the wire type this backend accepts internally, but
 * a redacted private/public entry omits it entirely (not nulled) —
 * `JSON.stringify` drops an `undefined` property, so the field is simply
 * absent from the response rather than present-but-null. */
export interface PublicActiveSweep {
  hostId: string;
  visibility: "public" | "private";
  repo?: string;
  issue?: number;
  sweepId?: string;
  phase?: string;
  startedAt?: string;
  enteredPhaseAt?: string;
  model?: string;
  effort?: string;
  updatedAt: string;
}

/** Redact one `ActiveSweepState` (the Durable Object's live per-sweep
 * entry). Mirrors `redactHistoryRecord`'s field selection: repo/issue/sweep
 * id are stripped for a private, unauthenticated view; every other field
 * (phase, timing, model/effort) is aggregate/lifecycle metadata, not
 * repo-identifying, and survives. */
export function redactActiveSweep(sweep: ActiveSweepState, isAuthenticated: boolean): PublicActiveSweep {
  if (!isPrivateAndUnauthenticated(sweep.visibility, isAuthenticated)) {
    return sweep;
  }
  return {
    hostId: sweep.hostId,
    visibility: sweep.visibility,
    phase: sweep.phase,
    startedAt: sweep.startedAt,
    enteredPhaseAt: sweep.enteredPhaseAt,
    model: sweep.model,
    effort: sweep.effort,
    updatedAt: sweep.updatedAt,
  };
}

export interface RedactedFleetSnapshot {
  hosts: FleetSnapshot["hosts"];
  activeSweeps: PublicActiveSweep[];
}

/** Redact a full `FleetSnapshot`: every host's `health`/`tokens` entry is
 * projected through `redactPayload` (a no-op in practice today — see the
 * module doc — but kept on the same single mechanism as every other kind),
 * and every `activeSweeps` entry through `redactActiveSweep`. */
export function redactFleetSnapshot(snapshot: FleetSnapshot, isAuthenticated: boolean): RedactedFleetSnapshot {
  const hosts: FleetSnapshot["hosts"] = {};
  for (const [hostId, entry] of Object.entries(snapshot.hosts)) {
    hosts[hostId] = {
      ...(entry.health && {
        health: { record: redactPayload("host.health", entry.health.record), updatedAt: entry.health.updatedAt },
      }),
      ...(entry.tokens && {
        tokens: { record: redactPayload("tokens.snapshot", entry.tokens.record), updatedAt: entry.tokens.updatedAt },
      }),
    };
  }
  return {
    hosts,
    activeSweeps: snapshot.activeSweeps.map((sweep) => redactActiveSweep(sweep, isAuthenticated)),
  };
}

// ---------------------------------------------------------------------------
// `GET /api/events` / `GET /public/events` — live tail SSE frames
// ---------------------------------------------------------------------------

const SSE_DATA_PREFIX = "data: ";
const SSE_FRAME_SUFFIX = "\n\n";

interface LiveTailFramePayload {
  topic: string;
  event: {
    hostId: string;
    emittedAt: string;
    schemaVersion: number;
    record: Record<string, unknown>;
  };
}

/**
 * Redact one already-framed SSE chunk from `query.ts`'s `createLiveTailStream`
 * (`src/index.ts` pipes every stream chunk through this before it reaches
 * the client). A non-`data:` frame (the leading `retry:`/comment preamble,
 * or a `: keepalive` comment) carries no record and passes through
 * unchanged. A malformed/unexpected `data:` frame also passes through
 * unchanged rather than throwing — this function must never crash a live
 * connection; `query.ts`'s own frames are always well-formed in practice, so
 * this is a defensive fallback, not the primary control.
 */
export function redactSseFrame(frameText: string, isAuthenticated: boolean): string {
  if (isAuthenticated || !frameText.startsWith(SSE_DATA_PREFIX) || !frameText.endsWith(SSE_FRAME_SUFFIX)) {
    return frameText;
  }

  const jsonText = frameText.slice(SSE_DATA_PREFIX.length, -SSE_FRAME_SUFFIX.length);
  let payload: LiveTailFramePayload;
  try {
    payload = JSON.parse(jsonText) as LiveTailFramePayload;
  } catch {
    return frameText;
  }
  if (!payload || typeof payload !== "object" || !payload.event || typeof payload.event !== "object") {
    return frameText;
  }

  const visibility = decodeVisibility(payload.event.record?.visibility);
  if (visibility === "public") return frameText;

  const redacted: LiveTailFramePayload = {
    topic: payload.topic,
    event: {
      hostId: payload.event.hostId,
      emittedAt: payload.event.emittedAt,
      schemaVersion: payload.event.schemaVersion,
      record: redactPayload(payload.topic, payload.event.record ?? {}),
    },
  };
  return `${SSE_DATA_PREFIX}${JSON.stringify(redacted)}${SSE_FRAME_SUFFIX}`;
}

/** Wrap a live-tail `ReadableStream<Uint8Array>` so every SSE frame is
 * redacted before it reaches the client. Relies on the stream's own framing
 * discipline (`createLiveTailStream` enqueues one complete `data: ...\n\n`
 * — or `retry:`/`: keepalive` — string per chunk, never a partial frame or
 * multiple frames in one chunk), which the Streams spec preserves 1:1
 * through a `TransformStream` (no chunk coalescing on the writable→readable
 * path). */
export function redactLiveTailStream(
  stream: ReadableStream<Uint8Array>,
  isAuthenticated: boolean,
): ReadableStream<Uint8Array> {
  if (isAuthenticated) return stream;

  const decoder = new TextDecoder();
  const encoder = new TextEncoder();
  const transform = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      const frameText = decoder.decode(chunk, { stream: true });
      controller.enqueue(encoder.encode(redactSseFrame(frameText, isAuthenticated)));
    },
  });
  return stream.pipeThrough(transform);
}
