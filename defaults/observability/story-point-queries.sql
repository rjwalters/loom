-- Story-point calibration: the canonical question set SP1..SP5 (Issue #9430).
--
-- BACKEND-NEUTRAL: these run unchanged on ClickStack's ClickHouse and on
-- SigNoz's ClickHouse, because they read only `loom_analytics.raw_landing_cost`,
-- which each backend's own `story-point-extract.sql` defines. Run that extract
-- file first. The questions, their definitions, and what this set deliberately
-- cannot answer are in `story-point-questions.md`. The rubric these numbers
-- calibrate is `defaults/docs/story-points.md`; SP4/SP5 take the bucket cut
-- points as parameters precisely so the #9434 calibration loop can re-run them
-- without editing SQL.
--
-- Windows are BOUND, never edited in: rewriting a literal into the SQL is the
-- hand-written-SQL habit this file exists to abolish.
--
--   clickhouse-client \
--     --param_since='2026-09-14 00:00:00' --param_until='2026-09-30 00:00:00' \
--     --param_top_n=10 \
--     --param_cut_1_2=16000000 --param_cut_2_3=24000000 \
--     --param_cut_3_5=39000000 --param_cut_5_8=60000000 \
--     --param_cut_8_13=90000000 \
--     --queries-file story-point-queries.sql
--
-- All five answers come back in one pass, in SP order. The cut parameters are
-- total tokens (tokens_in + tokens_out) and must be strictly increasing.

-- SP1. The window's clean landings, each with its three cost measures. A NULL
-- measure is an unmeasured landing (SP3 counts those), never a zero-cost one.
SELECT
    repo,
    issue,
    pr_number,
    sweep_id,
    finished_at,
    model,
    tokens_in + tokens_out     AS tokens,
    lines_added + lines_deleted AS lines_changed,
    total_duration_sec          AS wall_sec
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND clean_landing = 1
ORDER BY tokens DESC NULLS LAST, repo, sweep_id
LIMIT {top_n:UInt32};

-- SP2. The cost distribution of clean landings: p10/p50/p90/max for each of
-- the three measures. Tokens are the primary anchor (queue-independent);
-- lines and wall are cross-checks. The measured subset keeps an absent
-- tokens_status (pre-#9440 records carrying the pair) but excludes `suspect`
-- (#9454); `not_spawned`/`unattributable` never carry the pair, so the
-- presence filter already drops them.
SELECT
    'tokens' AS measure,
    count() AS n_measured,
    round(quantile(0.1)(toFloat64(tokens_in + tokens_out))) AS p10,
    round(quantile(0.5)(toFloat64(tokens_in + tokens_out))) AS p50,
    round(quantile(0.9)(toFloat64(tokens_in + tokens_out))) AS p90,
    round(toFloat64(max(tokens_in + tokens_out))) AS max
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND clean_landing = 1
  AND tokens_in IS NOT NULL AND tokens_out IS NOT NULL
  AND (tokens_status IS NULL OR tokens_status = 'measured')
UNION ALL
SELECT
    'lines_changed' AS measure,
    count(),
    round(quantile(0.1)(toFloat64(lines_added + lines_deleted))),
    round(quantile(0.5)(toFloat64(lines_added + lines_deleted))),
    round(quantile(0.9)(toFloat64(lines_added + lines_deleted))),
    round(toFloat64(max(lines_added + lines_deleted)))
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND clean_landing = 1
  AND lines_added IS NOT NULL AND lines_deleted IS NOT NULL
UNION ALL
SELECT
    'wall_sec' AS measure,
    count(),
    round(quantile(0.1)(toFloat64(total_duration_sec))),
    round(quantile(0.5)(toFloat64(total_duration_sec))),
    round(quantile(0.9)(toFloat64(total_duration_sec))),
    round(toFloat64(max(total_duration_sec)))
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND clean_landing = 1
  AND total_duration_sec > 0
ORDER BY measure;

-- SP3. What the clean-landing filter excluded, and why. Read BESIDE SP2: the
-- distribution describes 172-of-10579-style survivors, and every blank above
-- is one of these rows. `success_without_pr` is the #9441 no-op re-dispatch
-- shape that a bare `result = 'success'` filter would have admitted.
SELECT
    count() AS ships,
    countIf(result != 'success') AS not_success,
    countIf(result = 'success' AND pr_number IS NULL) AS success_without_pr,
    countIf(phase_breakdown_present = 0) AS without_phase_breakdown,
    countIf(phase_breakdown_present = 1 AND judge_entries != 1) AS judge_entries_not_one,
    countIf(doctor_engaged) AS doctor_engaged,
    countIf(clean_landing = 1) AS clean_landings,
    countIf(clean_landing = 1 AND tokens_in IS NULL) AS clean_tokens_absent,
    countIf(clean_landing = 1 AND tokens_status = 'suspect') AS clean_tokens_suspect
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime};

-- SP4. Bucket occupancy under candidate cut points: how the token-measured
-- clean landings distribute across the Fibonacci scale when the cuts are the
-- given parameters. This is the #9434 calibration hook — bounds are derived
-- from SP2's distribution and re-validated here, never edited into the SQL.
SELECT
    multiIf(
        tokens_in + tokens_out <= {cut_1_2:UInt64}, '1',
        tokens_in + tokens_out <= {cut_2_3:UInt64}, '2',
        tokens_in + tokens_out <= {cut_3_5:UInt64}, '3',
        tokens_in + tokens_out <= {cut_5_8:UInt64}, '5',
        tokens_in + tokens_out <= {cut_8_13:UInt64}, '8',
        '13') AS points,
    count() AS landings,
    round(100 * count() / sum(count()) OVER (), 1) AS pct
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND clean_landing = 1
  AND tokens_in IS NOT NULL AND tokens_out IS NOT NULL
  AND (tokens_status IS NULL OR tokens_status = 'measured')
GROUP BY points
ORDER BY toUInt16OrZero(points);

-- SP5. Do the anchor and the cross-checks agree? Per bucket (same cuts as
-- SP4), the median of each measure beside the others. Where lines or wall
-- separate buckets far less than tokens do, they are weaker discriminators —
-- wall-clock additionally carries the queue-depth caveat — and the rubric's
-- "measures disagree" instructions consume exactly this table.
SELECT
    multiIf(
        tokens_in + tokens_out <= {cut_1_2:UInt64}, '1',
        tokens_in + tokens_out <= {cut_2_3:UInt64}, '2',
        tokens_in + tokens_out <= {cut_3_5:UInt64}, '3',
        tokens_in + tokens_out <= {cut_5_8:UInt64}, '5',
        tokens_in + tokens_out <= {cut_8_13:UInt64}, '8',
        '13') AS points,
    count() AS landings,
    round(quantile(0.5)(toFloat64(tokens_in + tokens_out))) AS tokens_p50,
    round(quantile(0.5)(toFloat64(lines_added + lines_deleted))) AS lines_p50,
    round(quantile(0.5)(toFloat64(total_duration_sec))) AS wall_p50
FROM loom_analytics.raw_landing_cost
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND clean_landing = 1
  AND tokens_in IS NOT NULL AND tokens_out IS NOT NULL
  AND (tokens_status IS NULL OR tokens_status = 'measured')
GROUP BY points
ORDER BY toUInt16OrZero(points);
