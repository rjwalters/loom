import type { HistoryRecord } from "../src/types.js";

let nextId = 1;

/** Reset the fixture id counter between tests that care about exact `id`
 * values (most don't). */
export function resetFixtureIds(): void {
  nextId = 1;
}

export function makeSweepCompleted(overrides: {
  sweepId: string;
  emittedAt: string;
  result: string;
  repo?: string;
  hostId?: string;
  id?: number;
}): HistoryRecord {
  return {
    id: overrides.id ?? nextId++,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: overrides.hostId ?? "host-a",
    kind: "sweep.completed",
    repo: overrides.repo ?? "rjwalters/loom",
    visibility: "public",
    issue: 1000,
    sweepId: overrides.sweepId,
    ingestedAt: overrides.emittedAt,
    record: {
      kind: "sweep.completed",
      repo: overrides.repo ?? "rjwalters/loom",
      visibility: "public",
      issue: 1000,
      sweep_id: overrides.sweepId,
      completed_at: overrides.emittedAt,
      result: overrides.result,
    },
  };
}

export function makeSweepOutcome(overrides: {
  sweepId: string;
  emittedAt: string;
  result?: string;
  model?: string;
  totalDurationSec?: number;
  phaseDurations?: { phase: string; duration_sec: number }[];
  repo?: string;
  hostId?: string;
  id?: number;
}): HistoryRecord {
  return {
    id: overrides.id ?? nextId++,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: overrides.hostId ?? "host-a",
    kind: "sweep.outcome",
    repo: overrides.repo ?? "rjwalters/loom",
    visibility: "public",
    issue: 1000,
    sweepId: overrides.sweepId,
    ingestedAt: overrides.emittedAt,
    record: {
      kind: "sweep.outcome",
      repo: overrides.repo ?? "rjwalters/loom",
      visibility: "public",
      issue: 1000,
      sweep_id: overrides.sweepId,
      model: overrides.model,
      phase_durations: overrides.phaseDurations ?? [],
      total_duration_sec: overrides.totalDurationSec,
      result: overrides.result,
    },
  };
}

/** A full "sweep" fixture: paired `sweep.completed` + `sweep.outcome`
 * records for one sweepId, the common case charts correlate. */
export function makeCompletedSweepPair(args: {
  sweepId: string;
  emittedAt: string;
  result: string;
  model: string;
  totalDurationSec: number;
  phaseDurations: { phase: string; duration_sec: number }[];
  repo?: string;
  hostId?: string;
}): HistoryRecord[] {
  return [
    makeSweepOutcome({
      sweepId: args.sweepId,
      emittedAt: args.emittedAt,
      result: args.result,
      model: args.model,
      totalDurationSec: args.totalDurationSec,
      phaseDurations: args.phaseDurations,
      repo: args.repo,
      hostId: args.hostId,
    }),
    makeSweepCompleted({
      sweepId: args.sweepId,
      emittedAt: args.emittedAt,
      result: args.result,
      repo: args.repo,
      hostId: args.hostId,
    }),
  ];
}
/**
 * Sample `/api/fleet-state` payloads.
 *
 * These are wire-shaped (`unknown` JSON, snake_case inside `record`), not
 * pre-parsed view models, so every test that uses them exercises `parse.ts`
 * the same way a live response would. Field names and value shapes are copied
 * from `../../docs/query-api.md` and `.loom/docs/telemetry-schema.md`.
 */

/** Fixed "now" for every time-dependent assertion. */
export const NOW = new Date("2026-07-30T12:10:00Z");

export function isoMinutesBefore(minutes: number, base: Date = NOW): string {
  return new Date(base.getTime() - minutes * 60_000).toISOString();
}

/** A healthy, busy host: full health, a healthy token pool, two live sweeps. */
export const HEALTHY_HOST_ID = "fleet-mac-1";

/** Reports fine, but only one account is left to rotate onto (2 total, 1
 * exhausted) → `degraded`: the pool is at the edge, not just "some accounts
 * spent". */
export const DEGRADED_HOST_ID = "fleet-linux-2";

/** Reports fine with a large pool and several accounts exhausted, but still
 * well clear of the degraded thresholds → `ok`. Pins #4864: a partially
 * spent pool operating exactly as designed must not read as degraded. */
export const PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID = "fleet-partial-6";

/** Last report is well past `STALE_AFTER_SEC` → `stale`. */
export const STALE_HOST_ID = "fleet-old-3";

/** Present only in `activeSweeps` — no `hosts` entry at all. */
export const SWEEP_ONLY_HOST_ID = "fleet-new-4";

/** Present in `hosts` with health but zero sweeps — the idle-host case. */
export const IDLE_HOST_ID = "fleet-idle-5";

export function multiHostSnapshot(): unknown {
  return {
    hosts: {
      [HEALTHY_HOST_ID]: {
        health: {
          record: {
            kind: "host.health",
            captured_at: isoMinutesBefore(2),
            daemon_version: "0.16.0",
            // #4956 build identity. The degraded host below deliberately
            // omits both, standing in for a record from a pre-#4956 daemon.
            build_commit: "8c16fb5b",
            built_at: isoMinutesBefore(360),
            uptime_sec: 86_400,
            logical_cpus: 28,
            cpu_idle_fraction: 0.83,
            load_per_core: 0.51,
            worktree_root_free_gb: 200,
          },
          updatedAt: isoMinutesBefore(2),
        },
        tokens: {
          record: {
            kind: "tokens.snapshot",
            captured_at: isoMinutesBefore(2),
            accounts: [
              {
                account: "agent-1",
                rank: 0,
                usage_fraction: 0.42,
                limit_window_reset_at: "2026-07-30T18:00:00Z",
                exhausted: false,
              },
              { account: "agent-2", rank: 1, usage_fraction: 0.11, exhausted: false },
            ],
          },
          updatedAt: isoMinutesBefore(2),
        },
      },
      [DEGRADED_HOST_ID]: {
        health: {
          record: {
            kind: "host.health",
            daemon_version: "0.16.0",
            uptime_sec: 3_600,
            logical_cpus: 8,
            // cpu_idle_fraction / load_per_core / worktree_root_free_gb are
            // absent — the "probe could not measure" case the schema mandates
            // be rendered as unknown, never zero.
          },
          updatedAt: isoMinutesBefore(1),
        },
        tokens: {
          record: {
            kind: "tokens.snapshot",
            accounts: [
              // An exhausted account that reports when its 7d window resets —
              // the case the whole countdown column exists for ("when does
              // this exhausted account come back?", issue #4874). Days out,
              // not hours: that is the distinction between "capacity returns
              // shortly" and "this host is spent until Saturday".
              {
                account: "agent-3",
                usage_fraction: 1,
                limit_window_reset_at: "2026-08-02T03:00:00Z",
                exhausted: true,
              },
              { account: "agent-4", exhausted: false },
            ],
          },
          updatedAt: isoMinutesBefore(1),
        },
      },
      [STALE_HOST_ID]: {
        health: {
          record: { kind: "host.health", daemon_version: "0.15.0", uptime_sec: 120 },
          updatedAt: isoMinutesBefore(90),
        },
      },
      [PARTIALLY_EXHAUSTED_HEALTHY_HOST_ID]: {
        health: {
          record: {
            kind: "host.health",
            daemon_version: "0.16.0",
            uptime_sec: 43_200,
            logical_cpus: 16,
            cpu_idle_fraction: 0.6,
            load_per_core: 0.8,
            worktree_root_free_gb: 300,
          },
          updatedAt: isoMinutesBefore(2),
        },
        tokens: {
          record: {
            kind: "tokens.snapshot",
            captured_at: isoMinutesBefore(2),
            // 14 accounts, 5 exhausted: 9 available (well above the
            // low-availability threshold) and 5/14 ≈ 0.36 exhausted (well
            // below the high-exhaustion threshold) — routine rotation.
            accounts: [
              { account: "p1", usage_fraction: 1, exhausted: true },
              { account: "p2", usage_fraction: 1, exhausted: true },
              { account: "p3", usage_fraction: 1, exhausted: true },
              { account: "p4", usage_fraction: 1, exhausted: true },
              { account: "p5", usage_fraction: 1, exhausted: true },
              { account: "p6", usage_fraction: 0.7, exhausted: false },
              { account: "p7", usage_fraction: 0.5, exhausted: false },
              { account: "p8", usage_fraction: 0.4, exhausted: false },
              { account: "p9", usage_fraction: 0.3, exhausted: false },
              { account: "p10", usage_fraction: 0.2, exhausted: false },
              { account: "p11", usage_fraction: 0.1, exhausted: false },
              { account: "p12", usage_fraction: 0.1, exhausted: false },
              { account: "p13", usage_fraction: 0.05, exhausted: false },
              { account: "p14", usage_fraction: 0.05, exhausted: false },
            ],
          },
          updatedAt: isoMinutesBefore(2),
        },
      },
      [IDLE_HOST_ID]: {
        health: {
          record: {
            kind: "host.health",
            daemon_version: "0.16.0",
            uptime_sec: 7_200,
            logical_cpus: 12,
            cpu_idle_fraction: 0.99,
            load_per_core: 0.02,
            worktree_root_free_gb: 512,
          },
          updatedAt: isoMinutesBefore(3),
        },
        tokens: { record: { kind: "tokens.snapshot", accounts: [] }, updatedAt: isoMinutesBefore(3) },
      },
    },
    activeSweeps: [
      {
        hostId: HEALTHY_HOST_ID,
        sweepId: "sweep-issue-4703-0",
        repo: "rjwalters/loom",
        visibility: "public",
        issue: 4703,
        phase: "builder",
        startedAt: isoMinutesBefore(10),
        enteredPhaseAt: isoMinutesBefore(4),
        model: "opus",
        effort: "high",
        updatedAt: isoMinutesBefore(4),
      },
      {
        hostId: HEALTHY_HOST_ID,
        sweepId: "sweep-issue-4749-0",
        repo: "rjwalters/loom",
        visibility: "public",
        issue: 4749,
        // No phase yet: sweep.started has arrived, sweep.phase has not.
        startedAt: isoMinutesBefore(1),
        model: "opus",
        updatedAt: isoMinutesBefore(1),
      },
      {
        hostId: SWEEP_ONLY_HOST_ID,
        sweepId: "sweep-issue-9001-0",
        visibility: "private",
        phase: "judge",
        startedAt: isoMinutesBefore(6),
        enteredPhaseAt: isoMinutesBefore(2),
        updatedAt: isoMinutesBefore(2),
      },
    ],
  };
}

export const EMPTY_SNAPSHOT: unknown = { hosts: {}, activeSweeps: [] };
