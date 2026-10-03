-- Synthetic `signoz_logs.distributed_logs_v2` rows for the SigNoz ETA-accuracy
-- live proof (`loom-daemon/tests/signoz_eta_queries.rs`, Issue #8528 scope
-- items 4 and 5 / Issue #9289).
--
-- `eta-queries.sql` is the last query artifact in this trial whose only guard
-- was static: `eta_artifacts.rs` ties every attribute it reads to one the OTLP
-- mapping emits and the gateway forwards, and `signoz_trial_artifacts.rs`
-- checks the same against the allowlist. Neither can see whether the SQL
-- computes the right number. This fixture exists so the committed file can be
-- executed verbatim and every documented claim in its header observed.
--
-- Four things this fixture gets right on purpose, none of them a fixture
-- choice:
--
--  1. The DDL mirrors the **live trial deployment's** own
--     `signoz_logs.distributed_logs_v2` for every column the queries touch:
--     `timestamp UInt64` (nanoseconds, which is why the queries multiply the
--     `since` bound by 1000000000), `body String`, and
--     `attributes_string Map(LowCardinality(String), String)` /
--     `attributes_number Map(LowCardinality(String), Float64)` /
--     `attributes_bool Map(LowCardinality(String), Bool)`. Read from the
--     running deployment, not invented here.
--  2. Which map an attribute lands in follows
--     `observability/otlp/mapping/eta.rs`, not convenience: every `kv_string`
--     call site goes to `attributes_string`, every `kv_int`/`kv_double` to
--     `attributes_number`, and the four `kv_bool` sites
--     (`loom.eta.primary`, `loom.eta.covered`,
--     `loom.eta.provenance_complete`, `loom.eta.outcome_provenance_complete`)
--     to `attributes_bool`.
--  3. An estimate's **body is its whole explanation**, so Q3's
--     `JSONExtractKeysAndValuesRaw(e.body, 'features')` has a real `features`
--     object to expand — including the three non-numeric shapes the header's
--     null-vs-zero rule is written around.
--  4. `opt_int` only emits a field it has, so an unscored outcome has **no**
--     `loom.eta.error_sec` key at all rather than a zero. Group D below is
--     that row; group H is a genuinely measured zero. They must never merge.
--
-- Layout (ids are the join key `loom.eta.estimate_id`):
--
--   A  `a-0` … `a-20`   land-v1 / revA / land / org/alpha. 21 scored pairs,
--                       p25/p50/p75 = 1000/2000/3000 and actual lead
--                       2000 + (n-10)*200, i.e. errors -2000 … +2000 step 200.
--                       11 of 21 land inside [p25, p75]. Features carry one
--                       perfectly-correlated, one perfectly-anticorrelated,
--                       one constant, one partially-measured and four
--                       non-numeric entries.
--   E  one extra copy of `a-0`'s OUTCOME and of `a-1`'s ESTIMATE — delivery is
--      at least once, and no section may count either twice.
--   B  `b-0` … `b-4`    land-v1 / revB / land / org/alpha. A slow build: five
--                       identical +5000 errors, none covered. `b-0` sits
--                       EXACTLY on the `since` bound.
--   J  `j-0`            land-v1 / revB, one second BEFORE the bound, error
--                       999999. If the window leaks, group B's answers move.
--   K  `k-0`            boundary-v0, its OUTCOME at exactly the bound and its
--                       ESTIMATE an hour before it. Present in Q1/Q2 and —
--                       necessarily, not by accident — absent from Q3.
--   F  `f-0`            land-v1 / revA / **start** / org/alpha, p50 = 100, so
--                       a different `horizon_bucket`.
--   G  `g-0`            land-v1 / revA / land / **org/beta**. Its estimate
--                       records no numeric feature, so it does not perturb
--                       group A's correlations.
--   H  `h-0`            stage-v2 / revC / land / org/alpha, error **0**.
--   I  `i-0`            `loom.eta.heuristic` **absent** from both rows — the
--                       shape the data takes if the gateway stops forwarding
--                       the key. Its group key collides with ROLLUP's own
--                       subtotal rows.
--   C  `c-0`/`c-1`/`c-2`  kind `finish`, the three ways provenance is
--                       incomplete: both builds unpinned; the OBSERVING build
--                       unpinned; and `loom.eta.provenance_complete` missing
--                       from the map entirely. All three are scored rows that
--                       Q1-Q3 must drop and section 0 must count.
--   D  `d-0`            an `abandoned` outcome: counted, never scored.
--      `d-1`            a refusal estimate (`loom.eta.no_estimate_reason`),
--                       which still carries `loom.eta.trigger` — refusals are
--                       a SUBSET of estimates, not a disjoint population.

CREATE DATABASE IF NOT EXISTS signoz_logs;

-- Exactly the columns `eta-queries.sql` reads, with the types the live
-- deployment's `SHOW CREATE TABLE signoz_logs.distributed_logs_v2` reports
-- (ClickHouse 25.12.5.44, 2026-10-01).
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
-- A. 21 estimate records. 2026-09-15T00:00:00Z + n hours.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    1789430400000000000 + toUInt64(n) * 3600000000000,
    concat(
        '{"estimate_id":"a-', toString(n), '","kind":"land","heuristic":"land-v1"',
        ',"features":{',
            '"queue_depth":', toString(n),
            ',"slack_sec":', toString(20 - n),
            ',"open_prs":7',
            ',"unmeasured":null',
            ',"label":"refactor"',
            ',"numeric_string":"42"',
            ',"flaky":true',
            ',"partial":', if(n < 12, toString(n), 'null'),
        '}}'
    ),
    map('loom.repo', 'org/alpha',
        'loom.story', concat('story-a-', toString(n)),
        'loom.eta.estimate_id', concat('a-', toString(n)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', 'land-v1',
        'loom.eta.trigger', 'transition',
        'loom.eta.version', '0.19.600',
        'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.tree_state', 'clean',
        'loom.eta.stage', 'review_wait',
        'loom.eta.horizon_bucket', '15m_1h'),
    map('loom.issue', toFloat64(9000 + n),
        'loom.eta.age_sec', 0.0,
        'loom.eta.p25_sec', 1000.0,
        'loom.eta.p50_sec', 2000.0,
        'loom.eta.p75_sec', 3000.0,
        'loom.eta.samples_min', 50.0),
    map('loom.eta.primary', true,
        'loom.eta.provenance_complete', true),
    map('host.id', 'loom-signoz-eta-fixture')
FROM (SELECT toInt64(number) AS n FROM numbers(21));

-- A. the 21 matching outcomes. actual lead = 2000 + (n - 10) * 200.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    1789434000000000000 + toUInt64(n) * 3600000000000,
    concat('{"score":{"outcome":"landed"},"estimate":{"estimate_id":"a-', toString(n), '"}}'),
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat('a-', toString(n)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', 'land-v1',
        'loom.eta.outcome', 'landed',
        'loom.eta.outcome_source', 'label_event',
        'loom.eta.result', 'merged',
        'loom.eta.version', '0.19.600',
        'loom.eta.revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.tree_state', 'clean',
        'loom.eta.outcome_version', '0.19.600',
        'loom.eta.outcome_revision', 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.outcome_tree_state', 'clean',
        'loom.eta.stage', 'review_wait',
        'loom.eta.age_bucket', 'lt_15m',
        'loom.eta.horizon_bucket', '15m_1h'),
    map('loom.issue', toFloat64(9000 + n),
        'loom.eta.p50_sec', 2000.0,
        'loom.eta.lead_sec', toFloat64(2000 + (n - 10) * 200),
        'loom.eta.error_sec', toFloat64((n - 10) * 200),
        'loom.eta.abs_error_sec', toFloat64(abs((n - 10) * 200)),
        'loom.eta.pinball_loss_sec',
            0.25 * greatest(toFloat64(1000 + (n - 10) * 200), 0.)
          + 0.75 * greatest(toFloat64(-1000 - (n - 10) * 200), 0.)
          + 0.5  * greatest(toFloat64((n - 10) * 200), 0.)
          + 0.5  * greatest(toFloat64(-(n - 10) * 200), 0.)
          + 0.75 * greatest(toFloat64(-1000 + (n - 10) * 200), 0.)
          + 0.25 * greatest(toFloat64(1000 - (n - 10) * 200), 0.),
        'loom.eta.outcome_resolution_sec', 0.0,
        'loom.eta.rework_rounds_actual', 0.0,
        'loom.eta.samples_min', 50.0),
    map('loom.eta.provenance_complete', true,
        'loom.eta.outcome_provenance_complete', true,
        'loom.eta.covered',
            toBool((2000 + (n - 10) * 200) >= 1000 AND (2000 + (n - 10) * 200) <= 3000)),
    map('host.id', 'loom-signoz-eta-fixture')
FROM (SELECT toInt64(number) AS n FROM numbers(21));

-- ---------------------------------------------------------------------------
-- E. at-least-once delivery: `a-0`'s outcome and `a-1`'s estimate again,
--    byte-identical. `LIMIT 1 BY estimate_id` (outcomes) and
--    `GROUP BY estimate_id … any(body)` (estimates) are what make this
--    invisible to every section.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
SELECT * FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.eta.estimate_id'] = 'a-0'
  AND mapContains(attributes_string, 'loom.eta.outcome');

INSERT INTO signoz_logs.distributed_logs_v2
SELECT * FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.eta.estimate_id'] = 'a-1'
  AND mapContains(attributes_string, 'loom.eta.trigger');

-- ---------------------------------------------------------------------------
-- B. revB, the slow build: five identical +5000 errors. `b-0` is at exactly
--    the `since` bound (2026-09-01T00:00:00Z), which the queries include.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    1788220800000000000 + toUInt64(n) * 3600000000000,
    concat(
        '{"estimate_id":"b-', toString(n), '","kind":"land","heuristic":"land-v1"',
        ',"features":{"queue_depth":', toString(50 + n),
        ',"slack_sec":', toString(50 - n), ',"unmeasured":null}}'
    ),
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat('b-', toString(n)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', 'land-v1',
        'loom.eta.trigger', 'poll',
        'loom.eta.version', '0.19.599',
        'loom.eta.revision', 'b1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.tree_state', 'clean',
        'loom.eta.stage', 'builder',
        'loom.eta.horizon_bucket', '15m_1h'),
    map('loom.issue', toFloat64(8000 + n),
        'loom.eta.p25_sec', 1000.0, 'loom.eta.p50_sec', 2000.0, 'loom.eta.p75_sec', 3000.0,
        'loom.eta.samples_min', 11.0),
    map('loom.eta.primary', true, 'loom.eta.provenance_complete', true),
    map('host.id', 'loom-signoz-eta-fixture')
FROM (SELECT toInt64(number) AS n FROM numbers(5));

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    1788224400000000000 + toUInt64(n) * 3600000000000,
    concat('{"score":{"outcome":"landed"},"estimate":{"estimate_id":"b-', toString(n), '"}}'),
    map('loom.repo', 'org/alpha',
        'loom.eta.estimate_id', concat('b-', toString(n)),
        'loom.eta.kind', 'land',
        'loom.eta.heuristic', 'land-v1',
        'loom.eta.outcome', 'landed',
        'loom.eta.outcome_source', 'label_event',
        'loom.eta.version', '0.19.599',
        'loom.eta.revision', 'b1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.tree_state', 'clean',
        'loom.eta.outcome_version', '0.19.599',
        'loom.eta.outcome_revision', 'b1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
        'loom.eta.outcome_tree_state', 'clean',
        'loom.eta.horizon_bucket', '15m_1h',
        'loom.eta.age_bucket', 'lt_15m'),
    map('loom.issue', toFloat64(8000 + n),
        'loom.eta.p50_sec', 2000.0,
        'loom.eta.lead_sec', 7000.0,
        'loom.eta.error_sec', 5000.0,
        'loom.eta.abs_error_sec', 5000.0,
        'loom.eta.pinball_loss_sec', 7000.0,
        'loom.eta.rework_rounds_actual', 2.0,
        'loom.eta.samples_min', 11.0),
    map('loom.eta.provenance_complete', true,
        'loom.eta.outcome_provenance_complete', true,
        'loom.eta.covered', false),
    map('host.id', 'loom-signoz-eta-fixture')
FROM (SELECT toInt64(number) AS n FROM numbers(5));

-- ---------------------------------------------------------------------------
-- J. one second before the bound, with an absurd error. Same heuristic,
--    revision and kind as group B, so a leaking lower bound shows up as a
--    sixth scored row there and a wrecked MAE rather than as a new group.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1788220799000000000,
 '{"estimate_id":"j-0","kind":"land","heuristic":"land-v1","features":{"queue_depth":999,"slack_sec":0}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'j-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'poll', 'loom.eta.version': '0.19.599',
  'loom.eta.revision': 'b1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 7999., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1788220799000000000,
 '{"score":{"outcome":"landed"},"estimate":{"estimate_id":"j-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'j-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'landed',
  'loom.eta.outcome_source': 'label_event', 'loom.eta.version': '0.19.599',
  'loom.eta.revision': 'b1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.599',
  'loom.eta.outcome_revision': 'b1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 7999., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 1001999.,
  'loom.eta.error_sec': 999999., 'loom.eta.abs_error_sec': 999999.,
  'loom.eta.pinball_loss_sec': 999999.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true,
  'loom.eta.covered': false},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- K. the lower bound itself, on its own heuristic so its presence is a crisp
--    one-row-or-no-row answer rather than a count that has to be read twice.
--    Its OUTCOME sits at exactly 2026-09-01T00:00:00Z — the `since` value the
--    queries are run with — and `timestamp >= …` includes it. Its ESTIMATE is
--    one hour EARLIER, i.e. outside the window: Q1/Q2 read only outcomes and
--    so still score it, while Q3's estimate-side subquery applies the same
--    bound and therefore cannot see its features at all. That asymmetry is
--    real and is not a fixture artefact — a `land` estimate made days before
--    the merge it predicted behaves exactly this way.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1788217200000000000,
 '{"estimate_id":"k-0","kind":"land","heuristic":"boundary-v0","features":{"queue_depth":1}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'k-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'boundary-v0', 'loom.eta.trigger': 'transition',
  'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9600., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1788220800000000000,
 '{"score":{"outcome":"landed"},"estimate":{"estimate_id":"k-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'k-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'boundary-v0', 'loom.eta.outcome': 'landed',
  'loom.eta.outcome_source': 'label_event', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9600., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 2700.,
  'loom.eta.error_sec': 700., 'loom.eta.abs_error_sec': 700., 'loom.eta.pinball_loss_sec': 850.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true,
  'loom.eta.covered': true},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- F. a `start` estimate: p50 = 100 puts it in the `lt_15m` horizon bucket, so
--    Q1 must report it separately from the `land` population of the same
--    heuristic and revision.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789516800000000000,
 '{"estimate_id":"f-0","kind":"start","heuristic":"land-v1","features":{"queue_depth":1,"unmeasured":null}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'f-0', 'loom.eta.kind': 'start',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'dispatch', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.stage': 'queued', 'loom.eta.horizon_bucket': 'lt_15m'},
 {'loom.issue': 9100., 'loom.eta.p25_sec': 60., 'loom.eta.p50_sec': 100., 'loom.eta.p75_sec': 200.,
  'loom.eta.samples_min': 80.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789516920000000000,
 '{"score":{"outcome":"started"},"estimate":{"estimate_id":"f-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'f-0', 'loom.eta.kind': 'start',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'started',
  'loom.eta.outcome_source': 'sweep_dispatch', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': 'lt_15m',
  'loom.eta.age_bucket': 'lt_15m'},
 {'loom.issue': 9100., 'loom.eta.p50_sec': 100., 'loom.eta.lead_sec': 120.,
  'loom.eta.error_sec': 20., 'loom.eta.abs_error_sec': 20., 'loom.eta.pinball_loss_sec': 45.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true,
  'loom.eta.covered': true},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- G. a second repository. Same heuristic/revision/kind/horizon as group A, so
--    only Q1's `repo` column separates them — and `loom.repo` is the column
--    the `{repo}` parameter filters on. Its estimate records NO numeric
--    feature, deliberately: Q3 does not group by repo, so a numeric feature
--    here would join into group A's correlations.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789520400000000000,
 '{"estimate_id":"g-0","kind":"land","heuristic":"land-v1","features":{"unmeasured":null}}',
 {'loom.repo': 'org/beta', 'loom.eta.estimate_id': 'g-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'transition', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 42., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789522700000000000,
 '{"score":{"outcome":"landed"},"estimate":{"estimate_id":"g-0"}}',
 {'loom.repo': 'org/beta', 'loom.eta.estimate_id': 'g-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'landed',
  'loom.eta.outcome_source': 'label_event', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 42., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 2300.,
  'loom.eta.error_sec': 300., 'loom.eta.abs_error_sec': 300., 'loom.eta.pinball_loss_sec': 650.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true,
  'loom.eta.covered': true},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- H. a genuinely MEASURED ZERO error, on its own heuristic and revision. The
--    pair to group D: `mapContains(attributes_number, 'loom.eta.error_sec')`
--    is TRUE here and FALSE there, which is the whole absent-vs-zero rule.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789524000000000000,
 '{"estimate_id":"h-0","kind":"land","heuristic":"stage-v2","features":{"queue_depth":3}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'h-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'stage-v2', 'loom.eta.trigger': 'transition', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'c1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9200., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789526000000000000,
 '{"score":{"outcome":"landed"},"estimate":{"estimate_id":"h-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'h-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'stage-v2', 'loom.eta.outcome': 'landed',
  'loom.eta.outcome_source': 'label_event', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'c1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'c1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9200., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 2000.,
  'loom.eta.error_sec': 0., 'loom.eta.abs_error_sec': 0., 'loom.eta.pinball_loss_sec': 500.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true,
  'loom.eta.covered': true},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- I. `loom.eta.heuristic` absent from both rows. Nothing in ClickHouse objects
--    to reading a missing Map key: it answers the value type's default, so
--    `attributes_string['loom.eta.heuristic']` is `''` — the SAME key
--    ClickHouse's own ROLLUP fills its subtotal rows with.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789527600000000000,
 '{"estimate_id":"i-0","kind":"land","features":{"queue_depth":4}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'i-0', 'loom.eta.kind': 'land',
  'loom.eta.trigger': 'transition', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9300., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789529700000000000,
 '{"score":{"outcome":"landed"},"estimate":{"estimate_id":"i-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'i-0', 'loom.eta.kind': 'land',
  'loom.eta.outcome': 'landed', 'loom.eta.outcome_source': 'label_event',
  'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9300., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 2100.,
  'loom.eta.error_sec': 100., 'loom.eta.abs_error_sec': 100., 'loom.eta.pinball_loss_sec': 550.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true,
  'loom.eta.covered': true},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- C. the three incomplete-provenance shapes, all on kind `finish` so section 0
--    reports them in groups of their own. Every one is a SCORED row with a
--    large error: if Q1/Q2/Q3 stop excluding them, the numbers move visibly.
--
--   c-0  a tarball build on both sides: revision/tree_state `unknown`,
--        `loom.eta.provenance_complete` FALSE on both rows.
--   c-1  pinned estimate, unpinned OBSERVING build:
--        `loom.eta.outcome_provenance_complete` FALSE.
--   c-2  pinned estimate, and the outcome's
--        `loom.eta.provenance_complete` key MISSING from attributes_bool.
--        `= true` must reject it (the Map default is `false`) and
--        section 0's `!= true` must count it.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789531200000000000,
 '{"estimate_id":"c-0","kind":"finish","heuristic":"land-v1","features":{"queue_depth":5}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'c-0', 'loom.eta.kind': 'finish',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'transition',
  'loom.eta.version': '0.19.600', 'loom.eta.revision': 'unknown',
  'loom.eta.tree_state': 'unknown', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9400., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': false},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789533200000000000,
 '{"score":{"outcome":"finished"},"estimate":{"estimate_id":"c-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'c-0', 'loom.eta.kind': 'finish',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'finished',
  'loom.eta.outcome_source': 'sweep_terminal', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'unknown', 'loom.eta.tree_state': 'unknown',
  'loom.eta.outcome_version': '0.19.600', 'loom.eta.outcome_revision': 'unknown',
  'loom.eta.outcome_tree_state': 'unknown', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9400., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 6242.,
  'loom.eta.error_sec': 4242., 'loom.eta.abs_error_sec': 4242., 'loom.eta.pinball_loss_sec': 5942.},
 {'loom.eta.provenance_complete': false, 'loom.eta.outcome_provenance_complete': false,
  'loom.eta.covered': false},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789534800000000000,
 '{"estimate_id":"c-1","kind":"finish","heuristic":"land-v1","features":{"queue_depth":6}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'c-1', 'loom.eta.kind': 'finish',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'transition',
  'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9401., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789536800000000000,
 '{"score":{"outcome":"finished"},"estimate":{"estimate_id":"c-1"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'c-1', 'loom.eta.kind': 'finish',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'finished',
  'loom.eta.outcome_source': 'sweep_terminal', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.601', 'loom.eta.outcome_revision': 'unknown',
  'loom.eta.outcome_tree_state': 'unknown', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9401., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 6343.,
  'loom.eta.error_sec': 4343., 'loom.eta.abs_error_sec': 4343., 'loom.eta.pinball_loss_sec': 6043.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': false,
  'loom.eta.covered': false},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789538400000000000,
 '{"estimate_id":"c-2","kind":"finish","heuristic":"land-v1","features":{"queue_depth":7}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'c-2', 'loom.eta.kind': 'finish',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'transition',
  'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9402., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789540400000000000,
 '{"score":{"outcome":"finished"},"estimate":{"estimate_id":"c-2"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'c-2', 'loom.eta.kind': 'finish',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'finished',
  'loom.eta.outcome_source': 'sweep_terminal', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9402., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 6444.,
  'loom.eta.error_sec': 4444., 'loom.eta.abs_error_sec': 4444., 'loom.eta.pinball_loss_sec': 6144.},
 {'loom.eta.outcome_provenance_complete': true, 'loom.eta.covered': false},
 {'host.id': 'loom-signoz-eta-fixture'});

-- ---------------------------------------------------------------------------
-- D. counted, never scored.
--
--   d-0  an `abandoned` outcome (the issue closed as not planned). `opt_int`
--        emitted no error field at all, so there is no `loom.eta.error_sec`
--        key — not a zero. Compare group H.
--   d-1  a REFUSAL: an estimate that declined to predict. It still carries
--        `loom.eta.trigger`, so section 0's `estimates` column includes it;
--        `refusals` is a subset of `estimates`, and adding the two columns
--        double-counts this row.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789542000000000000,
 '{"estimate_id":"d-0","kind":"land","heuristic":"land-v1","features":{"queue_depth":8}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'd-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'transition',
  'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9500., 'loom.eta.p25_sec': 1000., 'loom.eta.p50_sec': 2000., 'loom.eta.p75_sec': 3000.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789544000000000000,
 '{"score":{"outcome":"abandoned"},"estimate":{"estimate_id":"d-0"}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'd-0', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.outcome': 'abandoned',
  'loom.eta.outcome_source': 'issue_closed', 'loom.eta.result': 'not_planned',
  'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean',
  'loom.eta.outcome_version': '0.19.600',
  'loom.eta.outcome_revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
  'loom.eta.outcome_tree_state': 'clean', 'loom.eta.horizon_bucket': '15m_1h'},
 {'loom.issue': 9500., 'loom.eta.p50_sec': 2000., 'loom.eta.lead_sec': 2000.},
 {'loom.eta.provenance_complete': true, 'loom.eta.outcome_provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'}),
(1789545600000000000,
 '{"estimate_id":"d-1","kind":"land","heuristic":"land-v1","no_estimate_reason":"insufficient_samples","features":{"queue_depth":9}}',
 {'loom.repo': 'org/alpha', 'loom.eta.estimate_id': 'd-1', 'loom.eta.kind': 'land',
  'loom.eta.heuristic': 'land-v1', 'loom.eta.trigger': 'transition',
  'loom.eta.no_estimate_reason': 'insufficient_samples', 'loom.eta.version': '0.19.600',
  'loom.eta.revision': 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', 'loom.eta.tree_state': 'clean'},
 {'loom.issue': 9501.},
 {'loom.eta.primary': true, 'loom.eta.provenance_complete': true},
 {'host.id': 'loom-signoz-eta-fixture'});
