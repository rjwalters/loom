-- Loom fleet observability backend — initial schema (Epic #4702, Phase 2).
--
-- Two tables:
--   hosts   — per-host ingest key auth (hashed keys, individually revocable).
--   records — durable history of every accepted telemetry record, one row
--             per record (envelopes with N records in a batch produce N rows).
--
-- The Durable Object (src/fleetState.ts) holds live/current state and is
-- NOT backed by this schema — it is independent per-object SQLite/KV storage
-- Cloudflare manages itself.

CREATE TABLE hosts (
  host_id    TEXT PRIMARY KEY,
  key_hash   TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL,
  revoked_at TEXT
);

-- Every ingest request authenticates by hashing the presented bearer token
-- and looking it up here — this index makes that lookup O(log n) instead of
-- a table scan.
CREATE INDEX idx_hosts_key_hash ON hosts (key_hash);

CREATE TABLE records (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  schema_version INTEGER NOT NULL,
  emitted_at     TEXT NOT NULL,
  host_id        TEXT NOT NULL,
  kind           TEXT NOT NULL,
  repo           TEXT,
  -- Fail-safe default: mirrors the daemon's RepoVisibility decode contract
  -- (.loom/docs/telemetry-schema.md) — anything that is not exactly the
  -- string "public" must persist as "private". This column-level default
  -- only covers a missing value at the SQL layer; the actual fail-safe
  -- decision is made in code (src/telemetry.ts `decodeVisibility`) before
  -- the INSERT, so this default is a defense-in-depth backstop, not the
  -- primary control.
  visibility     TEXT NOT NULL DEFAULT 'private',
  issue          INTEGER,
  sweep_id       TEXT,
  -- The full envelope's record payload, verbatim, as JSON — so a
  -- forward-compatible higher schema_version's extra fields are never lost
  -- even though this migration only indexes the fields known today.
  payload        TEXT NOT NULL,
  ingested_at    TEXT NOT NULL
);

-- Query patterns from the AC ("queryable by host/repo/time range") plus the
-- retention sweep's own access pattern (oldest-first eviction).
CREATE INDEX idx_records_host_time ON records (host_id, emitted_at);
CREATE INDEX idx_records_repo_time ON records (repo, emitted_at);
CREATE INDEX idx_records_ingested_at ON records (ingested_at);
