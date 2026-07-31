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

export type SweepResult = "success" | "failure" | "cancelled" | "blocked";

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

/** A single row from `GET /api/history`'s `records` array. */
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
