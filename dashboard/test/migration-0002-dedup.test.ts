import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";

// Regression test for Issue #5107: production already had duplicate
// (kind, sweep_id) rows for `sweep.completed`/`sweep.outcome` predating the
// partial UNIQUE index added by migrations/0002_idempotent_terminal_records.sql
// (Issue #5084/#5106) — `CREATE UNIQUE INDEX` fails outright
// (SQLITE_CONSTRAINT_UNIQUE) on data that already violates the constraint it
// is about to create.
//
// `test/apply-migrations.ts` already applied every migration — including
// 0002 — to this file's isolated in-memory D1 instance before this test body
// runs, so the unique index already exists here. That setup never exercises
// this bug: it always starts from an *empty* table, which trivially has no
// duplicates to violate the constraint. To reproduce the real failure mode
// (and prove the fix), each test below rewinds this database to production's
// actual pre-migration state — schema from 0001, no unique index, and
// duplicate rows inserted the way they arrived on the daemon side: a raw
// `INSERT` that bypasses `src/index.ts`'s `INSERT OR IGNORE` guard (itself
// only a future-duplicates guard, not a backfill) — then re-applies exactly
// migration 0002's statements and asserts on the result.

const MIGRATION_NAME = "0002_idempotent_terminal_records.sql";

/** Insert a `records` row directly via raw SQL, bypassing the ingest
 * handler's `INSERT OR IGNORE` guard entirely — reproducing a duplicate row
 * that predates that guard's existence. Returns the row's assigned id. */
async function insertRawRecord(
  db: D1Database,
  opts: { kind: string; sweepId: string | null; emittedAt: string },
): Promise<number> {
  const result = await db
    .prepare(
      `INSERT INTO records (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, payload, ingested_at)
       VALUES (1, ?, 'host-abc', ?, 'rjwalters/loom', 'public', 4703, ?, '{}', ?)`,
    )
    .bind(opts.emittedAt, opts.kind, opts.sweepId, opts.emittedAt)
    .run();
  const lastRowId = result.meta.last_row_id;
  expect(typeof lastRowId).toBe("number");
  return lastRowId;
}

/** Reset this test's D1 instance to production's actual pre-#5107-fix
 * state: the unique index (already created once by the per-file migration
 * setup) is dropped, reproducing "0002 never successfully applied". */
async function dropExistingUniqueIndex(db: D1Database): Promise<void> {
  await db.exec("DROP INDEX IF EXISTS idx_records_terminal_sweep_once");
}

/** Re-apply exactly migration 0002's statements (dedup DELETE + CREATE
 * UNIQUE INDEX) against the current DB state, using the same parsed
 * queries `wrangler d1 migrations apply` / the per-file test setup use
 * (env.TEST_MIGRATIONS, built by `readD1Migrations` in vitest.config.ts). */
async function reapplyMigration0002(db: D1Database): Promise<void> {
  const migration = env.TEST_MIGRATIONS.find((m) => m.name === MIGRATION_NAME);
  if (!migration) {
    throw new Error(`migration ${MIGRATION_NAME} not found in TEST_MIGRATIONS`);
  }
  for (const query of migration.queries) {
    await db.prepare(query).run();
  }
}

async function recordsFor(
  db: D1Database,
  kind: string,
  sweepId: string,
): Promise<{ id: number }[]> {
  const { results } = await db
    .prepare("SELECT id FROM records WHERE kind = ? AND sweep_id = ? ORDER BY id")
    .bind(kind, sweepId)
    .all();
  return results as { id: number }[];
}

describe("migrations/0002_idempotent_terminal_records.sql — dedup pre-existing duplicates (Issue #5107)", () => {
  it("applies successfully against pre-existing duplicate sweep.completed/sweep.outcome rows, keeping MAX(id)", async () => {
    await dropExistingUniqueIndex(env.DB);

    // Two pre-existing duplicate sweep.completed rows for the same sweep_id
    // — the second (higher id) is the more recently ingested one and must
    // be the one that survives.
    const completedFirst = await insertRawRecord(env.DB, {
      kind: "sweep.completed",
      sweepId: "sweep-dup-completed",
      emittedAt: "2026-07-30T12:00:00Z",
    });
    const completedSecond = await insertRawRecord(env.DB, {
      kind: "sweep.completed",
      sweepId: "sweep-dup-completed",
      emittedAt: "2026-07-30T12:00:05Z",
    });

    // Three pre-existing duplicate sweep.outcome rows for the same
    // sweep_id — same expectation, just more of them.
    const outcomeFirst = await insertRawRecord(env.DB, {
      kind: "sweep.outcome",
      sweepId: "sweep-dup-outcome",
      emittedAt: "2026-07-30T12:01:00Z",
    });
    const outcomeSecond = await insertRawRecord(env.DB, {
      kind: "sweep.outcome",
      sweepId: "sweep-dup-outcome",
      emittedAt: "2026-07-30T12:01:05Z",
    });
    const outcomeThird = await insertRawRecord(env.DB, {
      kind: "sweep.outcome",
      sweepId: "sweep-dup-outcome",
      emittedAt: "2026-07-30T12:01:10Z",
    });

    // A non-duplicated sweep.completed row (unique sweep_id) — must survive
    // untouched.
    const uniqueCompleted = await insertRawRecord(env.DB, {
      kind: "sweep.completed",
      sweepId: "sweep-unique-completed",
      emittedAt: "2026-07-30T12:02:00Z",
    });

    // Re-applying migration 0002 against data that already violates the
    // uniqueness it is about to create must NOT throw
    // SQLITE_CONSTRAINT_UNIQUE (the reported production failure).
    await expect(reapplyMigration0002(env.DB)).resolves.not.toThrow();

    const completedRows = await recordsFor(env.DB, "sweep.completed", "sweep-dup-completed");
    expect(completedRows).toHaveLength(1);
    expect(completedRows[0]?.id).toBe(completedSecond);
    expect(completedRows[0]?.id).not.toBe(completedFirst);

    const outcomeRows = await recordsFor(env.DB, "sweep.outcome", "sweep-dup-outcome");
    expect(outcomeRows).toHaveLength(1);
    expect(outcomeRows[0]?.id).toBe(outcomeThird);
    expect(outcomeRows[0]?.id).not.toBe(outcomeFirst);
    expect(outcomeRows[0]?.id).not.toBe(outcomeSecond);

    const uniqueRows = await recordsFor(env.DB, "sweep.completed", "sweep-unique-completed");
    expect(uniqueRows).toHaveLength(1);
    expect(uniqueRows[0]?.id).toBe(uniqueCompleted);

    // The index must be functional again afterwards: a fresh raw duplicate
    // insert (bypassing INSERT OR IGNORE) is rejected by the recreated
    // constraint.
    await expect(
      insertRawRecord(env.DB, {
        kind: "sweep.completed",
        sweepId: "sweep-dup-completed",
        emittedAt: "2026-07-30T12:03:00Z",
      }),
    ).rejects.toThrow();
  });

  it("does NOT touch duplicate rows of any other kind — only sweep.completed/sweep.outcome are deduped", async () => {
    await dropExistingUniqueIndex(env.DB);

    // sweep.phase legitimately has many rows per sweep_id (one per
    // lifecycle phase) — the dedup DELETE must never touch it, duplicate
    // sweep_id or not.
    const phaseFirst = await insertRawRecord(env.DB, {
      kind: "sweep.phase",
      sweepId: "sweep-shared",
      emittedAt: "2026-07-30T12:00:00Z",
    });
    const phaseSecond = await insertRawRecord(env.DB, {
      kind: "sweep.phase",
      sweepId: "sweep-shared",
      emittedAt: "2026-07-30T12:00:05Z",
    });

    // host.health carries no sweep_id at all.
    const healthFirst = await insertRawRecord(env.DB, {
      kind: "host.health",
      sweepId: null,
      emittedAt: "2026-07-30T12:00:00Z",
    });
    const healthSecond = await insertRawRecord(env.DB, {
      kind: "host.health",
      sweepId: null,
      emittedAt: "2026-07-30T12:00:05Z",
    });

    await reapplyMigration0002(env.DB);

    const phaseRows = await recordsFor(env.DB, "sweep.phase", "sweep-shared");
    expect(phaseRows.map((r) => r.id).sort((a, b) => a - b)).toEqual(
      [phaseFirst, phaseSecond].sort((a, b) => a - b),
    );

    const { results: healthRows } = await env.DB.prepare(
      "SELECT id FROM records WHERE kind = 'host.health' ORDER BY id",
    ).all();
    expect((healthRows as { id: number }[]).map((r) => r.id).sort((a, b) => a - b)).toEqual(
      [healthFirst, healthSecond].sort((a, b) => a - b),
    );
  });

  it("is a no-op when there are no pre-existing duplicates (the common/fresh-database case)", async () => {
    await dropExistingUniqueIndex(env.DB);

    const onlyCompleted = await insertRawRecord(env.DB, {
      kind: "sweep.completed",
      sweepId: "sweep-solo",
      emittedAt: "2026-07-30T12:00:00Z",
    });

    await expect(reapplyMigration0002(env.DB)).resolves.not.toThrow();

    const rows = await recordsFor(env.DB, "sweep.completed", "sweep-solo");
    expect(rows).toHaveLength(1);
    expect(rows[0]?.id).toBe(onlyCompleted);
  });
});
