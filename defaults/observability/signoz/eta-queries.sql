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
-- by `mapContains(attributes_number, 'loom.eta.error_sec')`.
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
