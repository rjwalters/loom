-- Fixture for the telemetry-hold-transitions replay in
-- signoz_host_export_queries.rs (#14094). Timestamps are literal unix
-- nanoseconds because the CLI inlines the window bounds; the replay builds
-- its statement around the exact [1000000000, 5000000000) window below.
-- Same DDL as fixture.sql: `timestamp` is UInt64 ns.
-- Columns: timestamp ns, loom.repo, loom.kind, loom.hold.reason_code, confidence.

CREATE DATABASE IF NOT EXISTS signoz_logs;

CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    observed_timestamp UInt64,
    body               String,
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    attributes_bool    Map(LowCardinality(String), Bool),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, observed_timestamp, body, attributes_string, attributes_number,
     attributes_bool, resources_string)
SELECT ts, 0, 'unused body',
       map('loom.kind', kind, 'loom.repo', repo, 'loom.hold.reason_code', code),
       if(isNull(confidence), map(), map('loom.hold.confidence_pct', assumeNotNull(confidence))),
       map(), map()
FROM values('ts UInt64, repo String, kind String, code String, confidence Nullable(Float64)',
    -- In the window, matching repo and reason (kept by both statements).
    (2000000000, 'o/one', 'hold.transition', 'merge-risk-self-release', 93),
    -- Wrong repo and wrong reason are both kept only by the all-reasons statement.
    (2500000000, 'o/two', 'hold.transition', 'merge-risk-self-release', NULL),
    (3000000000, 'o/one', 'hold.transition', 'merge-risk-pass', 80),
    -- Wrong kind is dropped even when the repo and reason match.
    (3500000000, 'o/one', 'sweep.event', 'merge-risk-self-release', 20),
    -- Inclusive since and exclusive until. Zero is distinct from absent confidence.
    ( 999999999, 'o/one', 'hold.transition', 'merge-risk-self-release', 12),
    (1000000000, 'o/one', 'hold.transition', 'merge-risk-self-release', 0),
    (5000000000, 'o/one', 'hold.transition', 'merge-risk-self-release', 26)
);
