-- Sweep facts: the upgrade path for an ALREADY-INSTALLED `sweep_facts` table
-- (Issue #9507). Read `sweep-facts-rollup.sql` first: it owns the canonical
-- table shape, and this file only brings an older installed table up to it.
--
-- DIALECT: Cloudflare D1 (SQLite). Why a separate file, and why it emits DDL
-- instead of running it:
--
--   * `sweep_facts` is DURABLE — it is the store that outlives D1's `records`
--     retention, so it can never be dropped and rebuilt from `records`: the
--     rows whose raw records were already evicted exist nowhere else.
--   * The rollup's `CREATE TABLE IF NOT EXISTS` is a no-op on an installed
--     table, so a column added to that DDL never reaches a database that ran
--     an earlier version of the file; the rollup's INSERT then fails with
--     "table sweep_facts has no column named …". That failure is loud and
--     writes nothing, but the table still needs the column.
--   * SQLite has no `ADD COLUMN IF NOT EXISTS` and no conditional DDL, so a
--     bare `ALTER TABLE … ADD COLUMN` is one-shot: a second run fails with
--     "duplicate column name". A file of ALTERs is therefore not re-runnable.
--
-- So this file is ONE read-only SELECT: it compares the canonical non-key
-- column list below against the installed table (`pragma_table_info`) and
-- returns, as `ddl`, exactly the `ALTER TABLE sweep_facts ADD COLUMN …`
-- statements still missing — in canonical order, each terminated. On an
-- up-to-date table it returns no rows; on a table that does not exist yet it
-- also returns no rows (a fresh install gets the whole shape from the rollup's
-- CREATE TABLE). Running it is always safe, and running the DDL it prints is
-- idempotent as a procedure, including from a half-applied state: a re-run
-- prints only what is still missing. `ADD COLUMN` keeps every existing row and
-- value; the new columns read NULL on historical rows, which is the
-- absent-vs-zero contract (the field was not measured), never a fabricated 0.
--
-- Runbook (before every rollup run against a database installed by an older
-- version of `sweep-facts-rollup.sql`; harmless when nothing is missing):
--
--   wrangler d1 execute loom-fleet-telemetry --remote --json \
--       --file sweep-facts-migrate.sql \
--     | jq -r '.[0].results[].ddl' > /tmp/sweep-facts-upgrade.sql
--   test -s /tmp/sweep-facts-upgrade.sql \
--     && wrangler d1 execute loom-fleet-telemetry --remote \
--            --file /tmp/sweep-facts-upgrade.sql
--   wrangler d1 execute loom-fleet-telemetry --remote --file sweep-facts-rollup.sql
--   wrangler d1 execute loom-fleet-telemetry --remote --file issue-effort.sql
--
-- then re-run this file: it must return no rows.
--
-- TWIN: the VALUES list is every non-key column of the rollup's
-- `CREATE TABLE IF NOT EXISTS sweep_facts`, with its declared type, in table
-- order. Adding a column there means adding its row here;
-- `loom-daemon/tests/sweep_facts_upgrade_sqlite.rs` fails if they disagree.
-- The identity columns (`repo`, `issue`, `sweep_id`) are the primary key and
-- cannot be added by ALTER, so they are not listed.
--
-- `char(59)` is the statement terminator `;`, spelled out so this file's own
-- text holds exactly one statement.
WITH canonical(position, name, type) AS (
    VALUES
        (1,  'host_id',                   'TEXT'),
        (2,  'emitted_at',                'TEXT'),
        (3,  'result',                    'TEXT'),
        (4,  'disposition',               'TEXT'),
        (5,  'failure_class',             'TEXT'),
        (6,  'tokens_status',             'TEXT'),
        (7,  'config_arm',                'TEXT'),
        (8,  'models_used',               'TEXT'),
        (9,  'total_duration_sec',        'INTEGER'),
        (10, 'phase_durations',           'TEXT'),
        (11, 'tokens_in',                 'INTEGER'),
        (12, 'tokens_out',                'INTEGER'),
        (13, 'tokens_by_model',           'TEXT'),
        (14, 'tokens_unattributed_in',    'INTEGER'),
        (15, 'tokens_unattributed_out',   'INTEGER'),
        (16, 'lines_added',               'INTEGER'),
        (17, 'lines_deleted',             'INTEGER'),
        (18, 'hw_lines_added',            'INTEGER'),
        (19, 'hw_lines_deleted',          'INTEGER'),
        (20, 'hw_files',                  'INTEGER'),
        (21, 'generated_lines',           'INTEGER'),
        (22, 'test_lines',                'INTEGER'),
        (23, 'story_points',              'INTEGER'),
        (24, 'doctor_cycles',             'INTEGER'),
        (25, 'judge_verdicts',            'TEXT'),
        (26, 'attempt_index',             'INTEGER'),
        (27, 'previous_sweep_id',         'TEXT'),
        (28, 'trigger',                   'TEXT'),
        (29, 'rework_substantive',        'INTEGER'),
        (30, 'rework_environmental',      'INTEGER'),
        (31, 'rework_substantive_sec',    'INTEGER'),
        (32, 'rework_environmental_sec',  'INTEGER'),
        (33, 'rework_substantive_open',   'INTEGER'),
        (34, 'rework_environmental_open', 'INTEGER'),
        (35, 'pr_number',                 'INTEGER'),
        (36, 'pr_numbers',                'TEXT'),
        (37, 'suspect',                   'INTEGER'),
        (38, 'schema_version',            'INTEGER')
)
SELECT 'ALTER TABLE sweep_facts ADD COLUMN ' || c.name || ' ' || c.type || char(59)
           AS ddl
FROM canonical c
WHERE EXISTS (SELECT 1 FROM pragma_table_info('sweep_facts'))
  AND NOT EXISTS (SELECT 1 FROM pragma_table_info('sweep_facts') p
                   WHERE p.name = c.name)
ORDER BY c.position
