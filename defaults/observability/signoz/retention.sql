-- Local-trial retention split (#8826): logs and traces 7 days, metrics 30 days.
-- TRIAL-ONLY (#10195): NEVER run this against the live harness-ops store, which
-- keeps logs 3650 days and traces/metrics 10 years. #8946 item 2 stays held.
-- Run AFTER setting the same split in SigNoz General Settings -> Retention
-- (logs 7 days, traces 7 days, metrics 30 days). The application API owns the
-- active signal tables and standard rollups; it leaves these existing
-- auxiliary/legacy tables at their upstream 15/30-day (or one-month) TTLs, so
-- this file sets them explicitly.
--
-- THIS FILE DELETES DATA THE MOMENT YOU RUN IT. `MODIFY TTL` materialises on
-- existing parts, so every row already past the new window goes immediately —
-- not at some later merge, and not recoverably. Read the five measured
-- behaviours below before applying it to a deployment you care about. Each was
-- observed by running this file against the pinned engine
-- (`loom-daemon/tests/signoz_retention_ttl.rs`, #8528), not inferred.
--
-- 1. STATUS 0 IS NOT A RETENTION OUTCOME. A per-host status 0 and an effective
--    `TTL ... + toIntervalDay(7)` in `system.tables` say the DDL parsed and was
--    stored. Whether an over-age row is actually deleted additionally depends on
--    `ttl_only_drop_parts`: where it is set, a part holding one over-age row and
--    one in-window row keeps BOTH, so data well past seven days stays queryable
--    until an unrelated merge rewrites that part. Read that setting alongside
--    the TTL — the README's check query does.
-- 2. THE `/ 1000` IS LOAD-BEARING. Every `*_milli` / `*_ms` column is integer
--    milliseconds. Dropping the divisor does not widen the window: `toDateTime`
--    SATURATES an out-of-range argument at 2106-02-07 06:28:15 and the interval
--    then overflows past it, so the TTL lands in the past and the statement
--    DELETES EVERY ROW IN THE TABLE while still reporting status 0.
-- 3. ONE FAILING STATEMENT ABORTS THE REST. `clickhouse-client --multiquery`
--    stops at the first error (a table a later pin renamed reports status 60,
--    UNKNOWN_TABLE). The metric statements are last, so a partial run shortens
--    logs/traces to 7 days and silently skips every statement that restores 30
--    days to metrics. Check that 16 host rows all report status 0.
-- 4. `MODIFY TTL` REPLACES, it does not merge. A table-level TTL's `DELETE
--    WHERE` / `GROUP BY` qualification is dropped silently (column TTLs
--    survive). That is why the resource fingerprint tables, which carry the
--    upstream 30-minute grace in exactly such a clause, MUST NOT be added here.
-- 5. Re-running is idempotent in DDL — identical effective TTLs, all statuses 0
--    — but not in effect: after an upgrade that restores an upstream qualified
--    TTL, a re-run drops that qualification again (see 4).
--
-- Metrics (>= 30 days) are the CI retro's trend asset: see "Retention" in
-- ../../docs/ci-observability.md. The metric statements below set exactly 30
-- days — they are no-ops on a fresh render (already upstream 30 days) and
-- restore the 30-day POLICY on a trial that ran the earlier all-seven-day
-- version of this file, which shortened them. They cannot restore the DATA:
-- that version deleted every metric row between 8 and 30 days old when it ran,
-- and re-widening the window afterwards returns nothing.
--
-- Apply only to this pinned isolated trial, then re-run queries.sql and the
-- effective-DDL check in the README's "Retention and operation" section.
-- Preserve schema/configuration tables and the resource-table 30-minute grace.

-- Logs and traces: 7 days.
ALTER TABLE signoz_logs.tag_attributes_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.tag_attributes_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.durationSort ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.signoz_index_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.signoz_spans ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.top_level_operations ON CLUSTER cluster
    MODIFY TTL time + INTERVAL 7 DAY;

-- Metrics: 30 days.
ALTER TABLE signoz_metrics.metadata ON CLUSTER cluster
    MODIFY TTL toDateTime(last_reported_unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp_ms / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_30m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_5m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_60s ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_30m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_5m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_60s ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.time_series_v4_reduced ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.time_series_v4_reduced_1day ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
