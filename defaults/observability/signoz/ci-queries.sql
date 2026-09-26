-- Standing build/CI retro queries (#8826, phase 3 of the build/CI-in-SigNoz set).
--
-- These answer the standing questions over what `loom-daemon ci-telemetry`
-- (#8824 runs/jobs, #8825 job logs) delivers to SigNoz through the neutral
-- gateway: what took long and where it went slow, whether build time is
-- trending up, which job regressed and since when, and which runs failed and
-- why. Policy and pipeline: ../../docs/ci-observability.md.
--
-- Two sources, two retention horizons — read the header of each section:
--   * Sections 1-3 read the `loom.ci.{run,job}.duration_ms` histograms
--     (signoz_metrics). Metrics are kept >= 30 days, so these are the trend
--     views. Metric labels are ONLY repo/workflow/job/runner/conclusion — never
--     a run id — so a metric row can say WHICH job regressed, not which run.
--   * Sections 4-6 read the `ci.run` / `ci.job` / `ci.job.log` log records
--     (signoz_logs), which carry run_id/job_id. Logs are kept 7 days, so these
--     are the "read a recent run" views. A `since` older than 7 days returns
--     only what retention has not yet deleted.
--
-- Vocabulary is pinned to what the daemon exports and the gateway forwards:
-- `loom-daemon/tests/signoz_trial_artifacts.rs` fails if any attribute key,
-- metric label, metric name or attribute *type container* below drifts from
-- the collector's `keep_keys` or from the daemon's CI record rendering. Such a
-- query does not error — it silently returns zero rows, which is
-- indistinguishable from the backend having lost the data.
--
-- How each histogram data point lands: every `ci.duration` record is ONE
-- delta-histogram point with count 1 and sum = that run's/job's duration, so
-- one `<metric>.sum` sample is exactly one observed duration and one
-- `<metric>.count` sample is exactly one run/job.
--
-- Duplicates. Delivery is at least once. The record sections (4-6)
-- de-duplicate on the record's own run_id/job_id. The metric sections (1-3)
-- CANNOT: a metric point carries no run/job identity, and durations are whole
-- seconds, so two genuinely distinct runs of one workflow that finish in the
-- same second with the same conclusion are byte-identical samples in one
-- series. De-duplicating on (fingerprint, timestamp, value) was measured to
-- drop 2.4% of real runs on live data (evidence.md, "CI retro queries"), so
-- sections 1-3 count every stored sample — as the SigNoz UI does — and
-- section 0 instead RECONCILES metric points against distinct records, so a
-- redelivered batch (metrics > records) or a lost one is visible, not hidden.
--
-- Run every statement in one pass with the private bundled client:
--
--   docker compose --env-file /absolute/private/signoz.env \
--     -f pours/deployment/compose.yaml exec -T \
--     loom-signoz-telemetrystore-clickhouse-0-0 \
--     clickhouse-client --multiquery \
--       --param_since='2026-09-01 00:00:00' --param_repo='' \
--       --param_bucket_hours=24 --param_window_hours=168 --param_top=20 \
--     < ci-queries.sql
--
-- Parameters (all five must be bound; every one is used):
--   since         DateTime (UTC) lower bound for sections 0-1 and 3-6
--   repo          'owner/name' to scope to one repository, '' for the whole org
--   bucket_hours  trend bucket width for sections 1 and 3 (24 = daily, 168 = weekly)
--   window_hours  section 2 compares [now - w, now) against [now - 2w, now - w)
--   top           row cap for the ranked sections 2, 4 and 6

-- 0. Preflight: which CI series and record kinds actually exist. An empty
--    result here means capture is not flowing (poller disabled, exporter not
--    configured, or gateway not forwarding), NOT that CI was idle — check
--    `loom-daemon ci-telemetry status` before reading any section below.
SELECT metric_name, type, temporality, uniqExact(fingerprint) AS series
FROM signoz_metrics.time_series_v4
WHERE metric_name LIKE 'loom.ci.%'
GROUP BY metric_name, type, temporality
ORDER BY metric_name;

SELECT multiIf(body IN ('ci.run', 'ci.job'), body,
               mapContains(attributes_number, 'loom.ci.chunk_index'), 'ci.job.log',
               'other') AS kind,
       count() AS rows,
       min(fromUnixTimestamp64Nano(toInt64(timestamp))) AS first_event,
       max(fromUnixTimestamp64Nano(toInt64(timestamp))) AS last_event
FROM signoz_logs.logs_v2
WHERE mapContains(attributes_number, 'loom.ci.run_id')
  AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
  AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
GROUP BY kind
ORDER BY kind;

-- 0b. Reconciliation: metric points vs distinct records over the same window.
--     Within the 7-day record horizon the two must be equal. `metric_points`
--     above `records` means a batch was delivered twice (sections 1-3 then
--     over-count by exactly the excess); below means metric points were lost.
--     Past 7 days only the metric column remains, by retention design.
SELECT 'run' AS unit,
       (SELECT count()
        FROM signoz_metrics.samples_v4 AS s
        INNER JOIN (SELECT fingerprint, any(labels) AS labels
                    FROM signoz_metrics.time_series_v4
                    WHERE metric_name = 'loom.ci.run.duration_ms.count'
                    GROUP BY fingerprint) AS t USING (fingerprint)
        WHERE s.metric_name = 'loom.ci.run.duration_ms.count'
          AND s.unix_milli >= toInt64(toUnixTimestamp({since:DateTime})) * 1000
          AND ({repo:String} = '' OR JSONExtractString(t.labels, 'repo') = {repo:String})
       ) AS metric_points,
       (SELECT uniqExact(attributes_string['loom.repo'],
                         attributes_number['loom.ci.run_id'],
                         attributes_number['loom.ci.run_attempt'])
        FROM signoz_logs.logs_v2
        WHERE body = 'ci.run'
          AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
          AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
       ) AS records
UNION ALL
SELECT 'job',
       (SELECT count()
        FROM signoz_metrics.samples_v4 AS s
        INNER JOIN (SELECT fingerprint, any(labels) AS labels
                    FROM signoz_metrics.time_series_v4
                    WHERE metric_name = 'loom.ci.job.duration_ms.count'
                    GROUP BY fingerprint) AS t USING (fingerprint)
        WHERE s.metric_name = 'loom.ci.job.duration_ms.count'
          AND s.unix_milli >= toInt64(toUnixTimestamp({since:DateTime})) * 1000
          AND ({repo:String} = '' OR JSONExtractString(t.labels, 'repo') = {repo:String})
       ),
       (SELECT uniqExact(attributes_string['loom.repo'], attributes_number['loom.ci.job_id'])
        FROM signoz_logs.logs_v2
        WHERE body = 'ci.job'
          AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
          AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
       );

-- 1. Duration trend. P50/P95/max of `loom.ci.job.duration_ms` per
--    repo + workflow + job, per `bucket_hours` bucket. A P95 that climbs across
--    buckets while P50 holds is a tail regression (one slow runner, a flaky
--    cache); both climbing is a real slowdown. `skipped` (0 ms) and
--    `cancelled` (cut short) jobs are excluded from the duration statistics in
--    sections 1-2 — their durations measure a trigger or a cut-off, not the
--    job's work; section 3 counts outcomes (per run). Metrics-backed: 30 days.
WITH series AS (
    SELECT fingerprint,
           any(JSONExtractString(labels, 'repo')) AS repo,
           any(JSONExtractString(labels, 'workflow')) AS workflow,
           any(JSONExtractString(labels, 'job')) AS job,
           any(JSONExtractString(labels, 'conclusion')) AS conclusion
    FROM signoz_metrics.time_series_v4
    WHERE metric_name = 'loom.ci.job.duration_ms.sum'
    GROUP BY fingerprint
)
SELECT t.repo, t.workflow, t.job,
       toStartOfInterval(toDateTime(intDiv(s.unix_milli, 1000)),
                         toIntervalHour({bucket_hours:UInt32})) AS bucket,
       count() AS jobs,
       round(quantileExact(0.5)(s.value) / 1000, 1) AS p50_s,
       round(quantileExact(0.95)(s.value) / 1000, 1) AS p95_s,
       round(max(s.value) / 1000, 1) AS max_s
FROM (
    SELECT fingerprint, unix_milli, value
    FROM signoz_metrics.samples_v4
    WHERE metric_name = 'loom.ci.job.duration_ms.sum'
      AND unix_milli >= toInt64(toUnixTimestamp({since:DateTime})) * 1000
) AS s
INNER JOIN series AS t USING (fingerprint)
WHERE ({repo:String} = '' OR t.repo = {repo:String})
  AND t.conclusion NOT IN ('skipped', 'cancelled')
GROUP BY t.repo, t.workflow, t.job, bucket
ORDER BY t.repo, t.workflow, t.job, bucket;

-- 2. Regression spotlight. Per repo + workflow + job, the P95 over the current
--    window [now - window_hours, now) against the prior window of the same
--    length, ranked by absolute P95 increase. Only jobs with >= 2 observations
--    in BOTH windows are ranked: a job that ran once is noise, and a job absent
--    from one window is new or retired, not regressed. Read the "since when"
--    from section 1's buckets for the job this names. Metrics-backed.
WITH series AS (
    SELECT fingerprint,
           any(JSONExtractString(labels, 'repo')) AS repo,
           any(JSONExtractString(labels, 'workflow')) AS workflow,
           any(JSONExtractString(labels, 'job')) AS job,
           any(JSONExtractString(labels, 'conclusion')) AS conclusion
    FROM signoz_metrics.time_series_v4
    WHERE metric_name = 'loom.ci.job.duration_ms.sum'
    GROUP BY fingerprint
),
toInt64(toUnixTimestamp(now())) * 1000 AS now_ms,
toInt64({window_hours:UInt32}) * 3600 * 1000 AS window_ms
SELECT t.repo, t.workflow, t.job,
       countIf(s.unix_milli >= now_ms - window_ms) AS current_jobs,
       countIf(s.unix_milli < now_ms - window_ms) AS prior_jobs,
       round(quantileExactIf(0.95)(s.value, s.unix_milli >= now_ms - window_ms) / 1000, 1) AS current_p95_s,
       round(quantileExactIf(0.95)(s.value, s.unix_milli < now_ms - window_ms) / 1000, 1) AS prior_p95_s,
       round(current_p95_s - prior_p95_s, 1) AS delta_s,
       round(100 * (current_p95_s - prior_p95_s) / nullIf(prior_p95_s, 0), 1) AS delta_pct
FROM (
    SELECT fingerprint, unix_milli, value
    FROM signoz_metrics.samples_v4
    WHERE metric_name = 'loom.ci.job.duration_ms.sum'
      AND unix_milli >= toInt64(toUnixTimestamp(now())) * 1000
                        - 2 * toInt64({window_hours:UInt32}) * 3600 * 1000
) AS s
INNER JOIN series AS t USING (fingerprint)
WHERE ({repo:String} = '' OR t.repo = {repo:String})
  AND t.conclusion NOT IN ('skipped', 'cancelled')
GROUP BY t.repo, t.workflow, t.job
HAVING current_jobs >= 2 AND prior_jobs >= 2
ORDER BY delta_s DESC, t.repo, t.workflow, t.job
LIMIT {top:UInt32};

-- 3. Outcome mix. Runs per repo + workflow + `bucket_hours` bucket split by
--    conclusion. `cancelled` is a first-class outcome column, never folded into
--    "other" or dropped: the #7779 cancellation storm (22 of 30 main runs
--    cancelled) is exactly the signal this view exists to surface. A run whose
--    conclusion GitHub did not report has no `conclusion` label at all and is
--    counted as `unreported`, never as success. Metrics-backed (run histogram's
--    `.count` series, one sample per run): >= 30 days.
WITH series AS (
    SELECT fingerprint,
           any(JSONExtractString(labels, 'repo')) AS repo,
           any(JSONExtractString(labels, 'workflow')) AS workflow,
           any(JSONExtractString(labels, 'conclusion')) AS conclusion
    FROM signoz_metrics.time_series_v4
    WHERE metric_name = 'loom.ci.run.duration_ms.count'
    GROUP BY fingerprint
)
SELECT t.repo, t.workflow,
       toStartOfInterval(toDateTime(intDiv(s.unix_milli, 1000)),
                         toIntervalHour({bucket_hours:UInt32})) AS bucket,
       sum(s.value) AS runs,
       sumIf(s.value, t.conclusion = 'success') AS success,
       sumIf(s.value, t.conclusion = 'failure') AS failure,
       sumIf(s.value, t.conclusion = 'cancelled') AS cancelled,
       sumIf(s.value, t.conclusion = '') AS unreported,
       runs - success - failure - cancelled - unreported AS other,
       round(failure / runs, 3) AS failure_ratio,
       round(cancelled / runs, 3) AS cancelled_ratio
FROM (
    SELECT fingerprint, unix_milli, value
    FROM signoz_metrics.samples_v4
    WHERE metric_name = 'loom.ci.run.duration_ms.count'
      AND unix_milli >= toInt64(toUnixTimestamp({since:DateTime})) * 1000
) AS s
INNER JOIN series AS t USING (fingerprint)
WHERE {repo:String} = '' OR t.repo = {repo:String}
GROUP BY t.repo, t.workflow, bucket
ORDER BY t.repo, t.workflow, bucket;

-- 4. Top slow jobs now. The longest individual jobs since `since`, with the
--    run_id/job_id to open in GitHub or to paste into section 5 / Logs
--    Explorer. Logs-backed (`ci.job` records): 7 days.
SELECT attributes_string['loom.repo'] AS repo,
       attributes_string['loom.ci.workflow'] AS workflow,
       attributes_string['loom.ci.job'] AS job,
       attributes_string['loom.ci.runner'] AS runner,
       attributes_string['loom.ci.conclusion'] AS conclusion,
       toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
       toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
       toUInt32(attributes_number['loom.ci.attempts']) AS run_attempt,
       attributes_bool['loom.ci.timed_out'] AS timed_out,
       round(attributes_number['loom.ci.duration_ms'] / 1000, 1) AS duration_s,
       attributes_string['loom.ci.completed_at'] AS completed_at
FROM signoz_logs.logs_v2
WHERE body = 'ci.job'
  AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
  AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
ORDER BY duration_s DESC, job_id
LIMIT 1 BY repo, job_id
LIMIT {top:UInt32};

-- 5. Failed run -> logs. Every failed/timed-out run since `since`, each of its
--    non-successful jobs, and how much of that job's log was captured
--    (`chunks_present` of `chunk_count`; 0 of 0 means no `ci.job.log` arrived —
--    log capture disabled, the repo log-excluded, or the download still
--    pending/failed per `ci-telemetry status`, never "the log was empty").
--    `logs_explorer_filter` is the exact Logs Explorer query for that job's log;
--    order the result by `loom.ci.chunk_index` ascending to reconstruct it.
--    Logs-backed: 7 days.
WITH failed_runs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.run_attempt']) AS run_attempt,
           attributes_string['loom.ci.conclusion'] AS run_conclusion,
           attributes_string['loom.ci.event'] AS event,
           attributes_string['loom.ci.ref'] AS git_ref,
           attributes_string['loom.ci.head_sha'] AS head_sha
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.run'
      AND attributes_string['loom.ci.conclusion'] IN ('failure', 'timed_out', 'startup_failure')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, run_id, run_attempt
),
failed_jobs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.attempts']) AS run_attempt,
           toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
           attributes_string['loom.ci.job'] AS job,
           attributes_string['loom.ci.conclusion'] AS job_conclusion,
           attributes_bool['loom.ci.timed_out'] AS timed_out
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.job'
      AND attributes_string['loom.ci.conclusion'] NOT IN ('success', 'skipped', 'neutral')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
    LIMIT 1 BY repo, job_id
),
job_logs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
           uniqExact(toUInt32(attributes_number['loom.ci.chunk_index'])) AS chunks_present,
           max(toUInt32(attributes_number['loom.ci.chunk_count'])) AS chunk_count,
           max(attributes_bool['loom.ci.truncated']) AS truncated
    FROM signoz_logs.logs_v2
    WHERE mapContains(attributes_number, 'loom.ci.chunk_index')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
    GROUP BY repo, job_id
)
SELECT r.repo AS repo, r.workflow AS workflow, r.run_id AS run_id,
       r.run_attempt AS run_attempt, r.run_conclusion AS run_conclusion,
       r.event AS event, r.git_ref AS git_ref, r.head_sha AS head_sha,
       j.job AS job, j.job_id AS job_id, j.job_conclusion AS job_conclusion,
       j.timed_out AS timed_out,
       l.chunks_present AS chunks_present, l.chunk_count AS chunk_count,
       l.truncated AS truncated,
       concat('loom.ci.job_id = ', toString(j.job_id)) AS logs_explorer_filter
FROM failed_runs AS r
LEFT JOIN failed_jobs AS j
  ON j.repo = r.repo AND j.run_id = r.run_id AND j.run_attempt = r.run_attempt
LEFT JOIN job_logs AS l
  ON l.repo = j.repo AND l.job_id = j.job_id
ORDER BY repo, run_id DESC, run_attempt DESC, job_id;

-- 6. Run waterfall summary. Per run attempt: wall-clock run duration against
--    its longest job. `longest_share` near 1.0 means one job IS the run (speed
--    that job up); well below 1.0 with few jobs means queueing or serialized
--    `needs:` chains dominate (look at job dependencies, not job speed). The
--    ranked list is the slowest runs since `since`. Logs-backed: 7 days.
WITH runs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.run_attempt']) AS run_attempt,
           attributes_string['loom.ci.conclusion'] AS conclusion,
           attributes_number['loom.ci.duration_ms'] AS run_ms
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.run'
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, run_id, run_attempt
),
jobs AS (
    SELECT repo, run_id, run_attempt,
           count() AS jobs,
           max(job_ms) AS longest_job_ms,
           argMax(job, job_ms) AS longest_job,
           sum(job_ms) AS summed_job_ms
    FROM (
        SELECT attributes_string['loom.repo'] AS repo,
               toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
               toUInt32(attributes_number['loom.ci.attempts']) AS run_attempt,
               toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
               attributes_string['loom.ci.job'] AS job,
               attributes_number['loom.ci.duration_ms'] AS job_ms
        FROM signoz_logs.logs_v2
        WHERE body = 'ci.job'
          AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
        LIMIT 1 BY repo, job_id
    )
    GROUP BY repo, run_id, run_attempt
)
SELECT r.repo AS repo, r.workflow AS workflow, r.run_id AS run_id,
       r.run_attempt AS run_attempt, r.conclusion AS conclusion,
       round(r.run_ms / 1000, 1) AS run_s,
       j.jobs,
       j.longest_job,
       round(j.longest_job_ms / 1000, 1) AS longest_job_s,
       round(j.longest_job_ms / nullIf(r.run_ms, 0), 2) AS longest_share,
       round(j.summed_job_ms / 1000, 1) AS summed_job_s
FROM runs AS r
INNER JOIN jobs AS j
  ON j.repo = r.repo AND j.run_id = r.run_id AND j.run_attempt = r.run_attempt
ORDER BY r.run_ms DESC, r.run_id
LIMIT {top:UInt32};
