/**
 * Sweep tools for Loom MCP server (Issue #3452 — Phase A of #3449).
 *
 * Exposes two tools for interacting with the loom-daemon's sweep registry:
 *
 *   - `dispatch_sweep`  Spawn a `/loom:sweep` child via the daemon's
 *                       in-memory registry. The daemon shells out to
 *                       `defaults/scripts/spawn-claude.sh` for token rotation
 *                       and detaches the child; tracking is purely in-memory
 *                       (no daemon-side state file is written).
 *   - `list_sweeps`     Query tracked sweeps, optionally filtered by state.
 *
 * Remaining monitoring tools (`get_sweep_status`, `tail_sweep_log`,
 * `cancel_sweep`, pub/sub bus) come in Phase C — out of scope for this PR.
 *
 * @see loom-daemon/src/types.rs — Request/Response variants.
 * @see loom-daemon/src/sweep_registry.rs — backend implementation.
 */

import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import { sendDaemonRequest } from "../shared/daemon.js";

// ============================================================================
// Wire types
// ============================================================================

/**
 * `SweepKind` mirrors the Rust enum in `loom-daemon/src/types.rs`.
 *
 * Serialized with serde's `tag = "type", content = "value"` shape:
 *   `{"type": "Issue", "value": 42}`
 *   `{"type": "PrSet", "value": [10, 20]}`
 *
 * NOTE: Phase A only fully implements `Issue`. `PrSet` is reserved for
 * a later phase of #3449 and will be rejected with an error by the daemon.
 */
export type SweepKind = { type: "Issue"; value: number } | { type: "PrSet"; value: number[] };

/**
 * `SweepState` mirrors the Rust enum's serde-tagged shape:
 *   `{"state": "Pending"}`
 *   `{"state": "Running"}`
 *   `{"state": "Exited", "details": {"code": 0 | null, "at": "..."}}`
 *   `{"state": "Crashed", "details": {"at": "..."}}`
 *
 * For filter inputs only the discriminant matters; the daemon compares
 * by `mem::discriminant`, so any matching tag works (placeholder `details`
 * are accepted).
 */
export type SweepState =
  | { state: "Pending" }
  | { state: "Running" }
  | { state: "Exited"; details?: { code?: number | null; at?: string } }
  | { state: "Crashed"; details?: { at?: string } };

/**
 * `SweepInfo` mirrors the Rust struct of the same name. This shape is
 * snapshot-tested in `loom-daemon/src/sweep_registry.rs` —
 * `sweep_info_schema_snapshot` will fail if the wire format drifts.
 */
export interface SweepInfo {
  sweep_id: string;
  kind: SweepKind;
  pid: number;
  token_name: string;
  log_path: string;
  idempotency_key?: string;
  started_at: string;
  state: SweepState;
  latest_phase?: string;
  pr_number?: number;
}

interface DispatchResponse {
  type: "SweepDispatched";
  payload: {
    sweep_id: string;
    pid: number;
    token_name: string;
    log_path: string;
  };
}

interface ListResponse {
  type: "SweepList";
  payload: {
    sweeps: SweepInfo[];
  };
}

interface ErrorResponse {
  type: "Error";
  payload?: { message?: string };
  message?: string;
}

interface StructuredErrorResponse {
  type: "StructuredError";
  payload: { message?: string };
}

type DaemonResponse =
  | DispatchResponse
  | ListResponse
  | ErrorResponse
  | StructuredErrorResponse
  | { type: string; payload?: unknown };

// ============================================================================
// Helpers
// ============================================================================

function extractError(response: DaemonResponse): string | null {
  if (response.type === "Error") {
    const r = response as ErrorResponse;
    return r.payload?.message || r.message || "Unknown error";
  }
  if (response.type === "StructuredError") {
    const r = response as StructuredErrorResponse;
    return r.payload?.message || "Unknown structured error";
  }
  return null;
}

function isStateTag(s: string): s is SweepState["state"] {
  return s === "Pending" || s === "Running" || s === "Exited" || s === "Crashed";
}

function buildStateFilter(stateArg: unknown): SweepState | null {
  if (typeof stateArg !== "string" || stateArg.length === 0) return null;
  if (!isStateTag(stateArg)) {
    return null;
  }
  if (stateArg === "Exited") {
    return { state: "Exited", details: {} };
  }
  if (stateArg === "Crashed") {
    return { state: "Crashed", details: {} };
  }
  return { state: stateArg };
}

// ============================================================================
// Tool implementations
// ============================================================================

async function dispatchSweep(args: {
  kind: SweepKind;
  idempotency_key?: string;
}): Promise<{ success: true; result: DispatchResponse["payload"] } | { success: false; error: string }> {
  try {
    const response = (await sendDaemonRequest({
      type: "DispatchSweep",
      payload: {
        kind: args.kind,
        idempotency_key: args.idempotency_key ?? null,
      },
    })) as DaemonResponse;

    if (response.type === "SweepDispatched") {
      const payload = (response as DispatchResponse).payload;
      return { success: true, result: payload };
    }

    const err = extractError(response);
    return { success: false, error: err ?? `Unexpected response: ${response.type}` };
  } catch (error) {
    return { success: false, error: `Error dispatching sweep: ${error}` };
  }
}

async function listSweeps(args: {
  state_filter?: SweepState | null;
}): Promise<{ success: true; sweeps: SweepInfo[] } | { success: false; error: string }> {
  try {
    const response = (await sendDaemonRequest({
      type: "ListSweeps",
      payload: {
        state_filter: args.state_filter ?? null,
      },
    })) as DaemonResponse;

    if (response.type === "SweepList") {
      const payload = (response as ListResponse).payload;
      return { success: true, sweeps: payload.sweeps };
    }

    const err = extractError(response);
    return { success: false, error: err ?? `Unexpected response: ${response.type}` };
  } catch (error) {
    return { success: false, error: `Error listing sweeps: ${error}` };
  }
}

// ============================================================================
// Tool definitions
// ============================================================================

/**
 * Sweep tool definitions exposed by the MCP server.
 *
 * Only two tools ship in Phase A. The remaining monitoring tools
 * (`get_sweep_status`, `tail_sweep_log`, `cancel_sweep`,
 * `subscribe_to_events`, `publish_event`) are reserved for Phase C of
 * epic #3449.
 */
export const sweepTools: Tool[] = [
  {
    name: "dispatch_sweep",
    description:
      "Spawn a `/loom:sweep` child via the loom-daemon's sweep registry. " +
      "The daemon shells out to `defaults/scripts/spawn-claude.sh` for token " +
      "rotation, detaches the child, and tracks it in memory. Returns the " +
      "sweep ID, child PID, token-account name, and per-sweep log path. " +
      "Phase A of epic #3449 — only `Issue` dispatch is fully implemented; " +
      "`PrSet` dispatch is reserved for a later phase.",
    inputSchema: {
      type: "object",
      properties: {
        kind: {
          type: "object",
          description:
            "What to dispatch. For an issue-keyed sweep, set " +
            '`{"Issue": <issue-number>}`. PR-set dispatch (`{"PrSet": [<n1>, <n2>]}`) is ' +
            "reserved for a future phase.",
          properties: {
            Issue: {
              type: "number",
              description: "GitHub issue number to dispatch /loom:sweep <N> against.",
            },
            PrSet: {
              type: "array",
              items: { type: "number" },
              description:
                "Reserved for Phase B+: PR numbers for Mode C PR-set dispatch.",
            },
          },
        },
        idempotency_key: {
          type: "string",
          description:
            "Optional dedup key. If a `Running` sweep with the same key " +
            "already exists, the existing sweep ID is returned with no new " +
            "spawn. Matches against `Exited`/`Crashed` entries are NOT deduped " +
            "(the dedup window is the lifetime of the Running entry).",
        },
      },
      required: ["kind"],
    },
  },
  {
    name: "list_sweeps",
    description:
      "List sweeps tracked by the loom-daemon's in-memory registry, " +
      "optionally filtered by lifecycle state. Returns a JSON array of " +
      "`SweepInfo` records matching the schema declared in " +
      "loom-daemon/src/types.rs. State entries older than the retention " +
      "window (~1h) are garbage-collected by the reaper, so callers should " +
      "not expect indefinite persistence of terminal sweeps.",
    inputSchema: {
      type: "object",
      properties: {
        state_filter: {
          type: "string",
          enum: ["Pending", "Running", "Exited", "Crashed"],
          description:
            "Optional lifecycle state filter. Omit to list all tracked sweeps.",
        },
      },
    },
  },
];

// ============================================================================
// Handler
// ============================================================================

function formatSweepLine(info: SweepInfo): string {
  const kind =
    info.kind.type === "Issue"
      ? `Issue #${info.kind.value}`
      : `PR-set [${info.kind.value.join(", ")}]`;
  const stateLabel = info.state.state;
  const parts = [
    `* ${info.sweep_id}`,
    `  Kind:       ${kind}`,
    `  PID:        ${info.pid}`,
    `  State:      ${stateLabel}`,
    `  Token:      ${info.token_name}`,
    `  Log:        ${info.log_path}`,
    `  Started:    ${info.started_at}`,
  ];
  if (info.latest_phase) parts.push(`  Phase:      ${info.latest_phase}`);
  if (info.pr_number !== undefined && info.pr_number !== null)
    parts.push(`  PR:         #${info.pr_number}`);
  if (info.idempotency_key)
    parts.push(`  Idem. key:  ${info.idempotency_key}`);
  return parts.join("\n");
}

/**
 * Handle sweep tool calls. Dispatched from `index.ts`.
 */
export async function handleSweepTool(
  name: string,
  args?: Record<string, unknown>
): Promise<{ type: "text"; text: string }[]> {
  switch (name) {
    case "dispatch_sweep": {
      const kindArg = args?.kind as SweepKind | undefined;
      if (!kindArg || typeof kindArg !== "object") {
        return [
          {
            type: "text",
            text:
              '=== Dispatch Sweep ===\n\nFailed\n\nError: `kind` is required (e.g. `{"Issue": 42}`)',
          },
        ];
      }

      // Normalize: the schema declares `kind` as an object with optional
      // `Issue` / `PrSet` keys; reshape into the serde-tagged variant the
      // daemon expects.
      let normalized: SweepKind | null = null;
      const anyKind = kindArg as unknown as { Issue?: number; PrSet?: number[]; type?: string; value?: unknown };
      if (typeof anyKind.Issue === "number") {
        normalized = { type: "Issue", value: anyKind.Issue };
      } else if (Array.isArray(anyKind.PrSet)) {
        normalized = { type: "PrSet", value: anyKind.PrSet };
      } else if (anyKind.type === "Issue" && typeof anyKind.value === "number") {
        normalized = { type: "Issue", value: anyKind.value };
      } else if (anyKind.type === "PrSet" && Array.isArray(anyKind.value)) {
        normalized = { type: "PrSet", value: anyKind.value as number[] };
      }
      if (!normalized) {
        return [
          {
            type: "text",
            text: '=== Dispatch Sweep ===\n\nFailed\n\nError: `kind` must be `{"Issue": <N>}` or `{"PrSet": [<N>, ...]}`',
          },
        ];
      }

      const idempotencyKey =
        typeof args?.idempotency_key === "string" ? (args.idempotency_key as string) : undefined;

      const result = await dispatchSweep({ kind: normalized, idempotency_key: idempotencyKey });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Dispatch Sweep ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      const body = [
        `Sweep ID:   ${result.result.sweep_id}`,
        `PID:        ${result.result.pid}`,
        `Token:      ${result.result.token_name}`,
        `Log:        ${result.result.log_path}`,
      ].join("\n");
      return [
        {
          type: "text",
          text: `=== Dispatch Sweep ===\n\nSuccess\n\n${body}`,
        },
      ];
    }

    case "list_sweeps": {
      const stateArg = args?.state_filter;
      const stateFilter = stateArg === undefined ? null : buildStateFilter(stateArg);

      const result = await listSweeps({ state_filter: stateFilter });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== List Sweeps ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      if (result.sweeps.length === 0) {
        return [
          {
            type: "text",
            text: "=== List Sweeps ===\n\nNo sweeps tracked.",
          },
        ];
      }
      const lines = result.sweeps.map(formatSweepLine).join("\n\n");
      return [
        {
          type: "text",
          text: `=== List Sweeps (${result.sweeps.length}) ===\n\n${lines}`,
        },
      ];
    }

    default:
      throw new Error(`Unknown sweep tool: ${name}`);
  }
}
