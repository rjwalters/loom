/**
 * The Live status board's view model (issue #9077) — everything the `#/live`
 * tab shows that can be computed without a DOM.
 *
 * The board is built for a screen someone glances at from across the room, so
 * this module answers the glance-level questions: how many hosts are up, how
 * much is running, how deep the queue is, where each sweep is in its
 * lifecycle, and a one-line sentence for each event on the live tail. The
 * rendering and the keyed DOM reconciliation live in `views/liveBoard.ts`.
 *
 * Same unknown-is-not-zero rule as the rest of the app: a value no host
 * reports is `undefined` here and renders as `—`, never as `0`.
 */

import { type FleetView, type HostView } from "./fleet";
import { fleetQueueTotals, summarizeHostQueue } from "./workQueue";
import type { ActiveSweep, LiveTailFrame } from "./types";

/** The sweep lifecycle, in order (`.loom/docs/telemetry-schema.md`
 * `sweep.phase`). The board's phase stepper draws one segment per entry. */
export const LIVE_PHASES = ["curator", "builder", "judge", "doctor", "merge"] as const;

/** Position of `phase` in `LIVE_PHASES`, or `-1` for a sweep that has not
 * reported a phase yet or reports one this build does not know. */
export function phaseIndex(phase: string | undefined): number {
  if (!phase) return -1;
  return (LIVE_PHASES as readonly string[]).indexOf(phase.toLowerCase());
}

/** The headline tiles. */
export interface BoardTiles {
  /** Hosts reporting recently (ok, throttled or degraded). */
  hostsOnline: number;
  /** Every host the board knows about, sweep-only and roster-missing included. */
  hostsTotal: number;
  running: number;
  /** Ready-to-dispatch issues across hosts with a current queue, or
   * `undefined` when no host has a current queue to count. */
  ready: number | undefined;
  blocked: number | undefined;
  /** Highest token-window usage on any online host, or `undefined` when no
   * online host reports one. */
  peakUsage: number | undefined;
  /** Hosts in a state that needs a person (`FleetView.needsAttention`). */
  attention: number;
}

/** Whether a host is currently pushing telemetry. */
export function isOnline(host: HostView): boolean {
  return host.status === "ok" || host.status === "throttled" || host.status === "degraded";
}

export function boardTiles(view: FleetView, now: Date): BoardTiles {
  const queue = fleetQueueTotals(view.hosts.map((host) => summarizeHostQueue(host, now)));
  const online = view.hosts.filter(isOnline);
  const usages = online
    .map((host) => host.tokens.peakUsage)
    .filter((value): value is number => value !== undefined);
  return {
    hostsOnline: online.length,
    hostsTotal: view.hosts.length,
    running: view.totalSweeps,
    ready: queue.currentHosts > 0 ? queue.ready : undefined,
    blocked: queue.currentHosts > 0 ? queue.blocked : undefined,
    peakUsage: usages.length > 0 ? Math.max(...usages) : undefined,
    attention: view.needsAttention,
  };
}

/** A host's CPU load as a 0–1 meter fill: `load_per_core` (1.0 = every core
 * busy) clamped, or `undefined` when the host does not report it. */
export function loadFraction(host: HostView): number | undefined {
  const load = host.entry.health?.record.load_per_core;
  if (load === undefined || !Number.isFinite(load)) return undefined;
  return Math.min(1, Math.max(0, load));
}

/** Every in-flight sweep across the fleet, longest-running first. Each
 * `HostView.sweeps` list is already sorted that way; this merges them. */
export function allSweeps(view: FleetView): ActiveSweep[] {
  const sweeps = view.hosts.flatMap((host) => host.sweeps);
  const start = (sweep: ActiveSweep): number => {
    const parsed = sweep.startedAt ? Date.parse(sweep.startedAt) : Number.NaN;
    return Number.isNaN(parsed) ? Number.POSITIVE_INFINITY : parsed;
  };
  return sweeps.sort((a, b) => start(a) - start(b) || a.sweepId.localeCompare(b.sweepId));
}

/** `"#123"` for a sweep on a known issue, with the repo's short name when it
 * is known — `"loom#123"`. A redacted private sweep has no repo. */
export function sweepTitle(repo: string | undefined, issue: number | undefined): string {
  const name = repo?.split("/").pop();
  if (issue === undefined) return name ?? "sweep";
  return name ? `${name}#${issue}` : `#${issue}`;
}

/** `"1:02:03"` / `"12:04"` — a stopwatch reading, which suits a board better
 * than `formatDuration`'s coarse `"1h 2m"` because it visibly ticks. */
export function formatStopwatch(seconds: number | undefined): string {
  if (seconds === undefined || !Number.isFinite(seconds)) return "—";
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const mm = String(minutes).padStart(hours > 0 ? 2 : 1, "0");
  const ss = String(secs).padStart(2, "0");
  return hours > 0 ? `${hours}:${mm}:${ss}` : `${mm}:${ss}`;
}

export type EventTone = "start" | "phase" | "ok" | "bad" | "warn" | "info";

/** One ticker line. */
export interface BoardEvent {
  tone: EventTone;
  /** `"loom#123"`-style subject, or `undefined` for a host-level event. */
  subject: string | undefined;
  /** What happened, as a short phrase ("entered judge", "completed · success"). */
  text: string;
  hostId: string;
  /** The event's own timestamp (`emittedAt`), for the ticker's "12s ago". */
  at: string;
  /** Link for `subject`, when it resolves. */
  repo: string | undefined;
  issue: number | undefined;
}

const RESULT_TONE: Readonly<Record<string, EventTone>> = {
  success: "ok",
  failure: "bad",
  cancelled: "warn",
  blocked: "warn",
};

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/**
 * A live-tail frame as a ticker sentence, or `null` for the kinds that are
 * housekeeping rather than news (`host.health`, `tokens.snapshot`,
 * `queue.snapshot`, …). Those still count as a host heartbeat — the view
 * pulses the host's pill for them — they just do not earn a ticker line.
 */
export function describeEvent(frame: LiveTailFrame): BoardEvent | null {
  const record = frame.event.record as Record<string, unknown>;
  const kind = str(record.kind) ?? frame.topic;
  const repo = str(record.repo);
  const issue = num(record.issue);
  const base = {
    subject: sweepTitle(repo, issue),
    hostId: frame.event.hostId,
    at: frame.event.emittedAt,
    repo,
    issue,
  };

  switch (kind) {
    case "sweep.started": {
      const model = str(record.model);
      return { ...base, tone: "start", text: model ? `sweep started · ${model}` : "sweep started" };
    }
    case "sweep.phase": {
      const phase = str(record.phase);
      return { ...base, tone: "phase", text: phase ? `entered ${phase}` : "changed phase" };
    }
    case "sweep.completed": {
      const result = str(record.result);
      return { ...base, tone: RESULT_TONE[result ?? ""] ?? "info", text: `completed · ${result ?? "unknown"}` };
    }
    case "sweep.outcome": {
      const pr = num(record.pr_number);
      const result = str(record.result);
      if (pr === undefined) return null; // `sweep.completed` already said it
      return { ...base, tone: RESULT_TONE[result ?? ""] ?? "info", text: `opened PR #${pr}` };
    }
    case "ephemeral_compute": {
      // Launch and completion share one kind; `ended_at` tells them apart
      // (`../../src/fleetState.ts` §"Live `compute:` entries").
      const verb = str(record.ended_at) ? "compute finished" : "compute launched";
      const instance = str(record.instance_type);
      return { ...base, subject: undefined, tone: "info", text: instance ? `${verb} · ${instance}` : verb };
    }
    default:
      return null;
  }
}
