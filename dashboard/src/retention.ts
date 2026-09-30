/**
 * Retention / eviction policy for the `records` table (Epic #4702, Phase 2
 * AC: "Retention policy bounds D1 growth (age- or size-based
 * eviction/rollup), documented and testable"; tiered size eviction per
 * 2AMLogic/2am#1608).
 *
 * Two independent bounds are enforced on every run, both configured via
 * `wrangler.toml` `[vars]` (no code change needed to retune):
 *
 *   - **Age-based**: rows older than `RETENTION_DAYS` (by `ingested_at`,
 *     the backend's own receive time — not the client-supplied
 *     `emitted_at`, which a misbehaving/clock-skewed host could spoof) are
 *     deleted. Mirrors the local JSONL rotation's age bound
 *     (`sweep_outcomes.rs`'s 30-day window), sized up for a multi-host,
 *     indefinitely-running backend (default 90 days). This bound is
 *     deliberately kind-blind: it IS the documented 90-day policy, and a
 *     sweep record that has aged out is history, not live state.
 *
 *   - **Size-based**: if the table still exceeds `MAX_RECORDS` rows after
 *     the age sweep, the oldest excess rows (by `id`, i.e. insertion order)
 *     are deleted until the table is back at the cap. This bounds worst-case
 *     storage even if a single very chatty host (or a burst of new hosts)
 *     would blow the age-based bound's effective size before enough time
 *     passes for it to kick in.
 *
 *     Since #1608 the size bound evicts in **two tiers**, because the
 *     cap's original kind-blind oldest-first order was silently destroying
 *     the only durable sweep-telemetry history: in production the table is
 *     majority health/queue snapshot rows (`host.health`, `tokens.snapshot`,
 *     …), so a blind "delete oldest" evicts `sweep.*` outcomes — the rows
 *     the dashboard's historical views exist for — exactly as fast as it
 *     evicts disposable snapshots. The tiers:
 *
 *       1. **Unprotected first** — the oldest rows whose `kind` is NOT a
 *          protected kind (see `PROTECTED_KIND_SQL`), until the cap is met.
 *          These are the high-volume, low-historical-value snapshots.
 *       2. **Oldest regardless of kind** — only if tier 1 alone was not
 *          enough (i.e. every remaining row is protected and the table is
 *          still over cap). The cap is a hard storage bound, so it must
 *          stay enforceable even against protected rows; this tier is the
 *          proof it still is.
 *
 *     Because tier 1's delete limit is the whole excess, tier 2 can only
 *     ever fire when tier 1 already deleted *every* unprotected row — so a
 *     protected row is never evicted while an older unprotected row
 *     survives, and `planSizeEviction` (pure, unit-tested) pins that
 *     ordering down without a database.
 *
 *     Eviction visibility (#1608): size eviction used to be completely
 *     silent — rows vanished and nothing recorded that it happened, which
 *     is how the history loss went unnoticed. `recordRetentionOutcome`
 *     (called from `src/index.ts`'s `scheduled` handler) now lands a
 *     `backend.retention` row in `records` itself whenever a sweep deleted
 *     anything, so the eviction event is queryable through the same
 *     history API whose contents it describes.
 *
 * The **default** `MAX_RECORDS` is 1,000,000 (raised from 500,000 by
 * #1608): the deployed store sat at the old cap with ~506k rows (~1.46 GB),
 * so size eviction — not the 90-day age bound — was the bound actually
 * firing and evicting history. 1M rows ≈ 3 GB, comfortably inside D1's
 * limits, and buys headroom so the cap is a true emergency brake rather
 * than the routine eviction mechanism.
 *
 * Rollup (aggregating evicted rows into a coarser summary) is deliberately
 * NOT implemented in this issue — eviction alone satisfies the AC
 * ("age- or size-based eviction/**rollup**", not both), and a rollup
 * schema is a design decision better made once Phase 3's query API defines
 * what aggregate shape it actually needs.
 */

import { CURRENT_SCHEMA_VERSION } from "./telemetry";

export interface RetentionConfig {
  retentionDays: number;
  maxRecords: number;
}

export interface RetentionResult {
  deletedByAge: number;
  /** Total rows deleted by the size cap (both tiers combined) — kept as the
   * single headline number for the `/admin/retention/run` response and the
   * cron log line. */
  deletedBySize: number;
  /** Size-eviction tier 2: protected rows (`sweep.*`/`session.*`) deleted
   * because tier 1 alone could not get under the cap. Non-zero means the
   * cap is genuinely pressing on the history this backend exists to keep —
   * raise `MAX_RECORDS` or shorten `RETENTION_DAYS` when this grows. */
  deletedBySizeProtected: number;
  /** Size-eviction tier 1: unprotected (snapshot-class) rows deleted. The
   * expected steady-state path — this counter growing alone is healthy. */
  deletedBySizeUnprotected: number;
}

/**
 * The protected-kind predicate, shared verbatim by the tier-1 COUNT and
 * DELETE so the two can never drift. Protected = the durable sweep history
 * (`sweep.%` covers started/phase/completed/outcome) plus the per-session
 * summary/analysis records. Kind is `NOT NULL` (migrations/0001_init.sql),
 * so `LIKE`/`=` here never see NULL.
 */
const PROTECTED_KIND_SQL =
  "(kind LIKE 'sweep.%' OR kind = 'session.summary' OR kind = 'session.analysis')";

/** `kind` of the backend's own liveness/telemetry rows (see
 * `recordRetentionOutcome`). `backend.` is deliberately not a prefix any
 * daemon emits, so these rows can never be confused with fleet data — and
 * they are NOT in `PROTECTED_KIND_SQL`, so a retention row from a previous
 * hour is itself evictable like any other snapshot-class row: the marker
 * must not become un-evictable ballast. */
export const RETENTION_LIVENESS_KIND = "backend.retention";

/** The planned size-eviction breakdown for one sweep — what tier 1
 * (unprotected, oldest first) and tier 2 (oldest regardless of kind) would
 * delete. Pure so the tier-ordering contract is testable without D1. */
export interface SizeEvictionPlan {
  /** Tier 1: oldest unprotected rows to delete. */
  unprotected: number;
  /** Tier 2: further rows (all unprotected rows are gone by the time this
   * is non-zero — see the module doc) deleted oldest-first regardless of
   * kind. */
  protectedOverflow: number;
}

/**
 * Plan one size eviction purely from counts (issue #1608). Given `remaining`
 * rows in the table, the `maxRecords` cap, and how many of the remaining
 * rows are unprotected, return how many each tier deletes:
 *
 *   - under cap (or exactly at it): nothing;
 *   - over cap: tier 1 takes the whole excess if the unprotected rows can
 *     cover it, otherwise every unprotected row; tier 2 takes the residue.
 *
 * The arithmetic IS the policy ("protected rows survive tier 1; tier 2 only
 * fires once nothing unprotected remains"), kept here rather than inline in
 * `runRetentionSweep` so tests can pin it without seeding a database.
 */
export function planSizeEviction(remaining: number, maxRecords: number, unprotectedCount: number): SizeEvictionPlan {
  if (remaining <= maxRecords) return { unprotected: 0, protectedOverflow: 0 };
  const excess = remaining - maxRecords;
  const unprotected = Math.min(excess, unprotectedCount);
  return { unprotected, protectedOverflow: excess - unprotected };
}

/** Parse the `[vars]` string bindings into a validated numeric config,
 * falling back to safe defaults if a var is absent or unparseable (a
 * misconfigured cron var must never silently disable retention). */
export function parseRetentionConfig(env: {
  RETENTION_DAYS?: string;
  MAX_RECORDS?: string;
}): RetentionConfig {
  const retentionDays = parsePositiveInt(env.RETENTION_DAYS, 90);
  // 1,000,000 since #1608 (was 500,000): see the module doc — the size cap
  // was the bound actually firing in production, and at ~2.9 KB/row the old
  // default left barely one churn-cycle of headroom while evicting the
  // sweep history tier 1 now shields.
  const maxRecords = parsePositiveInt(env.MAX_RECORDS, 1_000_000);
  return { retentionDays, maxRecords };
}

function parsePositiveInt(value: string | undefined, fallback: number): number {
  if (value === undefined) return fallback;
  const parsed = Number.parseInt(value, 10);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : fallback;
}

/**
 * Run one retention sweep against `db`. Age-based eviction runs first
 * (cheap: an indexed range delete on `ingested_at`, and deliberately
 * kind-blind — it is the documented 90-day policy, not the #1608 tiering
 * surface); size-based eviction only runs if the table is still over
 * `maxRecords` afterward, and then goes unprotected-first (tier 1) before
 * touching protected rows (tier 2) — see the module doc.
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

  let deletedBySizeProtected = 0;
  let deletedBySizeUnprotected = 0;
  if (remaining > config.maxRecords) {
    const unprotectedCountRow = await db
      .prepare(`SELECT COUNT(*) AS count FROM records WHERE NOT ${PROTECTED_KIND_SQL}`)
      .first<{ count: number }>();
    const plan = planSizeEviction(remaining, config.maxRecords, unprotectedCountRow?.count ?? 0);

    // Tier 1: the oldest unprotected rows, up to the whole excess.
    if (plan.unprotected > 0) {
      const tier1 = await db
        .prepare(
          `DELETE FROM records WHERE id IN (
             SELECT id FROM records WHERE NOT ${PROTECTED_KIND_SQL} ORDER BY id ASC LIMIT ?
           )`,
        )
        .bind(plan.unprotected)
        .run();
      deletedBySizeUnprotected = tier1.meta.changes ?? 0;
    }

    // Tier 2: only when tier 1 was not enough — oldest rows regardless of
    // kind (which, given tier 1's full-excess limit, are all protected).
    if (plan.protectedOverflow > 0) {
      const tier2 = await db
        .prepare("DELETE FROM records WHERE id IN (SELECT id FROM records ORDER BY id ASC LIMIT ?)")
        .bind(plan.protectedOverflow)
        .run();
      deletedBySizeProtected = tier2.meta.changes ?? 0;
    }
  }

  const deletedBySize = deletedBySizeUnprotected + deletedBySizeProtected;
  return { deletedByAge, deletedBySize, deletedBySizeProtected, deletedBySizeUnprotected };
}

/**
 * Eviction visibility (issue #1608): persist a small `backend.retention`
 * liveness row describing what a sweep deleted, so size evictions (which
 * destroyed sweep history silently before this) leave a queryable trace
 * through the very history API they shrink.
 *
 * - Inserts **only when the sweep deleted something** — a quiet sweep must
 *   not write one row per cron tick.
 * - `sweep_id` is synthetic and unique per wall-clock hour
 *   (`retention-<unix-hour>`): repeated sweeps in the same hour (e.g. a
 *   manual `/admin/retention/run` after the cron tick) reuse the id, and
 *   the check-then-insert below makes the same-hour replay idempotent
 *   instead of stacking duplicate rows. There is deliberately no new
 *   partial UNIQUE index for this kind — migrations/0002's index is scoped
 *   to the two terminal sweep kinds, and a code-level guard is enough for
 *   a single hourly writer.
 * - Column bindings mirror `src/index.ts`'s ingest INSERT exactly (NOT NULL
 *   columns of migrations/0001_init.sql all bound): `host_id` is the
 *   constant `"backend"` (this row's producer is the backend, not a fleet
 *   host), `repo`/`issue` are NULL (not repo-scoped), `visibility` is
 *   `"private"` (fail-safe default; fleet-internal ops metadata), and
 *   `schema_version` is `CURRENT_SCHEMA_VERSION` — the version this
 *   backend fully understands, same as a live envelope would carry.
 * - Not itself a protected kind (see `RETENTION_LIVENESS_KIND`), so old
 *   markers age out under the ordinary bounds instead of accumulating.
 *
 * Returns whether a row was written (false = nothing deleted, or this
 * hour's marker already exists) — the cron caller only needs "did it land";
 * tests assert the decision itself.
 */
export async function recordRetentionOutcome(
  db: D1Database,
  config: RetentionConfig,
  result: RetentionResult,
  now: Date = new Date(),
): Promise<boolean> {
  const deletedTotal = result.deletedByAge + result.deletedBySize;
  if (deletedTotal === 0) return false;

  const nowIso = now.toISOString();
  const sweepId = `retention-${Math.floor(now.getTime() / 3_600_000)}`;

  // Idempotent replay guard: a second outcome for the same hour is the same
  // fact — keep one row. (`INSERT OR IGNORE` alone would not dedupe: no
  // unique index covers this kind.)
  const existing = await db
    .prepare("SELECT id FROM records WHERE kind = ? AND sweep_id = ?")
    .bind(RETENTION_LIVENESS_KIND, sweepId)
    .first();
  if (existing !== null) return false;

  await db
    .prepare(
      `INSERT OR IGNORE INTO records
         (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, payload, ingested_at)
       VALUES (?, ?, ?, ?, NULL, 'private', NULL, ?, ?, ?)`,
    )
    .bind(
      CURRENT_SCHEMA_VERSION,
      nowIso,
      "backend",
      RETENTION_LIVENESS_KIND,
      sweepId,
      JSON.stringify({
        deletedByAge: result.deletedByAge,
        deletedBySizeProtected: result.deletedBySizeProtected,
        deletedBySizeUnprotected: result.deletedBySizeUnprotected,
        maxRecords: config.maxRecords,
      }),
      nowIso,
    )
    .run();
  return true;
}
