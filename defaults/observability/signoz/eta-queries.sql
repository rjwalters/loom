-- ETA accuracy queries (#9289, Phase 1). Model and record schemas:
-- ../../docs/eta.md and ../../docs/telemetry-schema.md (`eta.estimate` /
-- `eta.outcome`).
--
-- Rows: every `eta.estimate` and `eta.outcome` is one log record in
-- signoz_logs. An estimate row carries `loom.eta.trigger`; an outcome row
-- carries `loom.eta.outcome`. The body is the record's JSON (an estimate's
-- body is its whole `eta-explanation/v1` explanation).
--
-- Provenance: `loom.eta.revision` is always the ESTIMATING build's full git
-- SHA, on both kinds, so every accuracy view groups by (heuristic, revision)
-- as well as by heuristic: a daemon roll mid-window shows up as two rows
-- instead of silently averaging two builds. The observing build of an
-- outcome is `loom.eta.outcome_revision`. A build whose revision or tree
-- state is `unknown` (a tarball build) still reports, with
-- `loom.eta.provenance_complete` (estimating build) or
-- `loom.eta.outcome_provenance_complete` (observing build) false; Q1-Q3 keep
-- only rows where both are true, since an unpinned result cannot be
-- attributed to a heuristic's code. Section 0 counts the excluded rows.
--
-- Scored vs counted: `abandoned` outcomes (the issue closed as not planned)
-- and outcomes of refusals carry no `loom.eta.error_sec`; they are
-- counted by section 0 and excluded from every error, coverage and loss figure
-- by `mapContains(attributes_number, 'loom.eta.error_sec')`. A `censored`
-- outcome (#10233: expired unresolved, p90 already passed) carries only
-- `loom.eta.above_p90`, so it is excluded the same way and counted by Q4.
--
-- Q4-Q7 (#10233) are the views the promotion gate's live rules mirror: late
-- surprise on the common decidable subset, stability of the predicted landing
-- instant, convergence of the interval, and a time-weighted answer rate.
--
-- Duplicates: delivery is at least once, so every section de-duplicates
-- outcomes on `loom.eta.estimate_id` (`LIMIT 1 BY`).
--
-- Parameters (ClickHouse query parameters):
--   since   DateTime lower bound on the record time
--   repo    'owner/name' to scope to one repository, '' for all

-- 0. Preflight: estimates and outcomes per heuristic and build, and how many
--    outcomes were scored vs only counted (abandoned or refused).
SELECT attributes_string['loom.eta.heuristic'] AS heuristic,
       attributes_string['loom.eta.revision'] AS revision,
       attributes_string['loom.eta.kind'] AS kind,
       countIf(mapContains(attributes_string, 'loom.eta.trigger')) AS estimates,
       countIf(mapContains(attributes_string, 'loom.eta.no_estimate_reason')) AS refusals,
       countIf(mapContains(attributes_string, 'loom.eta.outcome')) AS outcomes,
       countIf(mapContains(attributes_number, 'loom.eta.error_sec')) AS scored,
       countIf(attributes_string['loom.eta.outcome'] = 'abandoned') AS abandoned,
       countIf(attributes_bool['loom.eta.provenance_complete'] != true
               OR (mapContains(attributes_string, 'loom.eta.outcome')
                   AND attributes_bool['loom.eta.outcome_provenance_complete'] != true))
           AS incomplete_provenance
FROM signoz_logs.distributed_logs_v2
WHERE mapContains(attributes_string, 'loom.eta.estimate_id')
  AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
  AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
GROUP BY heuristic, revision, kind
ORDER BY heuristic, revision, kind;

-- Q1. MAE, 25-75 coverage and bias per heuristic, revision, kind, repo and
--     horizon bucket (the bucket of the PREDICTED p50). Coverage near 0.5 is
--     calibrated; negative median error means the heuristic runs slow
--     (actual < p50), positive that it runs fast.
SELECT heuristic, revision, kind, repo, horizon_bucket,
       count() AS scored,
       round(avg(abs_error_sec)) AS mae_sec,
       round(avg(covered), 3) AS coverage_25_75,
       round(median(error_sec)) AS median_error_sec,
       round(avg(error_sec)) AS mean_error_sec
FROM (
    SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
           attributes_string['loom.eta.heuristic'] AS heuristic,
           attributes_string['loom.eta.revision'] AS revision,
           attributes_string['loom.eta.kind'] AS kind,
           attributes_string['loom.repo'] AS repo,
           attributes_string['loom.eta.horizon_bucket'] AS horizon_bucket,
           attributes_number['loom.eta.error_sec'] AS error_sec,
           attributes_number['loom.eta.abs_error_sec'] AS abs_error_sec,
           attributes_bool['loom.eta.covered'] AS covered
    FROM signoz_logs.distributed_logs_v2
    WHERE mapContains(attributes_string, 'loom.eta.outcome')
      AND mapContains(attributes_number, 'loom.eta.error_sec')
      AND attributes_bool['loom.eta.provenance_complete'] = true
      AND attributes_bool['loom.eta.outcome_provenance_complete'] = true
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY estimate_id
)
GROUP BY heuristic, revision, kind, repo, horizon_bucket
ORDER BY heuristic, revision, kind, repo, horizon_bucket;

-- Q2. Mean pinball loss per heuristic x kind, and per build. The pinball
--     (quantile) loss over p25/p50/p75 is the proper scoring rule for a
--     quantile forecast and the metric that decides a promotion.
--
--     `rolled_up` is how many of the three dimensions ROLLUP aggregated away
--     in that row: 0 is a real (heuristic, kind, revision) group, 3 is the
--     grand total. ROLLUP fills an aggregated column with the type's DEFAULT,
--     which for these String columns is `''` — the same value
--     `attributes_string['loom.eta.heuristic']` answers for a record whose
--     heuristic attribute is missing. Without this column a subtotal row and a
--     real group of unlabelled records are indistinguishable, and ROLLUP can
--     emit two rows with the identical key `('', '', '')`: the grand total
--     (`rolled_up` = 3) and the unlabelled heuristic's own total
--     (`rolled_up` = 2). Observed, not theorised — see
--     `loom-daemon/tests/signoz_eta_queries.rs`.
SELECT heuristic, revision, kind,
       grouping(heuristic) + grouping(kind) + grouping(revision) AS rolled_up,
       count() AS scored,
       round(avg(pinball_loss_sec)) AS mean_pinball_loss_sec,
       round(avg(abs_error_sec)) AS mae_sec,
       round(avg(covered), 3) AS coverage_25_75
FROM (
    SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
           attributes_string['loom.eta.heuristic'] AS heuristic,
           attributes_string['loom.eta.revision'] AS revision,
           attributes_string['loom.eta.kind'] AS kind,
           attributes_number['loom.eta.pinball_loss_sec'] AS pinball_loss_sec,
           attributes_number['loom.eta.abs_error_sec'] AS abs_error_sec,
           attributes_bool['loom.eta.covered'] AS covered
    FROM signoz_logs.distributed_logs_v2
    WHERE mapContains(attributes_string, 'loom.eta.outcome')
      AND mapContains(attributes_number, 'loom.eta.pinball_loss_sec')
      AND attributes_bool['loom.eta.provenance_complete'] = true
      AND attributes_bool['loom.eta.outcome_provenance_complete'] = true
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY estimate_id
)
GROUP BY ROLLUP(heuristic, kind, revision)
ORDER BY heuristic, kind, revision;

-- Q3. Feature ranking: which recorded feature tracks the error. Joins each
--     scored outcome to its estimate on `loom.eta.estimate_id`, expands the
--     estimate explanation's `features` object, and correlates every numeric
--     feature with the outcome's error.
--
--     Only features that are JSON **numbers** take part.
--     `JSONExtractKeysAndValuesRaw` hands back each value's raw text, so
--     `toFloat64OrNull` answers NULL for `null` (unmeasured), for `"refactor"`
--     (a string) and — the case that matters — for `"42"`, a number recorded
--     as a string: the quotes are part of the raw text. `WHERE value IS NOT
--     NULL` then drops the key entirely instead of ranking it.
--     The typed `JSONExtractKeysAndValues(body, 'features', 'Float64')` does
--     NOT merely risk reading an unmeasured feature as zero — measured on
--     ClickHouse 25.12.5 it drops `null` and `"refactor"` outright, and
--     *coerces* `"42"` to 42 and `true` to 1. That is the real damage: two
--     features that are not numbers at all enter the ranking as constants.
--
--     `distinct_values` exists because a feature that never varied is NOT
--     scored as uncorrelated. `rankCorr` average-ranks ties, so a
--     single-valued feature over n observations comes out at exactly **0.5**,
--     which `ORDER BY abs(rank_corr) DESC` ranks above any genuine
--     correlation weaker than that; `corr` says `nan` for the same column.
--     `distinct_values` = 1 is what tells the two apart. Do not read a 0.5
--     without checking it.
--
--     One asymmetry worth knowing: the estimate sub-select carries the SAME
--     `since` bound as the outcome one, so a scored outcome whose estimate was
--     made before the window contributes to section 0, Q1 and Q2 but has no
--     features here. Widen `since` past the longest lead time you care about
--     before reading this section as complete.
SELECT heuristic, revision, kind, feature,
       count() AS n,
       uniqExact(value) AS distinct_values,
       round(rankCorr(value, error_sec), 3) AS rank_corr,
       round(corr(value, error_sec), 3) AS pearson_corr
FROM (
    SELECT o.heuristic AS heuristic, o.revision AS revision, o.kind AS kind,
           kv.1 AS feature, toFloat64OrNull(kv.2) AS value, o.error_sec AS error_sec
    FROM (
        SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
               attributes_string['loom.eta.heuristic'] AS heuristic,
               attributes_string['loom.eta.revision'] AS revision,
               attributes_string['loom.eta.kind'] AS kind,
               attributes_number['loom.eta.error_sec'] AS error_sec
        FROM signoz_logs.distributed_logs_v2
        WHERE mapContains(attributes_string, 'loom.eta.outcome')
          AND mapContains(attributes_number, 'loom.eta.error_sec')
          AND attributes_bool['loom.eta.provenance_complete'] = true
          AND attributes_bool['loom.eta.outcome_provenance_complete'] = true
          AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
          AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
        LIMIT 1 BY estimate_id
    ) AS o
    INNER JOIN (
        SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id, any(body) AS body
        FROM signoz_logs.distributed_logs_v2
        WHERE mapContains(attributes_string, 'loom.eta.trigger')
          AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
          AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
        GROUP BY estimate_id
    ) AS e ON o.estimate_id = e.estimate_id
    ARRAY JOIN JSONExtractKeysAndValuesRaw(e.body, 'features') AS kv
)
WHERE value IS NOT NULL
GROUP BY heuristic, revision, kind, feature
HAVING n >= 20
ORDER BY abs(rank_corr) DESC;

-- Q4. Late surprise (#10233): how often the actual lands after p90, per
--     heuristic, revision and kind, on the COMMON DECIDABLE SUBSET. Every
--     heuristic of a kind estimates a subject at the same instant, so
--     `(repo, issue, kind, as_of)` names one comparison. It counts here only
--     when EVERY heuristic's outcome at that instant has a decided late
--     surprise (`loom.eta.above_p90` present): a heuristic that refused, or
--     recorded no p90, removes the instant for all of them, so leaving its
--     hard cases out cannot make it look better than one that answered them.
--     `as_of` is not an attribute; it is the record time (`actual_at`) minus
--     `loom.eta.lead_sec`.
--
--     A `censored` outcome is an estimate that expired unresolved with its
--     p90 already behind it: a decided late surprise, counted here, carrying
--     no error or loss (so absent from Q1/Q2/Q6). A rate near 0.10 is
--     calibrated; `censored` says how much of the rate is still-open work.
WITH decided AS (
    SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
           attributes_string['loom.eta.heuristic'] AS heuristic,
           attributes_string['loom.eta.revision'] AS revision,
           attributes_string['loom.eta.kind'] AS kind,
           attributes_string['loom.repo'] AS repo,
           attributes_number['loom.issue'] AS issue,
           toInt64(intDiv(timestamp, 1000000000))
               - toInt64(attributes_number['loom.eta.lead_sec']) AS as_of_sec,
           attributes_string['loom.eta.outcome'] AS outcome,
           mapContains(attributes_bool, 'loom.eta.above_p90') AS has_p90,
           attributes_bool['loom.eta.above_p90'] AS above_p90
    FROM signoz_logs.distributed_logs_v2
    WHERE mapContains(attributes_string, 'loom.eta.outcome')
      AND attributes_bool['loom.eta.provenance_complete'] = true
      AND attributes_bool['loom.eta.outcome_provenance_complete'] = true
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY estimate_id
)
SELECT heuristic, revision, kind,
       count() AS decided,
       countIf(outcome = 'censored') AS censored,
       round(avg(above_p90), 3) AS late_surprise_rate
FROM decided
WHERE has_p90
  AND (repo, issue, kind, as_of_sec) IN (
      SELECT repo, issue, kind, as_of_sec
      FROM decided
      GROUP BY repo, issue, kind, as_of_sec
      HAVING min(has_p90) = 1
  )
GROUP BY heuristic, revision, kind
ORDER BY heuristic, revision, kind;

-- Q5. Stability (#10233): how far the predicted landing INSTANT moves
--     between consecutive emissions of one series while nothing happened.
--     An `eta.estimate` row predicts landing at `as_of + p50`. A step from
--     one emission to the next of the same (repo, issue, kind, heuristic,
--     revision) series counts when both answered and the stage and the rework
--     count are unchanged — so the move is drift, not news. Measured on the
--     instant, not on remaining seconds: a perfectly steady ETA loses one
--     second of remaining time per second, which a remaining-seconds view
--     would report as movement. Diagnostic only; not a promotion gate.
SELECT heuristic, revision, kind,
       count() AS steps,
       round(median(shift_sec)) AS median_shift_sec,
       max(shift_sec) AS max_shift_sec
FROM (
    SELECT heuristic, revision, kind, abs(landing - prev_landing) AS shift_sec,
           seq, answered, prev_answered, stage, prev_stage, rework, prev_rework
    FROM (
        SELECT heuristic, revision, kind, landing, answered, stage, rework,
               row_number() OVER w AS seq,
               lagInFrame(landing) OVER w AS prev_landing,
               lagInFrame(answered) OVER w AS prev_answered,
               lagInFrame(stage) OVER w AS prev_stage,
               lagInFrame(rework) OVER w AS prev_rework
        FROM (
            SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
                   attributes_string['loom.repo'] AS repo,
                   attributes_number['loom.issue'] AS issue,
                   attributes_string['loom.eta.heuristic'] AS heuristic,
                   attributes_string['loom.eta.revision'] AS revision,
                   attributes_string['loom.eta.kind'] AS kind,
                   attributes_string['loom.eta.stage'] AS stage,
                   JSONExtractUInt(body, 'current_stage', 'rework_rounds') AS rework,
                   mapContains(attributes_number, 'loom.eta.p50_sec') AS answered,
                   toInt64(intDiv(timestamp, 1000000000)) AS as_of_sec,
                   as_of_sec + toInt64(attributes_number['loom.eta.p50_sec']) AS landing
            FROM signoz_logs.distributed_logs_v2
            WHERE mapContains(attributes_string, 'loom.eta.trigger')
              AND attributes_bool['loom.eta.provenance_complete'] = true
              AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
              AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
            LIMIT 1 BY estimate_id
        )
        WINDOW w AS (PARTITION BY repo, issue, kind, heuristic, revision ORDER BY as_of_sec
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
    )
)
WHERE seq > 1 AND answered AND prev_answered AND stage = prev_stage AND rework = prev_rework
GROUP BY heuristic, revision, kind
ORDER BY heuristic, revision, kind;

-- Q6. Convergence (#10233): interval width against the time that actually
--     remained. Per heuristic, revision, kind and bucket of the ACTUAL lead
--     (`loom.eta.lead_sec`, bucketed like `horizon_bucket`), the median
--     p25-p75 and p25-p90 widths of scored outcomes. A converging heuristic
--     narrows as the event nears; one whose width is the same at every lead
--     is not learning from the stage it is in. Only scored outcomes
--     (`loom.eta.error_sec` present): a censored outcome's lead is only a
--     lower bound and an abandonment's is no lead at all. `with_p90` counts
--     the rows the p25-p90 width is over (an estimate from before #10211
--     has none). Diagnostic only; not a promotion gate.
SELECT heuristic, revision, kind, lead_bucket,
       count() AS scored,
       round(median(p75 - p25)) AS median_p25_p75_sec,
       countIf(has_p90) AS with_p90,
       round(medianIf(p90 - p25, has_p90)) AS median_p25_p90_sec
FROM (
    SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
           attributes_string['loom.eta.heuristic'] AS heuristic,
           attributes_string['loom.eta.revision'] AS revision,
           attributes_string['loom.eta.kind'] AS kind,
           multiIf(attributes_number['loom.eta.lead_sec'] < 900, 'lt_15m',
                   attributes_number['loom.eta.lead_sec'] < 3600, '15m_1h',
                   attributes_number['loom.eta.lead_sec'] < 14400, '1h_4h',
                   attributes_number['loom.eta.lead_sec'] < 86400, '4h_24h',
                   'gt_24h') AS lead_bucket,
           attributes_number['loom.eta.p25_sec'] AS p25,
           attributes_number['loom.eta.p75_sec'] AS p75,
           attributes_number['loom.eta.p90_sec'] AS p90,
           mapContains(attributes_number, 'loom.eta.p90_sec') AS has_p90
    FROM signoz_logs.distributed_logs_v2
    WHERE mapContains(attributes_string, 'loom.eta.outcome')
      AND mapContains(attributes_number, 'loom.eta.error_sec')
      AND attributes_bool['loom.eta.provenance_complete'] = true
      AND attributes_bool['loom.eta.outcome_provenance_complete'] = true
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY estimate_id
)
GROUP BY heuristic, revision, kind, lead_bucket
ORDER BY heuristic, revision, kind, lead_bucket;

-- Q7. Answer rate (#10233), time-weighted. A refusal is emitted once, when
--     its reason first appears, and never refreshed; an answer is refreshed
--     every few minutes. "Answered rows / all rows" therefore overstates how
--     often a heuristic answers by however many refreshes its answers earn.
--     Here each emitted state stands until the series' next record — its
--     next emission or its outcome — and is weighted by how long it stood
--     (`stood_sec`). A series' last state with nothing after it in the
--     window is left out: how long it will stand is not known yet.
--     `answer_rate` = `answered_sec` / `total_sec`; compare heuristics within
--     one kind. The daemon's promotion gate counts the same thing once per
--     tracker pass instead (eta.md, "Adding a v2, and comparing it").
SELECT heuristic, revision, kind,
       count() AS states,
       sum(stood_sec) AS total_sec,
       sumIf(stood_sec, answered = 1) AS answered_sec,
       round(sumIf(stood_sec, answered = 1) / sum(stood_sec), 3) AS answer_rate
FROM (
    SELECT heuristic, revision, kind, answered, is_estimate, seq, n,
           next_t - t AS stood_sec
    FROM (
        SELECT heuristic, revision, kind, answered, is_estimate, t,
               leadInFrame(t) OVER w AS next_t,
               row_number() OVER w AS seq,
               count() OVER w AS n
        FROM (
            SELECT * FROM (
                SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
                       attributes_string['loom.repo'] AS repo,
                       attributes_number['loom.issue'] AS issue,
                       attributes_string['loom.eta.heuristic'] AS heuristic,
                       attributes_string['loom.eta.revision'] AS revision,
                       attributes_string['loom.eta.kind'] AS kind,
                       toUInt8(mapContains(attributes_number, 'loom.eta.p50_sec')) AS answered,
                       toUInt8(1) AS is_estimate,
                       toInt64(intDiv(timestamp, 1000000000)) AS t
                FROM signoz_logs.distributed_logs_v2
                WHERE mapContains(attributes_string, 'loom.eta.trigger')
                  AND attributes_bool['loom.eta.provenance_complete'] = true
                  AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
                  AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
                LIMIT 1 BY estimate_id
            )
            UNION ALL
            SELECT * FROM (
                SELECT attributes_string['loom.eta.estimate_id'] AS estimate_id,
                       attributes_string['loom.repo'] AS repo,
                       attributes_number['loom.issue'] AS issue,
                       attributes_string['loom.eta.heuristic'] AS heuristic,
                       attributes_string['loom.eta.revision'] AS revision,
                       attributes_string['loom.eta.kind'] AS kind,
                       toUInt8(0) AS answered,
                       toUInt8(0) AS is_estimate,
                       toInt64(intDiv(timestamp, 1000000000)) AS t
                FROM signoz_logs.distributed_logs_v2
                WHERE mapContains(attributes_string, 'loom.eta.outcome')
                  AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
                  AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
                LIMIT 1 BY estimate_id
            )
        )
        WINDOW w AS (PARTITION BY repo, issue, kind, heuristic ORDER BY t, is_estimate
                     ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)
    )
)
WHERE is_estimate = 1 AND seq < n
GROUP BY heuristic, revision, kind
ORDER BY heuristic, revision, kind;

-- QA. One estimate, stage by stage (#10957): what the estimate forecast for
--     each stage against what happened, and how much of the outcome's error
--     the stage carries. Pass the estimate id as the `estimate_id` query
--     parameter (a section that is run with `''` answers no rows).
--
--     Source: the `eta.outcome` body's `attribution.stages` (#10929; each
--     stage's predicted and actual entry and dwell, and `contribution_sec` =
--     actual - predicted dwell) LEFT JOINed to the `eta.estimate` body's
--     `stage_predictions` for the interval the estimate gave (`dwell_p90_sec`,
--     `reach_pct`). A stage the estimate did not forecast (visited anyway) has
--     NULL predictions; a stage forecast but never visited has
--     `actual_dwell_sec` 0. `unattributed_sec` is the same on every row: the
--     error no stage explains, so `sum(contribution_sec) + unattributed_sec`
--     is the outcome's `error_sec`. Delivery is at least once, so each side
--     takes one row (`LIMIT 1 BY`) and the stages are in path order.
--     No `since` bound: a lookup by id must not miss an old estimate.
SELECT o.stage AS stage,
       o.predicted_entry_sec AS predicted_entry_sec,
       o.actual_entry_sec AS actual_entry_sec,
       o.predicted_dwell_sec AS predicted_dwell_sec,
       o.actual_dwell_sec AS actual_dwell_sec,
       o.contribution_sec AS contribution_sec,
       o.unattributed_sec AS unattributed_sec,
       if(e.pred = '', NULL, JSONExtractInt(e.pred, 'dwell_p90')) AS predicted_dwell_p90_sec,
       if(e.pred = '', NULL, JSONExtractInt(e.pred, 'reach_pct')) AS reach_pct
FROM (
    SELECT kv.1 AS stage,
           JSONExtract(kv.2, 'predicted_entry_sec', 'Nullable(Int64)') AS predicted_entry_sec,
           JSONExtract(kv.2, 'actual_entry_sec', 'Nullable(Int64)') AS actual_entry_sec,
           JSONExtractInt(kv.2, 'predicted_dwell_sec') AS predicted_dwell_sec,
           JSONExtractInt(kv.2, 'actual_dwell_sec') AS actual_dwell_sec,
           JSONExtractInt(kv.2, 'contribution_sec') AS contribution_sec,
           JSONExtractInt(body, 'attribution', 'unattributed_sec') AS unattributed_sec
    FROM (
        SELECT body
        FROM signoz_logs.distributed_logs_v2
        WHERE mapContains(attributes_string, 'loom.eta.outcome')
          AND attributes_string['loom.eta.estimate_id'] = {estimate_id:String}
        LIMIT 1
    )
    ARRAY JOIN JSONExtractKeysAndValuesRaw(body, 'attribution', 'stages') AS kv
) AS o
LEFT JOIN (
    SELECT kv.1 AS stage, kv.2 AS pred
    FROM (
        SELECT body
        FROM signoz_logs.distributed_logs_v2
        WHERE mapContains(attributes_string, 'loom.eta.trigger')
          AND attributes_string['loom.eta.estimate_id'] = {estimate_id:String}
        LIMIT 1
    )
    ARRAY JOIN JSONExtractKeysAndValuesRaw(body, 'stage_predictions') AS kv
) AS e ON o.stage = e.stage
ORDER BY indexOf(['ready_wait', 'sweep.curator', 'sweep.builder', 'review_wait',
                  'doctor', 'merge_wait', 'merge_hold'], o.stage), o.stage;

-- QB. Per-heuristic, per-stage error bias (#10957): where each `land`
--     heuristic is systematically early or late. Source: the nightly
--     `eta.stage_attribution` rollup (one record per heuristic x stage, plus a
--     `stage` = 'unattributed' row, per UTC day, over the trailing
--     `window_days` days; chosen over recomputing from `eta.outcome` bodies so
--     the figure is the exact one the authority host folded, point-in-time).
--     `bias_sec` > 0 means the stage ran longer than forecast; `mean_abs_sec`
--     is the typical miss; `dominant_share` the share of that window's
--     outcomes whose largest miss was this stage. NULL (not 0) when `n` = 0.
--     Each day's window overlaps the previous six, so read the newest `day`
--     per heuristic, or plot one stage over `day`; do not sum days.
--     De-duplicated on the stable `row_id` (delivery is at least once). Not
--     scoped by `repo`: the rollup is fleet-wide, and the `repo` parameter is
--     not used here.
SELECT day, heuristic, stage, n, bias_sec, mean_abs_sec, dominant_share, window_days
FROM (
    SELECT attributes_string['loom.eta.stage_attribution.day'] AS day,
           attributes_string['loom.eta.stage_attribution.heuristic'] AS heuristic,
           attributes_string['loom.eta.stage_attribution.stage'] AS stage,
           attributes_number['loom.eta.stage_attribution.n'] AS n,
           if(mapContains(attributes_number, 'loom.eta.stage_attribution.bias_sec'),
              attributes_number['loom.eta.stage_attribution.bias_sec'], NULL) AS bias_sec,
           if(mapContains(attributes_number, 'loom.eta.stage_attribution.mean_abs_sec'),
              attributes_number['loom.eta.stage_attribution.mean_abs_sec'], NULL) AS mean_abs_sec,
           if(mapContains(attributes_number, 'loom.eta.stage_attribution.dominant_share'),
              attributes_number['loom.eta.stage_attribution.dominant_share'], NULL) AS dominant_share,
           attributes_number['loom.eta.stage_attribution.window_days'] AS window_days,
           attributes_string['loom.eta.stage_attribution.row_id'] AS row_id
    FROM signoz_logs.distributed_logs_v2
    WHERE mapContains(attributes_string, 'loom.eta.stage_attribution.row_id')
      AND attributes_bool['loom.eta.provenance_complete'] = true
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
    LIMIT 1 BY row_id
)
ORDER BY day DESC, heuristic, indexOf(['ready_wait', 'sweep.curator', 'sweep.builder',
                                       'review_wait', 'doctor', 'merge_wait', 'merge_hold',
                                       'unattributed'], stage);
