import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import {
  parseRetentionConfig,
  planSizeEviction,
  recordRetentionOutcome,
  RETENTION_LIVENESS_KIND,
  runRetentionSweep,
  type RetentionConfig,
  type RetentionResult,
} from "../src/retention";

async function insertRecord(db: D1Database, ingestedAt: string, kind = "host.health"): Promise<void> {
  await db
    .prepare(
      `INSERT INTO records
          (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, payload, ingested_at)
        VALUES (1, ?, 'host-abc', ?, NULL, 'private', NULL, NULL, '{}', ?)`,
    )
    .bind(ingestedAt, kind, ingestedAt)
    .run();
}

async function countRecords(db: D1Database): Promise<number> {
  const row = await db.prepare("SELECT COUNT(*) AS count FROM records").first<{ count: number }>();
  return row?.count ?? 0;
}

async function countByKind(db: D1Database, kind: string): Promise<number> {
  const row = await db.prepare("SELECT COUNT(*) AS count FROM records WHERE kind = ?").bind(kind).first<{ count: number }>();
  return row?.count ?? 0;
}

describe("parseRetentionConfig", () => {
  it("parses valid numeric vars", () => {
    expect(parseRetentionConfig({ RETENTION_DAYS: "30", MAX_RECORDS: "1000" })).toEqual({
      retentionDays: 30,
      maxRecords: 1000,
    });
  });

  it("falls back to safe defaults on missing or unparseable vars — never disables retention", () => {
    // 1,000,000 since 2AMLogic/2am#1608 (was 500,000): the deployed store
    // sat AT the old cap, so the size bound — not the 90-day age bound —
    // was the eviction actually firing and eating sweep history. 1M rows ≈
    // 3 GB, well inside D1's limits.
    expect(parseRetentionConfig({})).toEqual({ retentionDays: 90, maxRecords: 1_000_000 });
    expect(parseRetentionConfig({ RETENTION_DAYS: "not-a-number", MAX_RECORDS: "-5" })).toEqual({
      retentionDays: 90,
      maxRecords: 1_000_000,
    });
  });
});

describe("planSizeEviction — the tier-ordering policy, pinned without a database", () => {
  it("plans nothing when the table is within (or exactly at) the cap", () => {
    expect(planSizeEviction(5, 5, 5)).toEqual({ unprotected: 0, protectedOverflow: 0 });
    expect(planSizeEviction(3, 5, 0)).toEqual({ unprotected: 0, protectedOverflow: 0 });
  });

  it("tier 1 takes the whole excess when unprotected rows can cover it — protected rows survive", () => {
    // 7 rows, cap 5, 6 of them unprotected: the 2-row excess is entirely
    // tier 1's job; nothing protected is touched.
    expect(planSizeEviction(7, 5, 6)).toEqual({ unprotected: 2, protectedOverflow: 0 });
    // …even when the unprotected rows vastly outnumber the excess.
    expect(planSizeEviction(100, 3, 97)).toEqual({ unprotected: 97, protectedOverflow: 0 });
  });

  it("tier 2 fires only when EVERY unprotected row was already deleted and the table is still over cap", () => {
    // 5 rows, cap 2, only 2 unprotected: tier 1 deletes both, tier 2 takes
    // the 1-row residue out of the protected set.
    expect(planSizeEviction(5, 2, 2)).toEqual({ unprotected: 2, protectedOverflow: 1 });
    // All-protected table: tier 1 deletes nothing, tier 2 carries it all.
    expect(planSizeEviction(4, 1, 0)).toEqual({ unprotected: 0, protectedOverflow: 3 });
  });
});

describe("runRetentionSweep — age-based eviction", () => {
  it("deletes rows older than the retention window and keeps newer ones", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    const old = new Date(now.getTime() - 100 * 24 * 60 * 60 * 1000).toISOString(); // 100 days ago
    const recent = new Date(now.getTime() - 1 * 24 * 60 * 60 * 1000).toISOString(); // 1 day ago
    await insertRecord(env.DB, old);
    await insertRecord(env.DB, recent);

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 1_000_000 }, now);

    expect(result.deletedByAge).toBe(1);
    expect(await countRecords(env.DB)).toBe(1);
  });

  it("age eviction stays kind-blind — it evicts protected sweep rows too (the documented 90-day policy)", async () => {
    // Deliberately NOT part of the #1608 tiering: the tiering exists to
    // stop the SIZE cap from eating sweep history, not to make sweep rows
    // immortal. A sweep record older than RETENTION_DAYS is history that
    // has aged out, and must go like any other row.
    const now = new Date("2026-08-01T00:00:00Z");
    const old = new Date(now.getTime() - 100 * 24 * 60 * 60 * 1000).toISOString();
    await insertRecord(env.DB, old, "sweep.outcome");

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 1_000_000 }, now);

    expect(result.deletedByAge).toBe(1);
    expect(await countByKind(env.DB, "sweep.outcome")).toBe(0);
  });
});

describe("runRetentionSweep — size-based eviction, tiered (2AMLogic/2am#1608)", () => {
  it("tier 1 evicts the oldest UNPROTECTED excess rows, and protected rows survive", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    // 3 unprotected snapshots + 2 protected sweep.outcome rows, all recent
    // (the age sweep is a no-op), over a cap of 3.
    for (let i = 0; i < 3; i++) {
      await insertRecord(env.DB, new Date(now.getTime() - (10 - i) * 1000).toISOString(), "host.health");
    }
    for (let i = 0; i < 2; i++) {
      await insertRecord(env.DB, new Date(now.getTime() - (10 - i) * 1000).toISOString(), "sweep.outcome");
    }

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 3 }, now);

    // Excess is 2; both come out of the unprotected tier — the two OLDEST
    // host.health rows — and both sweep.outcome rows survive.
    expect(result.deletedByAge).toBe(0);
    expect(result.deletedBySize).toBe(2);
    expect(result.deletedBySizeUnprotected).toBe(2);
    expect(result.deletedBySizeProtected).toBe(0);
    expect(await countRecords(env.DB)).toBe(3);
    expect(await countByKind(env.DB, "sweep.outcome")).toBe(2);
    expect(await countByKind(env.DB, "host.health")).toBe(1);
  });

  it("tier 1 deletes the OLDEST unprotected rows first, not just any unprotected rows", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    // Insert oldest-first; cap 1 → excess 2 → the two oldest host.health
    // rows go, the newest survives.
    await insertRecord(env.DB, new Date(now.getTime() - 5000).toISOString(), "host.health");
    await insertRecord(env.DB, new Date(now.getTime() - 4000).toISOString(), "host.health");
    await insertRecord(env.DB, new Date(now.getTime() - 3000).toISOString(), "host.health");

    await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 1 }, now);

    const { results } = await env.DB.prepare("SELECT kind FROM records").all();
    expect(results).toEqual([{ kind: "host.health" }]);
  });

  it("tier 2 fires only when unprotected rows are exhausted — protected rows go oldest-first", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    // 2 unprotected + 3 protected, cap 2 → tier 1 deletes both unprotected,
    // tier 2 deletes the single OLDEST protected row; the two newest
    // protected rows survive.
    await insertRecord(env.DB, new Date(now.getTime() - 9000).toISOString(), "host.health");
    await insertRecord(env.DB, new Date(now.getTime() - 8000).toISOString(), "host.health");
    await insertRecord(env.DB, new Date(now.getTime() - 7000).toISOString(), "sweep.outcome");
    await insertRecord(env.DB, new Date(now.getTime() - 6000).toISOString(), "sweep.outcome");
    await insertRecord(env.DB, new Date(now.getTime() - 5000).toISOString(), "sweep.outcome");

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 2 }, now);

    expect(result.deletedByAge).toBe(0);
    expect(result.deletedBySize).toBe(3);
    expect(result.deletedBySizeUnprotected).toBe(2);
    expect(result.deletedBySizeProtected).toBe(1);
    expect(await countRecords(env.DB)).toBe(2);
    // Only the two NEWEST sweep.outcome rows remain — never an older
    // protected row while a newer one is evicted.
    const { results } = await env.DB.prepare("SELECT kind FROM records ORDER BY id").all();
    expect(results).toEqual([{ kind: "sweep.outcome" }, { kind: "sweep.outcome" }]);
  });

  it("covers every protected shape: sweep.% prefix, session.summary, session.analysis", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    await insertRecord(env.DB, new Date(now.getTime() - 9000).toISOString(), "sweep.started");
    await insertRecord(env.DB, new Date(now.getTime() - 8000).toISOString(), "sweep.phase");
    await insertRecord(env.DB, new Date(now.getTime() - 7000).toISOString(), "session.summary");
    await insertRecord(env.DB, new Date(now.getTime() - 6000).toISOString(), "session.analysis");
    // Two unprotected rows to absorb the whole excess.
    await insertRecord(env.DB, new Date(now.getTime() - 5000).toISOString(), "host.health");
    await insertRecord(env.DB, new Date(now.getTime() - 4000).toISOString(), "tokens.snapshot");

    // Cap 4 → excess 2, both unprotected: every protected row survives.
    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 4 }, now);

    expect(result.deletedBySizeUnprotected).toBe(2);
    expect(result.deletedBySizeProtected).toBe(0);
    expect(await countRecords(env.DB)).toBe(4);
    expect(await countByKind(env.DB, "sweep.started")).toBe(1);
    expect(await countByKind(env.DB, "sweep.phase")).toBe(1);
    expect(await countByKind(env.DB, "session.summary")).toBe(1);
    expect(await countByKind(env.DB, "session.analysis")).toBe(1);
  });

  it("is a no-op when the table is within both bounds", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    await insertRecord(env.DB, now.toISOString());

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 1_000_000 }, now);

    expect(result).toEqual({ deletedByAge: 0, deletedBySize: 0, deletedBySizeProtected: 0, deletedBySizeUnprotected: 0 });
    expect(await countRecords(env.DB)).toBe(1);
  });
});

describe("recordRetentionOutcome — eviction visibility liveness row (2AMLogic/2am#1608)", () => {
  const config: RetentionConfig = { retentionDays: 90, maxRecords: 1_000_000 };
  const now = new Date("2026-08-01T05:00:00Z");

  const sweepResult = (overrides: Partial<RetentionResult> = {}): RetentionResult => ({
    deletedByAge: 0,
    deletedBySize: 0,
    deletedBySizeProtected: 0,
    deletedBySizeUnprotected: 0,
    ...overrides,
  });

  async function livenessRows(): Promise<Record<string, unknown>[]> {
    const { results } = await env.DB
      .prepare("SELECT * FROM records WHERE kind = ? ORDER BY id")
      .bind(RETENTION_LIVENESS_KIND)
      .all();
    return results as Record<string, unknown>[];
  }

  it("writes no row when the sweep deleted nothing (no one row per cron tick)", async () => {
    const recorded = await recordRetentionOutcome(env.DB, config, sweepResult(), now);
    expect(recorded).toBe(false);
    expect(await livenessRows()).toHaveLength(0);
  });

  it("writes a row when anything was deleted — including age-only sweeps", async () => {
    const recorded = await recordRetentionOutcome(
      env.DB,
      config,
      sweepResult({ deletedByAge: 12, deletedBySize: 3, deletedBySizeUnprotected: 3 }),
      now,
    );
    expect(recorded).toBe(true);

    const rows = await livenessRows();
    expect(rows).toHaveLength(1);
    const row = rows[0];
    if (!row) throw new Error("expected the liveness row to exist");
    // Column bindings mirror the ingest INSERT's contract: host-level
    // backend row, not repo-scoped, fail-safe private.
    expect(row.schema_version).toBe(1);
    expect(row.host_id).toBe("backend");
    expect(row.repo).toBeNull();
    expect(row.visibility).toBe("private");
    expect(row.issue).toBeNull();
    expect(row.emitted_at).toBe(now.toISOString());
    expect(row.ingested_at).toBe(now.toISOString());
    expect(JSON.parse(row.payload as string)).toEqual({
      deletedByAge: 12,
      deletedBySizeProtected: 0,
      deletedBySizeUnprotected: 3,
      maxRecords: 1_000_000,
    });
  });

  it("sweep_id is synthetic and unique per unix hour — stable within, distinct across hours", async () => {
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedByAge: 1 }), now);
    const rows = await livenessRows();
    expect(rows[0]?.sweep_id).toBe(`retention-${Math.floor(now.getTime() / 3_600_000)}`);

    const nextHour = new Date(now.getTime() + 3_600_000);
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedByAge: 1 }), nextHour);
    const after = await livenessRows();
    expect(after).toHaveLength(2);
    expect(after[1]?.sweep_id).not.toBe(after[0]?.sweep_id);
  });

  it("a same-hour replay is idempotent — no duplicate row for the same fact", async () => {
    // E.g. the cron tick ran, then an operator forced /admin/retention/run
    // in the same hour: same hour, same eviction fact, one marker.
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedBySize: 2, deletedBySizeUnprotected: 2 }), now);
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedBySize: 2, deletedBySizeUnprotected: 2 }), now);
    expect(await livenessRows()).toHaveLength(1);
  });

  it("a DIFFERENT hour with deletions still gets its own row after a replay", async () => {
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedByAge: 1 }), now);
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedByAge: 1 }), now);
    await recordRetentionOutcome(env.DB, config, sweepResult({ deletedBySize: 1, deletedBySizeProtected: 1 }), new Date(now.getTime() + 3_600_000));
    expect(await livenessRows()).toHaveLength(2);
  });
});
