-- Idempotent terminal-record ingest (Issue #5084).
--
-- `sweep.completed` / `sweep.outcome` are each written exactly once per
-- sweep by the daemon's own reaper — but the daemon-side backfill drain
-- (loom-daemon/src/observability/backfill.rs) is only an *efficiency*
-- optimization against re-offering an already-queued record; it is not the
-- correctness mechanism. A crash between enqueueing a backfilled batch and
-- persisting the advanced cursor re-offers the same record on the next
-- pass, and a transport retry can do the same for a live-observed record.
-- Without a constraint here, either would silently double the row for that
-- sweep in `records` (and double-count it in any aggregate query over the
-- table).
--
-- A partial UNIQUE index — scoped to exactly the two kinds that are emitted
-- once per sweep — is the backend-side guarantee: `INSERT OR IGNORE`
-- (src/index.ts's ingest handler) silently absorbs a duplicate `(kind,
-- sweep_id)` pair instead of inserting a second row, however many times a
-- backfill pass or retry re-sends it. Every other `kind` (`sweep.started`,
-- `sweep.phase`, `tokens.snapshot`, `host.health`, …) is untouched — some
-- are legitimately emitted many times per sweep/host (`sweep.phase` once
-- per lifecycle phase) or carry no `sweep_id` at all (the host-level
-- kinds), and SQLite's partial-index WHERE clause means the constraint
-- never applies to their rows.
--
-- NULL `sweep_id` values are exempt from uniqueness by SQL's own NULL != NULL
-- rule, so this is safe even though `sweep_id` is nullable on the column.
--
-- Issue #5107: production already had duplicate `(kind, sweep_id)` rows for
-- these two kinds predating this constraint — the `INSERT OR IGNORE` guard
-- above is new in this same migration's originating PR (#5106) and only
-- prevents *future* duplicates, so `CREATE UNIQUE INDEX` failed outright
-- against pre-existing data with SQLITE_CONSTRAINT_UNIQUE. Dedup first,
-- keeping the most recently ingested row (`MAX(id)`) per pair, scoped to
-- exactly the two constrained kinds so every other kind's rows (which may
-- legitimately share a `sweep_id`, e.g. `sweep.phase`, or carry none at
-- all) are left untouched.
--
-- `sweep_id IS NOT NULL` on both sides mirrors the index's own NULL
-- exemption (GROUP BY treats all NULLs as one group, unlike the index's
-- per-SQL-NULL-!=-NULL semantics — without this filter a stray NULL
-- `sweep_id` row of either kind would collapse every other NULL-`sweep_id`
-- row of that kind down to one).
DELETE FROM records
WHERE kind IN ('sweep.completed', 'sweep.outcome')
  AND sweep_id IS NOT NULL
  AND id NOT IN (
    SELECT MAX(id) FROM records
    WHERE kind IN ('sweep.completed', 'sweep.outcome')
      AND sweep_id IS NOT NULL
    GROUP BY kind, sweep_id
  );

CREATE UNIQUE INDEX idx_records_terminal_sweep_once
  ON records (kind, sweep_id)
  WHERE kind IN ('sweep.completed', 'sweep.outcome');
