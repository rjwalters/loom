-- Run with the bundled clickhouse-client in the private ClickHouse container.
-- Replace this synthetic trace ID with the shared fixture/canary manifest ID.
SELECT trace_id, span_id, parent_span_id, name, kind_string,
       status_code_string, timestamp, duration_nano
FROM signoz_traces.signoz_index_v3
WHERE trace_id = '85270000000000000000000000000001'
ORDER BY timestamp, span_id;

-- Count replay duplicates separately; UI and storage need not deduplicate.
SELECT count() AS rows, uniqExact(trace_id, span_id) AS unique_spans
FROM signoz_traces.signoz_index_v3
WHERE trace_id = '85270000000000000000000000000001';

SELECT timestamp, trace_id, span_id, severity_text, body
FROM signoz_logs.logs_v2
WHERE trace_id = '85270000000000000000000000000001'
ORDER BY timestamp;

-- Metrics use millisecond timestamps; compare with manifest ns / 1,000,000.
SELECT metric_name, unix_milli, value
FROM signoz_metrics.samples_v4
WHERE metric_name = 'loom.host.synthetic_capacity'
ORDER BY unix_milli;

-- Verify retention in real local table DDL, not only the UI setting.
-- Distributed tables and materialized views do not own independent storage.
SELECT database, name, engine, create_table_query
FROM system.tables
WHERE database IN ('signoz_logs', 'signoz_traces', 'signoz_metrics')
  AND engine LIKE '%MergeTree%'
ORDER BY database, name;

SELECT database, table, sum(bytes_on_disk) AS active_bytes, sum(rows) AS rows
FROM system.parts
WHERE active AND database IN ('signoz_logs', 'signoz_traces', 'signoz_metrics')
GROUP BY database, table
ORDER BY database, table;
