-- Synthetic SigNoz metric rows for the live proof of
-- `signoz/github-shadow.sql` (#10343) -- `loom-daemon/tests/signoz_github_shadow_queries.rs`.
--
-- Same READ-surface schema as `signoz_queue_quota/fixture.sql` (the pinned
-- SigNoz metric tables): `labels` is a JSON STRING, timestamps are
-- MILLISECONDS, and every series has TWO `time_series_v4` hour-rows so a
-- query without the de-duplicating fingerprint sub-select double-counts.
--
-- The committed queries are windowed on `now()`, so rows hang off one
-- computed anchor: `base_ms` = toStartOfHour(now() - 3 h) (hour H, inside the
-- 24 h range, with H+1 also complete). A `github.ratelimit.reset` value is
-- `base_s + reset_offset_s`, an epoch in SECONDS as the daemon exports it.
--
-- Bucket A `{app-1, acme, core}` -- stale hosts, a genuine reset, jitter:
--   window 1 (reset H+40m):  h1 100 @1m, h1 150 @2m, h2 120 @3m (stale),
--                            h2 120 @4m (h1 gone, stale reading survives),
--                            h2 160 @5m           -> high-water 160
--   window 2 (reset H+100m, and H+100m+1s from h1 -- jitter, same window):
--                            h2 10 @45m, h1 30 @50m -> 30 in hour H
--                            h2 70 @H+1h10m         -> +40 in hour H+1
--   Expected: hour H = 160 + 30 = 190, hour H+1 = 40. The pre-fix recipe
--   (any drop = reset, whole lower value charged) billed 240 in hour H.
--
-- Bucket B `{app-2, beta, graphql}` -- two buckets interleaved under one
-- label set, as a pre-#10571 daemon could export (rjwalters/loom#10571): window X (reset H+30m) 4000 @1m,
-- 4100 @3m, 4150 @5m; window Y (reset H+50m) 30 @2m, 45 @4m. Expected hour
-- H = 4150 + 45 = 4195; the pre-fix recipe re-charged X on every switch and
-- billed 8250.
--
-- A pre-#10343 owner-less point `{app-1, core}` reading 9999 must be ignored.
--
-- Installations (#10571): bucket A and its `loom.forge.calls` are
-- installation 11, bucket B 22; the legacy point has none. Query 0 counts 2
-- for `github.ratelimit.used`, 1 for `loom.forge.calls`.
--
-- `loom.forge.calls` for bucket A in hour H: ok 150, error 20,
-- not_modified 50, plus a free `rate_limit` probe (`resource=other`) of 7.
-- Expected query 3 band: shadow_low = round(1 - 170/190, 3) = 0.105,
-- shadow_high = round(1 - 150/190, 3) = 0.211.

CREATE DATABASE IF NOT EXISTS signoz_metrics;
CREATE DATABASE IF NOT EXISTS signoz_traces;
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

CREATE TABLE signoz_traces.signoz_index_v3
(
    timestamp DateTime64(9),
    trace_id String,
    span_id String,
    parent_span_id String,
    name LowCardinality(String),
    duration_nano UInt64,
    status_code_string LowCardinality(String),
    attributes_string Map(LowCardinality(String), String),
    attributes_number Map(LowCardinality(String), Float64),
    attributes_bool Map(LowCardinality(String), Bool),
    resources_string Map(LowCardinality(String), String)
) ENGINE = Memory;

CREATE TABLE loom_fixture.anchor ENGINE = Memory AS
SELECT toUnixTimestamp(toStartOfHour(now() - INTERVAL 3 HOUR)) * 1000 AS base_ms,
       toUnixTimestamp(toStartOfHour(now() - INTERVAL 3 HOUR)) AS base_s,
       toDateTime64(toStartOfHour(now() - INTERVAL 3 HOUR), 9) AS base_ts;

CREATE TABLE loom_fixture.series_seed
(
    metric_name String,
    fingerprint UInt64,
    type String,
    temporality String,
    labels String
) ENGINE = Memory;

INSERT INTO loom_fixture.series_seed VALUES
    ('github.ratelimit.used',  11, 'Gauge', 'Unspecified', '{"account":"app-1","owner":"acme","resource":"core","role":"writer","installation":"11","host.id":"h1"}'),
    ('github.ratelimit.reset', 12, 'Gauge', 'Unspecified', '{"account":"app-1","owner":"acme","resource":"core","role":"writer","installation":"11","host.id":"h1"}'),
    ('github.ratelimit.used',  21, 'Gauge', 'Unspecified', '{"account":"app-1","owner":"acme","resource":"core","role":"writer","installation":"11","host.id":"h2"}'),
    ('github.ratelimit.reset', 22, 'Gauge', 'Unspecified', '{"account":"app-1","owner":"acme","resource":"core","role":"writer","installation":"11","host.id":"h2"}'),
    ('github.ratelimit.used',  31, 'Gauge', 'Unspecified', '{"account":"app-2","owner":"beta","resource":"graphql","role":"reader","installation":"22","host.id":"h1"}'),
    ('github.ratelimit.reset', 32, 'Gauge', 'Unspecified', '{"account":"app-2","owner":"beta","resource":"graphql","role":"reader","installation":"22","host.id":"h1"}'),
    ('github.ratelimit.used',  41, 'Gauge', 'Unspecified', '{"account":"app-1","resource":"core","host.id":"h3"}'),
    ('github.ratelimit.reset', 42, 'Gauge', 'Unspecified', '{"account":"app-1","resource":"core","host.id":"h3"}'),
    ('loom.forge.calls', 51, 'Sum', 'Delta', '{"account":"app-1","cred_owner":"acme","installation":"11","resource":"core","outcome":"ok","op":"issue.list","caller":"x","role":"writer","target_owner":"acme","host.id":"h1"}'),
    ('loom.forge.calls', 52, 'Sum', 'Delta', '{"account":"app-1","cred_owner":"acme","installation":"11","resource":"core","outcome":"error","op":"issue.list","caller":"x","role":"writer","target_owner":"acme","host.id":"h1"}'),
    ('loom.forge.calls', 53, 'Sum', 'Delta', '{"account":"app-1","cred_owner":"acme","installation":"11","resource":"core","outcome":"not_modified","op":"issue.list","caller":"x","role":"writer","target_owner":"acme","host.id":"h1"}'),
    ('loom.forge.calls', 54, 'Sum', 'Delta', '{"account":"app-1","cred_owner":"acme","installation":"11","resource":"other","outcome":"ok","op":"quota.rate-limit-reading","caller":"api.rate_limit","role":"writer","target_owner":"acme","host.id":"h1"}');

-- Two hour-rows per series (the SigNoz trap the sub-select exists for).
INSERT INTO signoz_metrics.time_series_v4
    (metric_name, type, temporality, fingerprint, unix_milli, labels)
SELECT s.metric_name, s.type, s.temporality, s.fingerprint, a.base_ms + h * 3600000, s.labels
FROM loom_fixture.series_seed AS s
CROSS JOIN loom_fixture.anchor AS a
CROSS JOIN (SELECT arrayJoin([0, 1]) AS h) AS hours;

-- One exported reading = a `used` point and a `reset` point at the same
-- instant on the same host (`used_fp`, `reset_fp`).
CREATE TABLE loom_fixture.reading_seed
(
    used_fp UInt64,
    reset_fp UInt64,
    offset_s Int64,
    used Float64,
    reset_offset_s Int64
) ENGINE = Memory;

-- Rows, in order: bucket A window 1 (reset H+40m = 2400 s, five readings,
-- two of them stale from h2); bucket A window 2 (reset H+100m = 6000 s, h1
-- sees 6001 -- jitter); bucket B's two interleaved windows (X 1800 s, Y
-- 3000 s); the ignored legacy owner-less point. ClickHouse does not accept
-- comments inside a VALUES list, so they live here.
INSERT INTO loom_fixture.reading_seed VALUES
    (11, 12,   60, 100, 2400),
    (11, 12,  120, 150, 2400),
    (21, 22,  180, 120, 2400),
    (21, 22,  240, 120, 2400),
    (21, 22,  300, 160, 2400),
    (21, 22, 2700,  10, 6000),
    (11, 12, 3000,  30, 6001),
    (21, 22, 4200,  70, 6000),
    (31, 32,   60, 4000, 1800),
    (31, 32,  120,   30, 3000),
    (31, 32,  180, 4100, 1800),
    (31, 32,  240,   45, 3000),
    (31, 32,  300, 4150, 1800),
    (41, 42,  360, 9999, 2400);

INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'github.ratelimit.used', r.used_fp, a.base_ms + r.offset_s * 1000, r.used
FROM loom_fixture.reading_seed AS r CROSS JOIN loom_fixture.anchor AS a;

INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'github.ratelimit.reset', r.reset_fp, a.base_ms + r.offset_s * 1000,
       toFloat64(a.base_s + r.reset_offset_s)
FROM loom_fixture.reading_seed AS r CROSS JOIN loom_fixture.anchor AS a;

-- `loom.forge.calls` deltas in hour H (two flushes for `ok`).
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.forge.calls', c.fp, a.base_ms + c.offset_s * 1000, c.v
FROM (SELECT arrayJoin([(51, 600, 100.0), (51, 1200, 50.0), (52, 600, 20.0),
                        (53, 600, 50.0), (54, 600, 7.0)]) AS t,
             t.1 AS fp, t.2 AS offset_s, t.3 AS v) AS c
CROSS JOIN loom_fixture.anchor AS a;

-- Two `invoke github` spans for query 4.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string)
SELECT a.base_ts + INTERVAL 10 MINUTE, 't1', 's1', '', 'invoke github', 1000, 'Ok',
       map('github.account', 'app-1', 'github.cred_owner', 'acme', 'github.resource', 'core',
           'github.billing', 'ok', 'github.http.requests', '3', 'github.http.source', 'headers')
FROM loom_fixture.anchor AS a
UNION ALL
SELECT a.base_ts + INTERVAL 11 MINUTE, 't2', 's2', '', 'invoke github', 1000, 'Error',
       map('github.account', 'app-1', 'github.cred_owner', 'acme', 'github.resource', 'core',
           'github.billing', 'ok', 'github.http.requests', 'unknown', 'github.http.source', 'none')
FROM loom_fixture.anchor AS a;
