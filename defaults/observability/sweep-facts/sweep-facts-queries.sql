-- Sweep facts: the canonical question set SF1..SF8 (Issues #9446, #9466,
-- #9433).
--
-- D1/SQLite dialect (JSON1 + window functions). Every question, its exact
-- definition, and what this set deliberately cannot answer are in
-- `sweep-facts-questions.md`. Do not read a number out of here without it —
-- especially "absent vs. zero". Run `sweep-facts-rollup.sql` first; SF1/SF5
-- also read the `issue_effort` view (`issue-effort.sql`) and SF2/SF8 read
-- `issue_landed_size` and `measured_point_values` (`landed-size.sql`, whose
-- `params_version` names the parameter set any LSI figure was computed under).
--
-- Windows are BOUND, never edited into a query. D1 has no client-side bind
-- parameters for a committed file, so the window lives in the `sf_window`
-- view below — the ONE place in this file a date literal may appear. Edit it
-- there, run the whole file:
--
--   wrangler d1 execute loom-fleet-telemetry --file sweep-facts-queries.sql
--
-- All eight answers come back in one pass, in SF order (SF1 as two
-- statements: the per-issue split, then its window coverage row).

-- The window every SF query reads. Edit HERE, once.
CREATE VIEW IF NOT EXISTS sf_window AS
SELECT '2026-09-22T00:00:00Z' AS since,
       '2026-09-29T00:00:00Z' AS until;

-- SF1. Per issue: what did the whole lifecycle cost versus the clean landing,
-- and can the lifecycle sum be trusted? `token_completeness = 0` means at
-- least one attempt's tokens are merely unknown (#9440) — the lifecycle sums
-- of such a row are lower bounds, and the clean column is the only comparable
-- one. NULL cells are missing data, never measured zeros.
--
-- The seconds partition (#9507): `clean_sec + substantive_rework_sec +
-- environmental_rework_sec + unattributed_sec = lifecycle_wall_sec +
-- overaccounted_sec` (see `issue-effort.sql` for the arithmetic and why
-- `clean_sec` is not `clean_wall_sec`). READ THE COVERAGE ROW BELOW FIRST:
-- the split describes only the attributable share of the window.
SELECT
    e.repo                   AS repo,
    e.issue                  AS issue,
    e.landing_pr_number      AS landing_pr_number,
    e.landed_at              AS landed_at,
    e.attempts               AS attempts,
    e.lifecycle_wall_sec     AS lifecycle_wall_sec,
    e.lifecycle_tokens_in    AS lifecycle_tokens_in,
    e.lifecycle_tokens_out   AS lifecycle_tokens_out,
    e.token_completeness     AS token_completeness,
    e.clean_wall_sec         AS clean_wall_sec,
    e.clean_tokens_in        AS clean_tokens_in,
    e.clean_tokens_out       AS clean_tokens_out,
    e.rework_substantive     AS rework_substantive,
    e.rework_environmental   AS rework_environmental,
    e.rework_substantive_open    AS rework_substantive_open,
    e.rework_environmental_open  AS rework_environmental_open,
    e.clean_sec                  AS clean_sec,
    e.substantive_rework_sec     AS substantive_rework_sec,
    e.environmental_rework_sec   AS environmental_rework_sec,
    e.unattributed_sec           AS unattributed_sec,
    e.overaccounted_sec          AS overaccounted_sec,
    e.attributed_attempts        AS attributed_attempts,
    e.attributed_wall_pct        AS attributed_wall_pct
FROM issue_effort e, sf_window w
WHERE e.landed_at >= w.since AND e.landed_at < w.until
ORDER BY e.lifecycle_tokens_in DESC, e.repo, e.issue;

-- SF1, coverage row (#9507) — read this BEFORE the split above. One row for
-- the whole window: how many of the landed issues' attempts and partitioned
-- seconds the trigger classification can attribute at all. The unattributed
-- population is `operator_redispatch`, `unknown` and every pre-#9444 record
-- (no `trigger`); until the trigger/rework writers (#9506/#9514) are on the
-- fleet it is nearly everything, which is the rollout boundary showing, not a
-- defect. Lifecycle columns repeat on every landing row of an issue, so the
-- issues are deduplicated before summing. An empty window yields one row of
-- zero issues and NULL shares — never a divide by zero.
SELECT
    count(*)                                         AS issues,
    sum(d.attempts)                                  AS attempts,
    sum(d.attributed_attempts)                       AS attributed_attempts,
    round(100.0 * sum(d.attributed_attempts)
          / NULLIF(sum(d.attempts), 0), 1)           AS attributed_attempts_pct,
    sum(d.lifecycle_wall_sec)                        AS lifecycle_wall_sec,
    sum(d.clean_sec)                                 AS clean_sec,
    sum(d.substantive_rework_sec)                    AS substantive_rework_sec,
    sum(d.environmental_rework_sec)                  AS environmental_rework_sec,
    sum(d.unattributed_sec)                          AS unattributed_sec,
    sum(d.overaccounted_sec)                         AS overaccounted_sec,
    round(100.0 * (sum(d.clean_sec) + sum(d.substantive_rework_sec)
                   + sum(d.environmental_rework_sec))
          / NULLIF(sum(d.clean_sec) + sum(d.substantive_rework_sec)
                   + sum(d.environmental_rework_sec)
                   + sum(d.unattributed_sec), 0), 1) AS attributed_wall_pct
FROM (
    SELECT DISTINCT e.repo, e.issue, e.attempts, e.attributed_attempts,
           e.lifecycle_wall_sec, e.clean_sec, e.substantive_rework_sec,
           e.environmental_rework_sec, e.unattributed_sec, e.overaccounted_sec
    FROM issue_effort e, sf_window w
    WHERE e.landed_at >= w.since AND e.landed_at < w.until
) d;

-- SF2. How much work lands per day: Σ LSI over the day's landings — the
-- size-weighted throughput KPI (#9466), and the headline half of the pair it
-- forms with SF8 (#9433): SF2 is what actually landed, SF8 is what the Curator
-- forecast would land and how much of the day was sized at all. Labels are
-- never summed (a "13" is not thirteen "1"s — see the measured bucket ratios in
-- `landed-size.sql`'s `measured_point_values`); the two provisional
-- measured-point columns are the
-- experiment's alternative until #9434 collapses them into one point value.
-- `without_token_component` counts landings whose LSI used two components, so
-- a day that mixes definitions is visible. LSI values are the v1-2026-10-02
-- fit's (#9934); the `params_version` column on every row says which fit
-- produced them, and a refit re-answers the same query unchanged.
SELECT
    date(v.landed_at)                          AS day,
    count(*)                                   AS landings,
    sum(v.LSI)                                 AS lsi_landed,
    sum(v.measured_point_tokens)               AS measured_point_tokens_landed,
    sum(v.measured_point_lines)                AS measured_point_lines_landed,
    count(*) - sum(v.landed_size_tokens_used)  AS without_token_component
FROM issue_landed_size v, sf_window w
WHERE v.landed_at >= w.since AND v.landed_at < w.until
GROUP BY day
ORDER BY day;

-- SF3. Which hosts attribute tokens a sweep could not have consumed: the
-- implausible-rate monitor (#9454's committed query) — token-bearing outcomes
-- exceeding 100k input tokens per second of recorded wall time, per host per
-- day, with the share and the emitter's own `suspect` flags beside them. A
-- regression in the attribution path shows up here as a host's share jumping.
SELECT
    f.host_id    AS host_id,
    date(f.emitted_at) AS day,
    count(*)     AS token_bearing,
    sum(CASE WHEN f.tokens_in * 1.0
                  / NULLIF(f.total_duration_sec, 0) > 100000
             THEN 1 ELSE 0 END)                       AS implausible,
    round(100.0 * sum(CASE WHEN f.tokens_in * 1.0
                                / NULLIF(f.total_duration_sec, 0) > 100000
                           THEN 1 ELSE 0 END)
               / count(*), 1)                         AS implausible_pct,
    sum(f.suspect)                                    AS flagged_suspect_by_emitter
FROM sweep_facts f, sf_window w
WHERE f.emitted_at >= w.since AND f.emitted_at < w.until
  AND f.tokens_in IS NOT NULL
  AND f.tokens_in > 0
GROUP BY f.host_id, day
ORDER BY implausible_pct DESC, f.host_id, day;

-- SF4. What do sweeps actually do: the disposition distribution over a
-- window (#9441). `unknown` must stay under 5% of outcomes over a week (the
-- acceptance bar); the `(absent)` bucket — records written before #9441 — is
-- counted separately, never folded into `unknown`: one is "truly
-- unobservable", the other is "observed before the field existed".
SELECT
    COALESCE(f.disposition, '(absent: pre-#9441)') AS disposition,
    count(*)                                       AS outcomes,
    round(100.0 * count(*) / sum(count(*)) OVER (), 1) AS pct,
    round(100.0 * sum(CASE WHEN f.disposition = 'unknown'
                           THEN 1 ELSE 0 END)
               / sum(count(*)) OVER (), 1)         AS unknown_pct
FROM sweep_facts f, sf_window w
WHERE f.emitted_at >= w.since AND f.emitted_at < w.until
GROUP BY disposition
ORDER BY outcomes DESC;

-- SF5. How many attempts does an issue take, and why: attempts split by
-- dispatch trigger into first / substantive / environmental / unclassified
-- (#9444's one committed query; "unclassified" is counted, never merged —
-- it is `operator_redispatch`, `unknown`, or a pre-#9444 record with no
-- trigger). The rework columns fold the in-sweep `rework_events`
-- classifications in via `issue_effort`, over issues that LANDED in the
-- window (their sums cover the issue's whole history). (Not answerable from
-- this store, and therefore not an SF question: whether a closed issue later
-- gained a queue label — that is forge state, not telemetry.)
WITH attempts AS (
    SELECT
        f.repo  AS repo,
        f.issue AS issue,
        count(*) AS attempts,
        sum(CASE WHEN f.trigger = 'first'
                 THEN 1 ELSE 0 END)                   AS first_dispatch,
        sum(CASE WHEN f.trigger IN ('retry_after_substantive_failure',
                                    'doctor_after_changes_requested')
                 THEN 1 ELSE 0 END)                   AS substantive_attempts,
        sum(CASE WHEN f.trigger IN ('retry_after_env_failure',
                                    'rebase_main_moved',
                                    'merge_conflict',
                                    'stale_base_rejudge',
                                    'ci_failure_fix')
                 THEN 1 ELSE 0 END)                   AS environmental_attempts,
        count(*) - sum(CASE WHEN f.trigger IN ('first',
                                    'retry_after_substantive_failure',
                                    'doctor_after_changes_requested',
                                    'retry_after_env_failure',
                                    'rebase_main_moved',
                                    'merge_conflict',
                                    'stale_base_rejudge',
                                    'ci_failure_fix')
                 THEN 1 ELSE 0 END)                   AS unclassified_attempts
    FROM sweep_facts f, sf_window w
    WHERE f.emitted_at >= w.since AND f.emitted_at < w.until
    GROUP BY f.repo, f.issue
),
rework AS (
    SELECT
        e.repo  AS repo,
        e.issue AS issue,
        max(e.rework_substantive)   AS rework_substantive,
        max(e.rework_environmental) AS rework_environmental
    FROM issue_effort e, sf_window w
    WHERE e.landed_at >= w.since AND e.landed_at < w.until
    GROUP BY e.repo, e.issue
)
SELECT
    a.repo                    AS repo,
    a.issue                   AS issue,
    a.attempts                AS attempts,
    a.first_dispatch          AS first_dispatch,
    a.substantive_attempts    AS substantive_attempts,
    a.environmental_attempts  AS environmental_attempts,
    a.unclassified_attempts   AS unclassified_attempts,
    r.rework_substantive      AS rework_substantive,
    r.rework_environmental    AS rework_environmental
FROM attempts a
LEFT JOIN rework r ON r.repo = a.repo AND r.issue = a.issue
ORDER BY a.attempts DESC, a.repo, a.issue;

-- SF6. Are tokens measured, or just missing: the outcome distribution by
-- tokens_status (#9440). The acceptance bar — ≥ 95% of outcomes conclusively
-- statused — is read against the `measured`/`suspect`/`not_spawned` rows;
-- `(absent)` is pre-#9440 history, `unattributable` is "spawned, usage
-- unreadable", and neither may be folded into a coverage percentage.
SELECT
    COALESCE(f.tokens_status, '(absent: pre-#9440)') AS tokens_status,
    count(*)                                         AS outcomes,
    round(100.0 * count(*) / sum(count(*)) OVER (), 1) AS pct,
    sum(CASE WHEN f.tokens_in IS NOT NULL THEN 1 ELSE 0 END) AS with_tokens_in,
    sum(f.suspect)                                   AS flagged_suspect
FROM sweep_facts f, sf_window w
WHERE f.emitted_at >= w.since AND f.emitted_at < w.until
GROUP BY tokens_status
ORDER BY outcomes DESC;

-- SF7. Is the rollup faithful to the raw records: reconciliation over the
-- window (#9446's acceptance criterion, the SF analogue of CT8).
--
--   raw_in_grain      raw sweep.outcome rows in the window that belong in the
--                     keyed grain (identity columns present).
--   raw_out_of_grain  raw rows excluded from the grain — the ABSENT-repo
--                     unattributed bucket (#9442) and absent identities. This
--                     population is the ONLY legitimate difference between
--                     the two sides, which is why it is counted here and not
--                     silently dropped by the rollup.
--   unexplained_drops raw_in_grain − fact_rows. MUST be 0 in every window: a
--                     positive value means a raw sweep has no fact row (a
--                     re-run the rollup's window missed — repair by re-running
--                     sweep-facts-rollup.sql over the window), a negative
--                     value means more facts than raw rows (duplicate
--                     injection or a window bound applied on the wrong
--                     column).
SELECT
    (SELECT count(*)
       FROM records r, sf_window w
      WHERE r.kind = 'sweep.outcome'
        AND r.emitted_at >= w.since AND r.emitted_at < w.until
        AND r.sweep_id IS NOT NULL
        AND r.repo IS NOT NULL
        AND r.issue IS NOT NULL)                          AS raw_in_grain,
    (SELECT count(*)
       FROM records r, sf_window w
      WHERE r.kind = 'sweep.outcome'
        AND r.emitted_at >= w.since AND r.emitted_at < w.until
        AND (r.sweep_id IS NULL OR r.repo IS NULL OR r.issue IS NULL))
                                                          AS raw_out_of_grain,
    (SELECT count(*)
       FROM sweep_facts f, sf_window w
      WHERE f.emitted_at >= w.since AND f.emitted_at < w.until)
                                                          AS fact_rows,
    (SELECT count(*)
       FROM records r, sf_window w
      WHERE r.kind = 'sweep.outcome'
        AND r.emitted_at >= w.since AND r.emitted_at < w.until
        AND r.sweep_id IS NOT NULL
        AND r.repo IS NOT NULL
        AND r.issue IS NOT NULL)
    - (SELECT count(*)
       FROM sweep_facts f, sf_window w
      WHERE f.emitted_at >= w.since AND f.emitted_at < w.until)
                                                          AS unexplained_drops;

-- SF8. Points landed per day and per ISO week: the FORECAST side of the
-- throughput KPI (#9433, epic #9429), read beside SF2's measured `lsi_landed`.
-- One statement, two grains — `grain` is `'day'` or `'iso_week'` and `bucket`
-- is that grain's key, so a window can be read at either resolution without a
-- second query drifting from this one's definitions.
--
-- Three rules this query exists to enforce, each of them load-bearing:
--
--   1. LANDED ONLY. The predicate mirrors `landed-size.sql`'s `landings` CTE
--      exactly (`disposition = 'landed'`, with the documented pre-#9441
--      fallback); if one changes, change both. A failed, cancelled or no-op
--      sweep lands nothing, so it contributes to neither the points columns
--      nor `landings`.
--   2. MISSING IS NOT ZERO. `points_missing` counts landings whose
--      `story_points` is absent — the CT7/SF4 data gap, reported BESIDE the
--      sums and never folded into them. A day whose points look low because
--      nobody sized the work must stay distinguishable from a day that
--      genuinely landed small work. `points_sized + points_missing` always
--      equals `landings`, which is how a reader checks the coverage.
--   3. LABELS ARE NOT A UNIT. Fibonacci labels are ORDINAL: the experiment's
--      pre-registered ±30% additivity test fails on every measured axis
--      (tokens 1 : 1.3 : 2.2 : 3.4 : 5.1 : 8.2; hand-written lines
--      1 : 8.5 : 21 : 47 : 82 : 197 — #9429). "Points landed" is therefore the
--      sum of the MEASURED point value of each landing's ASSIGNED class,
--      joined from `measured_point_values` (`landed-size.sql`) — two columns,
--      because the two components disagree by design until #9434 collapses
--      them into one. The raw label sum is still reported, but only under a
--      name that states what it is not: it is for spotting a mislabelled
--      window, never for reading size out of.
--
-- `landings` is reported alongside on purpose: over a measured 34-day window
-- issue count alone explains R² 0.83 of daily delivered size and summed points
-- raise that only to 0.94 — most daily variation is volume, not size mix.
--
-- `sized_without_measured_value` counts landings that carry an estimate with no
-- row in `measured_point_values`. It must be 0: `story_points` is constrained
-- to 1/2/3/5/8/13 by the emitter (telemetry-schema.md §`story_points`), so a
-- positive value is an emitter or vocabulary defect, counted here rather than
-- silently dropped by the LEFT JOIN.
--
-- `lsi_landed` is SF2's measured total, re-read through `issue_landed_size` on
-- the landing sweep rather than recomputed, so forecast and actual for the same
-- bucket sit in one row. It carries the v1-2026-10-02 fit's values (#9934) —
-- exactly as in SF2.
--
-- The ISO week key is computed, not `strftime`'d: `%V`/`%G` require SQLite
-- >= 3.46, so the week is keyed off the THURSDAY of the landing's ISO week
-- (`date(d, '-3 days', 'weekday 4')`), whose calendar year IS the ISO year by
-- definition and whose day-of-year yields the week number exactly
-- ((day_of_year - 1) / 7 + 1, since that Thursday always falls in Jan 1..7 for
-- week 1 and shifts by exactly 7 days per week thereafter).
WITH sf8_landed AS (
    SELECT
        f.repo       AS repo,
        f.issue      AS issue,
        f.sweep_id   AS sweep_id,
        f.emitted_at AS landed_at,
        f.story_points AS story_points
    FROM sweep_facts f, sf_window w
    WHERE f.emitted_at >= w.since AND f.emitted_at < w.until
      AND (f.disposition = 'landed'
           OR (f.disposition IS NULL
               AND f.result = 'success'
               AND f.pr_number IS NOT NULL))
),
sf8_points AS (
    SELECT
        date(l.landed_at)                                        AS day,
        strftime('%Y', date(l.landed_at, '-3 days', 'weekday 4'))
            || '-W'
            || substr('0' || ((CAST(strftime('%j',
                 date(l.landed_at, '-3 days', 'weekday 4')) AS INTEGER) - 1)
                 / 7 + 1), -2)                                   AS iso_week,
        l.story_points                                           AS story_points,
        m.measured_point_tokens                                  AS measured_point_tokens,
        m.measured_point_lines                                   AS measured_point_lines,
        v.LSI                                                    AS lsi
    FROM sf8_landed l
    -- The forecast join: the Curator's ASSIGNED class (an INTEGER label on the
    -- fact row) against the measured point table's TEXT class key.
    LEFT JOIN measured_point_values m
           ON m.size_class = CAST(l.story_points AS TEXT)
    -- The actual side, keyed on the landing sweep itself.
    LEFT JOIN issue_landed_size v
           ON v.repo = l.repo
          AND v.issue = l.issue
          AND v.landing_sweep_id = l.sweep_id
)
SELECT
    'day'                                          AS grain,
    p.day                                          AS bucket,
    count(*)                                       AS landings,
    sum(p.story_points IS NOT NULL)                AS points_sized,
    sum(p.story_points IS NULL)                    AS points_missing,
    sum(p.measured_point_tokens)                   AS measured_points_tokens_landed,
    sum(p.measured_point_lines)                    AS measured_points_lines_landed,
    sum(p.story_points IS NOT NULL
        AND p.measured_point_tokens IS NULL)       AS sized_without_measured_value,
    sum(p.story_points)                            AS labels_summed_ordinal_do_not_use_as_size,
    sum(p.lsi)                                     AS lsi_landed
FROM sf8_points p
GROUP BY p.day
UNION ALL
SELECT
    'iso_week'                                     AS grain,
    p.iso_week                                     AS bucket,
    count(*)                                       AS landings,
    sum(p.story_points IS NOT NULL)                AS points_sized,
    sum(p.story_points IS NULL)                    AS points_missing,
    sum(p.measured_point_tokens)                   AS measured_points_tokens_landed,
    sum(p.measured_point_lines)                    AS measured_points_lines_landed,
    sum(p.story_points IS NOT NULL
        AND p.measured_point_tokens IS NULL)       AS sized_without_measured_value,
    sum(p.story_points)                            AS labels_summed_ordinal_do_not_use_as_size,
    sum(p.lsi)                                     AS lsi_landed
FROM sf8_points p
GROUP BY p.iso_week
ORDER BY grain, bucket;

-- The window was the only state this file created.
DROP VIEW IF EXISTS sf_window;
