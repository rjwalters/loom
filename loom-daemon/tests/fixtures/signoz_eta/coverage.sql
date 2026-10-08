-- Synthetic `signoz_logs.distributed_logs_v2` rows for the live proof of
-- `eta-queries.sql` Q8, outcome coverage (#10933). Executed by
-- `loom-daemon/tests/signoz_eta_accuracy_views.rs`.
--
-- A separate fixture from `fixture.sql` and `accuracy_views.sql`: those pin
-- exact row counts, and Q8 buckets unresolved series by their age at query
-- time, so every record time here is relative to `now()`. The DDL and the
-- map each attribute lands in follow `fixture.sql` (and so
-- `observability/otlp/mapping/eta.rs`); an absent optional attribute is
-- absent from its map.
--
-- Layout, `cov-v1`, kind land, repo org/alpha (ago = seconds before now):
--   issue 900  two estimates (10 h, 9 h ago), both LANDED and scored; the
--              first outcome is delivered TWICE (at-least-once delivery).
--   issue 901  one estimate 2 h ago, no outcome      -> unresolved lt_4h.
--   issue 902  one estimate 30 h ago, no outcome     -> unresolved 1d_3d.
--   issue 903  one estimate 10 d ago, CENSORED (p50, no error): resolved.
--   issue 904  one REFUSAL 5 h ago (no p50), its item landed: `refused`,
--              never scored, not resolved.
--   issue 905  one estimate 5 d ago, ABANDONED (p50, no error): has an
--              outcome, not resolved.
--   issue 906  one estimate 20 d ago, no outcome     -> unresolved gt_7d.
-- `cov-v2`: issue 900, one estimate 10 h ago, landed and scored.
--
-- cov-v1: 8 estimates, 7 series, landed 2, censored 1, abandoned 1,
-- refused 1, scored 2, resolved 2 (900, 903): resolution_rate 2/7 = 0.286.

CREATE DATABASE IF NOT EXISTS signoz_logs;

CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    body               String,
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    attributes_bool    Map(LowCardinality(String), Bool),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(toUnixTimestamp(now()) - ago) * 1000000000,
    '{}',
    if(outcome = '',
       map('loom.repo', 'org/alpha',
           'loom.eta.estimate_id', id,
           'loom.eta.kind', 'land',
           'loom.eta.heuristic', h,
           'loom.eta.trigger', 'refresh'),
       map('loom.repo', 'org/alpha',
           'loom.eta.estimate_id', id,
           'loom.eta.kind', 'land',
           'loom.eta.heuristic', h,
           'loom.eta.outcome', outcome,
           'loom.eta.outcome_source', 'bus')),
    multiIf(answered = 1 AND scored = 1,
            map('loom.issue', toFloat64(issue), 'loom.eta.p50_sec', 1200.0,
                'loom.eta.error_sec', 60.0),
            answered = 1,
            map('loom.issue', toFloat64(issue), 'loom.eta.p50_sec', 1200.0),
            map('loom.issue', toFloat64(issue))),
    if(outcome = 'censored',
       map('loom.eta.provenance_complete', true, 'loom.eta.above_p90', true),
       map('loom.eta.provenance_complete', true)),
    map('host.id', 'loom-signoz-eta-coverage-fixture')
FROM values('id String, h String, issue UInt32, ago Int64, outcome String, answered UInt8, scored UInt8',
    -- estimates
    ('v1-900-a', 'cov-v1', 900, 36000, '', 1, 0),
    ('v1-900-b', 'cov-v1', 900, 32400, '', 1, 0),
    ('v1-901', 'cov-v1', 901, 7200, '', 1, 0),
    ('v1-902', 'cov-v1', 902, 108000, '', 1, 0),
    ('v1-903', 'cov-v1', 903, 864000, '', 1, 0),
    ('v1-904', 'cov-v1', 904, 18000, '', 0, 0),
    ('v1-905', 'cov-v1', 905, 432000, '', 1, 0),
    ('v1-906', 'cov-v1', 906, 1728000, '', 1, 0),
    ('v2-900', 'cov-v2', 900, 36000, '', 1, 0),
    -- outcomes
    ('v1-900-a', 'cov-v1', 900, 3600, 'landed', 1, 1),
    ('v1-900-a', 'cov-v1', 900, 3600, 'landed', 1, 1),
    ('v1-900-b', 'cov-v1', 900, 3600, 'landed', 1, 1),
    ('v1-903', 'cov-v1', 903, 3600, 'censored', 1, 0),
    ('v1-904', 'cov-v1', 904, 3600, 'landed', 0, 0),
    ('v1-905', 'cov-v1', 905, 3600, 'abandoned', 1, 0),
    ('v2-900', 'cov-v2', 900, 3600, 'landed', 1, 1)
);
