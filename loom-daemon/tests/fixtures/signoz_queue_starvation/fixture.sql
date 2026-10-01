-- Synthetic SigNoz metric rows for the live proof of the saved alert rule
-- `signoz/alerts/queue-starvation.json` (#8856) --
-- `loom-daemon/tests/signoz_queue_starvation_alert.rs`, Issue #8528 scope
-- item 4 ("host/token gauges") and scope item 5 ("missing stays
-- distinguishable from zero").
--
-- The column set and types mirror the pinned SigNoz v0.142.1 metric schema's
-- READ surface, identically to `fixtures/signoz_queue_quota/fixture.sql`:
-- `samples_v4(metric_name, fingerprint, unix_milli Int64, value Float64)` and
-- `time_series_v4(metric_name, fingerprint, unix_milli Int64, labels String)`
-- -- `labels` being a JSON **string** the alert's embedded query reads with
-- `JSONExtractString`, not a Map. The non-read columns present here (`env`,
-- `temporality`, `description`, `unit`, `type`, `is_monotonic`, `flags`,
-- `__normalized`) exist so a query that reached for one would fail rather than
-- silently not compile against a narrower mock.
--
-- UNLIKE every other SigNoz artifact in this trial, the alert's query is NOT
-- windowed on `now()`: it carries SigNoz's own `{{.start_timestamp_ms}}` /
-- `{{.end_timestamp_ms}}` placeholders, which the rule evaluator substitutes
-- with the evaluation window's bounds. That is why this fixture can and does
-- use FIXED timestamps -- the test substitutes the same two bounds it builds
-- the rows around, so every bucket count below is exact rather than
-- clock-dependent.
--
-- `loom_fixture.window` is the single source of those bounds. The test asserts
-- its length equals the committed rule's own `evalWindow`, so the two cannot
-- drift: shortening `evalWindow` in the JSON without reworking this fixture
-- fails the test by name instead of quietly changing which hosts fire.
--
-- Metric timestamps are MILLISECONDS, as the alert's `intDiv(s.unix_milli,
-- 1000)` states.

CREATE DATABASE IF NOT EXISTS signoz_metrics;
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

-- 2026-10-01T00:00:00Z .. 2026-10-01T00:15:00Z, minute-aligned at both ends so
-- the alert's `toStartOfInterval(..., INTERVAL 1 MINUTE)` lands in exactly
-- `(end_ms - start_ms) / 60000` buckets and the half-open `>= start` / `< end`
-- predicates have observable boundaries.
CREATE TABLE loom_fixture.window ENGINE = Memory AS
SELECT toUnixTimestamp(toDateTime('2026-10-01 00:00:00', 'UTC')) * 1000 AS start_ms,
       toUnixTimestamp(toDateTime('2026-10-01 00:15:00', 'UTC')) * 1000 AS end_ms;

-- One minute index per bucket of the window, 0-based.
CREATE TABLE loom_fixture.minute ENGINE = Memory AS
SELECT n.number AS idx,
       w.start_ms + n.number * 60000 AS ms
FROM loom_fixture.window AS w
CROSS JOIN (SELECT number FROM numbers(10000)) AS n
WHERE w.start_ms + n.number * 60000 < w.end_ms;

-- ===========================================================================
-- Series metadata. SigNoz writes ONE `time_series_v4` row per series per hour,
-- so the alert's `USING (fingerprint)` join multiplies every data point by its
-- series' hour-row count. Fingerprint 4001 gets TWO rows so that
-- multiplication is real here, not hypothetical: harmless for the alert's
-- `max()`, and the test runs a `sum()` counterfactual to show what `max()`
-- actually buys.
--
-- `labels` carries only the two keys the alert's query reads (`host.id`,
-- `state`), both of which `signoz_trial_artifacts.rs` independently asserts the
-- gateway's DATAPOINT `keep_keys` allowlist still forwards.
-- ===========================================================================

CREATE TABLE loom_fixture.series_seed
(
    fingerprint UInt64,
    labels String,
    hour_rows UInt8
) ENGINE = Memory;

-- The five series, and why each exists. (ClickHouse's `VALUES` parser rejects
-- `--` comments inside the tuple list, so they live here rather than inline.)
--
--   4001 host-starved       genuinely starved for the whole window: every
--                           bucket above zero. The host the alert exists to
--                           name. TWO hour-rows, so the duplicate join is real.
--   4002 host-healthy       healthy, and REPORTING it: `loom.queue.starved` = 0
--                           in every bucket, which the emitter sends
--                           unconditionally (`observability/ops/dwell.rs`
--                           emits both states even at zero). The alert's query
--                           has no `HAVING starved > 0` -- unlike
--                           `queue-dwell.sql` query 1 -- so these rows ARE
--                           returned, and it is the rule's own `op`/`target`
--                           threshold that must keep them from alerting.
--   4003 host-flapping      starved in some minutes and clear in others: the
--                           case that separates `matchType` "at least once"
--                           from "all the time".
--   4004 host-blocked-only  BLOCKED-queue starvation only, far above the
--                           threshold. The alert's name is "Loom ready queue
--                           starved", so its `state = 'ready'` filter must
--                           exclude this host entirely -- blocked work waiting
--                           on a dependency is not a capacity problem.
--   4005 (no host.id)       a ready-state series with NO `host.id` label, the
--                           shape the data would have if the gateway's
--                           allowlist ever stopped forwarding it.
--                           `JSONExtractString` answers '' rather than
--                           dropping the row, so the alert still fires but its
--                           annotation's "see the alert's host label" points
--                           at nothing. Observed behaviour, not a desired one.
INSERT INTO loom_fixture.series_seed (fingerprint, labels, hour_rows) VALUES
(4001, '{"host.id":"host-starved","state":"ready"}', 2),
(4002, '{"host.id":"host-healthy","state":"ready"}', 1),
(4003, '{"host.id":"host-flapping","state":"ready"}', 1),
(4004, '{"host.id":"host-blocked-only","state":"blocked"}', 1),
(4005, '{"state":"ready"}', 1);

INSERT INTO signoz_metrics.time_series_v4 (metric_name, fingerprint, unix_milli, labels)
SELECT 'loom.queue.starved',
       s.fingerprint,
       -- Hour-row timestamps are irrelevant to the alert (it never filters
       -- `t.unix_milli`); they are staggered only so the two rows of
       -- fingerprint 4001 are genuinely distinct rows.
       w.start_ms - r.number * 3600000,
       s.labels
FROM loom_fixture.series_seed AS s
CROSS JOIN loom_fixture.window AS w
CROSS JOIN (SELECT number FROM numbers(8)) AS r
WHERE r.number < s.hour_rows;

-- ===========================================================================
-- Data points, one per minute bucket per series.
-- ===========================================================================

-- host-starved: 1 + (idx % 3), so every bucket is 1, 2 or 3 -- never zero.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4001, m.ms, 1 + (m.idx % 3) FROM loom_fixture.minute AS m;

-- host-starved, bucket 0 only: two EXTRA points inside the same minute, rising
-- then falling (2, 3, then the generated 1 above). The bucket's answer is 3
-- only if the query takes the maximum -- not the last value (1), not the first
-- (2), and not the sum (6, or 12 through the duplicated hour-row).
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4001, w.start_ms + 10000, 2 FROM loom_fixture.window AS w;
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4001, w.start_ms + 20000, 3 FROM loom_fixture.window AS w;

-- host-healthy: a measured zero in every bucket.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4002, m.ms, 0 FROM loom_fixture.minute AS m;

-- host-healthy, OUT OF WINDOW on both sides, at a value far above the
-- threshold. One minute BEFORE `start_ms` and exactly AT `end_ms`. The alert's
-- window is half-open (`>= start`, `< end`), so neither may appear: the first
-- proves a past starvation does not re-alert forever, the second proves two
-- consecutive overlapping evaluations cannot both count the same boundary
-- point. Either leak turns this host from healthy into firing, so the test's
-- "host-healthy never fires" assertion is what catches it.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4002, w.start_ms - 60000, 99 FROM loom_fixture.window AS w;
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4002, w.end_ms, 99 FROM loom_fixture.window AS w;

-- host-flapping: 4 in even buckets, 0 in odd ones.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4003, m.ms, if(m.idx % 2 = 0, 4, 0) FROM loom_fixture.minute AS m;

-- host-blocked-only: well above the threshold in every bucket, wrong state.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4004, m.ms, 7 FROM loom_fixture.minute AS m;

-- the unlabelled series: above the threshold in every bucket.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved', 4005, m.ms, 5 FROM loom_fixture.minute AS m;

-- A DIFFERENT metric on a ready-state host, at a value far above the
-- threshold: `loom.queue.starved.by_reason` is a sibling in the same family
-- (`telemetry/ops.rs`) whose values are per-reason subtotals of the very
-- number this alert watches. The alert filters `s.metric_name =
-- 'loom.queue.starved'`, so a reason subtotal must not be mistaken for the
-- total. Its own fingerprint is reused from 4002 deliberately -- the
-- `fingerprint` join carries no metric-name predicate, so if the
-- `metric_name` filter were dropped these points would attach to
-- host-healthy's label set and make the healthy host fire.
INSERT INTO signoz_metrics.samples_v4 (metric_name, fingerprint, unix_milli, value)
SELECT 'loom.queue.starved.by_reason', 4002, m.ms, 42 FROM loom_fixture.minute AS m;
