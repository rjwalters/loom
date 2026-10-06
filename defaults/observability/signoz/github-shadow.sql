-- GitHub shadow-spend reconciliation (Issue #10343). How much of what GitHub
-- billed each rate-limit BUCKET did Loom's own accounting attribute? A bucket
-- is one App installation's pool: `(account, owner, resource)` — `account` is
-- `app-<id>`, `owner` the installation's owner, `resource` core|graphql|search.
-- Run with the bundled clickhouse-client in the private ClickHouse container.
-- Metric timestamps are milliseconds.
--
-- STATUS: vocabulary-guarded by `loom-daemon/tests/signoz_trial_artifacts.rs`
-- (every metric name, metric label and span attribute below must still be
-- emitted by the daemon and forwarded by the gateway's keep_keys), and all
-- five queries are executed verbatim against the pinned
-- `clickhouse/clickhouse-server:25.12.5` by
-- `loom-daemon/tests/signoz_github_shadow_queries.rs` in CI. Run read-only
-- against the live store during PR #10565's review; the one-hour 10 %
-- reconciliation is #10343's Slice 3.
--
-- Shadow = GitHub's Σ increments of `github.ratelimit.used` − the requests
-- Loom attributed to that bucket. It is reported as a BAND, not a point:
-- `loom.forge.calls{outcome="ok"}` is what GitHub surely charged, while an
-- `error` row may be a charged 4xx or a local failure that never sent
-- anything (#10271), so ok+error bounds the attributed figure from above.
-- 304s (`not_modified`) are free and the `gh api rate_limit` probe
-- (`resource="other"`, `op="quota.rate-limit-reading"`) is free; both are
-- excluded. This mirrors the local `charged` figure of
-- `loom-daemon forge calls --by bucket`, which counts `ok` rows only.
--
-- Since #10343 every `github.ratelimit.*` point carries `owner` and `role`,
-- and the legacy owner-less 60 s probe series is gone (it is booked into the
-- bucket book instead). A label set is still NOT guaranteed to be one bucket:
-- live data shows `(account, owner, resource)` series carrying two
-- interleaved hourly reset chains (rjwalters/loom#10571), and several hosts
-- export the same bucket with readings of different ages. So `used` is never
-- read as monotone across samples -- queries 1 and 3 key every reading by its
-- quota WINDOW (the paired `github.ratelimit.reset`) and charge each window's
-- high-water mark once. An operator's ambient-login host reports
-- `owner = '-'`, `role = 'ambient'`; it has no attributed counterpart in
-- query 2 and shows as all-shadow by construction.
--
-- time_series_v4 holds one row per series per hour, so every metric query
-- joins a de-duplicated fingerprint set (`any(labels) GROUP BY fingerprint`);
-- a plain join would count each sample once per hour-row.

-- 0. Preflight: the metric families exist and how SigNoz stored them.
--    `loom.forge.calls` is exported as a delta Sum: query 2 sums its samples,
--    which is only right while `temporality` reads Delta. If it reads
--    Cumulative, rewrite query 2 with query 1's lag-delta pattern.
SELECT metric_name, type, temporality, uniqExact(fingerprint) AS series
FROM signoz_metrics.time_series_v4
WHERE metric_name IN ('github.ratelimit.used', 'github.ratelimit.reset', 'loom.forge.calls')
GROUP BY metric_name, type, temporality
ORDER BY metric_name;

-- 1. GitHub-side spend per bucket per hour, last 24 h. A bucket's quota
--    WINDOW is identified by its reset epoch: every exported reading pairs
--    `github.ratelimit.used` with the `github.ratelimit.reset` the same host
--    exported for the same labels at the same instant. Resets within 2 s of
--    each other are one window (GitHub's reset jitters by a second between
--    headers and the probe). Within a window `used` only grows, so the window's
--    spend is its high-water mark: each hour is charged the rise of that
--    window's running maximum, and a window's first hour is charged its whole
--    high-water mark. Any lower reading in the same window -- a host that
--    stopped reporting while another kept exporting an older reading, an
--    out-of-order sample -- changes nothing; only a new window adds spend.
--    Several windows under one label set (the per-bucket series can carry two
--    interleaved buckets, #10343) are each counted once and summed, never
--    amplified. A window already open when the 24 h range starts charges its
--    pre-range spend to the range's first hour. Only bucket points are read:
--    a point without `owner` is a pre-#10343 daemon's single-credential
--    reading (a duplicate of one bucket under other labels) or another
--    emitter's series, and would merge unrelated windows.
WITH series AS (
    SELECT fingerprint, any(labels) AS labels
    FROM signoz_metrics.time_series_v4
    WHERE metric_name IN ('github.ratelimit.used', 'github.ratelimit.reset')
    GROUP BY fingerprint
),
points AS (
    SELECT JSONExtractString(t.labels, 'account') AS account,
           JSONExtractString(t.labels, 'owner') AS owner,
           JSONExtractString(t.labels, 'resource') AS resource,
           JSONExtractString(t.labels, 'host.id') AS host,
           s.unix_milli AS ms,
           maxIf(s.value, s.metric_name = 'github.ratelimit.used') AS used,
           maxIf(s.value, s.metric_name = 'github.ratelimit.reset') AS reset_epoch,
           countIf(s.metric_name = 'github.ratelimit.used') AS has_used,
           countIf(s.metric_name = 'github.ratelimit.reset') AS has_reset
    FROM signoz_metrics.samples_v4 AS s
    INNER JOIN series AS t USING (fingerprint)
    WHERE s.metric_name IN ('github.ratelimit.used', 'github.ratelimit.reset')
      AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 1 DAY) * 1000
      AND JSONExtractString(t.labels, 'account') != ''
      AND JSONExtractString(t.labels, 'owner') != ''
      AND JSONExtractString(t.labels, 'resource') != ''
    GROUP BY account, owner, resource, host, ms
    HAVING has_used > 0 AND has_reset > 0
),
resets AS (
    SELECT account, owner, resource, reset_epoch,
           if(reset_epoch - lagInFrame(reset_epoch, 1, toFloat64(0)) OVER
                  (PARTITION BY account, owner, resource ORDER BY reset_epoch
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) > 2, 1, 0) AS opens
    FROM (SELECT DISTINCT account, owner, resource, reset_epoch FROM points)
),
windows AS (
    SELECT account, owner, resource, reset_epoch,
           sum(opens) OVER (PARTITION BY account, owner, resource ORDER BY reset_epoch
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS window_id
    FROM resets
),
hourly AS (
    SELECT p.account AS account, p.owner AS owner, p.resource AS resource,
           w.window_id AS window_id,
           toStartOfHour(toDateTime(intDiv(p.ms, 1000))) AS hour,
           max(p.used) AS hwm
    FROM points AS p
    INNER JOIN windows AS w
      ON p.account = w.account AND p.owner = w.owner
     AND p.resource = w.resource AND p.reset_epoch = w.reset_epoch
    GROUP BY account, owner, resource, window_id, hour
),
running AS (
    SELECT account, owner, resource, window_id, hour,
           max(hwm) OVER (PARTITION BY account, owner, resource, window_id ORDER BY hour
                          ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS high_water
    FROM hourly
),
charged AS (
    SELECT account, owner, resource, window_id, hour, high_water,
           high_water - lagInFrame(high_water, 1, toFloat64(0)) OVER
               (PARTITION BY account, owner, resource, window_id ORDER BY hour
                ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS spent
    FROM running
)
SELECT account, owner, resource, hour,
       sum(spent) AS github_used,
       uniqExact(window_id) AS windows,
       max(high_water) AS peak_window_used
FROM charged
GROUP BY account, owner, resource, hour
ORDER BY hour, account, owner, resource;

-- 2. Attributed requests per bucket per hour, last 24 h, summed across hosts
--    (each host counts only its own calls). `cred_owner` is the bucket's
--    owner; `target_owner` (the repo the call was about) is not.
SELECT JSONExtractString(t.labels, 'account') AS account,
       JSONExtractString(t.labels, 'cred_owner') AS owner,
       JSONExtractString(t.labels, 'resource') AS resource,
       toStartOfHour(toDateTime(intDiv(s.unix_milli, 1000))) AS hour,
       sumIf(s.value, JSONExtractString(t.labels, 'outcome') = 'ok') AS attributed_min,
       sumIf(s.value, JSONExtractString(t.labels, 'outcome') IN ('ok', 'error')) AS attributed_max,
       sumIf(s.value, JSONExtractString(t.labels, 'outcome') = 'not_modified') AS free_304,
       sumIf(s.value, JSONExtractString(t.labels, 'outcome') = 'rate_limited') AS refused
FROM signoz_metrics.samples_v4 AS s
INNER JOIN (SELECT fingerprint, any(labels) AS labels
            FROM signoz_metrics.time_series_v4
            WHERE metric_name = 'loom.forge.calls'
            GROUP BY fingerprint) AS t USING (fingerprint)
WHERE s.metric_name = 'loom.forge.calls'
  AND JSONExtractString(t.labels, 'resource') != 'other'
  AND JSONExtractString(t.labels, 'op') != 'quota.rate-limit-reading'
  AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 1 DAY) * 1000
GROUP BY account, owner, resource, hour
ORDER BY hour, account, owner, resource;

-- 3. Shadow band per bucket-hour, last 24 h: queries 1 and 2 joined on
--    (account, owner, resource, hour).
--      shadow_low  = 1 - attributed_max / github_used
--      shadow_high = 1 - attributed_min / github_used
--    NULL (not 0) when GitHub reported no spend. A negative shadow_low means
--    `error` rows over-count local failures for that bucket; a bucket with
--    spend but no attributed row at all is external (or unattributed) spend.
WITH series AS (
    SELECT fingerprint, any(labels) AS labels
    FROM signoz_metrics.time_series_v4
    WHERE metric_name IN ('github.ratelimit.used', 'github.ratelimit.reset')
    GROUP BY fingerprint
),
points AS (
    SELECT JSONExtractString(t.labels, 'account') AS account,
           JSONExtractString(t.labels, 'owner') AS owner,
           JSONExtractString(t.labels, 'resource') AS resource,
           JSONExtractString(t.labels, 'host.id') AS host,
           s.unix_milli AS ms,
           maxIf(s.value, s.metric_name = 'github.ratelimit.used') AS used,
           maxIf(s.value, s.metric_name = 'github.ratelimit.reset') AS reset_epoch,
           countIf(s.metric_name = 'github.ratelimit.used') AS has_used,
           countIf(s.metric_name = 'github.ratelimit.reset') AS has_reset
    FROM signoz_metrics.samples_v4 AS s
    INNER JOIN series AS t USING (fingerprint)
    WHERE s.metric_name IN ('github.ratelimit.used', 'github.ratelimit.reset')
      AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 1 DAY) * 1000
      AND JSONExtractString(t.labels, 'account') != ''
      AND JSONExtractString(t.labels, 'owner') != ''
      AND JSONExtractString(t.labels, 'resource') != ''
    GROUP BY account, owner, resource, host, ms
    HAVING has_used > 0 AND has_reset > 0
),
resets AS (
    SELECT account, owner, resource, reset_epoch,
           if(reset_epoch - lagInFrame(reset_epoch, 1, toFloat64(0)) OVER
                  (PARTITION BY account, owner, resource ORDER BY reset_epoch
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) > 2, 1, 0) AS opens
    FROM (SELECT DISTINCT account, owner, resource, reset_epoch FROM points)
),
windows AS (
    SELECT account, owner, resource, reset_epoch,
           sum(opens) OVER (PARTITION BY account, owner, resource ORDER BY reset_epoch
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS window_id
    FROM resets
),
hourly AS (
    SELECT p.account AS account, p.owner AS owner, p.resource AS resource,
           w.window_id AS window_id,
           toStartOfHour(toDateTime(intDiv(p.ms, 1000))) AS hour,
           max(p.used) AS hwm
    FROM points AS p
    INNER JOIN windows AS w
      ON p.account = w.account AND p.owner = w.owner
     AND p.resource = w.resource AND p.reset_epoch = w.reset_epoch
    GROUP BY account, owner, resource, window_id, hour
),
running AS (
    SELECT account, owner, resource, window_id, hour,
           max(hwm) OVER (PARTITION BY account, owner, resource, window_id ORDER BY hour
                          ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS high_water
    FROM hourly
),
charged AS (
    SELECT account, owner, resource, window_id, hour, high_water,
           high_water - lagInFrame(high_water, 1, toFloat64(0)) OVER
               (PARTITION BY account, owner, resource, window_id ORDER BY hour
                ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS spent
    FROM running
),
spend AS (
    SELECT account, owner, resource, hour, sum(spent) AS github_used
    FROM charged
    GROUP BY account, owner, resource, hour
),
attributed AS (
    SELECT JSONExtractString(t.labels, 'account') AS account,
           JSONExtractString(t.labels, 'cred_owner') AS owner,
           JSONExtractString(t.labels, 'resource') AS resource,
           toStartOfHour(toDateTime(intDiv(s.unix_milli, 1000))) AS hour,
           sumIf(s.value, JSONExtractString(t.labels, 'outcome') = 'ok') AS attributed_min,
           sumIf(s.value, JSONExtractString(t.labels, 'outcome') IN ('ok', 'error')) AS attributed_max
    FROM signoz_metrics.samples_v4 AS s
    INNER JOIN (SELECT fingerprint, any(labels) AS labels
                FROM signoz_metrics.time_series_v4
                WHERE metric_name = 'loom.forge.calls'
                GROUP BY fingerprint) AS t USING (fingerprint)
    WHERE s.metric_name = 'loom.forge.calls'
      AND JSONExtractString(t.labels, 'resource') != 'other'
      AND JSONExtractString(t.labels, 'op') != 'quota.rate-limit-reading'
      AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 1 DAY) * 1000
    GROUP BY account, owner, resource, hour
)
SELECT g.account, g.owner, g.resource, g.hour, g.github_used,
       coalesce(a.attributed_min, 0) AS attributed_min,
       coalesce(a.attributed_max, 0) AS attributed_max,
       round(1 - coalesce(a.attributed_max, 0) / nullIf(g.github_used, 0), 3) AS shadow_low,
       round(1 - coalesce(a.attributed_min, 0) / nullIf(g.github_used, 0), 3) AS shadow_high
FROM spend AS g
LEFT JOIN attributed AS a
  ON g.account = a.account AND g.owner = a.owner
 AND g.resource = a.resource AND g.hour = a.hour
ORDER BY g.hour, g.account, g.owner, g.resource;

-- 4. Span cross-check, last 24 h: `invoke github` spans per bucket, billing
--    class and hour. For `github.billing = 'ok'`, `known_requests` must agree
--    with query 2's attributed_min per host; `unknown_request_spans` is how
--    many spans could not say how many requests they stood for (a porcelain
--    or bare `--paginate` call), and `not_sent` spans never reached GitHub.
SELECT attributes_string['github.account'] AS account,
       attributes_string['github.cred_owner'] AS owner,
       attributes_string['github.resource'] AS resource,
       attributes_string['github.billing'] AS billing,
       toStartOfHour(timestamp) AS hour,
       count() AS spans,
       sum(toUInt32OrZero(attributes_string['github.http.requests'])) AS known_requests,
       countIf(attributes_string['github.http.requests'] = 'unknown') AS unknown_request_spans,
       countIf(attributes_string['github.http.source'] = 'none') AS unknown_status_spans
FROM signoz_traces.signoz_index_v3
WHERE name = 'invoke github'
  AND timestamp >= now() - INTERVAL 1 DAY
GROUP BY account, owner, resource, billing, hour
ORDER BY hour, account, owner, resource, billing;
