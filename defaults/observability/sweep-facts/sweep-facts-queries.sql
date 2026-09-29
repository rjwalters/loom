-- Sweep facts: the canonical question set SF1..SF7 (Issues #9446, #9466).
--
-- D1/SQLite dialect (JSON1 + window functions). Every question, its exact
-- definition, and what this set deliberately cannot answer are in
-- `sweep-facts-questions.md`. Do not read a number out of here without it —
-- especially "absent vs. zero". Run `sweep-facts-rollup.sql` first; SF1/SF5
-- also read the `issue_effort` view (`issue-effort.sql`) and SF2 reads
-- `issue_landed_size` (`landed-size.sql`, whose `params_version` names the
-- parameter set any LSI figure was computed under).
--
-- Windows are BOUND, never edited into a query. D1 has no client-side bind
-- parameters for a committed file, so the window lives in the `sf_window`
-- view below — the ONE place in this file a date literal may appear. Edit it
-- there, run the whole file:
--
--   wrangler d1 execute loom-fleet-telemetry --file sweep-facts-queries.sql
--
-- All seven answers come back in one pass, in SF order.

-- The window every SF query reads. Edit HERE, once.
CREATE VIEW IF NOT EXISTS sf_window AS
SELECT '2026-09-22T00:00:00Z' AS since,
       '2026-09-29T00:00:00Z' AS until;

-- SF1. Per issue: what did the whole lifecycle cost versus the clean landing,
-- and can the lifecycle sum be trusted? `token_completeness = 0` means at
-- least one attempt's tokens are merely unknown (#9440) — the lifecycle sums
-- of such a row are lower bounds, and the clean column is the only comparable
-- one. NULL cells are missing data, never measured zeros.
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
    e.rework_environmental   AS rework_environmental
FROM issue_effort e, sf_window w
WHERE e.landed_at >= w.since AND e.landed_at < w.until
ORDER BY e.lifecycle_tokens_in DESC, e.repo, e.issue;

-- SF2. How much work lands per day: Σ LSI over the day's landings — the
-- size-weighted throughput KPI (#9466, consumed by #9433). Labels are never
-- summed (a "13" is not thirteen "1"s — see the measured bucket ratios in the
-- questions doc); the two provisional measured-point columns are the
-- experiment's alternative until #9434 collapses them into one point value.
-- `without_token_component` counts landings whose LSI used two components, so
-- a day that mixes definitions is visible. All LSI values are NULL under an
-- unfitted parameter set (`params_version = 'v0-unfitted'`) — fit first.
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

-- The window was the only state this file created.
DROP VIEW IF EXISTS sf_window;
