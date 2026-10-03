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
