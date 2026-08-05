import { describe, expect, it } from "vitest";
import { buildHealthMetricTrend } from "../src/charts/hostHealthTrend.js";
import type { HistoryRecord, TelemetryRecord } from "../src/types.js";

function healthRecord(overrides: {
  emittedAt: string;
  cpuIdleFraction?: number;
  worktreeRootFreeGb?: number;
  id?: number;
}): HistoryRecord {
  const record: Record<string, unknown> = { kind: "host.health" };
  if (overrides.cpuIdleFraction !== undefined) record.cpu_idle_fraction = overrides.cpuIdleFraction;
  if (overrides.worktreeRootFreeGb !== undefined) record.worktree_root_free_gb = overrides.worktreeRootFreeGb;

  return {
    id: overrides.id ?? 1,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: "host-a",
    kind: "host.health",
    ingestedAt: overrides.emittedAt,
    record: record as unknown as TelemetryRecord,
  };
}

describe("buildHealthMetricTrend", () => {
  it("builds one point per host.health record, in chronological order", () => {
    const records = [
      healthRecord({ emittedAt: "2026-08-01T12:00:00Z", cpuIdleFraction: 0.5, id: 2 }),
      healthRecord({ emittedAt: "2026-08-01T10:00:00Z", cpuIdleFraction: 0.8, id: 1 }),
    ];

    expect(buildHealthMetricTrend(records, "cpu_idle_fraction")).toEqual([
      { emittedAt: "2026-08-01T10:00:00Z", value: 0.8 },
      { emittedAt: "2026-08-01T12:00:00Z", value: 0.5 },
    ]);
  });

  // The single most important correctness detail per issue #5355: a probe
  // that could not measure a field omits it — never a fabricated zero.
  it("renders a record missing the field as a null gap, not zero", () => {
    const records = [
      healthRecord({ emittedAt: "2026-08-01T10:00:00Z", cpuIdleFraction: 0.8 }),
      healthRecord({ emittedAt: "2026-08-01T11:00:00Z" }), // no cpu_idle_fraction this tick
      healthRecord({ emittedAt: "2026-08-01T12:00:00Z", cpuIdleFraction: 0.6 }),
    ];

    const trend = buildHealthMetricTrend(records, "cpu_idle_fraction");
    expect(trend.map((point) => point.value)).toEqual([0.8, null, 0.6]);
  });

  it("ignores non-host.health records", () => {
    const records: HistoryRecord[] = [
      {
        id: 1,
        schemaVersion: 1,
        emittedAt: "2026-08-01T10:00:00Z",
        hostId: "host-a",
        kind: "sweep.outcome",
        ingestedAt: "2026-08-01T10:00:00Z",
        record: { kind: "sweep.outcome", sweep_id: "s1", result: "success" },
      },
      healthRecord({ emittedAt: "2026-08-01T11:00:00Z", worktreeRootFreeGb: 120 }),
    ];

    expect(buildHealthMetricTrend(records, "worktree_root_free_gb")).toEqual([
      { emittedAt: "2026-08-01T11:00:00Z", value: 120 },
    ]);
  });

  it("returns an empty array when there is no host.health history", () => {
    expect(buildHealthMetricTrend([], "cpu_idle_fraction")).toEqual([]);
  });

  it("treats a non-numeric field value as a gap rather than throwing", () => {
    const record = healthRecord({ emittedAt: "2026-08-01T10:00:00Z" });
    (record.record as unknown as Record<string, unknown>).cpu_idle_fraction = "unknown";
    expect(buildHealthMetricTrend([record], "cpu_idle_fraction")).toEqual([
      { emittedAt: "2026-08-01T10:00:00Z", value: null },
    ]);
  });
});
