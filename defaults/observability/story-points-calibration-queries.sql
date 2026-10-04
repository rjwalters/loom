-- Story-points calibration queries CAL1..CAL6 (Issue #9434, epic #9429):
-- the estimate-vs-actual loop — "did we get the story points right?"
--
-- D1/SQLite dialect (JSON1 + window functions). Reads the backend-neutral
-- fact table `sweep_facts` (`sweep-facts/sweep-facts-rollup.sql`) and the
-- landed-size views (`sweep-facts/landed-size.sql`) — the same seam
-- `story-points-queries.sql` uses, so per-backend extraction is already
-- provided: `sweep-facts/sweep-facts-extract-{signoz,clickstack}.sql`
-- normalize the same fact shape for the windows those backends hold; to run
-- this file over such a window, load those rows into `sweep_facts` and run
-- it unchanged (parity discipline: no backend-specific copy here, because no
-- backend-native table is read).
--
-- Run order: sweep-facts-rollup.sql, landed-size.sql, then this file.
--
--   wrangler d1 execute loom-fleet-telemetry --file story-points-calibration-queries.sql
--
-- Every definition below — the clean-landing filter, the churn split, the
-- rubric revision rule, the drift tolerance, the misassignment rule — is
-- fixed in `story-points-calibration-questions.md`. Do not read a number out
-- of here without it.
--
-- Windows are BOUND, never edited in: the window lives in `spc_window` below,
-- the ONE place in this file a window date literal may appear. (The dates
-- inside `rubric_revisions` are rubric history markers, not a window.)
--
-- HONEST EMPTY: `points:*` labels only began landing 2026-09-29 (#9528), so
-- the joined population (issues with a points label AND a completed sweep)
-- is expected to be tiny or empty at first. CAL1 states the population per
-- rubric revision even over an empty window, and every other view states its
-- n beside every figure: n = 0 reads as "the loop is not yet warm", not as a
-- broken query. No sample data is committed in this artifact — the synthetic
-- population that verifies these queries lives only in
-- `loom-daemon/tests/story_points_calibration/execution.rs`.
--
-- This file is the CALIBRATION side of epic #9429's closing pair. The
-- throughput side (points landed per day, SF8/#9433) lives in
-- `sweep-facts/sweep-facts-queries.sql`; the two share the fact table and
-- the landed-size seam but answer different questions.

-- The window every CAL query reads. Edit HERE, once. `since` is the
-- points-label epoch (#9528); `until` is exclusive.
CREATE VIEW IF NOT EXISTS spc_window AS
SELECT '2026-09-29T00:00:00Z' AS since,
       '2026-10-08T00:00:00Z' AS until;

-- The rubric revision timeline — the provenance rule (#9434, mirroring
-- signoz/eta-queries.sql's revision grouping): EVERY calibration view groups
-- by the revision a landing was sized under, so a rubric change mid-window
-- shows up as two populations, never a silent average. This view is the one
-- place in SQL the timeline is written, and it must name the same markers as
-- the `## Revision history` section of `docs/story-points.md` (the contract
-- test fails when they disagree, so neither copy can drift alone).
--
-- PLACEHOLDER CONVENTION: v1 has no predecessor and no end date, so the
-- single row below reaches from the epoch of the store to the open present;
-- the SECOND revision is the first that will carry an `until`. When CAL4
-- reports `drifted` for a class and the rubric is revised: record the change
-- in the doc's revision history, add the new row here and in
-- `rubric_classes`, and re-run. Editing either view means dropping it first
-- (`CREATE VIEW IF NOT EXISTS` will not replace an existing view):
--   DROP VIEW rubric_classes; DROP VIEW rubric_revisions;
CREATE VIEW IF NOT EXISTS rubric_revisions (revision, since, until) AS
SELECT 'v1', '1970-01-01T00:00:00Z', NULL;

-- The rubric's own class table, per revision — the CLAIM the calibration
-- scores against (the right-hand side of "did we get the story points
-- right?"). It mirrors the table in `docs/story-points.md` §Assign points:
-- the doc is authoritative for humans, this view is what the queries join,
-- and the contract test fails when the two disagree, so the mirror cannot
-- go stale. `hw_lines_median` is the primary anchor (the rubric: "trust
-- lines"); files and tokens are cross-checks. These are medians, not
-- bounds — the class bounds CAL5 needs are DERIVED from them (log-midway
-- between adjacent medians) so they are never written down twice.
-- VALUES, not UNION ALL — D1 caps compound SELECT at 5 terms (#10066).
CREATE VIEW IF NOT EXISTS rubric_classes
    (revision, size_class, hw_lines_median, hw_files_median, tokens_median) AS
VALUES
    ('v1', '1',  12, 1, 14000000),
    ('v1', '2', 110, 2, 19000000),
    ('v1', '3', 285, 4, 30000000),
    ('v1', '5', 610, 6, 50000000),
    ('v1', '8', 1000, 8, 73000000),
    ('v1', '13', 2000, 8, 110000000);

-- Every LANDING sweep in the window, sized or not, classified clean vs
-- churn. The landing predicate mirrors `landed-size.sql`'s `landings` CTE
-- and `story-points-queries.sql`'s `sp_landings` (disposition = 'landed',
-- with the documented pre-#9441 fallback); if one changes, change all three.
-- The ACTUAL side is the landing sweep's own measurement: hw_lines /
-- hw_files (LOC), tokens_in + tokens_out (per sweep, not per phase, #9443),
-- total_duration_sec (wall — queue-sensitive, a cross-check only, exactly as
-- the rubric says), and the landed-size LSI when its parameter set is fitted
-- (NULL while `lsi_params_version` says the parameter set is unfitted, as
-- the column itself reports).
--
-- `churn_class` partitions the population the clean-landing filter excludes
-- (counted, never silently dropped): no phase breakdown → the filter cannot
-- be verified; a missing judge phase → never judged; more than one judge → a
-- re-judge loop; a doctor phase or doctor_cycles > 0 → a repair loop.
CREATE VIEW IF NOT EXISTS spc_landings AS
WITH base AS (
    SELECT f.repo                            AS repo,
           f.issue                           AS issue,
           f.sweep_id                        AS sweep_id,
           f.emitted_at                      AS landed_at,
           f.story_points                    AS story_points,
           (SELECT rv.revision FROM rubric_revisions rv
             WHERE f.emitted_at >= rv.since
               AND (rv.until IS NULL OR f.emitted_at < rv.until)
             ORDER BY rv.since DESC LIMIT 1) AS rubric_revision,
           f.hw_lines_added + f.hw_lines_deleted AS hw_lines,
           f.hw_files                        AS hw_files,
           f.tokens_in + f.tokens_out        AS tokens,
           f.total_duration_sec              AS wall_sec,
           ls.LSI                            AS lsi,
           ls.params_version                 AS lsi_params_version,
           f.phase_durations                 AS phase_durations,
           CASE WHEN f.phase_durations IS NULL THEN NULL ELSE
             (SELECT count(*) FROM json_each(f.phase_durations) je
               WHERE json_extract(je.value, '$.phase') = 'judge') END AS n_judge,
           CASE WHEN f.phase_durations IS NULL THEN NULL ELSE
             (SELECT count(*) FROM json_each(f.phase_durations) je
               WHERE json_extract(je.value, '$.phase') = 'doctor') END AS n_doctor,
           f.doctor_cycles                   AS doctor_cycles
    FROM sweep_facts f, spc_window w
    LEFT JOIN issue_landed_size ls
           ON ls.repo = f.repo
          AND ls.issue = f.issue
          AND ls.landing_sweep_id = f.sweep_id
    WHERE (f.disposition = 'landed'
           OR (f.disposition IS NULL
               AND f.result = 'success'
               AND f.pr_number IS NOT NULL))
      AND f.emitted_at >= w.since
      AND f.emitted_at < w.until
)
SELECT repo, issue, sweep_id, landed_at, story_points, rubric_revision,
       hw_lines, hw_files, tokens, wall_sec, lsi, lsi_params_version,
       n_judge, n_doctor, doctor_cycles,
       CASE
         WHEN phase_durations IS NULL      THEN 'no_phase_data'
         WHEN n_judge = 0                  THEN 'no_judge_phase'
         WHEN n_judge > 1                  THEN 'rejudge_loop'
         WHEN n_doctor > 0                 THEN 'doctor_repair'
         WHEN doctor_cycles > 0            THEN 'doctor_cycles_only'
         ELSE 'clean'
       END AS churn_class
FROM base;

-- The SCORED population: clean landings that carry an estimate, with the
-- implied class each one's ACTUAL size maps to under the rubric in force
-- when it was sized. The implied class uses the rubric's own rule — "a class
-- covers the range around its median (midway to the neighbours on a log
-- scale)" — so the boundary between adjacent classes is the geometric mean
-- of their medians, and a landing belongs to the first class whose boundary
-- it does not exceed. The comparison is kept in EXACT integer arithmetic
-- (hw_lines * hw_lines <= median_k * median_next, both sides squared), which
-- is the same predicate as comparing against sqrt(median_k * median_next)
-- without needing a floating-point sqrt. The top class is open-ended; a
-- landing with no measured hw_lines has no implied class (NULL, never a
-- guess); a landing whose assigned value is not a rubric class (an emitter
-- defect the fact table's vocabulary should prevent) gets NULL ranks and is
-- counted by CAL1 rather than silently folded.
CREATE VIEW IF NOT EXISTS spc_scored AS
WITH ordered AS (
    SELECT revision, size_class, hw_lines_median,
           row_number() OVER (PARTITION BY revision
                              ORDER BY hw_lines_median) AS idx
    FROM rubric_classes
),
bounds AS (
    SELECT o.revision, o.size_class, o.idx,
           o.hw_lines_median * p.hw_lines_median AS upper_bound_squared
    FROM ordered o
    LEFT JOIN ordered p
           ON p.revision = o.revision AND p.idx = o.idx + 1
),
classified AS (
    SELECT l.*,
           CASE WHEN l.hw_lines IS NULL THEN NULL ELSE COALESCE(
             (SELECT b.size_class FROM bounds b
               WHERE b.revision = l.rubric_revision
                 AND l.hw_lines * l.hw_lines <= b.upper_bound_squared
               ORDER BY b.upper_bound_squared LIMIT 1),
             (SELECT t.size_class FROM ordered t
               WHERE t.revision = l.rubric_revision
               ORDER BY t.hw_lines_median DESC LIMIT 1))
           END AS implied_class
    FROM spc_landings l
    WHERE l.story_points IS NOT NULL
      AND l.churn_class = 'clean'
)
SELECT c.*,
       oa.idx AS assigned_rank,
       oi.idx AS implied_rank,
       abs(oa.idx - oi.idx) AS class_gap,
       CASE WHEN abs(oa.idx - oi.idx) <= 1 THEN 1 ELSE 0 END AS within_one_bucket
FROM classified c
LEFT JOIN ordered oa
       ON oa.revision = c.rubric_revision
      AND oa.size_class = CAST(c.story_points AS TEXT)
LEFT JOIN ordered oi
       ON oi.revision = c.rubric_revision
      AND oi.size_class = c.implied_class;

-- CAL1. What is the joined population, and is the loop warm yet? One row per
-- rubric revision (plus a '(outside rubric_revisions)' row iff landings fall
-- in a revision-window gap — counted, never silently dropped), over the
-- landings of the window: how many carry an estimate, how many of those the
-- clean-landing filter scores, and why it excludes the rest. This is the
-- view to read FIRST: every n below is its subdivision, and an all-zero row
-- over an empty window is the honest "not yet warm" answer, not an error.
WITH land AS (
    SELECT COALESCE(rubric_revision, '(outside rubric_revisions)') AS rev, l.*
    FROM spc_landings l
),
frame AS (
    SELECT revision AS rev FROM rubric_revisions
    UNION
    SELECT rev FROM land
)
SELECT f.rev AS rubric_revision,
       count(x.sweep_id)                                                    AS landings,
       sum(CASE WHEN x.story_points IS NOT NULL THEN 1 ELSE 0 END)          AS points_sized,
       sum(CASE WHEN x.sweep_id IS NOT NULL
                 AND x.story_points IS NULL THEN 1 ELSE 0 END)      AS points_unsized,
       sum(CASE WHEN x.churn_class = 'clean'
                 AND x.story_points IS NOT NULL THEN 1 ELSE 0 END)          AS clean_scored,
       sum(CASE WHEN x.churn_class = 'no_phase_data' THEN 1 ELSE 0 END)     AS excl_no_phase_data,
       sum(CASE WHEN x.churn_class = 'no_judge_phase' THEN 1 ELSE 0 END)    AS excl_no_judge_phase,
       sum(CASE WHEN x.churn_class = 'rejudge_loop' THEN 1 ELSE 0 END)      AS excl_rejudge_loop,
       sum(CASE WHEN x.churn_class = 'doctor_repair' THEN 1 ELSE 0 END)     AS excl_doctor_repair,
       sum(CASE WHEN x.churn_class = 'doctor_cycles_only' THEN 1 ELSE 0 END) AS excl_doctor_cycles_only,
       sum(CASE WHEN x.story_points IS NOT NULL AND NOT EXISTS
             (SELECT 1 FROM rubric_classes rc
               WHERE rc.revision = x.rubric_revision
                 AND rc.size_class = CAST(x.story_points AS TEXT))
            THEN 1 ELSE 0 END)                                              AS sized_without_rubric_class
FROM frame f
LEFT JOIN land x ON x.rev = f.rev
GROUP BY f.rev
ORDER BY f.rev;

-- CAL2. Per assigned bucket, what did the work actually cost? The
-- distribution (median, p25, p75) of each ACTUAL measure over the scored
-- population — LOC (hw_lines / hw_files), tokens, wall-clock, LSI — grouped
-- by rubric revision and by bucket, with the churn-excluded count BESIDE the
-- figures (excluded is counted, never silently dropped). Medians follow
-- SP4's exact convention (the mean of order statistics ⌊(n+1)/2⌋ and
-- ⌊(n+2)/2⌋); each quartile averages the pair of order statistics at
-- ⌊q(n+1)⌋ and ⌈q(n+1)⌉, clamped to [1, n] — the same inclusive-pair
-- convention as the median, degenerate to a single position whenever
-- q(n+1) is an integer. A NULL median with n_measured
-- 0 means the measure was absent on every landing of that bucket (missing,
-- never zero); the `lsi` arm is empty until the landed-size parameter set is
-- fitted (`landed-size.sql` params_version), which is the honest state.
WITH measured AS (
    SELECT rubric_revision, story_points, 'hw_lines' AS measure, hw_lines AS value
      FROM spc_scored WHERE hw_lines IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'hw_files', hw_files
      FROM spc_scored WHERE hw_files IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'tokens', tokens
      FROM spc_scored WHERE tokens IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'wall_sec', wall_sec
      FROM spc_scored WHERE wall_sec IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'lsi', lsi
      FROM spc_scored WHERE lsi IS NOT NULL
),
ranked AS (
    SELECT rubric_revision, story_points, measure, value,
           row_number() OVER (PARTITION BY rubric_revision, story_points, measure
                              ORDER BY value) AS rn,
           count(*)     OVER (PARTITION BY rubric_revision, story_points, measure) AS n
    FROM measured
),
agg AS (
    SELECT rubric_revision, story_points, measure,
           count(*) AS n_measured,
           avg(CASE WHEN rn IN ((n + 1) / 2, (n + 2) / 2) THEN value END) AS median_value,
           avg(CASE WHEN rn IN (max(1, (n + 1) / 4), min(n, (n + 4) / 4))
                    THEN value END)                                        AS p25_value,
           avg(CASE WHEN rn IN (max(1, 3 * (n + 1) / 4), min(n, (3 * n + 6) / 4))
                    THEN value END)                                        AS p75_value
    FROM ranked
    GROUP BY rubric_revision, story_points, measure
),
frame AS (
    SELECT rubric_revision, story_points,
           count(*) AS sized_landings,
           sum(CASE WHEN churn_class = 'clean' THEN 1 ELSE 0 END) AS clean_landings,
           sum(CASE WHEN churn_class <> 'clean' THEN 1 ELSE 0 END) AS excluded_churn
    FROM spc_landings
    WHERE story_points IS NOT NULL
    GROUP BY rubric_revision, story_points
),
measures(measure) AS (
    VALUES ('hw_files'), ('hw_lines'), ('lsi'), ('tokens'), ('wall_sec')
)
SELECT f.rubric_revision,
       f.story_points AS assigned_points,
       m.measure,
       f.sized_landings,
       f.clean_landings,
       f.excluded_churn,
       COALESCE(a.n_measured, 0) AS n_measured,
       a.median_value,
       a.p25_value,
       a.p75_value
FROM frame f
CROSS JOIN measures m
LEFT JOIN agg a
       ON a.rubric_revision = f.rubric_revision
      AND a.story_points = f.story_points
      AND a.measure = m.measure
ORDER BY f.rubric_revision, f.story_points, m.measure;

-- CAL3. Do bigger estimates actually cost more? Spearman's rank correlation
-- between the assigned points and each ACTUAL measure, per rubric revision —
-- the one committed query that answers "how accurate are assigned points?"
-- for any window. Ties receive average ranks on BOTH sides (the standard
-- Spearman correction; points are discrete, so ties are the norm), and ρ is
-- Pearson's r over those ranks; it is NULL when n < 2 or either side is
-- constant — an honest absence, never a fabricated 0.
--
-- Read it against the baselines recorded in the questions doc: the existing
-- complexity tier (holdout ρ 0.64) is the bar "points are accurate" must
-- clear; the retrospective reference is ρ 0.82 [0.74, 0.88] vs landed size;
-- the measurement ceiling is ≈ 0.90. The `within_one_bucket_*` columns are
-- the discrete companion (retrospective reference: 91% within one bucket):
-- the share of scored landings whose implied class is at most one bucket
-- from the assigned one. `excluded_from_score` carries the churn count
-- beside the score (excluded, never silently dropped).
--
-- The rank columns are TRUE average ranks: global position minus the
-- within-tie-group position plus the group's mean offset (k+1)/2, which
-- yields (first+last)/2 for a tied group and the plain position for an
-- untied one — the standard Spearman tie correction. (The tempting
-- `position - (k-1)/2` shortcut centers each group on its FIRST position
-- instead, a per-group shift that silently changes the correlation.)
WITH measured AS (
    SELECT rubric_revision, story_points, 'hw_lines' AS measure, hw_lines AS value
      FROM spc_scored WHERE hw_lines IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'hw_files', hw_files
      FROM spc_scored WHERE hw_files IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'tokens', tokens
      FROM spc_scored WHERE tokens IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'wall_sec', wall_sec
      FROM spc_scored WHERE wall_sec IS NOT NULL
    UNION ALL
    SELECT rubric_revision, story_points, 'lsi', lsi
      FROM spc_scored WHERE lsi IS NOT NULL
),
ranked AS (
    SELECT rubric_revision, measure, story_points, value,
           row_number() OVER (PARTITION BY rubric_revision, measure ORDER BY story_points)
             - row_number() OVER (PARTITION BY rubric_revision, measure, story_points)
             + (count(*) OVER (PARTITION BY rubric_revision, measure, story_points) + 1) / 2.0
             AS rank_points,
           row_number() OVER (PARTITION BY rubric_revision, measure ORDER BY value)
             - row_number() OVER (PARTITION BY rubric_revision, measure, value)
             + (count(*) OVER (PARTITION BY rubric_revision, measure, value) + 1) / 2.0
             AS rank_value
    FROM measured
),
corr AS (
    SELECT rubric_revision, measure,
           count(*)                AS n,
           (sum(rank_points * rank_points) - sum(rank_points) * 1.0 * sum(rank_points) / count(*))
             * (sum(rank_value * rank_value) - sum(rank_value) * 1.0 * sum(rank_value) / count(*))
               AS denom,
           sum(rank_points * rank_value)
             - sum(rank_points) * 1.0 * sum(rank_value) / count(*)
               AS num
    FROM ranked
    GROUP BY rubric_revision, measure
),
-- The square root of `denom` by Newton's method (y <- (y + denom/y) / 2
-- from above, so the iteration decreases monotonically and the relative
-- tolerance 1e-12 terminates it), as a recursive CTE rather than sqrt():
-- D1 ships SQLite's math functions but not every SQLite these artifacts
-- are re-verified under does, and a committed query that cannot run where
-- it is verified is a query nobody re-verifies. The iteration cap is
-- belt-and-braces; quadratic convergence reaches the tolerance in a handful
-- of steps after the initial halving. A degenerate group (n = 1, or either
-- side constant) has denom 0 and no row here, so its rho stays NULL.
newton AS (
    SELECT rubric_revision, measure, num, denom,
           (1.0 + denom) / 2.0 AS y, 0 AS iter
    FROM corr
    WHERE denom > 0
    UNION ALL
    SELECT rubric_revision, measure, num, denom,
           (y + denom / y) / 2.0, iter + 1
    FROM newton
    WHERE abs(y * y - denom) > denom * 1.0e-12
      AND iter < 128
),
best AS (
    SELECT n.rubric_revision, n.measure, n.num, n.y
    FROM newton n
    WHERE n.iter = (SELECT max(n2.iter) FROM newton n2
                     WHERE n2.rubric_revision = n.rubric_revision
                       AND n2.measure = n.measure)
),
agreement AS (
    SELECT rubric_revision,
           count(within_one_bucket) AS n_with_measured_size,
           sum(within_one_bucket)   AS within_one_bucket,
           sum(CASE WHEN class_gap >= 2 THEN 1 ELSE 0 END) AS outliers_reported_by_cal5
    FROM spc_scored
    GROUP BY rubric_revision
),
churn AS (
    SELECT rubric_revision, count(*) AS excluded_from_score
    FROM spc_landings
    WHERE story_points IS NOT NULL
      AND churn_class <> 'clean'
    GROUP BY rubric_revision
)
SELECT c.rubric_revision,
       c.measure,
       c.n,
       round(b.num / b.y, 3) AS spearman_rho,
       k.excluded_from_score,
       a.n_with_measured_size,
       a.within_one_bucket,
       round(100.0 * a.within_one_bucket / a.n_with_measured_size, 1)
           AS within_one_bucket_pct,
       a.outliers_reported_by_cal5
FROM corr c
LEFT JOIN best b
       ON b.rubric_revision = c.rubric_revision AND b.measure = c.measure
LEFT JOIN agreement a ON a.rubric_revision = c.rubric_revision
LEFT JOIN churn k ON k.rubric_revision = c.rubric_revision
ORDER BY c.rubric_revision, c.measure;

-- CAL4. Per-bucket drift: is a "3" really bigger than a "1" by roughly the
-- ratio the rubric claims? One row per rubric class per revision: the
-- measured median hw_lines of the bucket against the rubric's claimed
-- median, the ratio between them, and BOTH ratio-to-class-1 ladders (the
-- rubric's implied Fibonacci ratios vs the measured ones). The verdict
-- column is the standing report the acceptance criterion reads: `held` when
-- the measured median stays within [0.5x, 2x] of the claim (one Fibonacci
-- step — the resolution these ordinal classes carry), `drifted` when it does
-- not, `insufficient_n` below n = 20, `not_scored` for classes the window
-- never scored. A `drifted` verdict is the normal, expected trigger for a
-- rubric revision (see the questions doc's revision path) — for every class
-- except 13: an UPWARD drift at 13 means issues too big for one landing are
-- not being split, which is a curation fix, not a bound change.
WITH med AS (
    SELECT rubric_revision, story_points,
           count(*) AS n_scored,
           avg(CASE WHEN rn IN ((n + 1) / 2, (n + 2) / 2) THEN hw_lines END)
               AS median_hw_lines
    FROM (SELECT rubric_revision, story_points, hw_lines,
                 row_number() OVER (PARTITION BY rubric_revision, story_points
                                    ORDER BY hw_lines) AS rn,
                 count(*) OVER (PARTITION BY rubric_revision, story_points) AS n
          FROM spc_scored
          WHERE hw_lines IS NOT NULL)
    GROUP BY rubric_revision, story_points
),
anchor AS (
    SELECT rubric_revision,
           max(CASE WHEN story_points = 1 THEN median_hw_lines END) AS measured_class1
    FROM med
    GROUP BY rubric_revision
),
rubric AS (
    SELECT revision, size_class,
           CAST(size_class AS INTEGER) AS pts,
           hw_lines_median,
           first_value(hw_lines_median) OVER (PARTITION BY revision
                                              ORDER BY hw_lines_median) AS class1_median
    FROM rubric_classes
),
churn AS (
    SELECT rubric_revision, story_points, count(*) AS excluded_churn
    FROM spc_landings
    WHERE story_points IS NOT NULL
      AND churn_class <> 'clean'
    GROUP BY rubric_revision, story_points
)
SELECT r.revision AS rubric_revision,
       r.pts AS assigned_points,
       COALESCE(m.n_scored, 0) AS n_scored,
       m.median_hw_lines AS measured_median_hw_lines,
       r.hw_lines_median AS rubric_claimed_median,
       round(m.median_hw_lines * 1.0 / r.hw_lines_median, 2) AS measured_over_claimed,
       round(r.hw_lines_median * 1.0 / r.class1_median, 1) AS rubric_ratio_to_class1,
       round(m.median_hw_lines / a.measured_class1, 1) AS measured_ratio_to_class1,
       COALESCE(c.excluded_churn, 0) AS excluded_churn,
       CASE
         WHEN m.n_scored IS NULL                                 THEN 'not_scored'
         WHEN m.n_scored < 20                                    THEN 'insufficient_n'
         WHEN m.median_hw_lines * 1.0 / r.hw_lines_median
              BETWEEN 0.5 AND 2.0                               THEN 'held'
         ELSE 'drifted'
       END AS verdict
FROM rubric r
LEFT JOIN med m
       ON m.rubric_revision = r.revision AND m.story_points = r.pts
LEFT JOIN anchor a ON a.rubric_revision = r.revision
LEFT JOIN churn c
       ON c.rubric_revision = r.revision AND c.story_points = r.pts
ORDER BY r.revision, r.pts;

-- CAL5. The misassignment report: named outliers in BOTH directions — a "1"
-- that cost like an "8" (underestimated) and an "8" that landed trivially
-- (overestimated). This is what teaches the Curator. A row appears when the
-- assigned class and the class the landing's actual size implies are two or
-- more buckets apart (the implied class comes from the rubric's own
-- log-midway bounds — see `spc_scored`). Churn-excluded landings never
-- appear here (their cost is not a clean measurement); CAL6 counts them.
WITH churn AS (
    SELECT rubric_revision, count(*) AS excluded_from_score
    FROM spc_landings
    WHERE story_points IS NOT NULL
      AND churn_class <> 'clean'
    GROUP BY rubric_revision
)
SELECT s.rubric_revision,
       s.repo,
       s.issue,
       s.sweep_id,
       s.landed_at,
       s.story_points AS assigned_points,
       s.implied_class,
       s.class_gap,
       CASE WHEN s.implied_rank > s.assigned_rank THEN 'underestimated'
            WHEN s.implied_rank < s.assigned_rank THEN 'overestimated'
       END AS direction,
       s.hw_lines,
       s.hw_files,
       s.tokens,
       s.wall_sec,
       s.lsi,
       k.excluded_from_score
FROM spc_scored s
LEFT JOIN churn k ON k.rubric_revision = s.rubric_revision
WHERE s.class_gap >= 2
ORDER BY s.rubric_revision, s.class_gap DESC, s.repo, s.issue;

-- CAL6. Churn accounting: the sized landings the clean-landing filter
-- EXCLUDED, per bucket and per reason — counted separately, never silently
-- dropped. Repair loops (doctor_repair, doctor_cycles_only) and re-judge
-- loops (rejudge_loop) are process cost, not estimate error: scoring them
-- would blame the rubric for rework; dropping them would hide the rework.
-- The missing-data reasons (no_phase_data, no_judge_phase) are data gaps —
-- read them beside CAL1's totals. CAL2/CAL4 carry the per-bucket total
-- beside the score; this view is the reason-level breakdown.
SELECT rubric_revision,
       story_points AS assigned_points,
       churn_class AS exclusion_reason,
       count(*) AS excluded
FROM spc_landings
WHERE story_points IS NOT NULL
  AND churn_class <> 'clean'
GROUP BY rubric_revision, story_points, churn_class
ORDER BY rubric_revision, story_points, churn_class;

-- The window-scoped, derived views this file created (the rubric views and
-- their mirror contract with the doc stay: they are definitions, not state).
DROP VIEW IF EXISTS spc_scored;
DROP VIEW IF EXISTS spc_landings;
DROP VIEW IF EXISTS spc_window;
