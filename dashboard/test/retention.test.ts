import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { parseRetentionConfig, runRetentionSweep } from "../src/retention";

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

describe("parseRetentionConfig", () => {
  it("parses valid numeric vars", () => {
    expect(parseRetentionConfig({ RETENTION_DAYS: "30", MAX_RECORDS: "1000" })).toEqual({
      retentionDays: 30,
      maxRecords: 1000,
    });
  });

  it("falls back to safe defaults on missing or unparseable vars — never disables retention", () => {
    expect(parseRetentionConfig({})).toEqual({ retentionDays: 90, maxRecords: 500_000 });
    expect(parseRetentionConfig({ RETENTION_DAYS: "not-a-number", MAX_RECORDS: "-5" })).toEqual({
      retentionDays: 90,
      maxRecords: 500_000,
    });
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
});

describe("runRetentionSweep — size-based eviction", () => {
  it("evicts the oldest excess rows once the table exceeds maxRecords", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    // All rows are "recent" (age sweep is a no-op) but there are more than
    // the configured cap.
    for (let i = 0; i < 5; i++) {
      await insertRecord(env.DB, new Date(now.getTime() - i * 1000).toISOString());
    }

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 3 }, now);

    expect(result.deletedByAge).toBe(0);
    expect(result.deletedBySize).toBe(2);
    expect(await countRecords(env.DB)).toBe(3);
  });

  it("is a no-op when the table is within both bounds", async () => {
    const now = new Date("2026-08-01T00:00:00Z");
    await insertRecord(env.DB, now.toISOString());

    const result = await runRetentionSweep(env.DB, { retentionDays: 90, maxRecords: 500_000 }, now);

    expect(result).toEqual({ deletedByAge: 0, deletedBySize: 0 });
    expect(await countRecords(env.DB)).toBe(1);
  });
});
