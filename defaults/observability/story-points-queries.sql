-- Story-points distribution queries SP1..SP4 (Issue #9430, epic #9429).
--
-- ONE definition, D1/SQLite dialect (JSON1 + window functions), because
-- Cloudflare D1 `loom-fleet-telemetry` is the only store that holds the
-- history this phase needs (live SigNoz retains ~7 days; see
-- story-points-evidence.md). It reads `sweep_facts` only; run
-- `sweep-facts/sweep-facts-rollup.sql` first. Per-backend extraction is
-- already provided by `sweep-facts/sweep-facts-extract-{signoz,clickstack}.sql`,
-- which normalize the same fact shape for the windows those backends hold;
-- to run this file over such a window, load those rows into `sweep_facts`
-- and run it unchanged (parity discipline: no backend-specific copy here).
--
--   wrangler d1 execute loom-fleet-telemetry --file story-points-queries.sql
--
-- The window lives in `sp_window` (the ONE place a date literal may appear).
-- NULL is absent, never 0 (telemetry-schema.md).
--
-- Clean-landing filter (RELAXED; the strict one is not yet applicable):
--   landed      : disposition = 'landed', or pre-#9441 fallback
--                 (disposition IS NULL AND result='success' AND pr_number IS NOT NULL)
--   one judge   : exactly one 'judge' entry in phase_durations
--   no doctor   : no 'doctor' entry in phase_durations
-- Strict filter would be doctor_cycles = 0 AND exactly one judge; doctor_cycles
-- exists only since 2026-09-18 and per-phase data is #9443, so SP1 reports both.
-- Landing predicate mirrors sweep-facts/issue-effort.sql; change both together.

CREATE VIEW IF NOT EXISTS sp_window AS
SELECT '2026-08-15T00:00:00Z' AS since,
       '2026-09-29T00:00:00Z' AS until;

CREATE VIEW IF NOT EXISTS sp_landings AS
SELECT f.*,
       (SELECT count(*) FROM json_each(f.phase_durations) je
         WHERE json_extract(je.value, '$.phase') = 'judge')  AS n_judge,
       (SELECT count(*) FROM json_each(f.phase_durations) je
         WHERE json_extract(je.value, '$.phase') = 'doctor') AS n_doctor,
       f.hw_lines_added + f.hw_lines_deleted                 AS hw_lines,
       f.tokens_in + f.tokens_out                            AS tokens,
       json_extract(f.models_used, '$[0]')                   AS model
FROM sweep_facts f, sp_window w
WHERE (f.disposition = 'landed'
       OR (f.disposition IS NULL AND f.result = 'success' AND f.pr_number IS NOT NULL))
  AND f.emitted_at >= w.since AND f.emitted_at < w.until;

-- SP1. Filter accounting: how many landings each clause keeps or excludes.
SELECT count(*)                                                        AS landings,
       sum(phase_durations IS NULL)                                    AS no_phase_data,
       sum(n_judge = 1 AND n_doctor = 0)                               AS relaxed_clean,
       sum(n_judge <> 1)                                               AS excl_not_one_judge,
       sum(n_judge = 1 AND n_doctor > 0)                               AS excl_doctor_phase,
       sum(doctor_cycles = 0 AND n_judge = 1)                          AS strict_clean,
       sum(doctor_cycles IS NULL)                                      AS doctor_cycles_absent,
       sum(tokens IS NOT NULL)                                         AS with_tokens,
       sum(hw_lines IS NOT NULL)                                       AS with_hw_lines
FROM sp_landings;

-- SP2. Per-model token medians over the relaxed-clean set (input to the
-- model normalization factor; factor = model median / reference-model median).
WITH r AS (
  SELECT model, tokens,
         row_number() OVER (PARTITION BY model ORDER BY tokens) AS rn,
         count(*)     OVER (PARTITION BY model)                 AS n
  FROM sp_landings
  WHERE n_judge = 1 AND n_doctor = 0 AND tokens > 0
)
SELECT model, n, avg(tokens) AS median_tokens
FROM r WHERE rn IN ((n + 1) / 2, (n + 2) / 2)
GROUP BY model, n;

-- SP3. Fit constants for landed_size: mean and sd of log1p of each component
-- over the relaxed-clean set (raw tokens; apply SP2 factors when populating
-- sweep-facts/landed-size.sql's params). Feeds `params` there; do not
-- duplicate the fit anywhere else.
SELECT count(*)                                              AS n,
       avg(ln(1.0 + hw_lines))                               AS mean_log_hw_lines,
       sqrt(avg(ln(1.0 + hw_lines) * ln(1.0 + hw_lines))
            - avg(ln(1.0 + hw_lines)) * avg(ln(1.0 + hw_lines))) AS sd_log_hw_lines,
       avg(ln(1.0 + hw_files))                               AS mean_log_hw_files,
       sqrt(avg(ln(1.0 + hw_files) * ln(1.0 + hw_files))
            - avg(ln(1.0 + hw_files)) * avg(ln(1.0 + hw_files))) AS sd_log_hw_files
FROM sp_landings
WHERE n_judge = 1 AND n_doctor = 0 AND hw_lines IS NOT NULL AND hw_files IS NOT NULL;

-- SP4. Per-size-class medians (the rubric's cross-check table). Requires the
-- fitted `issue_landed_size` view (sweep-facts/landed-size.sql, params fitted
-- from SP2/SP3); before the fit its size_class is NULL and this returns only
-- the NULL class -- that is the honest answer, not an error.
WITH c AS (
  SELECT s.size_class, l.tokens, l.hw_lines, l.hw_files
  FROM sp_landings l
  JOIN issue_landed_size s
    ON s.repo = l.repo AND s.issue = l.issue AND s.landing_sweep_id = l.sweep_id
  WHERE l.n_judge = 1 AND l.n_doctor = 0
),
q AS (
  SELECT size_class, tokens, hw_lines, hw_files,
         row_number() OVER (PARTITION BY size_class ORDER BY hw_lines) AS rl,
         row_number() OVER (PARTITION BY size_class ORDER BY tokens)   AS rt,
         row_number() OVER (PARTITION BY size_class ORDER BY hw_files) AS rf,
         count(*)     OVER (PARTITION BY size_class)                   AS n
  FROM c
)
SELECT size_class, n,
       avg(CASE WHEN rl IN ((n + 1) / 2, (n + 2) / 2) THEN hw_lines END) AS median_hw_lines,
       avg(CASE WHEN rt IN ((n + 1) / 2, (n + 2) / 2) THEN tokens END) AS median_tokens,
       avg(CASE WHEN rf IN ((n + 1) / 2, (n + 2) / 2) THEN hw_files END) AS median_hw_files
FROM q GROUP BY size_class, n ORDER BY size_class;
