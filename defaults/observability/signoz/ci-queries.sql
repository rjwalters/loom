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
--   * Section 7 (#9007) reads BOTH: `ci.run` log records here, joined against
--     `loom_analytics.raw_ship_outcome` — the sweep-side view `../cycle-time-
--     extract.sql` defines. That statement must be applied at least once
--     first (its `CREATE DATABASE IF NOT EXISTS` / `CREATE OR REPLACE VIEW`
--     are idempotent, so re-running it changes nothing); section 7 then joins
--     to it read-only.
--   * Section 8 (#9337) reads `ci.run` log records only: CI time per
--     `loom.ci.trigger_reason` (7 days).
--   * Sections 9-10 (#9089) read `ci.job` log records only: per-job queue
--     wait (`loom.ci.queued_ms`, `started_at - created_at`) and, for a matrix
--     leg, shard imbalance (`loom.ci.shard.{index,total,kind}`). Both are
--     `None` on a job GitHub reported no `created_at` for, or a job whose
--     display name carries no `(k/N)` shard suffix (`ci.yml`'s two sharded
--     job families: `Rust Unit Tests` via
--     `cargo nextest run --partition`, and `Shell Test Suites` via
--     `LOOM_CI_SHARD`).
--   * Sections 11-13 (#9089) and 16-17 (#9456) are the ONLY sections that read
--     TRACES (`signoz_traces.signoz_index_v3`), not logs or metrics: step
--     timings live on `loom.ci.step` spans, per-suite timings on
--     `loom.ci.suite` spans and per-test timings on `loom.ci.test` spans, none
--     of which has a log record or a metric series of its own (a per-step,
--     per-suite or per-test histogram would multiply the 30-day series count
--     by every job's step count / every leg's suite count / every leg's test
--     count). Traces are kept 7 days, the same horizon as the log sections.
--   * Section 14 (#9089) reads BOTH `ci.run` and `ci.job` log records and
--     joins them, so a run's critical path can separate its own queue segment
--     from the dependency + queue + running time of the leg that set its floor
--     (7 days).
--   * Section 15 (#9089) reads `ci.job` log records only: per-job dependency
--     wait (`loom.ci.dependency_wait_ms`, time blocked on `needs:`
--     predecessors before the job was created) beside the runner-queue wait
--     section 9 ranks, so the two are never read as one number (7 days).
--   * Sections 16-17 (#9456) read `loom.ci.test` TRACES: the per-test half of
--     #9089's "top 20 slowest tests and suites", for the `nextest-partition`
--     legs that section 12 cannot see (it reads `loom.ci.suite`, which only
--     the `LOOM_CI_SHARD` shell legs emit). **Only the SLOW TAIL of each leg
--     is emitted** -- tests at or above `nextest::MIN_TEST_DURATION_MS`,
--     slowest first, capped at `nextest::MAX_TEST_SPANS_PER_JOB` per leg -- so
--     a test missing from these results is below the floor or outside the cap,
--     never evidence it did not run. Reading them as a complete test inventory
--     is the one wrong way to use them (7 days).
--   * Section 18 (#10670) reads `ci.run` and `ci.job` log records: main's
--     push-run cancellations split into pending runs superseded by the
--     concurrency bound (expected) and started runs cancelled (a rule-2
--     violation) (7 days).
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
--   since         DateTime (UTC) lower bound for sections 0-1, 3-8 and 18
--   repo          'owner/name' to scope to one repository, '' for the whole org
--   bucket_hours  trend bucket width for sections 1 and 3 (24 = daily, 168 = weekly)
--   window_hours  section 2 compares [now - w, now) against [now - 2w, now - w)
--   top           row cap for the ranked sections 2, 4, 6-7, 10-17

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
--
--    TWO DIFFERENT ABSENCES, and the columns keep them apart. A failed run does
--    not always have a non-successful job: a `startup_failure`, a cancelled
--    matrix parent, or a required check that never produced a job all leave the
--    `failed_jobs` side of the LEFT JOIN unmatched. ClickHouse fills an
--    unmatched side with each column's type ZERO, not NULL, so such a row would
--    otherwise read `job_id` 0, `timed_out` false, `0 of 0` chunks and a filter
--    of `loom.ci.job_id = 0` — indistinguishable from "a failed job whose log
--    never arrived", and a Logs Explorer query that silently returns nothing.
--    Hence the `if(j.job_id = 0, NULL, ...)` guard on every job- and log-sourced
--    column. Read the result as:
--      * `job_id` NULL             -> the run failed with NO non-successful job;
--                                     there is no job log to look for, and
--                                     `logs_explorer_filter` is empty.
--      * `job_id` set, `0 of 0`    -> the job failed and its log never arrived.
--      * `job_id` set, `2 of 3`    -> partial capture; chunk 1 lost in flight.
--    `truncated` is the sharpest case: a Bool has no zero meaning "unknown", so
--    without the guard a run with no job asserts that a log nobody holds was
--    not truncated. Executed and mutation-tested in
--    `loom-daemon/tests/signoz_ci_failed_run_logs.rs`; the guard is additionally
--    pinned in ordinary CI by that file's `section_five_null_guards_*` test.
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
       if(j.job_id = 0, NULL, j.job) AS job,
       if(j.job_id = 0, NULL, j.job_id) AS job_id,
       if(j.job_id = 0, NULL, j.job_conclusion) AS job_conclusion,
       if(j.job_id = 0, NULL, j.timed_out) AS timed_out,
       if(j.job_id = 0, NULL, l.chunks_present) AS chunks_present,
       if(j.job_id = 0, NULL, l.chunk_count) AS chunk_count,
       if(j.job_id = 0, NULL, l.truncated) AS truncated,
       if(j.job_id = 0, '', concat('loom.ci.job_id = ', toString(j.job_id))) AS logs_explorer_filter
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

-- 7. Per-issue ship breakdown (#9007 join keys). For each sweep recorded in
--    `loom_analytics.raw_ship_outcome` (populated by
--    `../cycle-time-extract.sql` — apply that file first; `CREATE DATABASE`/
--    `CREATE OR REPLACE VIEW` are idempotent, so re-running it here is safe),
--    joins the `ci.run` triggered by that issue's `feature/issue-N` branch and
--    reports Builder / Judge / merge-phase seconds beside the CI run's own
--    wall-clock duration. The join key is the **issue** number recovered from
--    `loom.ci.ref` with the same `feature/issue-N` convention
--    `claim_reconciliation::parse_issue_from_branch` applies fleet-side — CI
--    LOG records (unlike the `loom.ci.run`/`loom.ci.job` SPANS this issue also
--    adds `loom.ci.head_sha`/`loom.ci.ref`/`loom.pr_number` to) carry no PR/
--    issue attribute of their own to join on directly. Logs-backed for the CI
--    side: 7 days; the sweep side is the rollup's own (much longer) window.
--
--    Two claims this section deliberately does NOT make, rather than
--    overclaiming a split the data cannot support:
--    - CI is split into two segments: `ci_queued_s` (`loom.ci.queued_ms`,
--      GitHub's `created_at` to `run_started_at`: waiting for a runner) and
--      `ci_wall_s` (`started_at` to `completed_at`: on a runner). The queue
--      segment was added as a #9007 follow-up. It is NULL for a run recorded
--      before that, or one GitHub reported no start for. It is never 0 by
--      default. Both overlap the sweep phase columns (CI runs while Builder or
--      Judge waits on it), so they are not additive with them.
--    - "Lead time" is the SWEEP's own `total_duration_sec` (sweep start to
--      terminal state), not issue-filed-to-merged: no forge issue-open
--      timestamp reaches this telemetry stream — see
--      `../cycle-time-questions.md` "What this question set cannot answer".
WITH ci_runs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           extractGroups(attributes_string['loom.ci.ref'], '^feature/issue-([0-9]+)$')[1]
                                                            AS issue_str,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.run_attempt']) AS run_attempt,
           attributes_string['loom.ci.conclusion'] AS ci_conclusion,
           attributes_number['loom.ci.duration_ms'] AS ci_ms,
           if(mapContains(attributes_number, 'loom.ci.queued_ms'),
              attributes_number['loom.ci.queued_ms'], NULL) AS ci_queued_ms
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.run'
      AND mapContains(attributes_string, 'loom.ci.ref')
      AND match(attributes_string['loom.ci.ref'], '^feature/issue-[0-9]+$')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, run_id, run_attempt
)
SELECT ro.repo AS repo,
       ro.issue AS issue,
       ro.pr_number AS pr_number,
       ro.result AS sweep_result,
       ro.finished_at AS ship_finished_at,
       round(ro.total_duration_sec, 1) AS lead_time_s_sweep_proxy,
       if(has(ro.phases, 'builder'),
          ro.phase_durations_sec[indexOf(ro.phases, 'builder')], NULL) AS builder_s,
       if(has(ro.phases, 'judge'),
          ro.phase_durations_sec[indexOf(ro.phases, 'judge')], NULL) AS judge_s,
       if(has(ro.phases, 'merge'),
          ro.phase_durations_sec[indexOf(ro.phases, 'merge')], NULL) AS merge_wait_s,
       c.run_id AS ci_run_id,
       c.ci_conclusion AS ci_conclusion,
       round(c.ci_queued_ms / 1000, 1) AS ci_queued_s,
       round(c.ci_ms / 1000, 1) AS ci_wall_s
FROM loom_analytics.raw_ship_outcome AS ro
LEFT JOIN ci_runs AS c
  ON c.repo = ro.repo AND toUInt32OrZero(c.issue_str) = ro.issue
WHERE ro.finished_at >= {since:DateTime}
  AND ({repo:String} = '' OR ro.repo = {repo:String})
ORDER BY ro.finished_at DESC
LIMIT {top:UInt32};

-- 8. CI time by trigger reason (#9337). Why did each run happen, and how much
--    running and queue time did each reason cost? `stale_main_bump` is the
--    time lost to the #8508 re-date commit re-running CI after `main` moved;
--    `flaky_retry` is in-place re-runs; `new_commit` is fresh code (including
--    merge-from-main heads — the documented under-count, see
--    ../../docs/ci-observability.md "Trigger attribution"). Runs recorded
--    before #9337 carry no reason and report as `unrecorded`. Logs-backed:
--    7 days.
WITH runs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.run_attempt']) AS run_attempt,
           if(mapContains(attributes_string, 'loom.ci.trigger_reason'),
              attributes_string['loom.ci.trigger_reason'], 'unrecorded') AS trigger_reason,
           attributes_number['loom.ci.duration_ms'] AS run_ms,
           if(mapContains(attributes_number, 'loom.ci.queued_ms'),
              attributes_number['loom.ci.queued_ms'], NULL) AS queued_ms
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.run'
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, run_id, run_attempt
)
SELECT repo,
       trigger_reason,
       count() AS runs,
       round(sum(run_ms) / 1000 / 60, 1) AS run_minutes,
       round(sum(queued_ms) / 1000 / 60, 1) AS queued_minutes,
       round(100 * sum(run_ms) / sum(sum(run_ms)) OVER (PARTITION BY repo), 1)
                                                            AS pct_of_repo_run_time
FROM runs
GROUP BY repo, trigger_reason
ORDER BY repo, run_minutes DESC;

-- 9. Per-job queue-wait percentiles (#9089). P50/P90/max of
--    `loom.ci.queued_ms` (`started_at - created_at`) per repo + workflow +
--    job, per `bucket_hours` bucket -- the per-job analogue of section 1's
--    duration trend, so a runner-queue-cap burst localizes to one job family
--    instead of only showing up in the run-level queue segment (section 7's
--    `ci_queued_s`). On 2026-09-26 a burst reached p90 212s / max 1064s while
--    the median stayed 6s -- ALERT when p90 exceeds 60s. A job GitHub
--    reported no `created_at` for (a pre-#9089 recording) contributes no
--    sample, not a zero. Logs-backed (`ci.job` records): 7 days.
WITH job_queue AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           attributes_string['loom.ci.job'] AS job,
           toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
           attributes_number['loom.ci.queued_ms'] AS queued_ms,
           timestamp AS ts
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.job'
      AND mapContains(attributes_number, 'loom.ci.queued_ms')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, job_id
)
SELECT repo, workflow, job,
       toStartOfInterval(fromUnixTimestamp64Nano(toInt64(ts)),
                         toIntervalHour({bucket_hours:UInt32})) AS bucket,
       count() AS jobs,
       round(quantileExact(0.5)(queued_ms) / 1000, 1) AS p50_s,
       round(quantileExact(0.9)(queued_ms) / 1000, 1) AS p90_s,
       round(max(queued_ms) / 1000, 1) AS max_s
FROM job_queue
GROUP BY repo, workflow, job, bucket
ORDER BY repo, workflow, job, bucket;

-- 10. Shard imbalance (#9089). For each run attempt's matrix legs sharing one
--     `loom.ci.shard.kind` (`nextest-partition` / `shell-suite-shard`), the
--     spread between its slowest and fastest leg -- the rebalancing signal
--     the issue names (the same Unit partition's test step ranged 50s-139s
--     between runs, guesswork without this). A `shard_kind = 'none'` job
--     never appears here. Ranked by absolute spread, descending.
--     Logs-backed (`ci.job` records): 7 days.
WITH sharded_jobs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.attempts']) AS run_attempt,
           attributes_string['loom.ci.shard.kind'] AS shard_kind,
           toUInt32(attributes_number['loom.ci.shard.total']) AS shard_total,
           toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
           attributes_number['loom.ci.duration_ms'] AS duration_ms
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.job'
      AND mapContains(attributes_string, 'loom.ci.shard.kind')
      AND attributes_string['loom.ci.shard.kind'] != 'none'
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, job_id
)
SELECT repo, workflow, run_id, run_attempt, shard_kind,
       max(shard_total) AS legs,
       round(min(duration_ms) / 1000, 1) AS fastest_leg_s,
       round(max(duration_ms) / 1000, 1) AS slowest_leg_s,
       round((max(duration_ms) - min(duration_ms)) / 1000, 1) AS spread_s,
       round(100 * (max(duration_ms) - min(duration_ms)) / nullIf(max(duration_ms), 0), 1)
                                                            AS spread_pct
FROM sharded_jobs
GROUP BY repo, workflow, run_id, run_attempt, shard_kind
ORDER BY spread_s DESC, repo, run_id
LIMIT {top:UInt32};

-- 11. Where did a job's time go, step by step (#9089). P50/P90/max of the
--     `loom.ci.step` span durations per repo + workflow + job + step, ranked
--     by p90 -- the question a job span alone cannot answer ("did this Rust
--     leg's ~250s go to compiling or to running tests?"), which every #9065
--     tuning decision had to pull from the jobs API by hand. `shard_kind` is
--     carried on the step span itself, so a matrix leg's steps are already
--     separated here without a trace join.
--
--     TRACES, not logs (see the header): `signoz_traces.signoz_index_v3`, 7
--     days. Duplicate delivery is permitted on this wire, so de-duplicate on
--     the derived span id before aggregating -- a replayed batch is
--     byte-identical in (trace_id, span_id), which is exactly what makes that
--     safe. A step GitHub reported no start or no completion for (one the job
--     never reached) has no span at all, so it never counts as a fast step.
WITH steps AS (
    SELECT attributes_string['loom.repo']            AS repo,
           attributes_string['loom.ci.workflow']     AS workflow,
           attributes_string['loom.ci.job']          AS job,
           attributes_string['loom.ci.step']         AS step,
           attributes_string['loom.ci.shard.kind']   AS shard_kind,
           any(duration_nano)                        AS duration_nano
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.ci.step'
      AND timestamp >= {since:DateTime}
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    GROUP BY repo, workflow, job, step, shard_kind, trace_id, span_id
)
SELECT repo, workflow, job, step, shard_kind,
       count() AS runs,
       round(quantileExact(0.5)(duration_nano) / 1e9, 1) AS p50_s,
       round(quantileExact(0.9)(duration_nano) / 1e9, 1) AS p90_s,
       round(max(duration_nano) / 1e9, 1)                AS max_s,
       round(sum(duration_nano) / 1e9, 1)                AS total_s
FROM steps
GROUP BY repo, workflow, job, step, shard_kind
ORDER BY p90_s DESC, total_s DESC
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 12. Top slowest shell test suites (#9089). P50/P90/max/total of the
--     `loom.ci.suite` span durations per repo + workflow + job + suite, ranked
--     by total time, over the trace horizon. This is the question a job span
--     and even a step span cannot answer: a `Shell Test Suites (hermetic, 1/2)`
--     leg's ~113s is one `run:` step, and only the suite spans say which of the
--     leg's ~118 suites spent it. `retried_runs` is the #7791 retry count for
--     the same suite over the same window, so a suite that is slow because it
--     runs twice is distinguishable from one that is simply slow -- the input
--     #7789's quarantine decision wants.
--
--     TRACES, 7 days, same as section 11 (suite spans have no log record and no
--     metric series: a per-suite histogram would multiply the 30-day series
--     count by every leg's suite count). De-duplicate on the derived span id
--     before aggregating.
--
--     A suite that did NOT run in a leg (skipped by the live-daemon guard,
--     missing from disk) has no span at all, so it never appears here as a fast
--     suite. `outcome` is the suite's own pass/fail/skip, never its job's
--     GitHub conclusion.
WITH suites AS (
    SELECT attributes_string['loom.repo']                 AS repo,
           attributes_string['loom.ci.workflow']          AS workflow,
           attributes_string['loom.ci.job']               AS job,
           attributes_string['loom.ci.suite']             AS suite,
           anyIf(1, attributes_string['loom.ci.suite.retried'] = 'true') AS retried,
           anyIf(1, attributes_string['loom.ci.suite.outcome'] = 'fail') AS failed,
           any(duration_nano)                             AS duration_nano
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.ci.suite'
      AND timestamp >= {since:DateTime}
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    GROUP BY repo, workflow, job, suite, trace_id, span_id
)
SELECT repo, workflow, job, suite,
       count()                                           AS runs,
       sum(retried)                                      AS retried_runs,
       sum(failed)                                       AS failed_runs,
       round(quantileExact(0.5)(duration_nano) / 1e9, 1) AS p50_s,
       round(quantileExact(0.9)(duration_nano) / 1e9, 1) AS p90_s,
       round(max(duration_nano) / 1e9, 1)                AS max_s,
       round(sum(duration_nano) / 1e9, 1)                AS total_s
FROM suites
GROUP BY repo, workflow, job, suite
ORDER BY total_s DESC, p90_s DESC
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 13. Suite-level shard rebalance (#9089). Section 10 says WHETHER a run's
--     legs are imbalanced, from their job durations. This says what to MOVE:
--     per leg, the summed suite time it carried and its slowest suite, so a
--     `LOOM_CI_SHARD` split can be rebalanced by moving named suites rather
--     than by re-running the matrix and hoping.
--
--     One row per (run attempt, leg). `leg_suite_s` is the sum of that leg's
--     suite spans, which is LESS than the job's wall time (checkout, toolchain
--     setup and the runner's own overhead are steps, not suites) and, because
--     suites run concurrently within a leg, MORE than the wall time of the step
--     that ran them. Both gaps are expected; the comparison that matters here
--     is between legs of the same run, not between a leg and its own job span.
--
--     TRACES, 7 days.
WITH suites AS (
    SELECT attributes_string['loom.repo']               AS repo,
           attributes_string['loom.ci.workflow']        AS workflow,
           attributes_string['loom.ci.run_id']          AS run_id,
           attributes_string['loom.ci.shard.kind']      AS shard_kind,
           attributes_string['loom.ci.shard.index']     AS shard_index,
           attributes_string['loom.ci.shard.total']     AS shard_total,
           attributes_string['loom.ci.suite']           AS suite,
           any(duration_nano)                           AS duration_nano
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.ci.suite'
      AND timestamp >= {since:DateTime}
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    GROUP BY repo, workflow, run_id, shard_kind, shard_index, shard_total, suite,
             trace_id, span_id
)
SELECT repo, workflow, run_id, shard_kind,
       shard_index, shard_total,
       count()                                  AS suites_run,
       round(sum(duration_nano) / 1e9, 1)       AS leg_suite_s,
       argMax(suite, duration_nano)             AS slowest_suite,
       round(max(duration_nano) / 1e9, 1)       AS slowest_suite_s
FROM suites
GROUP BY repo, workflow, run_id, shard_kind, shard_index, shard_total
ORDER BY run_id DESC, leg_suite_s DESC
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 14. Critical path per run, queue and dependency wait included (#9089).
--     Section 6 compares a run's wall time to its longest job's RUNNING time,
--     and can only say "queueing or a serialized needs: chain dominates"
--     without saying which. Every job now carries all three of its segments --
--     `loom.ci.dependency_wait_ms`, then `loom.ci.queued_ms`, then
--     `loom.ci.duration_ms` -- so this ranks each run's jobs by their sum and
--     names the one that actually set the floor:
--       `critical_job` / `critical_total_s`  the job with the largest
--                                           dependency+queue+running sum, and
--                                           that sum
--       `critical_dep_wait_s`               how much of it was blocked on
--                                           `needs:` predecessors, BEFORE the
--                                           job was created at all
--       `critical_queued_s`                 how much was then waiting for a
--                                           runner, never conflated with work
--       `run_queued_s`                      the RUN's own queue segment (#9007),
--                                           which is time before any job existed
--       `unexplained_s`                     run wall time minus (run queue +
--                                           critical job total)
--
--     Read `critical_dep_wait_s` first when `unexplained_s` used to be the only
--     signal: a large value there is the `build-daemon` fan-in, measured rather
--     than inferred, and the fix is to shorten the predecessor (or to stop
--     depending on it), not to speed up the leg itself.
--
--     `unexplained_s` survives as the residual for what the three segments
--     still do not cover -- a MULTI-level `needs:` chain whose critical job is
--     not the last link, and clock/window mismatch between the run row and its
--     jobs. Expect it near zero on a run with a single dependency level, and
--     slightly negative on a run whose jobs overlap the run's own reported
--     window. It remains a residual, not a measurement.
--
--     A job GitHub reported no `created_at` for contributes 0 for both segments
--     here rather than dropping out of its run's critical path entirely --
--     unlike sections 9 and 15, which are percentiles and must not be fed a
--     fake zero. Logs-backed (`ci.run` + `ci.job` records): 7 days.
WITH runs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.run_attempt']) AS run_attempt,
           attributes_string['loom.ci.conclusion'] AS conclusion,
           attributes_number['loom.ci.duration_ms'] AS run_ms,
           attributes_number['loom.ci.queued_ms'] AS run_queued_ms
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.run'
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, run_id, run_attempt
),
jobs AS (
    SELECT repo, run_id, run_attempt,
           count() AS jobs,
           argMax(job, total_ms) AS critical_job,
           max(total_ms) AS critical_total_ms,
           argMax(queued_ms, total_ms) AS critical_queued_ms,
           argMax(dep_wait_ms, total_ms) AS critical_dep_wait_ms,
           sum(queued_ms) AS summed_queued_ms
    FROM (
        SELECT attributes_string['loom.repo'] AS repo,
               toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
               toUInt32(attributes_number['loom.ci.attempts']) AS run_attempt,
               toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
               attributes_string['loom.ci.job'] AS job,
               attributes_number['loom.ci.queued_ms'] AS queued_ms,
               attributes_number['loom.ci.dependency_wait_ms'] AS dep_wait_ms,
               attributes_number['loom.ci.duration_ms']
                 + attributes_number['loom.ci.queued_ms']
                 + attributes_number['loom.ci.dependency_wait_ms'] AS total_ms
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
       round(r.run_queued_ms / 1000, 1) AS run_queued_s,
       j.jobs,
       j.critical_job,
       round(j.critical_total_ms / 1000, 1) AS critical_total_s,
       round(j.critical_dep_wait_ms / 1000, 1) AS critical_dep_wait_s,
       round(j.critical_queued_ms / 1000, 1) AS critical_queued_s,
       round(j.summed_queued_ms / 1000, 1) AS summed_job_queued_s,
       round((r.run_ms - r.run_queued_ms - j.critical_total_ms) / 1000, 1)
         AS unexplained_s
FROM runs AS r
INNER JOIN jobs AS j
  ON j.repo = r.repo AND j.run_id = r.run_id AND j.run_attempt = r.run_attempt
ORDER BY r.run_ms DESC, r.run_id
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 15. Dependency wait vs. runner-queue wait, per job (#9089, issue problem 5).
--     Section 9 answers "how long did this job wait for a RUNNER". This
--     answers the question that was invisible beside it: how long it waited on
--     its `needs:` predecessors BEFORE GitHub created it at all. The two are
--     different problems with different fixes -- a large queue wait is the
--     account's concurrent-job cap (add capacity, shrink the matrix), a large
--     dependency wait is workflow shape (`build-daemon` fan-in: shorten the
--     predecessor, or stop depending on it) -- and until now both landed in
--     one undifferentiated "the run was slow but no job was".
--
--     `dependency_wait_ms` is `created_at` minus the run attempt's EARLIEST job
--     creation, so an ungated job measures ~0 by construction: GitHub creates a
--     `needs:`-gated job only once its predecessors finish. Sub-second values
--     are job-creation lag, not a serialized edge -- hence `gated_jobs`, which
--     counts only the legs above the 2000 ms gate below and is the column to
--     read before trusting the percentiles beside it.
--
--     ALERT when `p90_dep_s` for a job family exceeds its own `p90_queue_s`:
--     that family is gated more than it is capacity-starved, and no amount of
--     runner capacity will move it.
--
--     A job GitHub reported no `created_at` for (a pre-#9089 recording)
--     contributes no sample to either percentile, never a zero -- the same rule
--     section 9 follows and the opposite of section 14's, which needs every job
--     of a run present to rank them. Logs-backed (`ci.job` records): 7 days.
WITH job_waits AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           attributes_string['loom.ci.job'] AS job,
           toUInt64(attributes_number['loom.ci.job_id']) AS job_id,
           attributes_number['loom.ci.dependency_wait_ms'] AS dep_wait_ms,
           attributes_number['loom.ci.queued_ms'] AS queued_ms,
           attributes_number['loom.ci.duration_ms'] AS duration_ms
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.job'
      AND mapContains(attributes_number, 'loom.ci.dependency_wait_ms')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, job_id
)
SELECT repo, workflow, job,
       count() AS jobs,
       -- 2000 ms: above GitHub's own job-creation lag (observed sub-second),
       -- below any real predecessor job. Legs under it are not `needs:`-gated.
       countIf(dep_wait_ms > 2000) AS gated_jobs,
       round(quantileExact(0.5)(dep_wait_ms) / 1000, 1) AS p50_dep_s,
       round(quantileExact(0.9)(dep_wait_ms) / 1000, 1) AS p90_dep_s,
       round(max(dep_wait_ms) / 1000, 1) AS max_dep_s,
       round(quantileExact(0.9)(queued_ms) / 1000, 1) AS p90_queue_s,
       round(quantileExact(0.9)(duration_ms) / 1000, 1) AS p90_run_s,
       -- What share of this family's typical end-to-end time is spent before
       -- it is even eligible for a runner.
       round(100 * quantileExact(0.9)(dep_wait_ms)
             / nullIf(quantileExact(0.9)(dep_wait_ms + queued_ms + duration_ms), 0), 1)
                                                        AS p90_dep_pct
FROM job_waits
GROUP BY repo, workflow, job
ORDER BY p90_dep_s DESC, repo, workflow, job
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 16. Top slowest Rust tests (#9456). P50/P90/max/total of the `loom.ci.test`
--     span durations per repo + workflow + binary + test, ranked by total
--     time, over the trace horizon. This is the per-test half of #9089's
--     proposal 4 ("top 20 slowest tests and suites"); section 12 is the suite
--     half and cannot answer this one, because it reads `loom.ci.suite` spans,
--     which only the `LOOM_CI_SHARD` shell legs emit. A `Rust Unit Tests
--     (1/3)` leg's ~110s test step is one `loom.ci.step` span; only these say
--     which of its 4,242 tests spent it.
--
--     `flaky_runs` is the #7789 signal: the same test under the same binary
--     that failed at least once and ultimately passed (nextest wrote a
--     `<flakyFailure>`/`<rerunFailure>` for it). A test that is slow BECAUSE it
--     is retried is distinguishable here from one that is simply slow, which is
--     what a quarantine decision needs. `failed_runs` counts the final-attempt
--     failures (`fail`) and the executions nextest could not complete
--     (`error`).
--
--     TRACES, 7 days, same as sections 11-13 (test spans have no log record and
--     no metric series: the metric label allowlist admits no test dimension,
--     and a per-test histogram would multiply the 30-day series count by every
--     leg's test count). De-duplicate on the derived span id before
--     aggregating.
--
--     **`runs` is NOT how many times the test ran.** Only each leg's slow tail
--     is emitted (see the file header), so `runs` counts the CI runs in which
--     this test was slow enough to be emitted. A test absent from these results
--     is below the duration floor or outside the per-leg cap -- never evidence
--     that it did not run. `outcome` is the test's own pass/fail/flaky/error,
--     never its job's GitHub conclusion.
WITH tests AS (
    SELECT attributes_string['loom.repo']               AS repo,
           attributes_string['loom.ci.workflow']        AS workflow,
           attributes_string['loom.ci.test.binary']     AS binary,
           attributes_string['loom.ci.test']            AS test,
           anyIf(1, attributes_string['loom.ci.test.outcome'] = 'flaky') AS flaky,
           anyIf(1, attributes_string['loom.ci.test.outcome'] IN ('fail', 'error')) AS failed,
           any(duration_nano)                           AS duration_nano
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.ci.test'
      AND timestamp >= {since:DateTime}
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    GROUP BY repo, workflow, binary, test, trace_id, span_id
)
SELECT repo, workflow, binary, test,
       count()                                           AS runs,
       sum(flaky)                                        AS flaky_runs,
       sum(failed)                                       AS failed_runs,
       round(quantileExact(0.5)(duration_nano) / 1e9, 1) AS p50_s,
       round(quantileExact(0.9)(duration_nano) / 1e9, 1) AS p90_s,
       round(max(duration_nano) / 1e9, 1)                AS max_s,
       round(sum(duration_nano) / 1e9, 1)                AS total_s
FROM tests
GROUP BY repo, workflow, binary, test
ORDER BY total_s DESC, p90_s DESC
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 17. Test-level partition rebalance (#9456). Section 10 says WHETHER a run's
--     `nextest-partition` legs are imbalanced, from their job durations;
--     section 13 says what to move for the SHELL legs. This says what to move
--     for the nextest legs: per leg, the summed slow-tail test time it carried
--     and its slowest named test, so a `--partition count:k/N` split can be
--     reasoned about from named tests rather than by re-running the matrix and
--     hoping.
--
--     One row per (run attempt, leg). Read `leg_tail_s` only BETWEEN legs of
--     the same run and the same family: it is the sum of that leg's emitted
--     slow tail, which is far less than the leg's test-step time (every test
--     below the floor is excluded by design) and, because nextest runs tests
--     concurrently, not comparable to any wall clock. `tail_tests` is how many
--     spans the leg emitted -- at `nextest::MAX_TEST_SPANS_PER_JOB` the cap is
--     binding and `leg_tail_s` is a floor, not a total.
--
--     `job` is in the GROUP BY precisely so a second
--     nextest-partition family sharding 1..3 (the `Rust OTLP Feature Tests`
--     family existed until #10823) would not be merged with `Rust Unit Tests`:
--     legs of different families with the same `(k/N)` are unrelated
--     partitions.
--
--     TRACES, 7 days.
WITH tests AS (
    SELECT attributes_string['loom.repo']               AS repo,
           attributes_string['loom.ci.workflow']        AS workflow,
           attributes_string['loom.ci.run_id']          AS run_id,
           attributes_string['loom.ci.job']             AS job,
           attributes_string['loom.ci.shard.index']     AS shard_index,
           attributes_string['loom.ci.shard.total']     AS shard_total,
           attributes_string['loom.ci.test.binary']     AS binary,
           attributes_string['loom.ci.test']            AS test,
           any(duration_nano)                           AS duration_nano
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.ci.test'
      AND attributes_string['loom.ci.shard.kind'] = 'nextest-partition'
      AND timestamp >= {since:DateTime}
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    GROUP BY repo, workflow, run_id, job, shard_index, shard_total, binary, test,
             trace_id, span_id
)
SELECT repo, workflow, run_id, job,
       shard_index, shard_total,
       count()                                  AS tail_tests,
       round(sum(duration_nano) / 1e9, 1)        AS leg_tail_s,
       argMax(test, duration_nano)              AS slowest_test,
       argMax(binary, duration_nano)            AS slowest_test_binary,
       round(max(duration_nano) / 1e9, 1)       AS slowest_test_s
FROM tests
GROUP BY repo, workflow, run_id, job, shard_index, shard_total
ORDER BY run_id DESC, leg_tail_s DESC
LIMIT {top:UInt32};

-- ---------------------------------------------------------------------------
-- 18. Main cancellation split (#10670). How often a default-branch push run
--     ends `cancelled`, and WHY. `ci.yml` bounds main with one concurrency
--     group and `cancel-in-progress: false` (ci-principles.md rule 2): the
--     oldest run keeps running, the newest waits, and every push in between
--     supersedes the previous still-PENDING run. Such a run never started, so
--     GitHub created no job for it, and it carries no `ci.job` record here.
--     That is the bound working, and during a merge burst it is most of main's
--     `cancelled` count (2026-10-06: 54 of 100 main runs, all job-less).
--
--     The two kinds of `cancelled` are therefore told apart by whether ANY
--     `ci.job` record exists for the run attempt:
--       * `superseded_before_start` -- no job: a pending run dropped by the
--         concurrency bound. Expected; not a verdict on the commit.
--       * `cancelled_after_start`   -- at least one job: a started main run
--         was cancelled. A rule-2 violation (or a human pressing Cancel) and
--         must stay 0; loom-daemon's `main_cancel` lint is what keeps workflow
--         edits from reintroducing one (#7779 was this column at 22/30).
--     `verified_ratio` is the share of main runs that reached a verdict
--     (neither cancelled nor unreported) -- how continuously main is checked.
--
--     Section 3 already shows a workflow's overall cancelled ratio from
--     metrics, but metric labels carry no ref or event, so it cannot isolate
--     main pushes from PR supersession; this section can. The default branch
--     is matched as `main` (the run's `head_branch`; `refs/heads/main` is
--     accepted too); edit the literals for a repository whose default branch
--     differs. A job record can lag its run
--     record by one poll, so a run cancelled in the last few minutes may
--     briefly read `superseded_before_start`. `run_jobs` reads one day
--     before `since`, so a run that completed just after `since` keeps the
--     job records stamped just before it.
--
--     Logs-backed (`ci.run` + `ci.job` records): 7 days.
WITH main_runs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           attributes_string['loom.ci.workflow'] AS workflow,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.run_attempt']) AS run_attempt,
           attributes_string['loom.ci.conclusion'] AS conclusion
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.run'
      AND attributes_string['loom.ci.event'] = 'push'
      AND attributes_string['loom.ci.ref'] IN ('main', 'refs/heads/main')
      AND timestamp >= toUInt64(toUnixTimestamp({since:DateTime})) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    LIMIT 1 BY repo, run_id, run_attempt
),
run_jobs AS (
    SELECT attributes_string['loom.repo'] AS repo,
           toUInt64(attributes_number['loom.ci.run_id']) AS run_id,
           toUInt32(attributes_number['loom.ci.attempts']) AS run_attempt,
           uniqExact(toUInt64(attributes_number['loom.ci.job_id'])) AS jobs
    FROM signoz_logs.logs_v2
    WHERE body = 'ci.job'
      -- One day of lookback before `since`: a run whose `ci.run` record lands
      -- just after `since` can have job records stamped just before it, and
      -- dropping those would misread a started run as superseded.
      AND timestamp >= (toUInt64(toUnixTimestamp({since:DateTime})) - 86400) * 1000000000
      AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
    GROUP BY repo, run_id, run_attempt
)
SELECT r.repo AS repo, r.workflow AS workflow,
       count() AS runs,
       countIf(r.conclusion = 'success') AS success,
       countIf(r.conclusion IN ('failure', 'timed_out', 'startup_failure')) AS failed,
       countIf(r.conclusion = 'cancelled') AS cancelled,
       countIf(r.conclusion = 'cancelled' AND j.jobs = 0) AS superseded_before_start,
       countIf(r.conclusion = 'cancelled' AND j.jobs > 0) AS cancelled_after_start,
       countIf(r.conclusion = '') AS unreported,
       round(cancelled / runs, 3) AS cancelled_ratio,
       round((runs - cancelled - unreported) / runs, 3) AS verified_ratio
FROM main_runs AS r
LEFT JOIN run_jobs AS j
  ON j.repo = r.repo AND j.run_id = r.run_id AND j.run_attempt = r.run_attempt
GROUP BY repo, workflow
ORDER BY cancelled_after_start DESC, runs DESC, repo, workflow;
