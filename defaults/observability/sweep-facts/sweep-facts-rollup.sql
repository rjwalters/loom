-- Sweep facts: the durable per-sweep rollup that outlives D1's retention
-- (Issue #9446). The questions it answers live in `sweep-facts-queries.sql`
-- (SF1..SF8); the definitions — including "absent vs. zero" — are fixed in
-- `sweep-facts-questions.md`. Read that before reading a number out of any of
-- these tables.
--
-- DIALECT: Cloudflare D1 (SQLite, JSON1 enabled). This mirrors the cycle-time
-- rollup (#8665) one level down, but the stores differ by design: the
-- cycle-time rollup lives on each observability backend's ClickHouse and needs
-- a per-backend extraction view, while D1 *is* the raw store — `records` and
-- `sweep_facts` share one database, so this file reads `records` directly and
-- no extraction seam is needed on this side. The ClickHouse-side extractions
-- (`sweep-facts-extract-{clickstack,signoz}.sql`) normalize the same payload
-- into the same fact shape for the windows those backends still hold.
--
-- Run with wrangler (edit the window in the `params` CTE — it is the only
-- place a date literal may appear in this file):
--
--   wrangler d1 execute loom-fleet-telemetry --file sweep-facts-rollup.sql
--
-- Idempotence (#9446 acceptance): re-running over an already-ingested window
-- must not duplicate rows. The cycle-time rollup answers this with a
-- ReplacingMergeTree; the D1 analogue is `INSERT OR REPLACE` on the primary
-- key — a re-run overwrites each `(repo, issue, sweep_id)` row in place, and a
-- corrected extraction wins on re-run. (`records` itself deduplicates
-- terminal sweeps upstream, migrations/0002, so the source is already
-- one-row-per-sweep.)

-- ===========================================================================
-- 1. Durable storage
-- ===========================================================================

-- One row per terminal sweep. Every payload field is optional on the wire
-- (telemetry-schema.md omits empty/unset keys and forbids coercing a missing
-- one to 0/''/"unknown"), so every column the payload feeds is NULLable and
-- stays NULL when the key is absent. The `disposition` (#9441),
-- `tokens_status` (#9440), `hw_*`/`generated_lines`/`test_lines` (#9466),
-- `story_points` (#9432/#9433) and lineage (#9444) columns are NULL in bulk
-- until those emitters land and the backfill re-runs — that is the rollup
-- working, not failing.
CREATE TABLE IF NOT EXISTS sweep_facts (
    repo                   TEXT    NOT NULL,
    issue                  INTEGER NOT NULL,
    sweep_id               TEXT    NOT NULL,
    host_id                TEXT,
    emitted_at             TEXT,
    result                 TEXT,
    disposition            TEXT,
    failure_class          TEXT,
    tokens_status          TEXT,
    config_arm             TEXT,
    models_used            TEXT,
    total_duration_sec     INTEGER,
    phase_durations        TEXT,
    tokens_in              INTEGER,
    tokens_out             INTEGER,
    tokens_by_model        TEXT,
    tokens_unattributed_in INTEGER,
    tokens_unattributed_out INTEGER,
    lines_added            INTEGER,
    lines_deleted          INTEGER,
    hw_lines_added         INTEGER,
    hw_lines_deleted       INTEGER,
    hw_files               INTEGER,
    generated_lines        INTEGER,
    test_lines             INTEGER,
    story_points           INTEGER,
    doctor_cycles          INTEGER,
    judge_verdicts         TEXT,
    attempt_index          INTEGER,
    previous_sweep_id      TEXT,
    trigger                TEXT,
    rework_substantive     INTEGER,
    rework_environmental   INTEGER,
    pr_number              INTEGER,
    pr_numbers             TEXT,
    suspect                INTEGER,
    schema_version         INTEGER,
    -- The sweep identity. `repo` is always a forge slug or absent (#9442) and
    -- rows with an absent identity component are excluded from the grain
    -- below, so the composite key is well-defined. WITHOUT ROWID keeps the
    -- table clustered on the key the question set always groups by.
    PRIMARY KEY (repo, issue, sweep_id)
) WITHOUT ROWID;

-- ===========================================================================
-- 2. Ingest (the one write path)
-- ===========================================================================

-- Envelope-level fields (emitted_at, host_id, schema_version, repo, issue,
-- sweep_id) are columns of `records` (migrations/0001_init.sql) and are read
-- from there; everything else is read out of the verbatim JSON payload with
-- json_extract — the same read surface `loom-ui:src/query.ts` and the
-- issue-level analysis queries already use.
--
-- The WHERE clause is the grain: identity columns must be present. An absent
-- `repo` is the #9442 unattributed bucket — it is excluded here so the key
-- stays well-defined, and SF7 counts the excluded population explicitly, so
-- the exclusion can never silently eat drops.
--
-- `rework_substantive` / `rework_environmental` (#9444): counts of
-- `rework_events` entries per classification. Absent key → NULL (the timeline
-- was not read); `[]` → 0 (read, and no rework happened) — walked with
-- json_each so the count is per-classification, never a total folded over it.
--
-- `suspect` (#9454): a derived 0/1 flag, not a measurement — 1 exactly when
-- the emitter's plausibility guard downgraded this record's tokens to
-- `tokens_status = 'suspect'`.
--
-- `story_points` (#9432, consumed by SF8/#9433): the Curator's *a priori* size
-- estimate — the numeric value of the issue's single `points:*` label, one of
-- 1/2/3/5/8/13. It is the only FORECAST column on this table; everything else
-- is a measurement. It stays NULL — never 0 — in all four situations
-- telemetry-schema.md §`story_points` enumerates (no label, an
-- out-of-vocabulary label, more than one label, a failed/skipped label read),
-- so "unsized" and "sized at nothing" can never be confused. SF8 counts the
-- NULLs as an explicit data gap for exactly that reason. The values are
-- ORDINAL, not a unit (a "13" is not thirteen "1"s): never `sum()` this column
-- as a size — see SF8 and `landed-size.sql`'s `measured_point_values`.
--
-- The window lives in `params`, the only date literals in this file.
WITH params AS (
    SELECT '2026-08-15T00:00:00Z' AS since,
           '2026-10-01T00:00:00Z' AS until
)
INSERT OR REPLACE INTO sweep_facts
    (repo, issue, sweep_id, host_id, emitted_at, result,
     disposition, failure_class, tokens_status, config_arm,
     models_used, total_duration_sec, phase_durations,
     tokens_in, tokens_out, tokens_by_model, tokens_unattributed_in, tokens_unattributed_out,
     lines_added, lines_deleted,
     hw_lines_added, hw_lines_deleted, hw_files, generated_lines, test_lines,
     story_points,
     doctor_cycles, judge_verdicts,
     attempt_index, previous_sweep_id, trigger,
     rework_substantive, rework_environmental,
     pr_number, pr_numbers, suspect, schema_version)
SELECT
    r.repo                                            AS repo,
    r.issue                                           AS issue,
    r.sweep_id                                        AS sweep_id,
    r.host_id                                         AS host_id,
    r.emitted_at                                      AS emitted_at,
    json_extract(r.payload, '$.result')               AS result,
    json_extract(r.payload, '$.disposition')          AS disposition,
    json_extract(r.payload, '$.failure_class')        AS failure_class,
    json_extract(r.payload, '$.tokens_status')        AS tokens_status,
    json_extract(r.payload, '$.config.arm')           AS config_arm,
    json_extract(r.payload, '$.models_used')          AS models_used,
    json_extract(r.payload, '$.total_duration_sec')   AS total_duration_sec,
    json_extract(r.payload, '$.phase_durations')      AS phase_durations,
    json_extract(r.payload, '$.tokens_in')            AS tokens_in,
    json_extract(r.payload, '$.tokens_out')           AS tokens_out,
    json_extract(r.payload, '$.tokens_by_model')      AS tokens_by_model,
    json_extract(r.payload, '$.tokens_unattributed.tokens_in')  AS tokens_unattributed_in,
    json_extract(r.payload, '$.tokens_unattributed.tokens_out') AS tokens_unattributed_out,
    json_extract(r.payload, '$.lines_added')          AS lines_added,
    json_extract(r.payload, '$.lines_deleted')        AS lines_deleted,
    json_extract(r.payload, '$.hw_lines_added')       AS hw_lines_added,
    json_extract(r.payload, '$.hw_lines_deleted')     AS hw_lines_deleted,
    json_extract(r.payload, '$.hw_files')             AS hw_files,
    json_extract(r.payload, '$.generated_lines')      AS generated_lines,
    json_extract(r.payload, '$.test_lines')           AS test_lines,
    json_extract(r.payload, '$.story_points')         AS story_points,
    json_extract(r.payload, '$.doctor_cycles')        AS doctor_cycles,
    json_extract(r.payload, '$.judge_verdicts')       AS judge_verdicts,
    json_extract(r.payload, '$.attempt_index')        AS attempt_index,
    json_extract(r.payload, '$.previous_sweep_id')    AS previous_sweep_id,
    json_extract(r.payload, '$.trigger')              AS trigger,
    CASE
        WHEN json_extract(r.payload, '$.rework_events') IS NULL THEN NULL
        ELSE (SELECT count(*) FROM json_each(r.payload, '$.rework_events') je
              WHERE json_extract(je.value, '$.classification') = 'substantive')
    END                                               AS rework_substantive,
    CASE
        WHEN json_extract(r.payload, '$.rework_events') IS NULL THEN NULL
        ELSE (SELECT count(*) FROM json_each(r.payload, '$.rework_events') je
              WHERE json_extract(je.value, '$.classification') = 'environmental')
    END                                               AS rework_environmental,
    json_extract(r.payload, '$.pr_number')            AS pr_number,
    json_extract(r.payload, '$.pr_numbers')           AS pr_numbers,
    CASE WHEN json_extract(r.payload, '$.tokens_status') = 'suspect'
         THEN 1 ELSE 0 END                            AS suspect,
    r.schema_version                                  AS schema_version
FROM records r, params p
WHERE r.kind = 'sweep.outcome'
  AND r.sweep_id IS NOT NULL
  AND r.repo     IS NOT NULL
  AND r.issue    IS NOT NULL
  AND r.emitted_at >= p.since
  AND r.emitted_at <  p.until;
