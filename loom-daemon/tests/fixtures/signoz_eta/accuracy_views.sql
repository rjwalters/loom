-- Synthetic `signoz_logs.distributed_logs_v2` rows for the live proof of
-- `eta-queries.sql` Q4-Q7 (#10233): late surprise on the common decidable
-- subset, stability, convergence and the time-weighted answer rate.
-- Executed by `loom-daemon/tests/signoz_eta_accuracy_views.rs`.
--
-- A separate fixture from `fixture.sql` on purpose: that one pins exact row
-- counts for section 0 and Q1-Q3, and these populations would move them.
-- The DDL and the map each attribute lands in follow `fixture.sql` (and so
-- `observability/otlp/mapping/eta.rs`): `kv_string` -> attributes_string,
-- `kv_int`/`kv_double` -> attributes_number, `kv_bool` -> attributes_bool,
-- and an attribute the mapping only emits when present (`opt_int`, the
-- `if let Some` bools) is simply ABSENT from the map otherwise.
--
-- Layout (record times are seconds after 2026-09-15T00:00:00Z):
--
--   Q4  `late-a` / `late-b`, kind land, issues 500-511, one instant each at
--       i * 3600, both heuristics at every instant. Outcome rows only.
--         i 0-8   landed after 1800 s; late-a never late, late-b late at i=0.
--         i 9     CENSORED for both (expired after 31 days, p90 passed):
--                 a decided late surprise, no error field.
--         i 10-11 late-a late; late-b REFUSED (no p-values, no above_p90).
--       On the common decidable subset (i 0-9): late-a 1/10, late-b 2/10.
--       Without it late-a reads 3/12 = 0.25 and the refusing late-b "wins".
--   Q5  `stable-v1` / `drifty-v1`, issue 600, estimates at 86400 + k*300.
--         stable-v1 k 0-3 review_wait, p50 = 3600 - 300k: the landing INSTANT
--                   never moves. k 4 enters `judge` with p50 1000 — a
--                   transition, which is news, not drift.
--         drifty-v1 k 0-3 review_wait, p50 = 3600 every time: the instant
--                   slides 300 s per refresh while nothing happens.
--   Q6  `conv-v1`, issues 800-805, outcomes of estimates at 3 * 86400.
--         j 0-1  lead 600:   p25/p75/p90 = 1000/1300/1500.
--         j 2-3  lead 7200:  1000/4000/6000.
--         j 4    lead 7200:  1000/4000, no p90 (pre-#10211 estimate).
--         j 5    censored, lead 40 days: must not read as a lead time.
--   Q7  `answer-v1`, estimates at 2 * 86400 + …:
--         issue 700  answered at 0, 300, 600 (refreshes), landed at 900.
--         issue 701  refused once at 0 (never refreshed), landed at 900.
--         issue 702  answered at 0 and nothing since: its duration is unknown.
--       Rows: 4 of 5 answered (or 3 of 4 counted); time: 900 of 1800 s.

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

-- ---------------------------------------------------------------------------
-- Q4. late-a and late-b outcomes.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    (1789430400 + toUInt64(i) * 3600 + toUInt64(lead)) * 1000000000,
    '{}',
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat(h, '-', toString(i)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', h,
        'loom.eta.outcome', if(i = 9, 'censored', 'landed'),
        'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678'),
    if(decided AND i != 9,
       map('loom.issue', toFloat64(500 + i), 'loom.eta.lead_sec', toFloat64(lead),
           'loom.eta.error_sec', 0.0, 'loom.eta.p90_sec', 1700.0),
       map('loom.issue', toFloat64(500 + i), 'loom.eta.lead_sec', toFloat64(lead))),
    if(decided,
       map('loom.eta.provenance_complete', true,
           'loom.eta.outcome_provenance_complete', true,
           'loom.eta.above_p90', late),
       map('loom.eta.provenance_complete', true,
           'loom.eta.outcome_provenance_complete', true)),
    map('host.id', 'loom-signoz-eta-views-fixture')
FROM (
    SELECT h, i,
           if(i = 9, 31 * 86400, 1800) AS lead,
           h = 'late-a' OR i < 10 AS decided,
           if(h = 'late-a', i >= 9, i = 0 OR i = 9) AS late
    FROM (SELECT arrayJoin(['late-a', 'late-b']) AS h) AS hs
    CROSS JOIN (SELECT toInt64(number) AS i FROM numbers(12)) AS ns
);

-- ---------------------------------------------------------------------------
-- Q5. stable-v1 and drifty-v1 estimates, one series each.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    (1789516800 + toUInt64(k) * 300) * 1000000000,
    concat('{"current_stage":{"stage":"', stage, '","rework_rounds":0}}'),
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat(h, '-', toString(k)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', h,
        'loom.eta.trigger', if(k = 0 OR k = 4, 'transition', 'refresh'),
        'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.stage', stage),
    map('loom.issue', 600.0, 'loom.eta.p50_sec', toFloat64(p50)),
    map('loom.eta.primary', h = 'stable-v1', 'loom.eta.provenance_complete', true),
    map('host.id', 'loom-signoz-eta-views-fixture')
FROM (
    SELECT h, k,
           if(k = 4, 'judge', 'review_wait') AS stage,
           multiIf(h = 'drifty-v1', 3600, k = 4, 1000, 3600 - 300 * k) AS p50
    FROM (SELECT arrayJoin(['stable-v1', 'drifty-v1']) AS h) AS hs
    CROSS JOIN (SELECT toInt64(number) AS k FROM numbers(5)) AS ks
    WHERE h = 'stable-v1' OR k < 4
);

-- ---------------------------------------------------------------------------
-- Q6. conv-v1 outcomes.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    (1789689600 + toUInt64(lead)) * 1000000000,
    '{}',
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat('conv-', toString(j)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', 'conv-v1',
        'loom.eta.outcome', if(j = 5, 'censored', 'landed'),
        'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678'),
    multiIf(j = 5,
            map('loom.issue', toFloat64(800 + j), 'loom.eta.lead_sec', toFloat64(lead),
                'loom.eta.p25_sec', 1000.0, 'loom.eta.p75_sec', 4000.0,
                'loom.eta.p90_sec', 6000.0),
            j = 4,
            map('loom.issue', toFloat64(800 + j), 'loom.eta.lead_sec', toFloat64(lead),
                'loom.eta.error_sec', 0.0, 'loom.eta.p25_sec', 1000.0,
                'loom.eta.p75_sec', 4000.0),
            map('loom.issue', toFloat64(800 + j), 'loom.eta.lead_sec', toFloat64(lead),
                'loom.eta.error_sec', 0.0, 'loom.eta.p25_sec', 1000.0,
                'loom.eta.p75_sec', toFloat64(p75), 'loom.eta.p90_sec', toFloat64(p90))),
    if(j = 4,
       map('loom.eta.provenance_complete', true,
           'loom.eta.outcome_provenance_complete', true),
       map('loom.eta.provenance_complete', true,
           'loom.eta.outcome_provenance_complete', true,
           'loom.eta.above_p90', j = 5)),
    map('host.id', 'loom-signoz-eta-views-fixture')
FROM (
    SELECT j,
           multiIf(j < 2, 600, j < 5, 7200, 40 * 86400) AS lead,
           if(j < 2, 1300, 4000) AS p75,
           if(j < 2, 1500, 6000) AS p90
    FROM (SELECT toInt64(number) AS j FROM numbers(6))
);

-- ---------------------------------------------------------------------------
-- Q7. answer-v1 estimates and outcomes.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    (1789603200 + toUInt64(at)) * 1000000000,
    '{}',
    if(answered,
       map('loom.repo', 'org/alpha',
           'loom.eta.estimate_id', concat('ans-', toString(issue), '-', toString(at)),
           'loom.eta.kind', 'land',
           'loom.eta.heuristic', 'answer-v1',
           'loom.eta.trigger', if(at = 0, 'first', 'refresh'),
           'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
           'loom.eta.stage', 'review_wait'),
       map('loom.repo', 'org/alpha',
           'loom.eta.estimate_id', concat('ans-', toString(issue), '-', toString(at)),
           'loom.eta.kind', 'land',
           'loom.eta.heuristic', 'answer-v1',
           'loom.eta.trigger', 'first',
           'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
           'loom.eta.no_estimate_reason', 'no_dispatch_plan')),
    if(answered,
       map('loom.issue', toFloat64(issue), 'loom.eta.p50_sec', toFloat64(2000 - at)),
       map('loom.issue', toFloat64(issue))),
    map('loom.eta.primary', true, 'loom.eta.provenance_complete', true),
    map('host.id', 'loom-signoz-eta-views-fixture')
FROM (
    SELECT 700 AS issue, arrayJoin([0, 300, 600]) AS at, true AS answered
    UNION ALL SELECT 701, 0, false
    UNION ALL SELECT 702, 0, true
);

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    (1789603200 + 900) * 1000000000,
    '{}',
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat('ans-', toString(issue), '-0'),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', 'answer-v1',
        'loom.eta.outcome', 'landed',
        'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678'),
    map('loom.issue', toFloat64(issue), 'loom.eta.lead_sec', 900.0),
    map('loom.eta.provenance_complete', true, 'loom.eta.outcome_provenance_complete', true),
    map('host.id', 'loom-signoz-eta-views-fixture')
FROM (SELECT arrayJoin([700, 701]) AS issue);
