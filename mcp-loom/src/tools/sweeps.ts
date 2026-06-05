/**
 * Sweep tools for Loom MCP server.
 *
 * Phase A (Issue #3452) shipped `dispatch_sweep` + `list_sweeps`.
 * Phase B (Issue #3453) shipped the event bus IPC surface (consumed below).
 * Phase C (Issue #3455) adds the monitoring + subscription tools:
 *
 *   - `dispatch_sweep`      Spawn a `/loom:sweep` child via the daemon.
 *   - `list_sweeps`         Query tracked sweeps, optionally filtered.
 *   - `get_sweep_status`    Fetch a single SweepInfo + N recent events.
 *   - `tail_sweep_log`      Read last N lines from a sweep's log file.
 *   - `subscribe_to_events` Long-lived stream filtered by topic prefix.
 *   - `publish_event`       Operator override / test escape hatch.
 *   - `cancel_sweep`        SIGTERM -> grace -> SIGKILL; transitions to Exited.
 *   - `tail_event_bus`      Debug fire-hose subscription with --since window.
 *
 * @see loom-daemon/src/types.rs — Request/Response variants.
 * @see loom-daemon/src/sweep_registry.rs — backend implementation.
 * @see loom-daemon/src/event_bus.rs — pub/sub bus + topic taxonomy.
 */

import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import { sendDaemonRequest, sendDaemonStreamRequest } from "../shared/daemon.js";

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

interface SweepStatusResponse {
  type: "SweepStatus";
  payload: {
    info: SweepInfo | null;
  };
}

interface SweepLogTailResponse {
  type: "SweepLogTail";
  payload: {
    sweep_id: string;
    lines: string[];
    log_path: string;
  };
}

interface SweepCancelledResponse {
  type: "SweepCancelled";
  payload: {
    sweep_id: string;
    pid: number;
    sigkill_sent: boolean;
    was_running: boolean;
  };
}

interface EventPublishedResponse {
  type: "EventPublished";
  payload: {
    topic: string;
    receivers: number;
  };
}

type DaemonResponse =
  | DispatchResponse
  | ListResponse
  | SweepStatusResponse
  | SweepLogTailResponse
  | SweepCancelledResponse
  | EventPublishedResponse
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

/**
 * Parse a duration string of the form `<N>s`, `<N>m`, or `<N>h` (case-
 * insensitive). Returns the duration in milliseconds, or `null` for any
 * invalid input. Used by `tail_event_bus --since`.
 *
 * Examples:
 *   "10m" -> 600000
 *   "1h"  -> 3600000
 *   "30s" -> 30000
 *   "10"  -> null (no unit)
 *   "1d"  -> null (unsupported unit; days not in scope)
 */
export function parseDuration(input: string): number | null {
  const trimmed = input.trim();
  if (trimmed.length < 2) return null;
  const match = trimmed.match(/^(\d+)([smhSMH])$/);
  if (!match) return null;
  const value = parseInt(match[1], 10);
  if (!Number.isFinite(value) || value <= 0) return null;
  const unit = match[2].toLowerCase();
  switch (unit) {
    case "s":
      return value * 1000;
    case "m":
      return value * 60 * 1000;
    case "h":
      return value * 60 * 60 * 1000;
    default:
      return null;
  }
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

// ---------------------------------------------------------------------------
// Phase C: monitoring + subscription helpers (Issue #3455)
// ---------------------------------------------------------------------------

async function getSweepStatus(args: {
  sweep_id: string;
}): Promise<{ success: true; info: SweepInfo | null } | { success: false; error: string }> {
  try {
    const response = (await sendDaemonRequest({
      type: "GetSweepStatus",
      payload: { sweep_id: args.sweep_id },
    })) as DaemonResponse;

    if (response.type === "SweepStatus") {
      return { success: true, info: (response as SweepStatusResponse).payload.info };
    }
    const err = extractError(response);
    return { success: false, error: err ?? `Unexpected response: ${response.type}` };
  } catch (error) {
    return { success: false, error: `Error fetching sweep status: ${error}` };
  }
}

async function tailSweepLog(args: {
  sweep_id: string;
  lines: number;
}): Promise<
  | { success: true; payload: SweepLogTailResponse["payload"] }
  | { success: false; error: string }
> {
  try {
    const response = (await sendDaemonRequest({
      type: "TailSweepLog",
      payload: { sweep_id: args.sweep_id, lines: args.lines },
    })) as DaemonResponse;

    if (response.type === "SweepLogTail") {
      return { success: true, payload: (response as SweepLogTailResponse).payload };
    }
    const err = extractError(response);
    return { success: false, error: err ?? `Unexpected response: ${response.type}` };
  } catch (error) {
    return { success: false, error: `Error tailing sweep log: ${error}` };
  }
}

async function cancelSweep(args: {
  sweep_id: string;
  grace_secs: number;
}): Promise<
  | { success: true; payload: SweepCancelledResponse["payload"] }
  | { success: false; error: string }
> {
  try {
    const response = (await sendDaemonRequest({
      type: "CancelSweep",
      payload: { sweep_id: args.sweep_id, grace_secs: args.grace_secs },
    })) as DaemonResponse;

    if (response.type === "SweepCancelled") {
      return { success: true, payload: (response as SweepCancelledResponse).payload };
    }
    const err = extractError(response);
    return { success: false, error: err ?? `Unexpected response: ${response.type}` };
  } catch (error) {
    return { success: false, error: `Error cancelling sweep: ${error}` };
  }
}

async function publishEvent(args: {
  topic: string;
  payload: unknown;
}): Promise<
  | { success: true; payload: EventPublishedResponse["payload"] }
  | { success: false; error: string }
> {
  try {
    const response = (await sendDaemonRequest({
      type: "PublishEvent",
      payload: { topic: args.topic, payload: args.payload },
    })) as DaemonResponse;

    if (response.type === "EventPublished") {
      return { success: true, payload: (response as EventPublishedResponse).payload };
    }
    const err = extractError(response);
    return { success: false, error: err ?? `Unexpected response: ${response.type}` };
  } catch (error) {
    return { success: false, error: `Error publishing event: ${error}` };
  }
}

async function streamEvents(args: {
  topics: string[];
  durationMs?: number;
  maxLines?: number;
}): Promise<{ success: true; lines: string[]; closedByTimeout: boolean; elapsedMs: number } | { success: false; error: string }> {
  try {
    const result = await sendDaemonStreamRequest(
      {
        type: "SubscribeEvents",
        payload: { topics: args.topics },
      },
      {
        durationMs: args.durationMs,
        maxLines: args.maxLines,
      }
    );
    return { success: true, ...result };
  } catch (error) {
    return { success: false, error: `Error streaming events: ${error}` };
  }
}

// ============================================================================
// Tool definitions
// ============================================================================

/**
 * Sweep tool definitions exposed by the MCP server.
 *
 * Phase A shipped `dispatch_sweep` + `list_sweeps`. Phase C (Issue #3455)
 * adds the six monitoring + subscription tools that follow.
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
  {
    name: "get_sweep_status",
    description:
      "Return the `SweepInfo` for a single sweep, plus optionally the N " +
      "most-recent events observed on the in-memory event bus for that " +
      "sweep's topics. Recent events are collected via a short subscribe " +
      "window (default ~200ms); set `recent_events` to 0 to skip that step " +
      "and return only the SweepInfo. Returns an error if the sweep ID is " +
      "unknown to the registry.",
    inputSchema: {
      type: "object",
      properties: {
        sweep_id: {
          type: "string",
          description: "The sweep ID returned by `dispatch_sweep`.",
        },
        recent_events: {
          type: "number",
          description:
            "Maximum number of recent events to surface alongside the " +
            "SweepInfo. Defaults to 10. The bus is in-memory and transient " +
            "— this is a best-effort recent-history sample, not a replay log.",
        },
      },
      required: ["sweep_id"],
    },
  },
  {
    name: "tail_sweep_log",
    description:
      "Read the last N lines of a sweep's per-sweep log file " +
      "(`.loom/logs/sweep-issue-<N>.log`). The log path is resolved from " +
      "the registry entry — callers don't need to know the workspace layout. " +
      "Returns an error if the sweep ID is unknown or the log file is " +
      "missing on disk.",
    inputSchema: {
      type: "object",
      properties: {
        sweep_id: {
          type: "string",
          description: "The sweep ID returned by `dispatch_sweep`.",
        },
        lines: {
          type: "number",
          description: "Number of trailing lines to return. Defaults to 100.",
        },
      },
      required: ["sweep_id"],
    },
  },
  {
    name: "subscribe_to_events",
    description:
      "Open a long-lived subscription to the loom-daemon's in-memory event " +
      "bus, filtered by topic prefix. Events arrive as line-delimited JSON " +
      "frames matching the `Response::EventStream { events: [Event] }` shape " +
      "from `loom-daemon/src/types.rs`. Topic matching is segment-aligned " +
      "prefix match (`sweep.issue` matches `sweep.issue.123.phase` but not " +
      "`sweep.issuetype.foo`). The MCP layer caps each subscription with a " +
      "`duration` window so a single tool call returns deterministically; " +
      "set a longer duration for continuous monitoring scenarios. The bus " +
      "taxonomy is frozen for v0.10.0 (see `.loom/docs/daemon-reference.md`).",
    inputSchema: {
      type: "object",
      properties: {
        topics: {
          type: "array",
          items: { type: "string" },
          description:
            "Topic prefixes to subscribe to. Pass an empty array (or omit) " +
            "to receive all events — equivalent to `tail_event_bus` but " +
            "without the time-window default.",
        },
        duration: {
          type: "string",
          description:
            "How long to keep the subscription open. Accepts `<N>s`, `<N>m`, " +
            "or `<N>h` (case-insensitive). Defaults to `30s`. Set a longer " +
            "window for continuous monitoring.",
        },
        max_events: {
          type: "number",
          description:
            "Optional upper bound on the number of frames returned. The " +
            "subscription closes once either bound (duration or count) is " +
            "reached, whichever comes first.",
        },
      },
    },
  },
  {
    name: "publish_event",
    description:
      "Publish a JSON event onto the loom-daemon's in-memory bus. Operator " +
      "override and testing escape hatch — production publishes happen via " +
      "the sweep skill, not this tool. Returns the number of receivers the " +
      "event routed to. `topic` should follow the frozen taxonomy " +
      "(`sweep.issue.{N}.phase` etc.); the bus accepts arbitrary topic " +
      "strings, but downstream consumers only subscribe to documented topics.",
    inputSchema: {
      type: "object",
      properties: {
        topic: {
          type: "string",
          description:
            "Topic string. See `.loom/docs/daemon-reference.md` for the " +
            "frozen taxonomy.",
        },
        payload: {
          description:
            "Opaque JSON payload. Per-topic schemas are documented in " +
            "`defaults/.claude/commands/loom/sweep.md`.",
        },
      },
      required: ["topic", "payload"],
    },
  },
  {
    name: "cancel_sweep",
    description:
      "Cancel a running sweep. Sends SIGTERM to the child PID, waits the " +
      "grace window (default 30 seconds), then escalates to SIGKILL if the " +
      "child is still alive. Transitions the registry entry from `Running` " +
      "to `Exited{code: None, at: <now>}` regardless of which signal " +
      "delivered the kill, and releases the per-issue lock. Idempotent: " +
      "calling against an already-terminal sweep returns success with " +
      "`was_running: false`.",
    inputSchema: {
      type: "object",
      properties: {
        sweep_id: {
          type: "string",
          description: "The sweep ID returned by `dispatch_sweep`.",
        },
        grace: {
          type: "number",
          description:
            "Seconds between SIGTERM and SIGKILL. Defaults to 30. A value " +
            "of 0 escalates to SIGKILL immediately after the first poll " +
            "iteration (~100ms).",
        },
      },
      required: ["sweep_id"],
    },
  },
  {
    name: "tail_event_bus",
    description:
      "Debug-oriented fire-hose subscription that streams ALL events on " +
      "the bus regardless of topic (added per curator risk note D — " +
      "multi-child interactions are qualitatively harder to debug than " +
      "hermetic children). `--since` accepts `<N>s`, `<N>m`, or `<N>h` and " +
      "caps how long the subscription stays open (the bus is in-memory and " +
      "transient — `--since` is a streaming window, not a backward-looking " +
      "replay filter). For topic-filtered streams, use `subscribe_to_events`.",
    inputSchema: {
      type: "object",
      properties: {
        since: {
          type: "string",
          description:
            "Streaming window duration. Accepts `<N>s`, `<N>m`, or `<N>h`. " +
            "Defaults to `10m`.",
        },
        max_events: {
          type: "number",
          description:
            "Optional upper bound on the number of frames returned. The " +
            "subscription closes once either bound (duration or count) is " +
            "reached, whichever comes first.",
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

    // ----- Phase C: monitoring + subscription tools -----

    case "get_sweep_status": {
      const sweepId = typeof args?.sweep_id === "string" ? args.sweep_id : "";
      if (!sweepId) {
        return [
          {
            type: "text",
            text: "=== Get Sweep Status ===\n\nFailed\n\nError: `sweep_id` is required.",
          },
        ];
      }
      const recentEventsArg = args?.recent_events;
      const recentEvents =
        typeof recentEventsArg === "number" && Number.isFinite(recentEventsArg)
          ? Math.max(0, Math.floor(recentEventsArg))
          : 10;

      const statusResult = await getSweepStatus({ sweep_id: sweepId });
      if (!statusResult.success) {
        return [
          {
            type: "text",
            text: `=== Get Sweep Status ===\n\nFailed\n\n${statusResult.error}`,
          },
        ];
      }
      if (!statusResult.info) {
        return [
          {
            type: "text",
            text: `=== Get Sweep Status ===\n\nNo sweep with ID: ${sweepId}`,
          },
        ];
      }

      // Surface the SweepInfo as a structured JSON block plus a short
      // human summary. Then attempt a short subscribe window for
      // recent events — best-effort, never fails the whole tool.
      const info = statusResult.info;
      const sections: string[] = [
        `=== Get Sweep Status: ${info.sweep_id} ===`,
        "",
        formatSweepLine(info),
        "",
        "Raw SweepInfo:",
        "```json",
        JSON.stringify(info, null, 2),
        "```",
      ];

      if (recentEvents > 0) {
        const topicPrefix =
          info.kind.type === "Issue" ? `sweep.issue.${info.kind.value}` : "sweep";
        const stream = await streamEvents({
          topics: [topicPrefix],
          durationMs: 200,
          maxLines: recentEvents,
        });
        if (stream.success) {
          if (stream.lines.length === 0) {
            sections.push("", `Recent events (last ${recentEvents}, prefix=${topicPrefix}): none observed in 200ms window.`);
          } else {
            sections.push(
              "",
              `Recent events (${stream.lines.length}, prefix=${topicPrefix}):`,
              "```",
              ...stream.lines,
              "```",
            );
          }
        } else {
          sections.push("", `Recent events: subscribe failed (${stream.error}).`);
        }
      }

      return [{ type: "text", text: sections.join("\n") }];
    }

    case "tail_sweep_log": {
      const sweepId = typeof args?.sweep_id === "string" ? args.sweep_id : "";
      if (!sweepId) {
        return [
          {
            type: "text",
            text: "=== Tail Sweep Log ===\n\nFailed\n\nError: `sweep_id` is required.",
          },
        ];
      }
      const linesArg = args?.lines;
      const lines =
        typeof linesArg === "number" && Number.isFinite(linesArg)
          ? Math.max(0, Math.floor(linesArg))
          : 100;

      const result = await tailSweepLog({ sweep_id: sweepId, lines });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Tail Sweep Log ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      const { log_path, lines: tail } = result.payload;
      const body =
        tail.length === 0
          ? "(log is empty)"
          : tail.join("\n");
      return [
        {
          type: "text",
          text: `=== Tail Sweep Log: ${sweepId} ===\n\nLog: ${log_path}\nLines: ${tail.length}\n\n${body}`,
        },
      ];
    }

    case "subscribe_to_events": {
      const topicsArg = args?.topics;
      const topics = Array.isArray(topicsArg)
        ? topicsArg.filter((t): t is string => typeof t === "string")
        : [];
      const durationArg = args?.duration;
      let durationMs = 30000;
      if (typeof durationArg === "string" && durationArg.length > 0) {
        const parsed = parseDuration(durationArg);
        if (parsed === null) {
          return [
            {
              type: "text",
              text:
                "=== Subscribe To Events ===\n\nFailed\n\nError: invalid `duration` " +
                `'${durationArg}'. Use formats like '30s', '10m', or '1h'.`,
            },
          ];
        }
        durationMs = parsed;
      }
      const maxLinesArg = args?.max_events;
      const maxLines =
        typeof maxLinesArg === "number" && Number.isFinite(maxLinesArg) && maxLinesArg > 0
          ? Math.floor(maxLinesArg)
          : undefined;

      const result = await streamEvents({ topics, durationMs, maxLines });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Subscribe To Events ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      const summary = [
        `Topics:           ${topics.length === 0 ? "(all)" : topics.join(", ")}`,
        `Frames received:  ${result.lines.length}`,
        `Elapsed:          ${result.elapsedMs}ms`,
        `Closed by:        ${result.closedByTimeout ? "duration window" : "daemon / max"}`,
      ].join("\n");
      const body =
        result.lines.length === 0
          ? "(no events observed)"
          : result.lines.join("\n");
      return [
        {
          type: "text",
          text: `=== Subscribe To Events ===\n\n${summary}\n\nFrames:\n${body}`,
        },
      ];
    }

    case "publish_event": {
      const topic = typeof args?.topic === "string" ? args.topic : "";
      if (!topic) {
        return [
          {
            type: "text",
            text: "=== Publish Event ===\n\nFailed\n\nError: `topic` is required.",
          },
        ];
      }
      if (!("payload" in (args ?? {}))) {
        return [
          {
            type: "text",
            text: "=== Publish Event ===\n\nFailed\n\nError: `payload` is required (use {} for empty).",
          },
        ];
      }
      const payload = (args as { payload: unknown }).payload;

      const result = await publishEvent({ topic, payload });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Publish Event ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      return [
        {
          type: "text",
          text:
            `=== Publish Event ===\n\nSuccess\n\nTopic:      ${result.payload.topic}\n` +
            `Receivers:  ${result.payload.receivers}`,
        },
      ];
    }

    case "cancel_sweep": {
      const sweepId = typeof args?.sweep_id === "string" ? args.sweep_id : "";
      if (!sweepId) {
        return [
          {
            type: "text",
            text: "=== Cancel Sweep ===\n\nFailed\n\nError: `sweep_id` is required.",
          },
        ];
      }
      const graceArg = args?.grace;
      const graceSecs =
        typeof graceArg === "number" && Number.isFinite(graceArg) && graceArg >= 0
          ? Math.floor(graceArg)
          : 30;

      const result = await cancelSweep({ sweep_id: sweepId, grace_secs: graceSecs });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Cancel Sweep ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      const { pid, sigkill_sent, was_running } = result.payload;
      const lines = [
        `Sweep ID:      ${result.payload.sweep_id}`,
        `PID:           ${pid}`,
        `Was running:   ${was_running}`,
        `SIGKILL sent:  ${sigkill_sent}`,
        `Grace:         ${graceSecs}s`,
      ].join("\n");
      const summary = was_running
        ? sigkill_sent
          ? "Child survived SIGTERM; escalated to SIGKILL."
          : "Child terminated within grace window."
        : "Already terminal; no signal sent (idempotent success).";
      return [
        {
          type: "text",
          text: `=== Cancel Sweep ===\n\n${summary}\n\n${lines}`,
        },
      ];
    }

    case "tail_event_bus": {
      const sinceArg = args?.since;
      let durationMs = 10 * 60 * 1000; // default: 10m
      if (typeof sinceArg === "string" && sinceArg.length > 0) {
        const parsed = parseDuration(sinceArg);
        if (parsed === null) {
          return [
            {
              type: "text",
              text:
                "=== Tail Event Bus ===\n\nFailed\n\nError: invalid `since` " +
                `'${sinceArg}'. Use formats like '30s', '10m', or '1h'.`,
            },
          ];
        }
        durationMs = parsed;
      }
      const maxLinesArg = args?.max_events;
      const maxLines =
        typeof maxLinesArg === "number" && Number.isFinite(maxLinesArg) && maxLinesArg > 0
          ? Math.floor(maxLinesArg)
          : undefined;

      // tail_event_bus subscribes to ALL topics (empty filter) — its
      // only knob over subscribe_to_events is the catch-all default
      // and the operator-friendly `--since` window naming.
      const result = await streamEvents({ topics: [], durationMs, maxLines });
      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Tail Event Bus ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }
      const summary = [
        `Window:           ${typeof sinceArg === "string" ? sinceArg : "10m (default)"}`,
        `Frames received:  ${result.lines.length}`,
        `Elapsed:          ${result.elapsedMs}ms`,
        `Closed by:        ${result.closedByTimeout ? "since window" : "daemon / max"}`,
      ].join("\n");
      const body =
        result.lines.length === 0
          ? "(no events observed)"
          : result.lines.join("\n");
      return [
        {
          type: "text",
          text: `=== Tail Event Bus ===\n\n${summary}\n\nFrames:\n${body}`,
        },
      ];
    }

    default:
      throw new Error(`Unknown sweep tool: ${name}`);
  }
}
