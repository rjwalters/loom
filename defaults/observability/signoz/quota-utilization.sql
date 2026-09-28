-- Subscription quota utilization (Issue #9005). Standing queries over the
-- per-account `tokens.snapshot` gauges: `loom.tokens.usage_fraction` (5-hour
-- window), `loom.tokens.usage_fraction_weekly` (rolling 7-day window) and
-- `loom.tokens.exhausted`, each labelled `account` and `provider`. Run with the
-- bundled clickhouse-client in the private ClickHouse container. Metric
-- timestamps are milliseconds.
--
-- Absent is not zero. A provider with no utilization source (Codex,
-- OpenCode/Z.ai, Kimi) emits no usage series at all, only `exhausted`, so every
-- utilization column below is NULL for it, never 0. That is why query 1 uses
-- maxOrNullIf(): plain maxIf() returns 0 when no row matches.
--
-- time_series_v4 holds one row per series per hour, so every query joins a
-- de-duplicated fingerprint set. The same account can be reported by several
-- hosts sharing one pool; utilization is account-wide at the provider, so
-- queries take the max across hosts.

-- 1. Per-account 5h and weekly utilization, hourly peak, last 7 days.
--    (The panel: one line per account per window.)
SELECT JSONExtractString(t.labels, 'provider') AS provider,
       JSONExtractString(t.labels, 'account') AS account,
       toStartOfHour(toDateTime(intDiv(s.unix_milli, 1000))) AS hour,
       maxOrNullIf(s.value, s.metric_name = 'loom.tokens.usage_fraction') AS util_5h,
       maxOrNullIf(s.value, s.metric_name = 'loom.tokens.usage_fraction_weekly') AS util_weekly
FROM signoz_metrics.samples_v4 AS s
INNER JOIN (SELECT fingerprint, any(labels) AS labels
            FROM signoz_metrics.time_series_v4
            WHERE metric_name IN ('loom.tokens.usage_fraction', 'loom.tokens.usage_fraction_weekly')
            GROUP BY fingerprint) AS t USING (fingerprint)
WHERE s.metric_name IN ('loom.tokens.usage_fraction', 'loom.tokens.usage_fraction_weekly')
  AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 7 DAY) * 1000
GROUP BY provider, account, hour
ORDER BY hour, provider, account;

-- 2. Idle headroom at each weekly reset, last 30 days: the utilization an
--    account had reached when its 7-day window rolled over, and the unused
--    capacity (1 - that) thrown away at the reset. A reset is detected as a
--    drop of at least 0.10 from the previous sample; the value before the drop
--    is the window's final utilization. Sampling is every ~5 min but the
--    ranking refresh is ~10 min, so the final reading can trail the true
--    end-of-window value slightly (headroom is an upper bound).
WITH samples AS (
    SELECT JSONExtractString(t.labels, 'provider') AS provider,
           JSONExtractString(t.labels, 'account') AS account,
           s.unix_milli AS ms,
           max(s.value) AS value
    FROM signoz_metrics.samples_v4 AS s
    INNER JOIN (SELECT fingerprint, any(labels) AS labels
                FROM signoz_metrics.time_series_v4
                WHERE metric_name = 'loom.tokens.usage_fraction_weekly'
                GROUP BY fingerprint) AS t USING (fingerprint)
    WHERE s.metric_name = 'loom.tokens.usage_fraction_weekly'
      AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 30 DAY) * 1000
    GROUP BY provider, account, ms
),
ordered AS (
    SELECT provider, account, ms, value,
           lagInFrame(toNullable(value)) OVER w AS prev_value,
           lagInFrame(toNullable(ms)) OVER w AS prev_ms
    FROM samples
    WINDOW w AS (PARTITION BY provider, account ORDER BY ms
                 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
)
SELECT provider,
       account,
       toDateTime(intDiv(prev_ms, 1000)) AS window_end,
       round(prev_value, 3) AS util_at_reset,
       round(1 - least(prev_value, 1), 3) AS idle_headroom
FROM ordered
WHERE prev_value IS NOT NULL AND prev_value - value >= 0.10
ORDER BY window_end, provider, account;

-- 3. What fraction of available subscription capacity did we use last week,
--    per provider? Each account contributes its final weekly utilization from
--    its most recent reset in the last 7 days (query 2's detection), or, when
--    no reset was seen in that span, the highest weekly reading it reached.
--    `accounts` counts every account the pool reported (via `exhausted`, which
--    every provider emits); `accounts_measured` counts those with a weekly
--    source. A provider with none reads coverage = 'unknown' and a NULL
--    fraction: no source, no number.
WITH weekly AS (
    SELECT JSONExtractString(t.labels, 'provider') AS provider,
           JSONExtractString(t.labels, 'account') AS account,
           s.unix_milli AS ms,
           max(s.value) AS value
    FROM signoz_metrics.samples_v4 AS s
    INNER JOIN (SELECT fingerprint, any(labels) AS labels
                FROM signoz_metrics.time_series_v4
                WHERE metric_name = 'loom.tokens.usage_fraction_weekly'
                GROUP BY fingerprint) AS t USING (fingerprint)
    WHERE s.metric_name = 'loom.tokens.usage_fraction_weekly'
      AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 7 DAY) * 1000
    GROUP BY provider, account, ms
),
ordered AS (
    SELECT provider, account, ms, value,
           lagInFrame(toNullable(value)) OVER w AS prev_value
    FROM weekly
    WINDOW w AS (PARTITION BY provider, account ORDER BY ms
                 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
),
per_account_weekly AS (
    SELECT provider, account,
           if(countIf(prev_value - value >= 0.10) > 0,
              argMaxIf(prev_value, ms, prev_value - value >= 0.10),
              max(value)) AS used_fraction
    FROM ordered
    GROUP BY provider, account
),
pool AS (
    SELECT DISTINCT JSONExtractString(labels, 'provider') AS provider,
                    JSONExtractString(labels, 'account') AS account
    FROM signoz_metrics.time_series_v4
    WHERE metric_name = 'loom.tokens.exhausted'
      AND unix_milli >= toUnixTimestamp(now() - INTERVAL 7 DAY) * 1000
)
SELECT p.provider AS provider,
       count() AS accounts,
       countIf(w.used_fraction IS NOT NULL) AS accounts_measured,
       if(accounts_measured = 0, 'unknown', 'measured') AS coverage,
       round(avg(w.used_fraction), 3) AS used_fraction_last_week,
       round(1 - avg(if(w.used_fraction > 1, 1, w.used_fraction)), 3) AS idle_fraction_last_week
FROM pool AS p
LEFT JOIN per_account_weekly AS w ON w.provider = p.provider AND w.account = p.account
GROUP BY provider
ORDER BY provider
SETTINGS join_use_nulls = 1;
