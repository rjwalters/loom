-- Landed size (LSI): the size axis of the throughput KPI (Issue #9466).
--
-- One row per landing: landed_size, the mean of the available standardized
-- components; LSI = exp(landed_size), so **1.0 is a median landing** and
-- sums/ratios of LSI are defined; the Fibonacci size class via fixed cuts; and
-- the measured point values per class.
--
--   standardize:  z(component) = (log1p(x) − mean) / sd      (params below)
--   components:   log1p(hw_lines), log1p(hw_files),
--                 log1p(tokens / model_factor)
--   landed_size:  mean of the components that are PRESENT
--   LSI:          exp(landed_size)        — 1.0 = a median landing
--   size_class:   Fibonacci label via fixed cuts on LSI
--
-- `hw_lines` is the landing's hand-written churn: hw_lines_added +
-- hw_lines_deleted (#9466's hw_* fields; the same added+deleted convention as
-- the #9430 "lines changed" measure). `tokens` is tokens_in + tokens_out
-- (#9430's primary anchor), normalized per model. Absent inputs stay absent:
-- a component with a NULL input is NULL and drops out of the mean — never 0 —
-- and a landing with no component at all has a NULL landed_size, a NULL LSI
-- and a NULL class.
--
-- D1/SQLite dialect (JSON1 + math functions); run `sweep-facts-rollup.sql`
-- first. The landing predicate mirrors `issue-effort.sql`'s `landings` CTE
-- exactly (disposition = 'landed', with the documented pre-#9441 fallback);
-- if one changes, change both.
--
-- This file defines TWO views, in this order: `measured_point_values` (the
-- per-class point table) and `issue_landed_size` (the per-landing index, which
-- reads it). Both are `CREATE VIEW IF NOT EXISTS`, so re-defining either after
-- an edit means dropping it first — `DROP VIEW issue_landed_size;
-- DROP VIEW measured_point_values;` then re-running this file.

-- ---------------------------------------------------------------------------
-- Measured point values per size class, from the story-points experiment's
-- bucket ratios (#9466): tokens 1 : 1.3 : 2.2 : 3.4 : 5.1 : 8.2, lines
-- 1 : 8.5 : 21 : 47 : 82 : 197.
--
-- This is the ONE place those numbers are written down (#9433): SF2 reaches
-- them through `issue_landed_size` below, for a landing's MEASURED class, and
-- SF8 joins this view directly on the Curator's ASSIGNED `story_points` class,
-- for the forecast side. Two copies of the ratios would be two definitions of
-- "a point", so there is one view and both sides read it.
--
-- PROVISIONAL — the two components disagree by design (that is the
-- experiment's finding: raw Fibonacci labels are not a unit on any component),
-- and #9434's calibration loop collapses them into one point value per class.
-- Sums over landings must use LSI or these measured values, NEVER the raw
-- labels. `size_class` is TEXT because it is a label, not a quantity.
-- Class 21 is deliberately absent: it has no measured landing yet, so a
-- landing in it joins to NULL rather than to an extrapolation — and it is not
-- a legal `story_points` value either (telemetry-schema.md §`story_points`),
-- so on the forecast side a non-NULL estimate that misses this table is an
-- out-of-vocabulary emitter defect, which SF8 counts rather than hides.
-- ---------------------------------------------------------------------------
CREATE VIEW IF NOT EXISTS measured_point_values
    (size_class, measured_point_tokens, measured_point_lines) AS
          SELECT '1',  1.0,   1.0
UNION ALL SELECT '2',  1.3,   8.5
UNION ALL SELECT '3',  2.2,  21.0
UNION ALL SELECT '5',  3.4,  47.0
UNION ALL SELECT '8',  5.1,  82.0
UNION ALL SELECT '13', 8.2, 197.0;

CREATE VIEW IF NOT EXISTS issue_landed_size AS
WITH
    -- ---------------------------------------------------------------------------
    -- Parameters — NAMED and VERSIONED (#9466: "the parameter file versioned").
    --
    -- v1-2026-10-02 — the first fit (#9934), produced by
    -- `fit-landed-size.mjs` beside this file on the baseline window
    -- [2026-09-29T00:00:00Z, 2026-10-02T00:00:00Z): 379 landings, of which
    -- 111 carried the #9466 hw_* components (the emitter's first four days)
    -- and 374 carried a clean token reading. The window is deliberately the
    -- component emitters' whole life to date; it is YOUNG, and the mixture
    -- is uneven (111 three-component landings against ~263 token-only ones),
    -- so the median LSI over the window sits below 1.0 — the refit is part
    -- of the artifact: rerun the script over a longer window once hw_*
    -- coverage matures, paste its `params` block here, bump params_version.
    -- The script refuses a degenerate fit (a constant component, sd = 0)
    -- rather than publishing one.
    --
    -- v0-unfitted (the initial state) shipped every constant as NULL on
    -- purpose: an unfitted standardization must not silently report raw
    -- log-ratios as z-scores, so landed_size and LSI stayed NULL for every
    -- row (SF2 honestly reported nothing) while the raw components stayed
    -- exposed for the fit to be checked against.
    -- ---------------------------------------------------------------------------
    params AS (
        SELECT
            'v1-2026-10-02'            AS params_version,
            '2026-09-29T00:00:00Z'     AS baseline_start,   -- TEXT, RFC 3339
            '2026-10-02T00:00:00Z'     AS baseline_end,     -- the fit's window
            -- Standardization constants for the three components, each fitted
            -- on log1p of the component over the baseline window (population
            -- SD; the script's header records the exact definitions):
            5.77683                    AS mean_log_hw_lines,   -- REAL
            1.965216                   AS sd_log_hw_lines,     -- REAL, > 0
            1.852509                   AS mean_log_hw_files,   -- REAL
            0.769972                   AS sd_log_hw_files,     -- REAL, > 0
            0.742732                   AS mean_log_norm_tokens,-- REAL
            0.352462                   AS sd_log_norm_tokens,  -- REAL, > 0
            -- Fibonacci size-class cuts: UPPER bounds on LSI. A median landing
            -- (LSI 1.0) sits at the first cut; the top class is open-ended at
            -- 21. Fixed numbers, chosen once here — never recomputed per
            -- query — so a class means the same thing in every window.
            1.0                        AS cut_1,
            2.0                        AS cut_2,
            3.0                        AS cut_3,
            5.0                        AS cut_4,
            8.0                        AS cut_5,
            13.0                       AS cut_6
    ),
    -- Per-model token normalization factors (#9466: tokens "differ by model,
    -- so they need per-model normalization"). One row per model id as it
    -- appears in `models_used`, fitted as the geometric mean of the model's
    -- token-eligible landings in the baseline window (`fit-landed-size.mjs`
    -- step 3); models with fewer than 5 eligible landings got NO factor —
    -- an under-fit factor must not impersonate a normalization — so their
    -- token component stays absent downstream. `<unattributed>` is the
    -- literal `models_used[0]` the fleet records when attribution failed;
    -- those sweeps' tokens are real readings, normalized by their own
    -- geometric mean like any other model's.
    model_token_factors(model, factor) AS (
        SELECT '<unattributed>',    30339879.008721
        UNION ALL SELECT 'claude-opus-5',   20869614.457459
        UNION ALL SELECT 'claude-opus-5-5',  5417277.931668
        UNION ALL SELECT 'claude-sonnet-5', 18208672.096412
        UNION ALL SELECT 'claude-sonnet-5-5', 2745672.191940
        UNION ALL SELECT 'glm-5.3',        30857811.255173
    ),
    -- Measured point values per class — read from the `measured_point_values`
    -- view above, which is the single definition of those bucket ratios
    -- (#9433). Nothing is restated here: a second copy of the ratios would be
    -- a second meaning of "a point".
    measured_points AS (
        SELECT size_class, measured_point_tokens, measured_point_lines
        FROM measured_point_values
    ),
    -- The landing sweeps — predicate mirrors `issue-effort.sql` `landings`.
    landings AS (
        SELECT f.repo          AS repo,
               f.issue         AS issue,
               f.sweep_id      AS landing_sweep_id,
               f.emitted_at    AS landed_at,
               f.hw_lines_added   AS hw_lines_added,
               f.hw_lines_deleted AS hw_lines_deleted,
               f.hw_files         AS hw_files,
               f.tokens_in        AS tokens_in,
               f.tokens_out       AS tokens_out,
               -- The token-measurement verdict (#9440/#9454): the tokens
               -- component below is fit and scored ONLY on a clean reading
               -- (`measured`, or the absent verdict of legacy rows) —
               -- `suspect`, `unattributable` and `not_spawned` are published
               -- numbers that are not a measurement, so they reach no
               -- standardization (#9934). The raw `tokens` column below the
               -- components keeps every reading: it is the fact, the flag is
               -- the KPI's honesty about it.
               f.tokens_status    AS tokens_status,
               -- The normalization lookup wants the sweep's model. `sweep_facts`
               -- carries `models_used` (sorted, deduped), not the dispatched
               -- model, so the factor lookup keys on the first entry; when a
               -- dispatched-`model` column lands on the fact table, switch this
               -- lookup to it. Absent models_used → NULL → NULL factor → the
               -- tokens component drops out (flagged below), never mis-normalizes.
               json_extract(f.models_used, '$[0]') AS model
        FROM sweep_facts f
        WHERE f.disposition = 'landed'
           OR (f.disposition IS NULL
               AND f.result = 'success'
               AND f.pr_number IS NOT NULL)
    ),
    -- The standardized components, as literal SQL. NULL propagates through
    -- every formula: absent input, absent factor, an unfitted param, or a
    -- token verdict that is not a measurement (#9440/#9454 — see the
    -- `tokens_status` column above) leaves the component NULL; division by
    -- an sd of 0 also yields NULL in SQLite, so a botched fit fails open
    -- into absence instead of infinity. The transform is log1p, which
    -- SQLite's math set does not carry — it is spelled ln(1.0 + x) here,
    -- identical for the integer counts ≥ 0 these components are, so the
    -- fit's log1p convention holds without a wrapper.
    components AS (
        SELECT
            l.repo, l.issue, l.landing_sweep_id, l.landed_at,
            p.params_version,
            l.hw_lines_added + l.hw_lines_deleted          AS hw_lines,
            l.hw_files                                     AS hw_files,
            l.tokens_in + l.tokens_out                     AS tokens,
            (ln(1.0 + l.hw_lines_added + l.hw_lines_deleted)
                 - p.mean_log_hw_lines) / p.sd_log_hw_lines  AS z_hw_lines,
            (ln(1.0 + l.hw_files)
                 - p.mean_log_hw_files) / p.sd_log_hw_files  AS z_hw_files,
            CASE
                WHEN l.tokens_status IS NULL OR l.tokens_status = 'measured'
                    THEN (ln(1.0 + (l.tokens_in + l.tokens_out)
                              / (SELECT mf.factor FROM model_token_factors mf
                                 WHERE mf.model = l.model))
                          - p.mean_log_norm_tokens) / p.sd_log_norm_tokens
            END                                            AS z_tokens
        FROM landings l, params p
    ),
    -- Mean of the AVAILABLE components: present components enter the
    -- numerator, absent ones drop out of both numerator and denominator. All
    -- three absent → 0/0 → NULL (an honest absence, not a zero). LSI =
    -- exp(landed_size): 1.0 is a median landing, so a fleet whose landings
    -- all doubled in size sums to twice the LSI it summed before.
    sizes AS (
        SELECT
            c.repo, c.issue, c.landing_sweep_id, c.landed_at,
            c.params_version, c.hw_lines, c.hw_files, c.tokens,
            c.z_hw_lines, c.z_hw_files, c.z_tokens,
            (COALESCE(c.z_hw_lines, 0) + COALESCE(c.z_hw_files, 0)
             + COALESCE(c.z_tokens, 0))
              / ((c.z_hw_lines IS NOT NULL) + (c.z_hw_files IS NOT NULL)
                 + (c.z_tokens IS NOT NULL))                AS landed_size,
            -- The tokens component is optional until #9440/#9454 land coverage
            -- and plausibility; the flag says which definition a row's
            -- landed_size used, so a sum that quietly mixes the two-component
            -- and three-component definitions is at least countable.
            (c.z_tokens IS NOT NULL)                        AS landed_size_tokens_used
        FROM components c
    )
SELECT
    s.repo                     AS repo,
    s.issue                    AS issue,
    s.landing_sweep_id         AS landing_sweep_id,
    s.landed_at                AS landed_at,
    s.params_version           AS params_version,
    s.hw_lines                 AS hw_lines,
    s.hw_files                 AS hw_files,
    s.tokens                   AS tokens,
    s.z_hw_lines               AS z_hw_lines,
    s.z_hw_files               AS z_hw_files,
    s.z_tokens                 AS z_tokens,
    s.landed_size              AS landed_size,
    exp(s.landed_size)         AS LSI,
    CASE
        WHEN s.landed_size IS NULL THEN NULL
        WHEN exp(s.landed_size) <= p.cut_1 THEN '1'
        WHEN exp(s.landed_size) <= p.cut_2 THEN '2'
        WHEN exp(s.landed_size) <= p.cut_3 THEN '3'
        WHEN exp(s.landed_size) <= p.cut_4 THEN '5'
        WHEN exp(s.landed_size) <= p.cut_5 THEN '8'
        WHEN exp(s.landed_size) <= p.cut_6 THEN '13'
        ELSE '21'
    END                        AS size_class,
    s.landed_size_tokens_used  AS landed_size_tokens_used,
    m.measured_point_tokens    AS measured_point_tokens,
    m.measured_point_lines     AS measured_point_lines
FROM sizes s, params p
LEFT JOIN measured_points m ON m.size_class = CASE
        WHEN s.landed_size IS NULL THEN NULL
        WHEN exp(s.landed_size) <= p.cut_1 THEN '1'
        WHEN exp(s.landed_size) <= p.cut_2 THEN '2'
        WHEN exp(s.landed_size) <= p.cut_3 THEN '3'
        WHEN exp(s.landed_size) <= p.cut_4 THEN '5'
        WHEN exp(s.landed_size) <= p.cut_5 THEN '8'
        WHEN exp(s.landed_size) <= p.cut_6 THEN '13'
        ELSE '21'
    END;
