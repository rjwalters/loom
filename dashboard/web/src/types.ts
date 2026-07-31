/**
 * Shared wire-format types for the fleet dashboard frontend.
 *
 * Mirrors the envelope + record kinds documented in
 * `.loom/docs/telemetry-schema.md` (Rust source of truth) and the SSE frame
 * shape documented in `dashboard/docs/query-api.md`'s `GET /api/events`
 * section. Kept intentionally loose (`Record<string, unknown>` plus known
 * fields) since `record` is the verbatim ingested JSON payload and new
 * fields are additive across schema versions (see that doc's
 * `schema_version` semantics section).
 */

/** The terminal results `sweep.completed`/`sweep.outcome`'s `result` field
 * takes on (`dashboard/docs/query-api.md`'s `result` query-param table). */
export type SweepResult = "success" | "failure" | "cancelled" | "blocked";

/** Runtime narrowing for `SweepResult`, used by transforms that read `result`
 * out of a `record` payload (which is verbatim ingested JSON, so its `result`
 * arrives as an unvalidated `unknown`). */
export function isSweepResult(value: unknown): value is SweepResult {
  return value === "success" || value === "failure" || value === "cancelled" || value === "blocked";
}

export type SweepPhaseName = "curator" | "builder" | "judge" | "doctor" | "merge";

/** The record kinds this frontend cares about (a subset of the full schema). */
export type RecordKind =
  | "sweep.started"
  | "sweep.phase"
  | "sweep.completed"
  | "sweep.outcome"
  | "tokens.snapshot"
  | "host.health";

export interface SweepStartedRecord {
  kind: "sweep.started";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  started_at: string;
  model?: string;
  effort?: string;
}

export interface SweepPhaseRecord {
  kind: "sweep.phase";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  phase: SweepPhaseName | string;
  entered_at: string;
}

export interface SweepCompletedRecord {
  kind: "sweep.completed";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  completed_at: string;
  result: SweepResult;
}

export interface SweepPhaseDuration {
  phase: SweepPhaseName | string;
  duration_sec: number;
}

export interface SweepOutcomeRecord {
  kind: "sweep.outcome";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  model?: string;
  effort?: string;
  config?: Record<string, string>;
  phase_durations?: SweepPhaseDuration[];
  total_duration_sec?: number;
  result: SweepResult;
  pr_number?: number;
}

/** Any record kind not modeled above — passed through opaquely. */
export interface OtherRecord {
  kind: string;
  [key: string]: unknown;
}

export type TelemetryRecord =
  | SweepStartedRecord
  | SweepPhaseRecord
  | SweepCompletedRecord
  | SweepOutcomeRecord
  | OtherRecord;

/** The `event` object nested inside every `GET /api/events` SSE frame. */
export interface LiveTailEvent {
  hostId: string;
  emittedAt: string;
  schemaVersion: number;
  record: TelemetryRecord;
}

/** One parsed `data:` frame from the live-tail SSE stream. */
export interface LiveTailFrame {
  topic: string;
  event: LiveTailEvent;
}

/**
 * A single row from `GET /api/history`'s (or `GET /public/history`'s)
 * `records` array, camelCased exactly as the API returns it.
 *
 * On `/public/history`, a `visibility: "private"` row has `repo`/`issue`/
 * `sweepId` nulled and `record` reduced to a per-`kind` field allowlist —
 * see `dashboard/src/redaction.ts`. Transforms over these rows must
 * therefore tolerate missing fields rather than trust `record`'s declared
 * shape, so that the same code works against either route with no
 * redaction-awareness of its own.
 */
export interface HistoryRecord {
  id: number;
  schemaVersion: number;
  emittedAt: string;
  hostId: string;
  kind: string;
  repo?: string | null;
  visibility?: "public" | "private" | null;
  issue?: number | null;
  sweepId?: string | null;
  ingestedAt: string;
  record: TelemetryRecord;
}

/** One page of `GET /api/history` / `GET /public/history` — mirrors
 * `dashboard/src/query.ts`'s `HistoryQueryResult`. */
export interface HistoryQueryResult {
  records: HistoryRecord[];
  /** `id` of the last record on this page, or `null` at the end of the
   * matching result set. Pass back as `?cursor=` to fetch the next page. */
  nextCursor: number | null;
}
