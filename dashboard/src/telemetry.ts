/**
 * Wire-format types and decode helpers for the fleet telemetry schema
 * (Epic #4702, Phase 1 — `.loom/docs/telemetry-schema.md`; Rust source of
 * truth `loom-daemon/src/telemetry/mod.rs`).
 *
 * This module is intentionally loose/defensive rather than a 1:1 typed
 * mirror of the Rust enum: the schema doc's `schema_version` contract
 * requires accepting an *unrecognized higher* version where possible, which
 * means a strict discriminated union that rejects unknown `kind`s would
 * violate the forward-compatibility rule. Instead we validate only the
 * envelope-level shape strictly (schema_version/emitted_at/host_id/record
 * all present and well-typed) and extract record-level fields
 * (`repo`/`visibility`/`issue`/`sweep_id`) defensively, whatever the `kind`.
 */

/** Current schema version this backend fully understands (mirrors the
 * daemon's `CURRENT_SCHEMA_VERSION`). Envelopes above this are still
 * accepted (forward-compatible) — see `validateEnvelope`. */
export const CURRENT_SCHEMA_VERSION = 1;

export type RepoVisibility = "public" | "private";

/**
 * Decode a `visibility` value using the exact fail-safe rule documented in
 * `.loom/docs/telemetry-schema.md`: only the exact (case-insensitive)
 * string `"public"` maps to public; a missing field, unknown label, wrong
 * type, or `null` all map to private. This must mirror the Rust
 * `RepoVisibility` custom `Deserialize` impl bit-for-bit — it is the
 * schema's anti-leak control and MUST NOT be loosened.
 */
export function decodeVisibility(value: unknown): RepoVisibility {
  if (typeof value === "string" && value.toLowerCase() === "public") {
    return "public";
  }
  return "private";
}

/** A decoded, envelope-shape-valid telemetry envelope. `record` is kept as
 * a loosely-typed object — see the module doc for why. */
export interface TelemetryEnvelope {
  schema_version: number;
  emitted_at: string;
  host_id: string;
  record: Record<string, unknown>;
}

/** Fields this backend extracts from `record` for indexing/querying,
 * regardless of `kind` (present on repo-referencing kinds; absent — left
 * `undefined` — on host-level kinds like `tokens.snapshot`/`host.health`). */
export interface ExtractedRecordFields {
  kind: string;
  repo: string | undefined;
  visibility: RepoVisibility;
  issue: number | undefined;
  sweepId: string | undefined;
}

/** One envelope failed shape validation; `index` is its position in the
 * ingested batch (for a clear per-record error message). */
export interface EnvelopeValidationError {
  index: number;
  reason: string;
}

/**
 * Validate the *envelope* shape of `raw` (does NOT validate `record`'s
 * internal shape beyond requiring it to be an object with a string `kind` —
 * see the module doc). Returns the decoded envelope on success, or a reason
 * string on failure.
 *
 * Per the schema doc's `schema_version` semantics: a **missing** or
 * non-integer `schema_version` is always malformed (reject), but an
 * integer `schema_version` above `CURRENT_SCHEMA_VERSION` is accepted
 * (forward-compatible) — the caller does not reject on version alone.
 */
export function validateEnvelope(
  raw: unknown,
  index: number,
): { ok: true; envelope: TelemetryEnvelope } | { ok: false; error: EnvelopeValidationError } {
  const fail = (reason: string) => ({ ok: false as const, error: { index, reason } });

  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return fail("envelope must be a JSON object");
  }
  const obj = raw as Record<string, unknown>;

  const schemaVersion = obj.schema_version;
  if (typeof schemaVersion !== "number" || !Number.isInteger(schemaVersion)) {
    // Covers both "missing" (undefined) and "present but wrong type" —
    // the schema doc: "never silently coerce a missing schema_version to
    // 0 — a record with no schema_version is malformed."
    return fail("missing or non-integer schema_version");
  }
  if (schemaVersion < 1) {
    return fail(`schema_version must be >= 1, got ${schemaVersion}`);
  }

  const emittedAt = obj.emitted_at;
  if (typeof emittedAt !== "string" || Number.isNaN(Date.parse(emittedAt))) {
    return fail("missing or unparseable emitted_at");
  }

  const hostId = obj.host_id;
  if (typeof hostId !== "string" || hostId.length === 0) {
    return fail("missing or empty host_id");
  }

  const record = obj.record;
  if (typeof record !== "object" || record === null || Array.isArray(record)) {
    return fail("missing or non-object record");
  }
  const recordObj = record as Record<string, unknown>;
  if (typeof recordObj.kind !== "string" || recordObj.kind.length === 0) {
    return fail("record.kind must be a non-empty string");
  }

  return {
    ok: true,
    envelope: {
      schema_version: schemaVersion,
      emitted_at: emittedAt,
      host_id: hostId,
      record: recordObj,
    },
  };
}

/** Extract the fields this backend indexes from a validated envelope's
 * `record`, defensively (works for any `kind`, known or forward-compatible
 * unknown). */
export function extractRecordFields(record: Record<string, unknown>): ExtractedRecordFields {
  const kind = record.kind as string;
  const repo = typeof record.repo === "string" ? record.repo : undefined;
  const visibility = decodeVisibility(record.visibility);
  const issue = typeof record.issue === "number" && Number.isInteger(record.issue) ? record.issue : undefined;
  const sweepId = typeof record.sweep_id === "string" ? record.sweep_id : undefined;
  return { kind, repo, visibility, issue, sweepId };
}
