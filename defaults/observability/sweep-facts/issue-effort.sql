-- Issue effort: one row per (repo, issue) landing, with BOTH effort views
-- side by side (Issue #9446):
--
--   lifecycle — every attempt of the issue: attempts, Σ wall, Σ tokens, and a
--               completeness flag saying whether any attempt's tokens are
--               merely unknown;
--   clean     — the landing sweep only, through the first judge;
--   rework    — split into substantive vs. environmental (#9444), as event
--               counts AND (#9507) as a seconds partition of the lifecycle.
--
-- Definitions live in `sweep-facts-questions.md`; run
-- `sweep-facts-rollup.sql` first (this view reads `sweep_facts`, nothing
-- raw). D1/SQLite dialect (JSON1).
--
-- Landing rule. The canonical signal is `disposition = 'landed'` (#9441). For
-- rows written before #9441 — no disposition at all — the documented fallback
-- is `result = 'success' AND pr_number IS NOT NULL`: #9441 measured that only
-- 337 of 8,808 successes carried a PR, so the pair is the closest
-- pre-disposition approximation of "this sweep landed work". The fallback is
-- only reachable when `disposition` IS NULL, so a post-#9441 record is never
-- silently re-classified by it.
--
-- Multi-landing issues. "One row per repo#issue per landing": an issue that
-- landed twice yields two rows with different `landing_sweep_id`s. The
-- lifecycle columns are ISSUE-level (identical on every landing row of the
-- same issue) — dedupe by (repo, issue) before summing lifecycle columns
-- across the view. Most issues have exactly one landing, so the row count is
-- the landed-issue count.
--
-- Absent vs. zero (telemetry-schema.md): sums over all-NULL columns return
-- NULL (an issue whose attempts carry no tokens has a NULL lifecycle token
-- sum, not 0), and `clean_wall_sec` is NULL unless the landing sweep actually
-- carried a phase breakdown reaching a judge entry.
--
-- The seconds partition (#9507). `clean_sec + substantive_rework_sec +
-- environmental_rework_sec + unattributed_sec` partitions the issue's wall
-- seconds; it equals `lifecycle_wall_sec + overaccounted_sec`, and
-- `overaccounted_sec` is 0 on every sane timeline. Two levels, exactly as
-- IE1 in `../issue-effort-queries.sql` does it:
--
--   * each attempt's measured in-sweep rework (`rework_*_sec`, from the
--     rollup) is charged to that event's classification, whatever the
--     attempt's trigger;
--   * the residual — attempt total minus measured rework, clamped at 0 — is
--     charged to the attempt's `trigger` bucket. `first` is the only source of
--     `clean_sec`. `operator_redispatch`, `unknown` and a pre-#9444 record
--     with no `trigger` go to `unattributed_sec` and are NEVER folded into a
--     rework bucket: an unattributable retry counted as rework is the exact
--     error the trigger field exists to prevent.
--
-- `clean_sec` is NOT `clean_wall_sec`: the latter is the landing sweep's
-- curator→first-judge wall (#9446); the former is the partition's clean bucket
-- (every `first` attempt's residual). Read them as two different questions.
--
-- The clamp. In-sweep rework is measured on the forge's clock and the attempt
-- total on the daemon's, so a pathological record can claim more rework than
-- its attempt lasted. The residual is clamped at 0 (a negative bucket is not
-- honest) and the excess is reported as `overaccounted_sec` rather than
-- silently absorbed, so the identity above holds exactly and a reader can see
-- when a figure is over-counted.
--
-- Open events. A rework event with no `duration_sec` contributes 0 seconds and
-- is counted in `rework_*_open` instead — "unknown", never "zero".
--
-- Coverage. `attributed_attempts` counts the attempts whose trigger is
-- classifiable; `attributed_wall_pct` is the share of partitioned seconds NOT
-- in `unattributed_sec` (NULL when there are no seconds — never a divide by
-- zero). Until the #9506/#9514 trigger/rework writers roll to the fleet, every
-- historical row reads as unattributed with NULL rework columns — that is the
-- rollout boundary made visible, not a defect; no backfill is possible.
--
-- TWIN: the trigger → bucket `CASE` in `charged` below is a copy of IE1's
-- `attempt_bucket` in `../issue-effort-queries.sql` (which reads raw
-- `records`; this reads `sweep_facts`). Do not edit one without the other:
-- `loom-daemon/tests/sweep_facts_artifacts.rs`
-- (`the_bundle_buckets_triggers_exactly_as_ie1_does`) fails if they disagree.
--
-- Re-installing (#9507). A view holds no data, so this file REPLACES any
-- installed `issue_effort` rather than `CREATE VIEW IF NOT EXISTS` — which
-- would silently keep an older definition (one without `clean_sec`, say) and
-- break every query reading the new columns. Re-running the file is a no-op
-- on an up-to-date database. The view reads the #9507 `rework_*_sec` /
-- `rework_*_open` columns, so an older installed `sweep_facts` must be
-- upgraded first (`sweep-facts-migrate.sql`).

DROP VIEW IF EXISTS issue_effort;

CREATE VIEW issue_effort AS
WITH
    -- The landing sweeps. One row per landing.
    landings AS (
        SELECT f.repo          AS repo,
               f.issue         AS issue,
               f.sweep_id      AS landing_sweep_id,
               f.emitted_at    AS landed_at,
               f.pr_number     AS landing_pr_number
        FROM sweep_facts f
        WHERE f.disposition = 'landed'
           OR (f.disposition IS NULL
               AND f.result = 'success'
               AND f.pr_number IS NOT NULL)
    ),
    -- Every sweep, charged (#9507): its measured in-sweep rework by class and
    -- its clamped residual by trigger. The bucket table is IE1's, copied —
    -- see TWIN above.
    charged AS (
        SELECT f.*,
               COALESCE(f.total_duration_sec, 0)        AS total_sec,
               COALESCE(f.rework_substantive_sec, 0)    AS in_sweep_substantive_sec,
               COALESCE(f.rework_environmental_sec, 0)  AS in_sweep_environmental_sec,
               CASE f.trigger
                   WHEN 'retry_after_substantive_failure' THEN 'substantive'
                   WHEN 'doctor_after_changes_requested'  THEN 'substantive'
                   WHEN 'retry_after_env_failure'         THEN 'environmental'
                   WHEN 'merge_conflict'                  THEN 'environmental'
                   WHEN 'stale_base_rejudge'              THEN 'environmental'
                   WHEN 'ci_failure_fix'                  THEN 'environmental'
                   WHEN 'rebase_main_moved'               THEN 'environmental'
                   WHEN 'first'                           THEN 'clean'
                   ELSE 'unattributed'   -- operator_redispatch, unknown, absent
               END                                      AS attempt_bucket
        FROM sweep_facts f
    ),
    residuals AS (
        SELECT c.*,
               MAX(c.total_sec - c.in_sweep_substantive_sec
                   - c.in_sweep_environmental_sec, 0)   AS residual_sec,
               MAX(c.in_sweep_substantive_sec
                   + c.in_sweep_environmental_sec - c.total_sec, 0)
                                                        AS overaccounted_sec
        FROM charged c
    ),
    -- Lifecycle: aggregated over EVERY sweep of the issue, not just the
    -- attempts up to a given landing.
    lifecycle AS (
        SELECT f.repo          AS repo,
               f.issue         AS issue,
               count(*)        AS attempts,
               sum(f.total_duration_sec) AS lifecycle_wall_sec,
               sum(f.tokens_in)          AS lifecycle_tokens_in,
               sum(f.tokens_out)         AS lifecycle_tokens_out,
               -- 1 when every attempt's tokens_status is one of the
               -- conclusive states — measured, suspect (#9454: published as
               -- suspect, countable), or not_spawned (a true zero). An absent
               -- or `unattributable` status marks the lifecycle sum
               -- incomplete; NULL-in is not counted by the IN comparison, so
               -- a pre-#9440 issue reads incomplete, which it is.
               CASE WHEN count(*) = sum(
                        CASE WHEN f.tokens_status IN
                                  ('measured', 'suspect', 'not_spawned')
                             THEN 1 ELSE 0 END)
                    THEN 1 ELSE 0 END AS token_completeness,
               sum(f.rework_substantive)   AS rework_substantive,
               sum(f.rework_environmental) AS rework_environmental,
               sum(f.rework_substantive_open)   AS rework_substantive_open,
               sum(f.rework_environmental_open) AS rework_environmental_open,
               sum(CASE WHEN f.attempt_bucket = 'clean'
                        THEN f.residual_sec ELSE 0 END)  AS clean_sec,
               sum(f.in_sweep_substantive_sec
                   + CASE WHEN f.attempt_bucket = 'substantive'
                          THEN f.residual_sec ELSE 0 END) AS substantive_rework_sec,
               sum(f.in_sweep_environmental_sec
                   + CASE WHEN f.attempt_bucket = 'environmental'
                          THEN f.residual_sec ELSE 0 END) AS environmental_rework_sec,
               sum(CASE WHEN f.attempt_bucket = 'unattributed'
                        THEN f.residual_sec ELSE 0 END)  AS unattributed_sec,
               sum(f.overaccounted_sec)                  AS overaccounted_sec,
               sum(CASE WHEN f.attempt_bucket <> 'unattributed'
                        THEN 1 ELSE 0 END)               AS attributed_attempts
        FROM residuals f
        GROUP BY f.repo, f.issue
    ),
    -- The partition is only reported where there is a lifecycle wall to
    -- partition: an issue none of whose attempts carried
    -- `total_duration_sec` has a NULL `lifecycle_wall_sec`, and its buckets
    -- are NULL too rather than a fabricated 0.
    effort_split AS (
        SELECT c.*,
               CASE WHEN c.lifecycle_wall_sec IS NULL THEN NULL
                    ELSE c.clean_sec END                 AS p_clean_sec,
               CASE WHEN c.lifecycle_wall_sec IS NULL THEN NULL
                    ELSE c.substantive_rework_sec END    AS p_substantive_rework_sec,
               CASE WHEN c.lifecycle_wall_sec IS NULL THEN NULL
                    ELSE c.environmental_rework_sec END  AS p_environmental_rework_sec,
               CASE WHEN c.lifecycle_wall_sec IS NULL THEN NULL
                    ELSE c.unattributed_sec END          AS p_unattributed_sec,
               CASE WHEN c.lifecycle_wall_sec IS NULL THEN NULL
                    ELSE c.overaccounted_sec END         AS p_overaccounted_sec,
               round(100.0 * (c.clean_sec + c.substantive_rework_sec
                              + c.environmental_rework_sec)
                     / NULLIF(c.clean_sec + c.substantive_rework_sec
                              + c.environmental_rework_sec
                              + c.unattributed_sec, 0), 1) AS attributed_wall_pct
        FROM lifecycle c
    ),
    -- Clean: the landing sweep only. The wall stops at (and includes) the
    -- FIRST `judge` entry of `phase_durations` — curator + builder + first
    -- judge (#9446). The walk is a json_each over the array in order; a
    -- landing sweep whose breakdown never reaches a judge gets NULL, and an
    -- absent breakdown stays NULL — never a partial sum masquerading as one.
    -- Tokens cannot be cut at the judge: they are per sweep, not per phase
    -- (#9443), so the clean-token columns are the landing sweep's own totals
    -- and say so.
    clean AS (
        SELECT f.repo            AS repo,
               f.issue           AS issue,
               f.sweep_id        AS sweep_id,
               f.tokens_in       AS clean_tokens_in,
               f.tokens_out      AS clean_tokens_out,
               (SELECT sum(json_extract(je.value, '$.duration_sec'))
                  FROM json_each(f.phase_durations) je
                 WHERE CAST(je.key AS INTEGER) <= COALESCE(
                           (SELECT MIN(CAST(je2.key AS INTEGER))
                              FROM json_each(f.phase_durations) je2
                             WHERE json_extract(je2.value, '$.phase') = 'judge'),
                           -1))  AS clean_wall_sec
        FROM sweep_facts f
    )
SELECT
    l.repo                  AS repo,
    l.issue                 AS issue,
    l.landing_sweep_id      AS landing_sweep_id,
    l.landed_at             AS landed_at,
    l.landing_pr_number     AS landing_pr_number,
    c.attempts              AS attempts,
    c.lifecycle_wall_sec    AS lifecycle_wall_sec,
    c.lifecycle_tokens_in   AS lifecycle_tokens_in,
    c.lifecycle_tokens_out  AS lifecycle_tokens_out,
    c.token_completeness    AS token_completeness,
    k.clean_wall_sec        AS clean_wall_sec,
    k.clean_tokens_in       AS clean_tokens_in,
    k.clean_tokens_out      AS clean_tokens_out,
    c.rework_substantive    AS rework_substantive,
    c.rework_environmental  AS rework_environmental,
    c.rework_substantive_open    AS rework_substantive_open,
    c.rework_environmental_open  AS rework_environmental_open,
    c.p_clean_sec                AS clean_sec,
    c.p_substantive_rework_sec   AS substantive_rework_sec,
    c.p_environmental_rework_sec AS environmental_rework_sec,
    c.p_unattributed_sec         AS unattributed_sec,
    c.p_overaccounted_sec        AS overaccounted_sec,
    c.attributed_attempts        AS attributed_attempts,
    c.attributed_wall_pct        AS attributed_wall_pct
FROM landings l
JOIN effort_split c ON c.repo = l.repo AND c.issue = l.issue
JOIN clean k
  ON k.repo = l.repo AND k.issue = l.issue AND k.sweep_id = l.landing_sweep_id;
