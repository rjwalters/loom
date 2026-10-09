-- Read surface for the fleet singleton-output alert's live proof
-- (`loom-daemon/tests/signoz_fleet_singleton_output_alert.rs`, Issue #10916 /
-- #10924 slice 2b).
--
-- The columns and types are the live trial deployment's own
-- `signoz_logs.distributed_logs_v2`, identically to
-- `fixtures/signoz_eta/fixture.sql` (read off ClickHouse 25.12.5.44 there, not
-- invented here): `timestamp UInt64` in NANOSECONDS, and the three typed
-- attribute maps. Which map an attribute lands in follows the OTLP mapping:
-- `loom.kind` / `loom.repo` / `loom.eta.no_estimate_reason` /
-- `loom.eta.kind` are `kv_string` (attributes_string), `loom.issue` is
-- `kv_int` (attributes_number).
--
-- Every scenario script (`incident.sql`, `healthy.sql`) runs AFTER this one;
-- the test also runs this file alone to prove that an empty logs table (no
-- data at all) fires every row.

CREATE DATABASE IF NOT EXISTS signoz_logs;
CREATE DATABASE IF NOT EXISTS loom_fixture;

CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    body               String,
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    attributes_bool    Map(LowCardinality(String), Bool),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

-- The instant each scenario is written around (seconds since the epoch).
-- `silenced_at` is when the incident's outage starts; `healthy.sql` sets it to
-- the same instant so both scenarios share one clock.
CREATE TABLE loom_fixture.anchor
(
    silenced_at Int64,
    evaluated_at Int64
) ENGINE = Memory;
