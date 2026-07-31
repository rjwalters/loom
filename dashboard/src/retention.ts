/**
 * Retention / eviction policy for the `records` table (Epic #4702, Phase 2
 * AC: "Retention policy bounds D1 growth (age- or size-based
 * eviction/rollup), documented and testable").
 *
 * Two independent bounds are enforced on every run, both configured via
 * `wrangler.toml` `[vars]` (no code change needed to retune):
 *
 *   - **Age-based**: rows older than `RETENTION_DAYS` (by `ingested_at`,
 *     the backend's own receive time — not the client-supplied
 *     `emitted_at`, which a misbehaving/clock-skewed host could spoof) are
 *     deleted. Mirrors the local JSONL rotation's age bound
 *     (`sweep_outcomes.rs`'s 30-day window), sized up for a multi-host,
 *     indefinitely-running backend (default 90 days).
 *   - **Size-based**: if the table still exceeds `MAX_RECORDS` rows after
 *     the age sweep, the oldest excess rows (by `id`, i.e. insertion order)
 *     are deleted until the table is back at the cap. This bounds worst-case
 *     storage even if a single very chatty host (or a burst of new hosts)
 *     would blow the age-based bound's effective size before enough time
 *     passes for it to kick in.
 *
 * Rollup (aggregating evicted rows into a coarser summary) is deliberately
 * NOT implemented in this issue — eviction alone satisfies the AC
 * ("age- or size-based eviction/**rollup**", not both), and a rollup
 * schema is a design decision better made once Phase 3's query API defines
 * what aggregate shape it actually needs.
 */

export interface RetentionConfig {
  retentionDays: number;
  maxRecords: number;
}

export interface RetentionResult {
  deletedByAge: number;
  deletedBySize: number;
}

/** Parse the `[vars]` string bindings into a validated numeric config,
 * falling back to safe defaults if a var is absent or unparseable (a
 * misconfigured cron var must never silently disable retention). */
export function parseRetentionConfig(env: {
  RETENTION_DAYS?: string;
  MAX_RECORDS?: string;
}): RetentionConfig {
  const retentionDays = parsePositiveInt(env.RETENTION_DAYS, 90);
  const maxRecords = parsePositiveInt(env.MAX_RECORDS, 500_000);
  return { retentionDays, maxRecords };
}

function parsePositiveInt(value: string | undefined, fallback: number): number {
  if (value === undefined) return fallback;
  const parsed = Number.parseInt(value, 10);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : fallback;
}

/**
 * Run one retention sweep against `db`. Age-based eviction runs first
 * (cheap: an indexed range delete on `ingested_at`); size-based eviction
 * only runs if the table is still over `maxRecords` afterward.
 */
export async function runRetentionSweep(
  db: D1Database,
  config: RetentionConfig,
  now: Date = new Date(),
): Promise<RetentionResult> {
  const cutoff = new Date(now.getTime() - config.retentionDays * 24 * 60 * 60 * 1000).toISOString();

  const ageResult = await db.prepare("DELETE FROM records WHERE ingested_at < ?").bind(cutoff).run();
  const deletedByAge = ageResult.meta.changes ?? 0;

  const countRow = await db.prepare("SELECT COUNT(*) AS count FROM records").first<{ count: number }>();
  const remaining = countRow?.count ?? 0;

  let deletedBySize = 0;
  if (remaining > config.maxRecords) {
    const excess = remaining - config.maxRecords;
    const sizeResult = await db
      .prepare("DELETE FROM records WHERE id IN (SELECT id FROM records ORDER BY id ASC LIMIT ?)")
      .bind(excess)
      .run();
    deletedBySize = sizeResult.meta.changes ?? 0;
  }

  return { deletedByAge, deletedBySize };
}
