-- Read surface for `signoz/ci-queries.sql`, executed by
-- `loom-daemon/tests/signoz_ci_failed_run_logs.rs` (Issue #8528).
--
-- WHAT THIS IS. The five tables SigNoz's own ingester creates, declared here
-- as `Memory` tables with the column names and types `ci-queries.sql` reads,
-- so the COMMITTED file can be run verbatim on the pinned
-- `clickhouse/clickhouse-server:25.12.5`. Only `signoz_logs.logs_v2` is
-- seeded: this fixture's subject is section 5 ("Failed run -> logs"), the one
-- section of that file the trial has never observed on `ci.job.log` chunk data
-- (`evidence.md`: *"Section 5's chunk join is unobserved on real `ci.job.log`
-- data (none reached the trial)"*). The other four tables exist so the
-- whole-file verbatim run parses and executes every section; they stay empty
-- on purpose — an empty section is a parse proof, not a behaviour proof, and
-- the test says which is which.
--
-- WHY THESE SHAPES. Every seeded attribute is placed in the container the
-- daemon's own mapper puts it in, never a convenience choice:
--
--   * `CiRunRecord` / `CiJobRecord` / `CiJobLogRecord::log_attributes()` in
--     `loom-daemon/src/telemetry/ci.rs` decide the container by `CiAttr`
--     variant — `CiAttr::Int` -> `attributes_number` (as `Float64`),
--     `CiAttr::Str` -> `attributes_string`, `CiAttr::Bool` ->
--     `attributes_bool`. `signoz_trial_artifacts.rs` already enforces that
--     correspondence statically for every key `ci-queries.sql` reads.
--   * `loom.repo` / `loom.repo.visibility` are prepended by
--     `observability/otlp/mapping/ci.rs::log_parts()`, which also sets `body`
--     to the record kind (`ci.run` / `ci.job` / `ci.job.log`) and the
--     observation time to the record's own `completed_at` — so a backfilled
--     record lands at the moment CI finished, not at poll time. Timestamps
--     below are therefore CI completion instants.
--   * A run attempt and a job attempt are read from DIFFERENT keys:
--     `ci.run` carries `loom.ci.run_attempt` (`records.rs:723`) and `ci.job`
--     carries `loom.ci.attempts` (`records.rs:820`, whose value is the job's
--     `run_attempt`). Section 5's `failed_runs`/`failed_jobs` join is
--     asymmetric for exactly that reason. Rows 6-7 below exist to make a
--     mutation that "fixes" the asymmetry fail loudly instead of silently
--     returning nothing.
--
-- THE WINDOW. `since` is bound to `2026-09-20 00:00:00` by the test
-- (1789862400 s). In-window rows are 2026-09-21/22; row 11 is 2026-09-19,
-- before it.

CREATE DATABASE IF NOT EXISTS signoz_logs;
CREATE DATABASE IF NOT EXISTS signoz_metrics;
CREATE DATABASE IF NOT EXISTS signoz_traces;

CREATE TABLE signoz_logs.logs_v2
(
    timestamp         UInt64,
    body              LowCardinality(String),
    attributes_string Map(LowCardinality(String), String),
    attributes_number Map(LowCardinality(String), Float64),
    attributes_bool   Map(LowCardinality(String), Bool),
    resources_string  Map(LowCardinality(String), String)
) ENGINE = Memory;

-- `cycle-time-extract.sql` (applied by the test before the whole-file run, so
-- section 7's `loom_analytics.raw_ship_outcome` join resolves) reads the
-- distributed alias of the same table. Empty: section 7 is a parse proof here.
CREATE TABLE signoz_logs.distributed_logs_v2 AS signoz_logs.logs_v2 ENGINE = Memory;

-- Sections 1-3 read metrics; sections 11-13 read traces. Declared, empty.
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

-- ---------------------------------------------------------------------------
-- 1. synthetic/ci-alpha run 9001 attempt 1 — FAILED. The subject run.
--    Delivered TWICE (at-least-once; `envelope_identity` keys a `ci.run` on
--    repo|run_id|run_attempt, so a replayed committed-but-unconfirmed unit
--    arrives again with identical content). `LIMIT 1 BY repo, run_id,
--    run_attempt` is what keeps the duplicate from fanning out every job row.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789992000000000000, 'ci.run',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.conclusion': 'failure',
  'loom.ci.event': 'pull_request', 'loom.ci.ref': 'refs/pull/4242/merge',
  'loom.ci.head_sha': '1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b'},
 {'loom.ci.run_id': 9001, 'loom.ci.run_attempt': 1},
 {}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.run',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.conclusion': 'failure',
  'loom.ci.event': 'pull_request', 'loom.ci.ref': 'refs/pull/4242/merge',
  'loom.ci.head_sha': '1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b'},
 {'loom.ci.run_id': 9001, 'loom.ci.run_attempt': 1},
 {}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 2. Run 9001's jobs. Note `loom.ci.attempts`, NOT `loom.ci.run_attempt`.
--    70001 failure  — log fully captured, 3 of 3, one chunk delivered twice.
--    70002 failure  — timed out, log TRUNCATED and partially captured, 2 of 3.
--    70003 failure  — no `ci.job.log` record reached the backend at all.
--    70004 success  — must not appear.
--    70005 skipped  — must not appear.
--    70006 cancelled — must appear: the filter is NOT IN
--                      ('success','skipped','neutral'), not = 'failure'.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Rust Unit Tests (1/3)',
  'loom.ci.conclusion': 'failure', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70001},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Shell Test Suites (2/2)',
  'loom.ci.conclusion': 'failure', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70002},
 {'loom.ci.timed_out': true}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Build',
  'loom.ci.conclusion': 'failure', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70003},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Lint',
  'loom.ci.conclusion': 'success', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70004},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Publish',
  'loom.ci.conclusion': 'skipped', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70005},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Docs',
  'loom.ci.conclusion': 'cancelled', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70006},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 3. Job 70001's log: chunks 0, 1, 2 of 3, with chunk 1 delivered TWICE.
--    `envelope_identity` keys a `ci.job.log` on repo|job_id|chunk_index, so
--    a replay re-sends an identical chunk — `uniqExact(chunk_index)` must
--    answer 3 where `count()` would answer 4. `loom.ci.truncated` is false
--    on every chunk of an untruncated log.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Rust Unit Tests (1/3)'},
 {'loom.ci.run_id': 9001, 'loom.ci.job_id': 70001,
  'loom.ci.chunk_index': 0, 'loom.ci.chunk_count': 3,
  'loom.ci.log_bytes_total': 19456},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Rust Unit Tests (1/3)'},
 {'loom.ci.run_id': 9001, 'loom.ci.job_id': 70001,
  'loom.ci.chunk_index': 1, 'loom.ci.chunk_count': 3,
  'loom.ci.log_bytes_total': 19456},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Rust Unit Tests (1/3)'},
 {'loom.ci.run_id': 9001, 'loom.ci.job_id': 70001,
  'loom.ci.chunk_index': 1, 'loom.ci.chunk_count': 3,
  'loom.ci.log_bytes_total': 19456},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Rust Unit Tests (1/3)'},
 {'loom.ci.run_id': 9001, 'loom.ci.job_id': 70001,
  'loom.ci.chunk_index': 2, 'loom.ci.chunk_count': 3,
  'loom.ci.log_bytes_total': 19456},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 4. Job 70002's log: a TRUNCATED log whose chunk 1 never arrived — 2 of 3.
--    `truncated` is true on EVERY chunk of a capped log (the reconstruction
--    contract on `CiJobLogRecord`), so `max(truncated)` reads true from
--    either surviving chunk; only the final marker chunk (index 2) carries
--    `loom.ci.truncation_note`.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Shell Test Suites (2/2)'},
 {'loom.ci.run_id': 9001, 'loom.ci.job_id': 70002,
  'loom.ci.chunk_index': 0, 'loom.ci.chunk_count': 3,
  'loom.ci.log_bytes_total': 4194304},
 {'loom.ci.truncated': true}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Shell Test Suites (2/2)',
  'loom.ci.truncation_note': 'log capped at 24 KiB (3 chunks)'},
 {'loom.ci.run_id': 9001, 'loom.ci.job_id': 70002,
  'loom.ci.chunk_index': 2, 'loom.ci.chunk_count': 3,
  'loom.ci.log_bytes_total': 4194304},
 {'loom.ci.truncated': true}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 5. synthetic/ci-alpha run 9002 attempt 1 — FAILED, but every job it ran
--    SUCCEEDED (the run failed at the workflow level: a required check that
--    never produced a job, a cancelled matrix parent, a `startup_failure`
--    style outcome). `failed_jobs` has nothing to offer it, so the LEFT JOIN
--    misses at the JOB level, not just the log level.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789995600000000000, 'ci.run',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'Nightly', 'loom.ci.conclusion': 'startup_failure',
  'loom.ci.event': 'schedule', 'loom.ci.ref': 'refs/heads/main',
  'loom.ci.head_sha': 'f0e1d2c3b4a5968778695a4b3c2d1e0f9a8b7c6d'},
 {'loom.ci.run_id': 9002, 'loom.ci.run_attempt': 1},
 {}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789995600000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'Nightly', 'loom.ci.job': 'Smoke',
  'loom.ci.conclusion': 'success', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9002, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70010},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 6. synthetic/ci-alpha run 9001 attempt 2 — the RETRY, which SUCCEEDED.
--    Its job carries `loom.ci.attempts` = 2. `failed_runs` keeps only
--    attempt 1 (attempt 2 is not a failure), and the join's
--    `j.run_attempt = r.run_attempt` must keep attempt 2's job off
--    attempt 1's row — a retry's green job must never be reported as part of
--    the failed attempt.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1790067600000000000, 'ci.run',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.conclusion': 'success',
  'loom.ci.event': 'pull_request', 'loom.ci.ref': 'refs/pull/4242/merge',
  'loom.ci.head_sha': '1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b'},
 {'loom.ci.run_id': 9001, 'loom.ci.run_attempt': 2},
 {}, {'host.id': 'loom-signoz-ci-fixture'}),
(1790067600000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Rust Unit Tests (1/3)',
  'loom.ci.conclusion': 'cancelled', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9001, 'loom.ci.attempts': 2, 'loom.ci.job_id': 70020},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 7. synthetic/ci-beta — a DIFFERENT repo whose failed job happens to reuse
--    job id 70001, and which has its own 5-chunk log under that id. The
--    `job_logs` CTE applies no repo predicate of its own; only
--    `l.repo = j.repo` on the join keeps beta's chunks out of alpha's row.
--    (GitHub job ids are globally unique, so this collision is synthetic —
--    the join condition it exercises is a real one in the committed SQL, and
--    an unexecuted defensive condition is exactly the kind that rots.)
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789992000000000000, 'ci.run',
 {'loom.repo': 'synthetic/ci-beta', 'loom.repo.visibility': 'private',
  'loom.ci.workflow': 'CI', 'loom.ci.conclusion': 'timed_out',
  'loom.ci.event': 'push', 'loom.ci.ref': 'refs/heads/main',
  'loom.ci.head_sha': 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'},
 {'loom.ci.run_id': 9100, 'loom.ci.run_attempt': 1},
 {}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-beta', 'loom.repo.visibility': 'private',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Integration',
  'loom.ci.conclusion': 'failure', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 9100, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70001},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-beta', 'loom.repo.visibility': 'private',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Integration'},
 {'loom.ci.run_id': 9100, 'loom.ci.job_id': 70001,
  'loom.ci.chunk_index': 0, 'loom.ci.chunk_count': 5,
  'loom.ci.log_bytes_total': 40960},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789992000000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-beta', 'loom.repo.visibility': 'private',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Integration'},
 {'loom.ci.run_id': 9100, 'loom.ci.job_id': 70001,
  'loom.ci.chunk_index': 1, 'loom.ci.chunk_count': 5,
  'loom.ci.log_bytes_total': 40960},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'});

-- ---------------------------------------------------------------------------
-- 8. Out of window: a synthetic/ci-alpha run that failed BEFORE `since`,
--    with its own failed job and a complete log. Nothing from it may appear.
-- ---------------------------------------------------------------------------
INSERT INTO signoz_logs.logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
(1789819200000000000, 'ci.run',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.conclusion': 'failure',
  'loom.ci.event': 'push', 'loom.ci.ref': 'refs/heads/main',
  'loom.ci.head_sha': 'cccccccccccccccccccccccccccccccccccccccc'},
 {'loom.ci.run_id': 8000, 'loom.ci.run_attempt': 1},
 {}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789819200000000000, 'ci.job',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Old',
  'loom.ci.conclusion': 'failure', 'loom.ci.runner': 'ubuntu-latest'},
 {'loom.ci.run_id': 8000, 'loom.ci.attempts': 1, 'loom.ci.job_id': 70099},
 {'loom.ci.timed_out': false}, {'host.id': 'loom-signoz-ci-fixture'}),
(1789819200000000000, 'ci.job.log',
 {'loom.repo': 'synthetic/ci-alpha', 'loom.repo.visibility': 'public',
  'loom.ci.workflow': 'CI', 'loom.ci.job': 'Old'},
 {'loom.ci.run_id': 8000, 'loom.ci.job_id': 70099,
  'loom.ci.chunk_index': 0, 'loom.ci.chunk_count': 1,
  'loom.ci.log_bytes_total': 512},
 {'loom.ci.truncated': false}, {'host.id': 'loom-signoz-ci-fixture'});
