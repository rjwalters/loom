-- Synthetic `signoz_index_v3` rows for the `usage-queries.sql` live proof
-- (`loom-daemon/tests/signoz_usage_queries.rs`, Issue #8528).
--
-- The column set and types mirror the pinned SigNoz v0.142.1 trace schema's
-- read surface: a `Map(LowCardinality(String), String)` attribute container and
-- a `DateTime64(9)` timestamp the committed queries compare to a `DateTime`
-- parameter. The point of this fixture is that the committed SQL is executed
-- VERBATIM against it, so every semantic the file claims (scope resolution,
-- unpriced-model exclusion, unknown-vs-measured-zero, at-least-once dedupe,
-- repo-by-trace-join) is an observation rather than a belief.
--
-- Every attribute value is a STRING, exactly as the trace mapper emits it
-- (`kv_string` over the whole attribute map) — including the token counts and
-- the USD estimate. If this fixture ever stores them as numbers, the test stops
-- proving the thing it exists to prove. `attributes_number` and
-- `attributes_bool` exist on the table and stay EMPTY for every span, which is
-- what lets the test execute the wrong-container read as a negative control:
-- `attributes_number['loom.tokens.total']` must return 0 without erroring.

CREATE DATABASE IF NOT EXISTS signoz_traces;

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

-- ---------------------------------------------------------------------------
-- Trace T1 — a daemon-dispatched sweep in org/alpha carrying BOTH scopes.
-- ---------------------------------------------------------------------------

-- Root sweep span: the ONLY place `loom.repo` appears in this trace, which is
-- what section 2's join has to find.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:00.000000000', 'T1', 'R1', '', 'loom.sweep', 5000000000, 'Ok',
 {'loom.repo': 'org/alpha', 'loom.sweep_id': 'S1', 'loom.result': 'merged'},
 {'host.id': 'loom-usage-fixture'});

-- Builder attempt, with usage measured.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:01.000000000', 'T1', 'A1', 'R1', 'loom.role_attempt', 1000000000, 'Ok',
 {'loom.role': 'builder', 'loom.sweep_id': 'S1', 'loom.result': 'success'},
 {'host.id': 'loom-usage-fixture'});

-- Judge attempt with NO usage child at all: usage unknown, never a zero.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:02.000000000', 'T1', 'A2', 'R1', 'loom.role_attempt', 1000000000, 'Ok',
 {'loom.role': 'judge', 'loom.sweep_id': 'S1', 'loom.result': 'approved'},
 {'host.id': 'loom-usage-fixture'});

-- Doctor attempt whose usage was measured as exactly zero.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:03.000000000', 'T1', 'A3', 'R1', 'loom.role_attempt', 1000000000, 'Ok',
 {'loom.role': 'doctor', 'loom.sweep_id': 'S1', 'loom.result': 'success'},
 {'host.id': 'loom-usage-fixture'});

-- Attempt-scoped usage under the builder attempt.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:01.500000000', 'T1', 'U1', 'A1', 'loom.runtime.usage', 500000000, 'Ok',
 {'loom.usage.scope': 'attempt', 'loom.model': 'claude-opus-5', 'loom.role': 'builder',
  'loom.runtime': 'claude', 'loom.sweep_id': 'S1',
  'loom.tokens.input': '100', 'loom.tokens.output': '50', 'loom.tokens.cache_read': '10',
  'loom.tokens.cache_write': '5', 'loom.tokens.cache_write_5m': '3',
  'loom.tokens.cache_write_1h': '2', 'loom.tokens.total': '165',
  'loom.cost.usd_estimate': '0.010000', 'gen_ai.cost.usd_estimate': '0.010000',
  'loom.pricing.source': 'compiled', 'loom.pricing.verified_on': '2026-09-18',
  'loom.daemon.version': '0.19.562'},
 {'host.id': 'loom-usage-fixture'});

-- Attempt-scoped usage under the doctor attempt: a MEASURED zero.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:03.500000000', 'T1', 'U3', 'A3', 'loom.runtime.usage', 500000000, 'Ok',
 {'loom.usage.scope': 'attempt', 'loom.model': 'claude-opus-5', 'loom.role': 'doctor',
  'loom.runtime': 'claude', 'loom.sweep_id': 'S1',
  'loom.tokens.input': '0', 'loom.tokens.output': '0', 'loom.tokens.cache_read': '0',
  'loom.tokens.cache_write': '0', 'loom.tokens.cache_write_5m': '0',
  'loom.tokens.cache_write_1h': '0', 'loom.tokens.total': '0',
  'loom.cost.usd_estimate': '0.000000', 'gen_ai.cost.usd_estimate': '0.000000',
  'loom.pricing.source': 'compiled', 'loom.pricing.verified_on': '2026-09-18',
  'loom.daemon.version': '0.19.562'},
 {'host.id': 'loom-usage-fixture'});

-- Execution-scoped usage for the whole sweep. Its counters OVERLAP U1/U3 by
-- construction — that is the double-count trap the scope resolution avoids.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:05.000000000', 'T1', 'U2', 'R1', 'loom.runtime.usage', 5000000000, 'Ok',
 {'loom.usage.scope': 'execution', 'loom.model': 'claude-opus-5',
  'loom.runtime': 'claude', 'loom.sweep_id': 'S1',
  'loom.tokens.input': '100', 'loom.tokens.output': '50', 'loom.tokens.cache_read': '10',
  'loom.tokens.cache_write': '5', 'loom.tokens.cache_write_5m': '3',
  'loom.tokens.cache_write_1h': '2', 'loom.tokens.total': '165',
  'loom.cost.usd_estimate': '0.010000', 'gen_ai.cost.usd_estimate': '0.010000',
  'loom.pricing.source': 'compiled', 'loom.pricing.verified_on': '2026-09-18',
  'loom.daemon.version': '0.19.562'},
 {'host.id': 'loom-usage-fixture'});

-- The SAME execution span delivered a second time. Delivery is at least once,
-- and a re-emit is byte-identical in (trace_id, span_id) because the id is
-- derived from name + scope + model. Every section must collapse this to one.
INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 10:00:05.000000000', 'T1', 'U2', 'R1', 'loom.runtime.usage', 5000000000, 'Ok',
 {'loom.usage.scope': 'execution', 'loom.model': 'claude-opus-5',
  'loom.runtime': 'claude', 'loom.sweep_id': 'S1',
  'loom.tokens.input': '100', 'loom.tokens.output': '50', 'loom.tokens.cache_read': '10',
  'loom.tokens.cache_write': '5', 'loom.tokens.cache_write_5m': '3',
  'loom.tokens.cache_write_1h': '2', 'loom.tokens.total': '165',
  'loom.cost.usd_estimate': '0.010000', 'gen_ai.cost.usd_estimate': '0.010000',
  'loom.pricing.source': 'compiled', 'loom.pricing.verified_on': '2026-09-18',
  'loom.daemon.version': '0.19.562'},
 {'host.id': 'loom-usage-fixture'});

-- ---------------------------------------------------------------------------
-- Trace T2 — an in-session run in org/beta with attempt scope only, on a model
-- the rate card does not know. Real tokens, NO cost attributes at all.
-- ---------------------------------------------------------------------------

INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 11:00:00.000000000', 'T2', 'R2', '', 'loom.sweep', 3000000000, 'Ok',
 {'loom.repo': 'org/beta', 'loom.sweep_id': 'S2', 'loom.result': 'merged'},
 {'host.id': 'loom-usage-fixture'});

INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 11:00:01.000000000', 'T2', 'A4', 'R2', 'loom.role_attempt', 1000000000, 'Ok',
 {'loom.role': 'builder', 'loom.sweep_id': 'S2', 'loom.result': 'success'},
 {'host.id': 'loom-usage-fixture'});

INSERT INTO signoz_traces.signoz_index_v3
    (timestamp, trace_id, span_id, parent_span_id, name, duration_nano,
     status_code_string, attributes_string, resources_string)
VALUES
('2026-09-20 11:00:01.500000000', 'T2', 'U4', 'A4', 'loom.runtime.usage', 500000000, 'Ok',
 {'loom.usage.scope': 'attempt', 'loom.model': 'unpriced-model-1', 'loom.role': 'builder',
  'loom.sweep_id': 'S2',
  'loom.tokens.input': '900', 'loom.tokens.output': '90', 'loom.tokens.cache_read': '9',
  'loom.tokens.cache_write': '0', 'loom.tokens.cache_write_5m': '0',
  'loom.tokens.cache_write_1h': '0', 'loom.tokens.total': '999',
  'loom.daemon.version': '0.19.562'},
 {'host.id': 'loom-usage-fixture'});
