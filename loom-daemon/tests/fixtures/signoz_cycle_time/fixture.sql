-- Synthetic `signoz_logs.distributed_logs_v2` rows for the SigNoz half of the
-- cycle-time live proof (`loom-daemon/tests/signoz_cycle_time.rs`, Issue
-- #8528 scope item 4 / Issue #8665).
--
-- These are the SAME seven `sweep.outcome` envelopes as
-- `loom-daemon/tests/fixtures/cycle_time/envelopes.jsonl.tmpl` (the
-- ClickStack live proof's fixture), translated by hand into the rows
-- `loom-daemon`'s real OTLP log mapper (`otlp/mapping.rs`,
-- `otlp/mapping/metadata.rs`) would produce once SigNoz's ingester writes
-- them to its own schema. Using the identical seven ships means CT1-CT8's
-- answers below can be compared, value for value, against the ones
-- `cycle_time_clickhouse.rs` already asserts for ClickStack — the same
-- fixture, the same canonical questions, two backends.
--
-- Three things this fixture gets right on purpose, none of them a fixture
-- choice:
--
--  1. `timestamp` is `UInt64` nanoseconds since the epoch, not a `DateTime64`.
--     `cycle-time-extract.sql` reads it with
--     `fromUnixTimestamp64Nano(toInt64(timestamp))`, which only makes sense
--     against a raw integer — SigNoz's real `logs_v2`/`distributed_logs_v2`
--     schema stores it that way.
--  2. Which map an attribute lands in follows the real mapper, not a
--     convenience choice: every `kv_int(...)` call site in `mapping.rs` /
--     `mapping/metadata.rs` for these fields (`loom.issue`,
--     `loom.total_duration_sec`, `loom.pr_number`, `loom.doctor_cycles`)
--     means SigNoz's ingester is the thing deciding they are numeric, so
--     they are seeded into `attributes_number` here, and everything else
--     (`kv_string` call sites, plus the array-valued `loom.phase_durations`,
--     which the mapper builds as a nested `KvlistValue` and which lands in
--     ClickHouse as a JSON string) goes into `attributes_string`.
--  3. `ship-fallback-check` (an eighth row, not one of the seven canonical
--     ships and deliberately excluded from every CT assertion by its
--     `synthetic/fallback-check` repo) puts `loom.issue` and
--     `loom.total_duration_sec` in `attributes_string` ONLY, and omits
--     `loom.pr_number` / `loom.doctor_cycles` from both maps entirely. The
--     extraction view's `if(mapContains(attributes_number, ...), ...,
--     toXOrNull(attributes_string[...]))` fallback exists because which map
--     a real ingester chooses is not this repository's decision to make —
--     this row is what proves the fallback engages instead of silently
--     returning zero rows, and that a key absent from both maps stays NULL.

CREATE DATABASE IF NOT EXISTS signoz_logs;

CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    body               LowCardinality(String),
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

-- 1. ship-alpha-101 — slow, dominated by `builder` (3000s of 3600s).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1789862400000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/alpha', 'loom.repo.visibility': 'public',
  'loom.sweep_id': 'ship-alpha-101', 'loom.result': 'success',
  'loom.model': 'synthetic-model-no-inference', 'loom.effort': 'high',
  'loom.runtime': 'synthetic-runtime', 'loom.provider': 'synthetic-provider',
  'loom.configured_model': 'synthetic-configured-model',
  'loom.phase_durations': '[{"phase":"curator","duration_sec":120},{"phase":"builder","duration_sec":3000},{"phase":"judge","duration_sec":300},{"phase":"merge","duration_sec":180}]'},
 {'loom.issue': 101, 'loom.total_duration_sec': 3600, 'loom.pr_number': 201, 'loom.doctor_cycles': 0},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 2. ship-alpha-102 — repair loop: `judge` runs twice (800s + 700s) and
-- outranks the single 900s `builder`; also #9443's per-phase token
-- attribution, with the re-judge deliberately UNMEASURED (no token keys).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1789866000000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/alpha', 'loom.repo.visibility': 'public',
  'loom.sweep_id': 'ship-alpha-102', 'loom.result': 'success',
  'loom.model': 'synthetic-model-no-inference', 'loom.effort': 'high',
  'loom.runtime': 'synthetic-runtime', 'loom.provider': 'synthetic-provider',
  'loom.configured_model': 'synthetic-configured-model',
  'loom.phase_durations': '[{"phase":"curator","duration_sec":60,"attempt":1,"tokens_in":4200,"tokens_out":510},{"phase":"builder","duration_sec":900,"attempt":1,"tokens_in":38000,"tokens_out":4900},{"phase":"judge","duration_sec":800,"attempt":1,"tokens_in":3100,"tokens_out":400},{"phase":"doctor","duration_sec":400,"attempt":1,"tokens_in":1800,"tokens_out":200},{"phase":"judge","duration_sec":700,"attempt":2},{"phase":"merge","duration_sec":100,"attempt":1,"tokens_in":90,"tokens_out":10}]'},
 {'loom.issue': 102, 'loom.total_duration_sec': 2960, 'loom.pr_number': 202, 'loom.doctor_cycles': 1},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 3. ship-beta-103 — slowest overall, but carries NO phase breakdown at all.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1789869600000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/beta', 'loom.repo.visibility': 'private',
  'loom.sweep_id': 'ship-beta-103', 'loom.result': 'success'},
 {'loom.issue': 103, 'loom.total_duration_sec': 5400, 'loom.pr_number': 203},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 4. ship-beta-104 — a failure, with a failure_class and no PR: not a ship.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1789873200000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/beta', 'loom.repo.visibility': 'private',
  'loom.sweep_id': 'ship-beta-104', 'loom.result': 'failure',
  'loom.model': 'synthetic-model-no-inference',
  'loom.runtime': 'synthetic-runtime', 'loom.provider': 'synthetic-provider',
  'loom.failure_class': 'synthetic-failure-class',
  'loom.phase_durations': '[{"phase":"builder","duration_sec":1200}]'},
 {'loom.issue': 104, 'loom.total_duration_sec': 1200},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 5. ship-alpha-105 — fast, a different runtime/provider/model/effort.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1789876800000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/alpha', 'loom.repo.visibility': 'public',
  'loom.sweep_id': 'ship-alpha-105', 'loom.result': 'success',
  'loom.model': 'synthetic-other-model', 'loom.effort': 'low',
  'loom.runtime': 'synthetic-other-runtime', 'loom.provider': 'synthetic-other-provider',
  'loom.phase_durations': '[{"phase":"curator","duration_sec":10},{"phase":"builder","duration_sec":200},{"phase":"judge","duration_sec":50},{"phase":"merge","duration_sec":5}]'},
 {'loom.issue': 105, 'loom.total_duration_sec': 265, 'loom.pr_number': 205, 'loom.doctor_cycles': 0},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 6. Duplicate of ship-alpha-101 — at-least-once delivery; must NOT double-count.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1789862400000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/alpha', 'loom.repo.visibility': 'public',
  'loom.sweep_id': 'ship-alpha-101', 'loom.result': 'success',
  'loom.model': 'synthetic-model-no-inference', 'loom.effort': 'high',
  'loom.runtime': 'synthetic-runtime', 'loom.provider': 'synthetic-provider',
  'loom.configured_model': 'synthetic-configured-model',
  'loom.phase_durations': '[{"phase":"curator","duration_sec":120},{"phase":"builder","duration_sec":3000},{"phase":"judge","duration_sec":300},{"phase":"merge","duration_sec":180}]'},
 {'loom.issue': 101, 'loom.total_duration_sec': 3600, 'loom.pr_number': 201, 'loom.doctor_cycles': 0},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 7. ship-alpha-106 — one day "old" relative to the others, so the window
-- spans more than one ISO week.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(1790380800000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/alpha', 'loom.repo.visibility': 'public',
  'loom.sweep_id': 'ship-alpha-106', 'loom.result': 'success',
  'loom.model': 'synthetic-model-no-inference', 'loom.effort': 'high',
  'loom.runtime': 'synthetic-runtime', 'loom.provider': 'synthetic-provider',
  'loom.phase_durations': '[{"phase":"builder","duration_sec":420},{"phase":"judge","duration_sec":60}]'},
 {'loom.issue': 106, 'loom.total_duration_sec': 480, 'loom.pr_number': 206, 'loom.doctor_cycles': 0},
 {'host.id': 'loom-signoz-cycle-time-fixture'});

-- 8. ship-fallback-check — NOT one of the seven canonical ships. Its
-- timestamp (year 2200) is deliberately past every `{until:DateTime}` this
-- test's CT1-8 run uses, so it never reaches the rollup backfill and cannot
-- perturb the six-ship CT1-8 parity comparison against ClickStack below; it
-- is queried directly from `raw_ship_outcome`, which has no time filter,
-- to prove the numeric-attribute fallback and true absence, described above.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, resources_string)
VALUES
(7258118400000000000, 'sweep.outcome',
 {'loom.repo': 'synthetic/fallback-check', 'loom.repo.visibility': 'public',
  'loom.sweep_id': 'ship-fallback-check', 'loom.result': 'success',
  'loom.issue': '900', 'loom.total_duration_sec': '999'},
 {},
 {'host.id': 'loom-signoz-cycle-time-fixture'});
