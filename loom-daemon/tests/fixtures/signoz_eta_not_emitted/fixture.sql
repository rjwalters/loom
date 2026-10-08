-- Schema-only read surface for the live proof of the saved alert rule
-- `signoz/alerts/eta-not-emitted.json` (#10898) --
-- `loom-daemon/tests/signoz_eta_not_emitted_alert.rs`. Same approach as
-- `fixtures/signoz_queue_starvation/fixture.sql`: the columns the rule's query
-- reads, with the types SigNoz uses, and a FIXED evaluation window the test
-- substitutes for `{{.start_timestamp_ms}}` / `{{.end_timestamp_ms}}`. Each
-- scenario's rows are inserted by the test, so every scenario runs in a fresh
-- engine process.
--
-- `signoz_logs.logs_v2.timestamp` is UInt64 NANOSECONDS; the metric tables
-- use Int64 MILLISECONDS. The rule converts the window bounds for the logs.

CREATE DATABASE IF NOT EXISTS signoz_metrics;
CREATE DATABASE IF NOT EXISTS signoz_logs;
CREATE DATABASE IF NOT EXISTS loom_fixture;

CREATE TABLE signoz_metrics.samples_v4
(
    env LowCardinality(String) DEFAULT 'default',
    temporality LowCardinality(String) DEFAULT 'Unspecified',
    metric_name LowCardinality(String),
    fingerprint UInt64,
    unix_milli Int64,
    value Float64,
    flags UInt32 DEFAULT 0
) ENGINE = Memory;

CREATE TABLE signoz_metrics.time_series_v4
(
    env LowCardinality(String) DEFAULT 'default',
    temporality LowCardinality(String) DEFAULT 'Unspecified',
    metric_name LowCardinality(String),
    description LowCardinality(String) DEFAULT '',
    unit LowCardinality(String) DEFAULT '1',
    type LowCardinality(String) DEFAULT 'Gauge',
    is_monotonic Bool DEFAULT false,
    fingerprint UInt64,
    unix_milli Int64,
    labels String,
    __normalized Bool DEFAULT true
) ENGINE = Memory;

CREATE TABLE signoz_logs.logs_v2
(
    timestamp UInt64,
    body String,
    attributes_string Map(String, String),
    attributes_number Map(String, Float64),
    attributes_bool Map(String, Bool)
) ENGINE = Memory;

-- 2026-10-08T00:00:00Z .. 2026-10-08T02:00:00Z: the rule's 2 h evalWindow.
CREATE TABLE loom_fixture.window ENGINE = Memory AS
SELECT toUnixTimestamp(toDateTime('2026-10-08 00:00:00', 'UTC')) * 1000 AS start_ms,
       toUnixTimestamp(toDateTime('2026-10-08 02:00:00', 'UTC')) * 1000 AS end_ms;
