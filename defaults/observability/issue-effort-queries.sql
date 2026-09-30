-- Per-issue effort, split into clean / substantive-rework / environmental-rework.
-- The canonical question set IE1..IE5 (Issue #9444).
--
-- TARGET: the fleet telemetry **D1** store (`loom-fleet-telemetry`), i.e. the
-- `records` table from `dashboard/migrations/0001_init.sql` — one row per
-- accepted telemetry record, with the record itself in `payload` as JSON.
-- SQLite dialect, `json_extract` / `json_each`. This is deliberately NOT the
-- ClickHouse `loom_analytics.*` rollup the CT queries read, and deliberately
-- NOT #9446's `sweep_facts` rollup (`defaults/observability/sweep-facts/`):
-- that rollup flattens `rework_events` into two per-classification COUNTS and
-- discards each event's `kind` and `duration_sec`, so every duration-level
-- question below (IE1, IE2, IE4) is unanswerable from it. The lineage fields
-- these need live on the raw `records` payload, so that is what is read here;
-- the two layers are complementary — `sweep_facts` answers "how many", this
-- file answers "how much, and which failure mode".
--
-- WHY THIS EXISTS. An issue's cost is not one number. Some of it is the work;
-- some of it is a reviewer finding the work wanting (so the issue is HARD);
-- and some of it is main moving, a PR conflicting, an approval going stale, CI
-- flaking or a spawn dying (which says nothing about how hard the issue is).
-- Folding the last two together makes a flaky week read as a hard one. The
-- classification table these queries apply is normative in
-- `defaults/docs/telemetry-schema.md` § "Attempt lineage, rework events, and
-- landing size"; it is restated here ONLY as the two `CASE` expressions in
-- `attempt` below, so the schema doc, `loom-daemon/src/telemetry/mod.rs`'s
-- `pub mod trigger` constants,
-- `loom-daemon/src/sweep_registry/outcome_journal/rework.rs`'s
-- `default_classification` table and this file cannot drift apart without
-- `loom-daemon/tests/issue_effort_artifacts.rs` failing.
--
-- These statements are EXECUTED in CI, not merely text-checked:
-- `loom-daemon/tests/issue_effort_sqlite.rs` runs this file verbatim on the
-- bundled SQLite, against the `records` DDL lifted out of
-- `dashboard/migrations/0001_init.sql`, and asserts IE1's split plus the
-- partition property (clean + substantive + environmental + unattributed ==
-- the summed attempt durations). Edit a query here and that test is what tells
-- you whether it still answers.
--
-- WINDOWS ARE BOUND, never edited in. Two runners that actually bind:
--
--   sqlite3 ./local-copy.db \
--     -cmd ".param set :since '2026-09-01T00:00:00Z'" \
--     -cmd ".param set :until '2026-10-01T00:00:00Z'" \
--     -cmd ".param set :top_n 20" \
--     ".read issue-effort-queries.sql"
--
--   the D1 HTTP API's `params` array (see dashboard/docs/query-api.md),
--   one statement per request, with :since/:until/:top_n supplied as binds.
--
-- Both details in that first recipe are load-bearing, and both were wrong in
-- this file's first draft until `loom-daemon/tests/issue_effort_sqlite.rs`
-- executed it:
--
--   * `sqlite3` runs every `-cmd` option BEFORE its positional arguments, so
--     the `.read` must be the POSITIONAL argument and the `.param set` lines
--     must be `-cmd`. Reversed, the file runs with nothing bound and every
--     query below returns zero rows against a populated table.
--   * `.param set` evaluates its value as SQL, where "..." is an IDENTIFIER
--     quote. The timestamps need SINGLE quotes or they bind as NULL.
--
-- Either mistake answers "this fleet has no rework" from a store full of it.
--
-- READ IE5 FIRST. Every number below is a share of the attempts this
-- classification can attribute. `operator_redispatch` and `unknown` triggers,
-- and every pre-#9444 record (no `trigger` key at all), land in
-- `unattributed_sec` and are never silently folded into a rework bucket. If
-- IE5 says coverage is low for your window, IE1..IE4 are describing a
-- minority of the cost.

-- ---------------------------------------------------------------------------
-- IE1. The headline split: per-issue effort as clean / substantive rework /
-- environmental rework, for any window.
--
-- Two levels, because rework happens both BETWEEN attempts and INSIDE one:
--
--   * every `rework_events[]` entry's own measured `duration_sec` is carved
--     out of its attempt's total and charged to that event's classification;
--   * the residual — the attempt minus its measured in-sweep rework — is
--     charged by the attempt's own `trigger`.
--
-- An attempt with `trigger = 'first'` contributes its residual to `clean_sec`;
-- that is the only source of clean time, which is why a first attempt riddled
-- with conflicts still reports most of itself as environmental.
-- ---------------------------------------------------------------------------
WITH attempt AS (
    SELECT
        r.repo                                                        AS repo,
        r.issue                                                       AS issue,
        r.sweep_id                                                    AS sweep_id,
        r.emitted_at                                                  AS finished_at,
        json_extract(r.payload, '$.trigger')                          AS trigger,
        json_extract(r.payload, '$.attempt_index')                    AS attempt_index,
        json_extract(r.payload, '$.disposition')                      AS disposition,
        json_extract(r.payload, '$.pr_number')                        AS pr_number,
        COALESCE(json_extract(r.payload, '$.total_duration_sec'), 0)  AS total_sec,
        -- Measured in-sweep rework, by class. An event whose clearing forge
        -- event was never observed carries no `duration_sec` and contributes
        -- 0 here on purpose -- it is counted in `open_rework_events` instead,
        -- never smoothed to a fabricated figure.
        (SELECT COALESCE(SUM(COALESCE(json_extract(e.value, '$.duration_sec'), 0)), 0)
           FROM json_each(COALESCE(json_extract(r.payload, '$.rework_events'), '[]')) e
          WHERE json_extract(e.value, '$.classification') = 'substantive')
                                                                      AS in_sweep_substantive_sec,
        (SELECT COALESCE(SUM(COALESCE(json_extract(e.value, '$.duration_sec'), 0)), 0)
           FROM json_each(COALESCE(json_extract(r.payload, '$.rework_events'), '[]')) e
          WHERE json_extract(e.value, '$.classification') = 'environmental')
                                                                      AS in_sweep_environmental_sec,
        (SELECT COUNT(*)
           FROM json_each(COALESCE(json_extract(r.payload, '$.rework_events'), '[]')) e)
                                                                      AS rework_events,
        (SELECT COUNT(*)
           FROM json_each(COALESCE(json_extract(r.payload, '$.rework_events'), '[]')) e
          WHERE json_extract(e.value, '$.duration_sec') IS NULL)      AS open_rework_events,
        -- The attempt-level half of the classification table. NULL trigger =
        -- a pre-#9444 record: unattributed, never guessed at.
        CASE json_extract(r.payload, '$.trigger')
            WHEN 'retry_after_substantive_failure' THEN 'substantive'
            WHEN 'doctor_after_changes_requested'  THEN 'substantive'
            WHEN 'retry_after_env_failure'         THEN 'environmental'
            WHEN 'merge_conflict'                  THEN 'environmental'
            WHEN 'stale_base_rejudge'              THEN 'environmental'
            WHEN 'ci_failure_fix'                  THEN 'environmental'
            WHEN 'rebase_main_moved'               THEN 'environmental'
            WHEN 'first'                           THEN 'clean'
            ELSE 'unattributed'   -- operator_redispatch, unknown, absent
        END                                                           AS attempt_bucket
      FROM records r
     WHERE r.kind = 'sweep.outcome'
       AND r.emitted_at >= :since
       AND r.emitted_at <  :until
),
charged AS (
    SELECT
        a.*,
        -- The residual cannot go negative: in-sweep rework is measured on the
        -- forge's clock and the attempt total on the daemon's, so a pathological
        -- timeline could over-account. Clamping is honest; a negative bucket is
        -- not.
        MAX(a.total_sec - a.in_sweep_substantive_sec - a.in_sweep_environmental_sec, 0)
            AS residual_sec
      FROM attempt a
)
SELECT
    repo,
    issue,
    COUNT(*)                                             AS attempts,
    MAX(COALESCE(attempt_index, 0))                      AS max_attempt_index,
    SUM(total_sec)                                       AS lifecycle_sec,
    SUM(CASE WHEN attempt_bucket = 'clean'
             THEN residual_sec ELSE 0 END)               AS clean_sec,
    SUM(in_sweep_substantive_sec
        + CASE WHEN attempt_bucket = 'substantive'
               THEN residual_sec ELSE 0 END)             AS substantive_rework_sec,
    SUM(in_sweep_environmental_sec
        + CASE WHEN attempt_bucket = 'environmental'
               THEN residual_sec ELSE 0 END)             AS environmental_rework_sec,
    SUM(CASE WHEN attempt_bucket = 'unattributed'
             THEN residual_sec ELSE 0 END)               AS unattributed_sec,
    SUM(rework_events)                                   AS rework_events,
    SUM(open_rework_events)                              AS open_rework_events,
    SUM(CASE WHEN disposition = 'landed' THEN 1 ELSE 0 END) AS landed_attempts
  FROM charged
 GROUP BY repo, issue
 ORDER BY lifecycle_sec DESC, repo, issue
 LIMIT :top_n;

-- ---------------------------------------------------------------------------
-- IE2. The issues whose cost was mostly the ENVIRONMENT, not the work.
--
-- This is the list an operator acts on: an issue at the top here was not hard,
-- it was fought. Ordered by absolute environmental seconds rather than by
-- share, so a 90%-environmental five-minute issue does not outrank a
-- 40%-environmental eight-hour one.
-- ---------------------------------------------------------------------------
WITH attempt AS (
    SELECT
        r.repo                                                        AS repo,
        r.issue                                                       AS issue,
        COALESCE(json_extract(r.payload, '$.total_duration_sec'), 0)  AS total_sec,
        (SELECT COALESCE(SUM(COALESCE(json_extract(e.value, '$.duration_sec'), 0)), 0)
           FROM json_each(COALESCE(json_extract(r.payload, '$.rework_events'), '[]')) e
          WHERE json_extract(e.value, '$.classification') = 'environmental')
                                                                      AS in_sweep_environmental_sec,
        CASE json_extract(r.payload, '$.trigger')
            WHEN 'retry_after_env_failure' THEN 1
            WHEN 'merge_conflict'          THEN 1
            WHEN 'stale_base_rejudge'      THEN 1
            WHEN 'ci_failure_fix'          THEN 1
            WHEN 'rebase_main_moved'       THEN 1
            ELSE 0
        END                                                           AS env_attempt
      FROM records r
     WHERE r.kind = 'sweep.outcome'
       AND r.emitted_at >= :since
       AND r.emitted_at <  :until
)
SELECT
    repo,
    issue,
    COUNT(*)                                        AS attempts,
    SUM(env_attempt)                                AS environmental_attempts,
    SUM(total_sec)                                  AS lifecycle_sec,
    SUM(in_sweep_environmental_sec
        + CASE WHEN env_attempt = 1 THEN total_sec ELSE 0 END)
                                                    AS environmental_sec,
    ROUND(100.0 * SUM(in_sweep_environmental_sec
                      + CASE WHEN env_attempt = 1 THEN total_sec ELSE 0 END)
          / NULLIF(SUM(total_sec), 0), 1)           AS environmental_pct
  FROM attempt
 GROUP BY repo, issue
HAVING environmental_sec > 0
 ORDER BY environmental_sec DESC, repo, issue
 LIMIT :top_n;

-- ---------------------------------------------------------------------------
-- IE3. How many attempts issues actually take, and what triggered the retries.
--
-- The distribution behind the 7,099 same-issue sweep pairs that landed within
-- five minutes of each other with nothing saying why.
-- ---------------------------------------------------------------------------
SELECT
    COALESCE(json_extract(payload, '$.trigger'), '(absent: pre-#9444)') AS trigger,
    COUNT(*)                                                            AS attempts,
    COUNT(DISTINCT repo || '#' || issue)                                AS issues,
    SUM(CASE WHEN json_extract(payload, '$.previous_sweep_id') IS NOT NULL
             THEN 1 ELSE 0 END)                                         AS with_predecessor,
    ROUND(AVG(COALESCE(json_extract(payload, '$.attempt_index'), 0)), 2) AS avg_attempt_index,
    MAX(COALESCE(json_extract(payload, '$.attempt_index'), 0))          AS max_attempt_index,
    SUM(COALESCE(json_extract(payload, '$.total_duration_sec'), 0))     AS total_sec
  FROM records
 WHERE kind = 'sweep.outcome'
   AND emitted_at >= :since
   AND emitted_at <  :until
 GROUP BY trigger
 ORDER BY total_sec DESC;

-- ---------------------------------------------------------------------------
-- IE4. In-sweep rework by kind: which specific failure mode costs the fleet
-- most, and how much of it was never bounded (`open` = the clearing forge
-- event was never observed, so the duration is unknown -- NOT zero).
--
-- `rebase` is in the vocabulary and will appear here the day its writer lands;
-- an empty row for it today means "not instrumented", not "never happens".
-- ---------------------------------------------------------------------------
SELECT
    json_extract(e.value, '$.kind')                                  AS kind,
    json_extract(e.value, '$.classification')                        AS classification,
    COUNT(*)                                                         AS events,
    COUNT(DISTINCT r.repo || '#' || r.issue)                         AS issues,
    SUM(CASE WHEN json_extract(e.value, '$.duration_sec') IS NULL
             THEN 1 ELSE 0 END)                                      AS open_events,
    SUM(COALESCE(json_extract(e.value, '$.duration_sec'), 0))        AS measured_sec,
    ROUND(AVG(json_extract(e.value, '$.duration_sec')))              AS avg_measured_sec
  FROM records r
  JOIN json_each(COALESCE(json_extract(r.payload, '$.rework_events'), '[]')) e
 WHERE r.kind = 'sweep.outcome'
   AND r.emitted_at >= :since
   AND r.emitted_at <  :until
 GROUP BY kind, classification
 ORDER BY measured_sec DESC, events DESC;

-- ---------------------------------------------------------------------------
-- IE5. Coverage -- READ THIS BEFORE IE1..IE4.
--
-- What fraction of attempts in the window this classification can actually
-- attribute. `attributed_pct` is the denominator every other number here is a
-- share of. The three unattributed populations are reported separately because
-- they need different fixes: `no_trigger` is retention/rollout (pre-#9444
-- records aging out), `operator_redispatch` is a dispatch-source plumb this
-- daemon does not have, and `unknown` is a real measurement gap to drive down.
-- ---------------------------------------------------------------------------
SELECT
    COUNT(*)                                                             AS attempts,
    SUM(CASE WHEN json_extract(payload, '$.trigger') IS NULL
             THEN 1 ELSE 0 END)                                          AS no_trigger,
    SUM(CASE WHEN json_extract(payload, '$.trigger') = 'operator_redispatch'
             THEN 1 ELSE 0 END)                                          AS operator_redispatch,
    SUM(CASE WHEN json_extract(payload, '$.trigger') = 'unknown'
             THEN 1 ELSE 0 END)                                          AS unknown_trigger,
    SUM(CASE WHEN json_extract(payload, '$.rework_events') IS NULL
             THEN 1 ELSE 0 END)                                          AS no_timeline_read,
    SUM(CASE WHEN json_extract(payload, '$.attempt_index') IS NULL
             THEN 1 ELSE 0 END)                                          AS no_attempt_index,
    ROUND(100.0 * SUM(CASE WHEN json_extract(payload, '$.trigger') IS NOT NULL
                            AND json_extract(payload, '$.trigger') NOT IN
                                ('operator_redispatch', 'unknown')
                           THEN 1 ELSE 0 END) / NULLIF(COUNT(*), 0), 1)  AS attributed_pct
  FROM records
 WHERE kind = 'sweep.outcome'
   AND emitted_at >= :since
   AND emitted_at <  :until;
