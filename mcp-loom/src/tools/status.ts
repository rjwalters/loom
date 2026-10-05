/**
 * Daemon status tool for Loom MCP server (issue #9950).
 *
 * The agent-relay half of the telemetry-warning principle: agents could not
 * see the daemon's observability state at all — `loom-daemon status` was a
 * human CLI surface, `DaemonStatusReport` flowed nowhere an agent reads, and
 * during a real outage (2AMLogic/2am#1878: an edge host's store tunnel dead
 * ~15h) the agent working on that host had the daemon's warnings in a log
 * file it never opened while the operator stayed uninformed. This tool makes
 * the daemon's status — and specifically its telemetry-export state — an
 * MCP surface, and instructs the agent to RELAY a problem verdict to the
 * user with the fix hint instead of filing it away as an internal detail.
 */

import type { Tool } from "@modelcontextprotocol/sdk/types.js";

import { sendDaemonRequest } from "../shared/daemon.js";

/** The observability-export state, snake_case on the wire (types.rs). */
export type ObservabilityExportState =
  | "disabled"
  | "starting"
  | "healthy"
  | "never_exported"
  | "failing"
  | "misconfigured"
  | "host_id_mismatch"
  | "unrecognized";

/** The subset of `DaemonStatusReport` this tool renders. */
export interface DaemonStatusReport {
  daemon_build_commit?: string | null;
  in_flight?: unknown[];
  observability_export?: ObservabilityExportStatus | null;
  observability_host_id_mismatch?: {
    daemon_host_id: string;
    ingest_host_id: string;
    first_seen_at: string;
  } | null;
  [key: string]: unknown;
}

export interface ObservabilityExportStatus {
  state: ObservabilityExportState;
  host_id?: string | null;
  ingest_host_id?: string | null;
  endpoint?: string | null;
  exporter?: string | null;
  started_at?: string | null;
  last_success_at?: string | null;
  last_failure_at?: string | null;
  last_failure_detail?: string | null;
  records_exported?: number | null;
  consecutive_failures?: number | null;
  flush_interval_secs?: number | null;
  endpoint_loopback?: boolean | null;
}

/** States that mean telemetry is demonstrably going wrong. */
export function isTelemetryProblem(state: ObservabilityExportState): boolean {
  return (
    state === "failing" ||
    state === "never_exported" ||
    state === "misconfigured" ||
    state === "host_id_mismatch"
  );
}

/**
 * Human wording for a state, mirroring `loom-daemon status`'s
 * `Observability: …` line closely enough that an agent's relay matches what
 * the operator would see on the CLI.
 */
export function describeObservability(
  s: ObservabilityExportStatus,
  now = new Date()
): string {
  const endpoint = s.endpoint ?? "(no endpoint)";
  const host = s.host_id ?? "unknown-host";
  const detail = s.last_failure_detail ? ` (${s.last_failure_detail})` : "";
  switch (s.state) {
    case "disabled":
      return "Observability: disabled (no telemetry export — opt in with observability.enabled=true)";
    case "misconfigured":
      return `Observability: MISCONFIGURED — enabled but not exporting → ${endpoint}${detail}`;
    case "starting":
      return `Observability: starting — exporter up as host_id=${host}, no batch acked yet → ${endpoint}`;
    case "never_exported":
      return `Observability: NEVER EXPORTED — running as host_id=${host} and no batch has EVER been acked; telemetry is not reaching ${endpoint}${detail}`;
    case "healthy": {
      const age = s.last_success_at
        ? `last export ${formatAge(now.getTime() - Date.parse(s.last_success_at))} ago`
        : "never";
      const local = s.endpoint_loopback
        ? " (LOCAL collector: delivery past this hop is not verified here)"
        : "";
      return `Observability: OK (first hop only) — ${age}, ${s.records_exported ?? 0} record(s) as host_id=${host} → ${endpoint}${local}`;
    }
    case "host_id_mismatch":
      return `Observability: HOST-ID MISMATCH — telemetry is landing under host_id=${s.ingest_host_id ?? "unknown"}, not ${host}`;
    case "failing":
      return `Observability: FAILING — ${s.consecutive_failures ?? 0} consecutive failed flush(es) as host_id=${host} → ${endpoint}${detail}`;
    default:
      return `Observability: unrecognized state "${s.state}" from a newer daemon — upgrade this client to read it`;
  }
}

// The Rust `ObservabilityExportStatus.ingest_host_id` rides the wire as
// `ingest_host_id` and is read directly in `describeObservability`.

function formatAge(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "?";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

/**
 * The relay directive: what the agent that called this tool is EXPECTED to
 * do with a problem verdict. Empty for healthy/disabled/starting — relay
 * pressure only applies to actual findings (#9950: "tell the user to fix
 * this", never silence while holding the evidence).
 */
export function relayDirective(
  s: ObservabilityExportStatus,
  mismatch?: { daemon_host_id: string; ingest_host_id: string; first_seen_at?: string } | null
): string {
  const fix =
    "check this host's OTel egress (edge collector / tunnel / ingest key) and the endpoint itself; `loom-daemon health` has the verdict, `loom-daemon status` the live line";
  if (mismatch) {
    return (
      `⚠️ TELEMETRY WARNING — RELAY TO THE USER: telemetry is being filed under ` +
      `host_id=${mismatch.ingest_host_id}, not ${mismatch.daemon_host_id} (the ingest key on this host ` +
      `is bound to that id). Tell the user; fix: install the key provisioned for ` +
      `${mismatch.daemon_host_id}, or set $LOOM_HOST_ID to match the key's binding.`
    );
  }
  switch (s.state) {
    case "failing":
    case "never_exported":
      return (
        `⚠️ TELEMETRY WARNING — RELAY TO THE USER: ${describeObservability(s)}. ` +
        `Every record the daemon produces is accumulating locally. Tell the user now; fix: ${fix}.`
      );
    case "misconfigured":
      return (
        `⚠️ TELEMETRY WARNING — RELAY TO THE USER: ${describeObservability(s)}. ` +
        `Tell the user; fix: ${fix}.`
      );
    default:
      return "";
  }
}

/**
 * Render the full tool output: the headline fields an agent needs, the
 * observability section (the #9950 relay surface), and — when telemetry is
 * demonstrably going wrong — the relay directive telling the agent to tell
 * the user.
 */
export function renderDaemonStatus(
  report: DaemonStatusReport,
  now = new Date()
): string {
  const lines: string[] = [];
  lines.push(
    `=== Loom Daemon Status ===`,
    `build: ${report.daemon_build_commit ?? "unknown"}`,
    `in-flight sweeps: ${report.in_flight?.length ?? 0}`,
  );
  const mismatch = report.observability_host_id_mismatch;
  if (mismatch) {
    lines.push(describeObservability({
      state: "host_id_mismatch",
      host_id: mismatch.daemon_host_id,
      ingest_host_id: mismatch.ingest_host_id,
    }, now));
  }
  const s = report.observability_export;
  if (s) {
    lines.push(describeObservability(s, now));
  } else {
    lines.push("Observability: unknown (older daemon binary — restart to pick up the export status surface)");
  }
  const directive = relayDirective(s ?? { state: "healthy" }, mismatch);
  if (directive) {
    lines.push("", directive);
  }
  return lines.join("\n");
}

export const statusTools: Tool[] = [
  {
    name: "daemon_status",
    description:
      "Get the Loom daemon's live status: build, in-flight sweeps, and — most importantly — the telemetry-export state. " +
      "When the output carries a TELEMETRY WARNING, you are expected to relay it to the user immediately with the fix hint " +
      "(issue #9950: tools must say telemetry is broken and tell the user how to fix it, never stay silent while holding the evidence).",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
  },
];

/**
 * Handle daemon_status calls: one bounded `Request::DaemonStatus` round trip
 * against the daemon's IPC socket, rendered for agent consumption.
 */
export async function handleStatusTool(
  name: string,
  _args?: Record<string, unknown>
): Promise<{ type: "text"; text: string }[]> {
  if (name !== "daemon_status") {
    throw new Error(`Unknown status tool: ${name}`);
  }
  try {
    const response = (await sendDaemonRequest(
      { type: "DaemonStatus", payload: null },
      Math.max(10_000, 2_000)
    )) as { type: string; payload?: DaemonStatusReport | null };
    const report = response?.payload;
    if (!report) {
      return [
        {
          type: "text",
          text: "daemon_status: the daemon answered but carried no status payload — it may predate the DaemonStatusReport surface; run `loom-daemon status` on the host instead.",
        },
      ];
    }
    return [{ type: "text", text: renderDaemonStatus(report) }];
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return [
      {
        type: "text",
        text:
          `daemon_status: could not reach the daemon (${message}). The daemon being down is itself ` +
          `worth relaying: sweeps and role ticks are not running, and — if the daemon is the telemetry ` +
          `producer on this host — its spans stopped with it. Tell the user; fix: check the daemon ` +
          `(launchctl print gui/$(id -u)/com.rjwalters.loom-daemon on macOS) and restart it.`,
      },
    ];
  }
}
