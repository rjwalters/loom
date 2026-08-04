import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";

// Regression test for issue #5107: production already had duplicate
// `(kind, sweep_id)` rows for `sweep.completed`/`sweep.outcome` predating
// migration 0002's partial UNIQUE index (added in #5106) — the
// `INSERT OR IGNORE` guard in src/index.ts's ingest handler is new in that
// same PR and only prevents *future* duplicates, so `CREATE UNIQUE INDEX`
// failed outright with SQLITE_CONSTRAINT_UNIQUE against pre-existing data.
//
// The suite-wide setup (test/apply-migrations.ts) already applies every
// migration — including 0002 — to a fresh, duplicate-free in-memory D1
// instance before any test runs, so 0002's CREATE UNIQUE INDEX trivially
// succeeds there and never exercises this failure mode. To reproduce it,
// this test drops the index its own copy of the schema already has (each
// test gets isolated storage, so this does not affect other test files/
// tests), seeds duplicate rows via a raw INSERT that bypasses the app's
// INSERT OR IGNORE guard — mirroring the pre-existing production rows — and
// then replays migration 0002's own statements (exactly as
// `wrangler d1 migrations apply` would split and run them, via the same
// `unstable_splitSqlQuery` the vitest-pool-workers config helper uses to
// build `env.TEST_MIGRATIONS`).

const MIGRATION_NAME = "0002_idempotent_terminal_records.sql";

async function insertRawRecord(kind: string, sweepId: string | null): Promise<void> {
  await env.DB.prepare(
    `INSERT INTO records
       (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, payload, ingested_at)
     VALUES (1, '2026-07-30T12:00:00Z', 'host-abc', ?, 'rjwalters/loom', 'public', 4703, ?, '{}', '2026-07-30T12:00:00Z')`,
  )
    .bind(kind, sweepId)
    .run();
}

async function idsFor(kind: string, sweepId: string): Promise<number[]> {
  const { results } = await env.DB.prepare("SELECT id FROM records WHERE kind = ? AND sweep_id = ? ORDER BY id")
    .bind(kind, sweepId)
    .all();
  return (results as { id: number }[]).map((r) => r.id);
}

describe("migration 0002 dedups pre-existing duplicate terminal records (issue #5107)", () => {
  it("applies cleanly against duplicate sweep.completed/sweep.outcome rows and keeps exactly one (the most recent) per (kind, sweep_id)", async () => {
    // Reproduce the pre-0002 schema state: drop the index this test's copy
    // already has (from the suite-wide setup) so CREATE UNIQUE INDEX below
    // has real duplicate data to contend with, just like production did.
    await env.DB.exec("DROP INDEX idx_records_terminal_sweep_once");

    // Duplicate sweep.completed rows for the same sweep_id.
    await insertRawRecord("sweep.completed", "sweep-dup-completed");
    await insertRawRecord("sweep.completed", "sweep-dup-completed");
    const [, keptCompletedId] = await idsFor("sweep.completed", "sweep-dup-completed");

    // Triplicate sweep.outcome rows for the same sweep_id.
    await insertRawRecord("sweep.outcome", "sweep-dup-outcome");
    await insertRawRecord("sweep.outcome", "sweep-dup-outcome");
    await insertRawRecord("sweep.outcome", "sweep-dup-outcome");
    const outcomeIdsBefore = await idsFor("sweep.outcome", "sweep-dup-outcome");
    const keptOutcomeId = outcomeIdsBefore[outcomeIdsBefore.length - 1];

    // A duplicate sweep_id for an UNCONSTRAINED kind — must survive the
    // dedup DELETE untouched, since sweep.phase legitimately emits many rows
    // per sweep (one per lifecycle phase).
    await insertRawRecord("sweep.phase", "sweep-dup-completed");
    await insertRawRecord("sweep.phase", "sweep-dup-completed");

    // A NULL sweep_id for a constrained kind — must also survive untouched;
    // NULL is exempt from the index's own uniqueness (SQL's NULL != NULL),
    // so the dedup DELETE must not collapse multiple NULL-sweep_id rows of
    // the same kind down to one either.
    await insertRawRecord("sweep.completed", null);
    await insertRawRecord("sweep.completed", null);

    const migration = env.TEST_MIGRATIONS.find((m) => m.name.endsWith(MIGRATION_NAME));
    if (!migration) {
      throw new Error(`migration ${MIGRATION_NAME} not found in env.TEST_MIGRATIONS`);
    }

    // This must not throw SQLITE_CONSTRAINT_UNIQUE — the exact failure this
    // issue is about.
    for (const query of migration.queries) {
      await env.DB.prepare(query).run();
    }

    expect(await idsFor("sweep.completed", "sweep-dup-completed")).toEqual([keptCompletedId]);
    expect(await idsFor("sweep.outcome", "sweep-dup-outcome")).toEqual([keptOutcomeId]);

    // Untouched: legitimately-multi-row kind, and NULL-sweep_id rows.
    expect(await idsFor("sweep.phase", "sweep-dup-completed")).toHaveLength(2);
    const { results: nullRows } = await env.DB.prepare(
      "SELECT id FROM records WHERE kind = 'sweep.completed' AND sweep_id IS NULL",
    ).all();
    expect(nullRows).toHaveLength(2);

    // The unique index itself must have been (re)created successfully.
    const index = await env.DB.prepare(
      "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_records_terminal_sweep_once'",
    ).first();
    expect(index).not.toBeNull();
  });

  it("does not touch any other kind's rows (sweep.started, tokens.snapshot, host.health)", async () => {
    await env.DB.exec("DROP INDEX idx_records_terminal_sweep_once");

    // Seed duplicate-sweep_id rows across a handful of unconstrained kinds —
    // some legitimately share a sweep_id, some carry none at all — none of
    // which the migration's dedup DELETE should touch.
    await insertRawRecord("sweep.started", "sweep-untouched");
    await insertRawRecord("tokens.snapshot", null);
    await insertRawRecord("tokens.snapshot", null);
    await insertRawRecord("host.health", null);
    await insertRawRecord("host.health", null);

    const migration = env.TEST_MIGRATIONS.find((m) => m.name.endsWith(MIGRATION_NAME));
    if (!migration) {
      throw new Error(`migration ${MIGRATION_NAME} not found in env.TEST_MIGRATIONS`);
    }
    for (const query of migration.queries) {
      await env.DB.prepare(query).run();
    }

    expect(await idsFor("sweep.started", "sweep-untouched")).toHaveLength(1);
    const { results: tokenRows } = await env.DB.prepare(
      "SELECT id FROM records WHERE kind = 'tokens.snapshot'",
    ).all();
    expect(tokenRows).toHaveLength(2);
    const { results: healthRows } = await env.DB.prepare("SELECT id FROM records WHERE kind = 'host.health'").all();
    expect(healthRows).toHaveLength(2);
  });
});
