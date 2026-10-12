-- Issue effort: one row per (repo, issue) landing, with BOTH effort views
-- side by side (Issue #9446):
--
--   lifecycle — every attempt of the issue: attempts, Σ wall, Σ tokens, and a
--               completeness flag saying whether any attempt's tokens are
--               merely unknown;
--   clean     — the landing sweep only, through the first judge;
--   rework    — split into substantive vs. environmental (#9444).
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

CREATE VIEW IF NOT EXISTS issue_effort AS
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
               sum(f.rework_environmental) AS rework_environmental
        FROM sweep_facts f
        GROUP BY f.repo, f.issue
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
    c.rework_environmental  AS rework_environmental
FROM landings l
JOIN lifecycle c ON c.repo = l.repo AND c.issue = l.issue
JOIN clean k
  ON k.repo = l.repo AND k.issue = l.issue AND k.sweep_id = l.landing_sweep_id;
