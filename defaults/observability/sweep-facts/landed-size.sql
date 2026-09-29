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

CREATE VIEW IF NOT EXISTS issue_landed_size AS
WITH
    -- ---------------------------------------------------------------------------
    -- Parameters — NAMED and VERSIONED (#9466: "the parameter file versioned").
    -- The fit that produces the values below is the one-time D1 backfill run,
    -- tracked in 2AMLogic/2am#1608: fit means/SDs and the per-model token
    -- factors on the baseline window, set the window bounds, bump
    -- params_version, and re-run this file. NOTHING ELSE changes.
    --
    -- v0-unfitted ships every fitted constant as NULL on purpose: an
    -- unfitted standardization must not silently report raw log-ratios as
    -- z-scores, so until the fit lands, landed_size and LSI are NULL for
    -- every row (SF2 honestly reports nothing) — the raw components are
    -- still exposed below so the fit can be checked against them.
    -- ---------------------------------------------------------------------------
    params AS (
        SELECT
            'v0-unfitted'              AS params_version,
            NULL                       AS baseline_start,   -- TEXT, RFC 3339;
            NULL                       AS baseline_end,     -- set during the fit
            -- Standardization constants for the three components, each fitted
            -- on log1p of the component over the baseline window:
            NULL                       AS mean_log_hw_lines,   -- REAL
            NULL                       AS sd_log_hw_lines,     -- REAL, > 0
            NULL                       AS mean_log_hw_files,   -- REAL
            NULL                       AS sd_log_hw_files,     -- REAL, > 0
            NULL                       AS mean_log_norm_tokens,-- REAL
            NULL                       AS sd_log_norm_tokens,  -- REAL, > 0
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
    -- appears in `models_used`. Deliberately EMPTY in v0-unfitted — an
    -- unfitted factor is NULL, never 1.0: a neutral-looking 1.0 would publish
    -- unnormalized tokens as normalized. Populated during the backfill fit
    -- (2AMLogic/2am#1608).
    model_token_factors(model, factor) AS (
        SELECT NULL AS model, NULL AS factor
        WHERE 0
    ),
    -- Measured point values per class, from the story-points experiment's
    -- bucket ratios (#9466): tokens 1 : 1.3 : 2.2 : 3.4 : 5.1 : 8.2,
    -- lines 1 : 8.5 : 21 : 47 : 82 : 197. PROVISIONAL — the two components
    -- disagree by design (that is the experiment's finding: raw Fibonacci
    -- labels are not a unit on any component), and #9434's calibration loop
    -- collapses them into one point value per class. Sums over landings must
    -- use LSI or these measured values, NEVER the raw labels. Class 21 has no
    -- measured landing yet — its point value is NULL, not an extrapolation.
    measured_points(size_class, measured_point_tokens, measured_point_lines) AS (
        SELECT '1',  1.0,   1.0
        UNION ALL SELECT '2',  1.3,   8.5
        UNION ALL SELECT '3',  2.2,  21.0
        UNION ALL SELECT '5',  3.4,  47.0
        UNION ALL SELECT '8',  5.1,  82.0
        UNION ALL SELECT '13', 8.2, 197.0
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
    -- every formula: absent input, absent factor, or an unfitted param leaves
    -- the component NULL; division by an sd of 0 also yields NULL in SQLite,
    -- so a botched fit fails open into absence instead of infinity. The
    -- transform is log1p, which SQLite's math set does not carry — it is
    -- spelled ln(1.0 + x) here, identical for the integer counts ≥ 0 these
    -- components are, so the fit's log1p convention holds without a wrapper.
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
            (ln(1.0 + (l.tokens_in + l.tokens_out)
                       / (SELECT mf.factor FROM model_token_factors mf
                          WHERE mf.model = l.model))
                 - p.mean_log_norm_tokens) / p.sd_log_norm_tokens AS z_tokens
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
