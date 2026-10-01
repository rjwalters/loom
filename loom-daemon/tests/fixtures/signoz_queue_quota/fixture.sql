-- Synthetic SigNoz metric and trace rows for the live proof of
-- `signoz/queue-dwell.sql` (#8856) and `signoz/quota-utilization.sql` (#9005)
-- -- `loom-daemon/tests/signoz_queue_quota_queries.rs`, Issue #8528 scope
-- item 4 ("host/token gauges").
--
-- The column set and types mirror the pinned SigNoz v0.142.1 metric schema's
-- READ surface: `samples_v4(metric_name, fingerprint, unix_milli Int64, value
-- Float64)` and `time_series_v4(metric_name, fingerprint, unix_milli Int64,
-- labels String)` -- `labels` being a JSON **string** the committed queries
-- read with `JSONExtractString`, not a Map. The non-read columns present here
-- (`env`, `temporality`, `description`, `unit`, `type`, `is_monotonic`,
-- `flags`, `__normalized`) exist so a query that reached for one would fail
-- rather than silently not compile against a narrower mock.
--
-- Every committed query in those two files is windowed on `now()` -- 24 h,
-- 7 days, 30 days -- so this fixture CANNOT use fixed timestamps. It anchors
-- every row to one of three computed points, and every anchor is deliberately
-- boundary-aligned so the committed queries' own `toStartOfInterval(..., 5
-- MINUTE)` / `toStartOfHour` / `toDate` grouping lands in a predictable number
-- of buckets no matter what time the test runs:
--
--   recent_ms  toStartOfHour(now() - 2 h)                  -- hour-aligned, so
--              also 5-minute-aligned; 1-3 h old, inside the 24 h window.
--   day_a_ms   toStartOfDay(now() - 3 days) + 1 h          -- 3-4 days old, one
--   day_b_ms   toStartOfDay(now() - 2 days) + 1 h          -- day apart, both
--              inside the 7-day and 30-day windows. Day-aligned + 1 h, so no
--              offset below can straddle midnight and split a `toDate` group.
--
-- Metric timestamps are MILLISECONDS, as both artifacts' headers state. The
-- trace rows use `DateTime64(9)` nanosecond timestamps, as the real trace
-- schema does.
--
-- Trace/span attributes are ALL strings, because every dispatch span attribute
-- is emitted from a `BTreeMap<String, String>` through `kv_string`
-- (`observability/ops/disposition.rs` inserts `rank.to_string()`,
-- `score.clone()`, ...). `attributes_number` exists on the table and stays
-- EMPTY, which is what lets the test run the wrong-container read as a
-- negative control.

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
SELECT toUnixTimestamp(toStartOfHour(now() - INTERVAL 2 HOUR)) * 1000 AS recent_ms,
       toUnixTimestamp(toStartOfDay(now() - INTERVAL 3 DAY) + INTERVAL 1 HOUR) * 1000 AS day_a_ms,
       toUnixTimestamp(toStartOfDay(now() - INTERVAL 2 DAY) + INTERVAL 1 HOUR) * 1000 AS day_b_ms,
       toDateTime64(toStartOfHour(now() - INTERVAL 2 HOUR), 9) AS recent_ts,
       toDateTime64(toStartOfDay(now() - INTERVAL 3 DAY) + INTERVAL 1 HOUR, 9) AS day_a_ts;

-- ===========================================================================
-- Metric data points. `anchor` names which of the three computed points the
-- row hangs off; `offset_ms` is added to it.
-- ===========================================================================

CREATE TABLE loom_fixture.sample_seed
(
    metric_name String,
    fingerprint UInt64,
    anchor LowCardinality(String),
    offset_ms Int64,
    value Float64
) ENGINE = Memory;

-- queue-dwell query 1: `loom.queue.starved` per host and state, last 24 h,
-- bucketed to 5 minutes with `max()` and filtered by `HAVING starved > 0`.
--
-- fp 1001 (host-aaa/ready) gets THREE points inside ONE 5-minute bucket, at a
-- rising-then-falling value, so the bucket's answer is 3 only if the query
-- takes the maximum rather than the last or the sum. fp 1001 also gets a
-- fourth point three days back, which the 24 h window must exclude.
--
-- fp 1002 (host-bbb/ready) is reporting a measured ZERO: the host is healthy.
-- `HAVING starved > 0` must drop it, and the test checks it is absent.
-- fp 1003 (host-aaa/curated) is a second state on the same host.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.queue.starved', 1001, 'recent',      0, 2),
('loom.queue.starved', 1001, 'recent',  60000, 3),
('loom.queue.starved', 1001, 'recent', 120000, 1),
('loom.queue.starved', 1001, 'day_a',       0, 9),
('loom.queue.starved', 1002, 'recent',      0, 0),
('loom.queue.starved', 1003, 'recent',      0, 1);

-- queue-dwell query 2: `loom.queue.starved.by_reason`, peak per reason.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.queue.starved.by_reason', 1101, 'recent',     0, 2),
('loom.queue.starved.by_reason', 1101, 'recent', 60000, 3),
('loom.queue.starved.by_reason', 1102, 'recent',     0, 1);

-- queue-dwell query 3: `loom.queue.oldest_wait` SECONDS, reported as hours
-- (`max(value) / 3600`), hourly peak over 7 days. 25200 s is exactly 7 h.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.queue.oldest_wait', 1201, 'day_a',      0, 25200),
('loom.queue.oldest_wait', 1201, 'day_a', 300000, 21600),
('loom.queue.oldest_wait', 1201, 'day_b',      0,  3600),
('loom.queue.oldest_wait', 1202, 'day_b',      0,  1800);

-- queue-dwell query 4: the one query in the file that SUMS, and therefore the
-- one that must de-duplicate `time_series_v4`'s per-hour rows. host-aaa's two
-- points sum to 1800 wait-seconds over 6 dispatches = 300 s = 5.0 min mean.
-- Both of its fingerprints carry two hour-rows below, so a plain join would
-- answer 3600/12 -- the same mean, but double the wait-seconds total.
--
-- host-bbb reports wait-seconds with NO `.samples` series at all (the drift a
-- renamed or dropped companion metric would cause). `nullIf(dispatches, 0)`
-- must make the mean NULL, never 0 and never a division error.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.queue.dispatch_wait',         1301, 'day_a',       0, 900),
('loom.queue.dispatch_wait',         1301, 'day_a', 3600000, 900),
('loom.queue.dispatch_wait.samples', 1302, 'day_a',       0,   4),
('loom.queue.dispatch_wait.samples', 1302, 'day_a', 3600000,   2),
('loom.queue.dispatch_wait',         1303, 'day_b',       0, 600);

-- quota-utilization: `loom.tokens.exhausted` is emitted for EVERY account
-- unconditionally (`otlp/mapping.rs`'s TokensSnapshot arm pushes it outside
-- any `if let`), which is why query 3's `pool` CTE uses it to count the whole
-- pool. acct-z (fp 2003) deliberately has NO utilization series of either kind
-- below: a provider with no usage source at all. It must vanish from query 1
-- and read coverage = 'unknown' with NULL fractions in query 3 -- never 0%.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.tokens.exhausted', 2001, 'day_a', 0, 0),
('loom.tokens.exhausted', 2002, 'day_a', 0, 0),
('loom.tokens.exhausted', 2003, 'day_a', 0, 1);

-- `loom.tokens.usage_fraction` (5 h window) is emitted ONLY when the provider
-- has a utilization source. acct-a has one on both days; acct-b only on day_b,
-- so acct-b's day_a row must read util_5h = NULL, not 0.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.tokens.usage_fraction', 2101, 'day_a', 0, 0.30),
('loom.tokens.usage_fraction', 2101, 'day_b', 0, 0.55),
('loom.tokens.usage_fraction', 2102, 'day_b', 0, 0.20);

-- `loom.tokens.usage_fraction_weekly` climbs, then drops across the weekly
-- reset. Query 2 detects the reset as a fall of >= 0.10 and reports the value
-- BEFORE the fall. acct-b's pre-reset reading is 1.05 -- over 1.0, which the
-- `least(prev_value, 1)` clamp must turn into 0 idle headroom, not -0.05.
INSERT INTO loom_fixture.sample_seed
    (metric_name, fingerprint, anchor, offset_ms, value)
VALUES
('loom.tokens.usage_fraction_weekly', 2201, 'day_a',       0, 0.40),
('loom.tokens.usage_fraction_weekly', 2201, 'day_a', 1800000, 0.82),
('loom.tokens.usage_fraction_weekly', 2201, 'day_b',       0, 0.05),
('loom.tokens.usage_fraction_weekly', 2202, 'day_a',       0, 0.60),
('loom.tokens.usage_fraction_weekly', 2202, 'day_a', 1800000, 1.05),
('loom.tokens.usage_fraction_weekly', 2202, 'day_b',       0, 0.10);

INSERT INTO signoz_metrics.samples_v4
    (metric_name, fingerprint, unix_milli, value)
SELECT s.metric_name,
       s.fingerprint,
       multiIf(s.anchor = 'recent', a.recent_ms,
               s.anchor = 'day_a', a.day_a_ms,
               a.day_b_ms) + s.offset_ms,
       s.value
FROM loom_fixture.sample_seed AS s
CROSS JOIN loom_fixture.anchor AS a;

-- ===========================================================================
-- Series metadata. SigNoz writes ONE `time_series_v4` row per series per
-- hour, which both artifacts' headers call out: the `fingerprint` join
-- multiplies every sample by however many hour-rows its series has.
-- Fingerprints 1001, 1201, 1301, 1302, 2201 and 2202 get TWO rows each so that
-- multiplication is real in this fixture, not hypothetical -- harmless for
-- `max()`, fatal for `sum()` without the de-duplicating sub-select.
--
-- `labels` carries only the keys the committed queries read. The real
-- TokensSnapshot mapping also attaches `rank` via `kv_int`, which is omitted
-- here rather than guessed at: how the SigNoz metrics exporter renders an
-- INTEGER attribute into this JSON string has not been observed on the trial,
-- and no committed query reads it.
-- ===========================================================================

CREATE TABLE loom_fixture.series_seed
(
    metric_name String,
    fingerprint UInt64,
    labels String,
    anchor LowCardinality(String),
    offset_ms Int64
) ENGINE = Memory;

INSERT INTO loom_fixture.series_seed
    (metric_name, fingerprint, labels, anchor, offset_ms)
VALUES
('loom.queue.starved', 1001, '{"host.id":"host-aaa","state":"ready"}', 'recent', 0),
('loom.queue.starved', 1001, '{"host.id":"host-aaa","state":"ready"}', 'recent', -3600000),
('loom.queue.starved', 1002, '{"host.id":"host-bbb","state":"ready"}', 'recent', 0),
('loom.queue.starved', 1003, '{"host.id":"host-aaa","state":"curated"}', 'recent', 0),
('loom.queue.starved.by_reason', 1101, '{"reason":"concurrency_cap"}', 'recent', 0),
('loom.queue.starved.by_reason', 1102, '{"reason":"repo_slice"}', 'recent', 0),
('loom.queue.oldest_wait', 1201, '{"host.id":"host-aaa","state":"ready"}', 'day_a', 0),
('loom.queue.oldest_wait', 1201, '{"host.id":"host-aaa","state":"ready"}', 'day_b', 0),
('loom.queue.oldest_wait', 1202, '{"host.id":"host-bbb","state":"ready"}', 'day_b', 0),
('loom.queue.dispatch_wait', 1301, '{"host.id":"host-aaa"}', 'day_a', 0),
('loom.queue.dispatch_wait', 1301, '{"host.id":"host-aaa"}', 'day_a', 3600000),
('loom.queue.dispatch_wait.samples', 1302, '{"host.id":"host-aaa"}', 'day_a', 0),
('loom.queue.dispatch_wait.samples', 1302, '{"host.id":"host-aaa"}', 'day_a', 3600000),
('loom.queue.dispatch_wait', 1303, '{"host.id":"host-bbb"}', 'day_b', 0),
('loom.tokens.exhausted', 2001, '{"provider":"anthropic","account":"acct-a"}', 'day_a', 0),
('loom.tokens.exhausted', 2002, '{"provider":"anthropic","account":"acct-b"}', 'day_a', 0),
('loom.tokens.exhausted', 2003, '{"provider":"zai","account":"acct-z"}', 'day_a', 0),
('loom.tokens.usage_fraction', 2101, '{"provider":"anthropic","account":"acct-a"}', 'day_a', 0),
('loom.tokens.usage_fraction', 2101, '{"provider":"anthropic","account":"acct-a"}', 'day_b', 0),
('loom.tokens.usage_fraction', 2102, '{"provider":"anthropic","account":"acct-b"}', 'day_b', 0),
('loom.tokens.usage_fraction_weekly', 2201, '{"provider":"anthropic","account":"acct-a"}', 'day_a', 0),
('loom.tokens.usage_fraction_weekly', 2201, '{"provider":"anthropic","account":"acct-a"}', 'day_b', 0),
('loom.tokens.usage_fraction_weekly', 2202, '{"provider":"anthropic","account":"acct-b"}', 'day_a', 0),
('loom.tokens.usage_fraction_weekly', 2202, '{"provider":"anthropic","account":"acct-b"}', 'day_b', 0);

INSERT INTO signoz_metrics.time_series_v4
    (metric_name, fingerprint, unix_milli, labels)
SELECT s.metric_name,
       s.fingerprint,
       multiIf(s.anchor = 'recent', a.recent_ms,
               s.anchor = 'day_a', a.day_a_ms,
               a.day_b_ms) + s.offset_ms,
       s.labels
FROM loom_fixture.series_seed AS s
CROSS JOIN loom_fixture.anchor AS a;

-- ===========================================================================
-- queue-dwell query 5: `loom.dispatch.disposition` / `loom.dispatch.admission`
-- SPANS (#9222, #9669) -- a different signal from the gauges above, in a
-- different database. The question is "why hasn't owner/repo#98 started?", so
-- the fixture includes three rows that must answer it and three that must be
-- filtered out: another issue, another repo, and the same issue outside the
-- 24 h window.
-- ===========================================================================

-- The parent tick. Excluded from query 5 by the name filter, and present so
-- the trailing note's documented fallback -- joining a cause-less disposition
-- row to its parent tick's `loom.dispatch.result` -- is exercisable.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts, 'QT1', 'TICK1', '', 'loom.dispatch.tick', 250000000, 'Ok',
       map('loom.dispatch.result', 'halted_main_red'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

-- A capacity-independent HALT, with its #9673 cause token present.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts, 'QT1', 'DISP1', 'TICK1', 'loom.dispatch.disposition', 1000000, 'Ok',
       map('loom.repo', 'owner/repo', 'loom.issue', '98',
           'loom.queue.disposition', 'workspace_halted',
           'loom.queue.state', 'ready',
           'loom.queue.rank', '1',
           'loom.queue.candidate_rank', '1',
           'loom.queue.total_candidates', '4',
           'loom.queue.priority_score', '0.87',
           'loom.queue.halt_cause', 'main_red'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

-- A minute later, a LEGACY halted row: `loom.queue.disposition` is
-- `workspace_halted` but `loom.queue.halt_cause` is absent from the map
-- entirely, the shape every such row had before #9673 added the cause token.
-- The committed query reads the key with
-- `attributes_string['loom.queue.halt_cause']`, which yields '' rather than
-- erroring, and the file's trailing note sends the reader to the parent tick's
-- `loom.dispatch.result` instead. Both halves are observed by the test.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(60), 'QTL', 'DISPL', 'TICKL',
       'loom.dispatch.disposition', 1000000, 'Ok',
       map('loom.repo', 'owner/repo', 'loom.issue', '98',
           'loom.queue.disposition', 'workspace_halted',
           'loom.queue.state', 'ready',
           'loom.queue.rank', '1'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(60), 'QTL', 'TICKL', '',
       'loom.dispatch.tick', 250000000, 'Ok',
       map('loom.dispatch.result', 'halted_main_red'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

-- Five minutes in, the same issue is held by capacity instead. A capacity row
-- never carries a halt cause (only a `workspace_halted` disposition can), so
-- its `halt_cause` cell is empty for a second, different reason than the
-- legacy row's: the attribute does not apply, rather than predating the field.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(300), 'QT2', 'DISP2', 'TICK2',
       'loom.dispatch.disposition', 1000000, 'Ok',
       map('loom.repo', 'owner/repo', 'loom.issue', '98',
           'loom.queue.disposition', 'capacity',
           'loom.queue.state', 'ready',
           'loom.queue.rank', '2',
           'loom.queue.candidate_rank', '2',
           'loom.queue.total_candidates', '4',
           'loom.queue.priority_score', '0.61',
           'loom.queue.previous_disposition', 'workspace_halted',
           'loom.queue.transition', 'workspace_halted->capacity'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(300), 'QT2', 'TICK2', '',
       'loom.dispatch.tick', 250000000, 'Ok',
       map('loom.dispatch.result', 'capacity_full'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

-- One second after that disposition, the tick's actual dispatch attempt was
-- rejected. `loom.dispatch.admission` adds the attempt's outcome; it is the
-- newest row, so it must sort FIRST under `ORDER BY timestamp DESC`.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(301), 'QT2', 'ADM1', 'TICK2',
       'loom.dispatch.admission', 2000000, 'Ok',
       map('loom.repo', 'owner/repo', 'loom.issue', '98',
           'loom.queue.state', 'ready',
           'loom.dispatch.admission_result', 'rejected',
           'loom.dispatch.reason', 'concurrency_cap'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

-- Negative control 1: a different issue in the same repo, newer than all three
-- rows above. If the issue predicate regressed it would sort first and the
-- test's row ordering would fail loudly.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(600), 'QT3', 'DISP3', 'TICK3',
       'loom.dispatch.disposition', 1000000, 'Ok',
       map('loom.repo', 'owner/repo', 'loom.issue', '97',
           'loom.queue.disposition', 'dispatched',
           'loom.queue.state', 'ready'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;

-- Negative control 2: the same issue NUMBER in a different repo. Issue numbers
-- are not globally unique, so a repo-less query would mix two repos' queues.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.recent_ts + toIntervalSecond(601), 'QT4', 'DISP4', 'TICK4',
       'loom.dispatch.disposition', 1000000, 'Ok',
       map('loom.repo', 'owner/other', 'loom.issue', '98',
           'loom.queue.disposition', 'capacity',
           'loom.queue.state', 'ready'),
       map('host.id', 'host-ccc')
FROM loom_fixture.anchor AS a;

-- Negative control 3: owner/repo#98 three days ago, outside the 24 h window.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
SELECT a.day_a_ts, 'QT0', 'DISP0', 'TICK0', 'loom.dispatch.disposition', 1000000, 'Ok',
       map('loom.repo', 'owner/repo', 'loom.issue', '98',
           'loom.queue.disposition', 'park_label',
           'loom.queue.state', 'blocked',
           'loom.queue.park_label', 'loom:blocked'),
       map('host.id', 'host-aaa')
FROM loom_fixture.anchor AS a;
