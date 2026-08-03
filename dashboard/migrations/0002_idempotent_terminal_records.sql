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
-- Production already has duplicate (kind, sweep_id) rows for these two kinds
-- predating this constraint — the `INSERT OR IGNORE` guard above only
-- prevents *future* duplicates, it does nothing for rows already in the
-- table. `CREATE UNIQUE INDEX` fails outright (SQLITE_CONSTRAINT_UNIQUE) on
-- data that already violates the target uniqueness, so dedup first: keep
-- exactly one row per (kind, sweep_id) pair, the most recently ingested one
-- (MAX(id)). This DELETE is a no-op on a database with no pre-existing
-- duplicates (e.g. the test suite's fresh in-memory D1), so it is safe to run
-- unconditionally ahead of the index creation (Issue #5107).
DELETE FROM records
WHERE kind IN ('sweep.completed', 'sweep.outcome')
  AND id NOT IN (
    SELECT MAX(id) FROM records
    WHERE kind IN ('sweep.completed', 'sweep.outcome')
    GROUP BY kind, sweep_id
  );

CREATE UNIQUE INDEX idx_records_terminal_sweep_once
  ON records (kind, sweep_id)
  WHERE kind IN ('sweep.completed', 'sweep.outcome');
