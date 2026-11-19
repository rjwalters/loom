//! Tests for the OpenCode session-store reader (Issue #8507).
//!
//! The fixture DB is built from the **live** `opencode 1.18.31` schema
//! (verified 2026-09-22, see the module doc's "Schema provenance"), including
//! the `credential`/`account` tables that sit beside `session` in the real
//! file — so the credential-isolation tests exercise the same shape a
//! production read would meet, not a sanitised stand-in.

use super::*;
use chrono::TimeZone as _;

/// One `session` row for [`seed_db`].
struct Row<'a> {
    model: &'a str,
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write: i64,
    directory: &'a str,
    created_ms: i64,
    /// Explicit `session.id` (#14048); `None` seeds the positional
    /// `session-<i>` default every pre-#14048 fixture used.
    id: Option<&'a str>,
}

fn row<'a>(model: &'a str, directory: &'a str, created_ms: i64) -> Row<'a> {
    Row {
        model,
        input: 0,
        output: 0,
        reasoning: 0,
        cache_read: 0,
        cache_write: 0,
        directory,
        created_ms,
        id: None,
    }
}

/// Seed a fixture `opencode.db` shaped like the live one: the `session`
/// columns [`SESSION_QUERY`] reads, PLUS `credential`/`account` tables
/// carrying planted secrets, so a test can prove the reader never touches
/// them rather than merely asserting on the rows it happens to return.
fn seed_db(db_path: &Path, rows: &[Row<'_>]) {
    let conn = Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
             id TEXT PRIMARY KEY,
             project_id TEXT,
             slug TEXT,
             directory TEXT NOT NULL,
             title TEXT,
             cost REAL DEFAULT 0 NOT NULL,
             tokens_input INTEGER DEFAULT 0 NOT NULL,
             tokens_output INTEGER DEFAULT 0 NOT NULL,
             tokens_reasoning INTEGER DEFAULT 0 NOT NULL,
             tokens_cache_read INTEGER DEFAULT 0 NOT NULL,
             tokens_cache_write INTEGER DEFAULT 0 NOT NULL,
             model TEXT,
             time_created INTEGER NOT NULL,
             time_updated INTEGER
         );
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
         CREATE TABLE credential (id TEXT PRIMARY KEY, access_token TEXT);
         CREATE TABLE account (id TEXT PRIMARY KEY, email TEXT);
         INSERT INTO credential (id, access_token) VALUES \
             ('poison', 'sk-fake-secret-should-never-be-read');
         INSERT INTO account (id, email) VALUES ('poison', 'operator@example.com');",
    )
    .unwrap();
    for (i, r) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO session (id, model, tokens_input, tokens_output, tokens_reasoning, \
             tokens_cache_read, tokens_cache_write, directory, title, time_created) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'title', ?9)",
            rusqlite::params![
                r.id.map(str::to_string)
                    .unwrap_or_else(|| format!("session-{i}")),
                r.model,
                r.input,
                r.output,
                r.reasoning,
                r.cache_read,
                r.cache_write,
                r.directory,
                r.created_ms,
            ],
        )
        .unwrap();
    }
}

/// A `session.model` value in the live shape.
fn model_json(id: &str, provider: &str) -> String {
    serde_json::json!({"id": id, "providerID": provider, "variant": "default"}).to_string()
}

fn dirs(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

// --- Security: the query surface itself ---------------------------------

#[test]
fn the_only_query_this_module_ever_issues_names_session_and_nothing_else() {
    let lowered = SESSION_QUERY.to_ascii_lowercase();
    assert!(lowered.contains("from session"), "{SESSION_QUERY}");
    assert!(
        !lowered.contains("credential") && !lowered.contains("account"),
        "the session-store reader's own query text must never reference the \
         credential/account tables that sit beside `session` in the same \
         file: {SESSION_QUERY}"
    );
    // One statement, not a batch that could smuggle a second table in.
    assert_eq!(SESSION_QUERY.matches("SELECT").count(), 1, "{SESSION_QUERY}");
    assert!(!SESSION_QUERY.contains(';'), "{SESSION_QUERY}");
}

#[test]
fn the_module_source_issues_no_sql_other_than_the_pinned_session_query() {
    // Behavioural tests can only prove what the rows *contain*; this proves
    // the module never grows a second query at all. `include_str!` reads this
    // module's own sibling source, so a future edit that adds a
    // `conn.prepare("SELECT ... FROM credential")` fails here with the reason.
    let source = include_str!("../opencode_usage.rs");
    let prepares = source.matches(".prepare(").count();
    assert_eq!(
        prepares, 1,
        "exactly one prepared statement (SESSION_QUERY) may exist in this module"
    );
    assert!(
        source.contains(".prepare(SESSION_QUERY)"),
        "the single prepared statement must be SESSION_QUERY itself"
    );
    for forbidden in ["credential", "account"] {
        assert!(
            !source
                .to_ascii_lowercase()
                .contains(&format!("from {forbidden}")),
            "this module must never read the {forbidden} table"
        );
    }
}

#[test]
fn a_planted_credential_never_surfaces_in_the_returned_rows_or_their_wire_form() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[Row {
            model: &model_json("zai-org/GLM-5.3", "friendli"),
            input: 10,
            output: 20,
            reasoning: 0,
            cache_read: 5,
            cache_write: 1,
            directory: "/repo/a",
            created_ms: 1_000,
            id: None,
        }],
    );
    let sessions = sessions_in(&db, &SessionFilter::directories(&dirs(&["/repo/a"])), None)
        .expect("db opened");
    assert_eq!(sessions.len(), 1);
    let rendered = format!("{sessions:?}");
    assert!(!rendered.contains("sk-fake-secret-should-never-be-read"), "{rendered}");
    assert!(!rendered.contains("operator@example.com"), "{rendered}");

    let totals = tokens_by_model_in(&db, &dirs(&["/repo/a"]), None).unwrap();
    let wire = serde_json::to_string(&totals).unwrap();
    assert!(!wire.contains("sk-fake-secret-should-never-be-read"), "{wire}");
    assert!(!wire.contains("operator@example.com"), "{wire}");
}

#[test]
fn works_identically_when_the_db_has_no_credential_or_account_tables_at_all() {
    // Proves there is no implicit dependency on those tables existing — a
    // database that never had them reads exactly like one that does.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
             id TEXT, model TEXT, tokens_input INTEGER, tokens_output INTEGER,
             tokens_reasoning INTEGER, tokens_cache_read INTEGER,
             tokens_cache_write INTEGER, directory TEXT, time_created INTEGER
         );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES (?1, ?2, 10, 20, 0, 5, 1, '/repo/a', 1000)",
        rusqlite::params!["ses_plain", model_json("zai-org/GLM-5.3", "friendli")],
    )
    .unwrap();
    drop(conn);

    let rows = tokens_by_model_in(&db, &dirs(&["/repo/a"]), None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "zai-org/GLM-5.3");
}

#[test]
fn the_database_is_opened_read_only_so_a_live_store_can_never_be_mutated() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(&db, &[row(&model_json("m", "p"), "/repo/a", 1_000)]);
    let uri = crate::tokens_pool::monitor_db::read_only_uri(&db);
    assert!(uri.contains("mode=ro"), "{uri}");
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    assert!(
        conn.execute("DELETE FROM session", []).is_err(),
        "a connection opened the way this module opens one must reject writes"
    );
}

// --- Model/token extraction ---------------------------------------------

#[test]
fn extracts_model_id_and_provider_and_folds_reasoning_into_output() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[Row {
            model: &model_json("zai-org/GLM-5.3", "friendli"),
            input: 100,
            output: 20,
            reasoning: 5,
            cache_read: 30,
            cache_write: 7,
            directory: "/repo/a",
            created_ms: 1_000,
            id: None,
        }],
    );
    let sessions =
        sessions_in(&db, &SessionFilter::directories(&dirs(&["/repo/a"])), None).unwrap();
    assert_eq!(sessions[0].provider.as_deref(), Some("friendli"));

    let rows = tokens_by_model_in(&db, &dirs(&["/repo/a"]), None).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.model, "zai-org/GLM-5.3");
    assert_eq!(row.speed, "standard");
    assert_eq!(row.service_tier, "standard");
    assert_eq!(row.input, 100);
    assert_eq!(row.output, 25, "reasoning (5) folds into output (20)");
    assert_eq!(row.cache_read, 30);
    assert_eq!(row.cache_write_5m, 0);
    assert_eq!(row.cache_write_1h, 7, "the flat cache-write counter lands in the 1h bucket");
}

#[test]
fn sums_multiple_sessions_of_the_same_model_and_keeps_distinct_models_separate() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[
            Row {
                input: 10,
                output: 1,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 1_000)
            },
            Row {
                input: 20,
                output: 2,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 2_000)
            },
            Row {
                input: 5,
                output: 5,
                ..row(&model_json("openai/gpt-5", "openai"), "/repo/a", 3_000)
            },
        ],
    );
    let rows = tokens_by_model_in(&db, &dirs(&["/repo/a"]), None).unwrap();
    assert_eq!(rows.len(), 2);
    let glm = rows.iter().find(|r| r.model == "zai-org/GLM-5.3").unwrap();
    assert_eq!((glm.input, glm.output), (30, 3));
    let gpt = rows.iter().find(|r| r.model == "openai/gpt-5").unwrap();
    assert_eq!((gpt.input, gpt.output), (5, 5));
}

#[test]
fn a_missing_or_malformed_model_field_is_skipped_never_guessed() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[
            Row {
                input: 10,
                ..row("not json", "/repo/a", 1_000)
            },
            Row {
                input: 10,
                ..row("{}", "/repo/a", 1_000)
            },
            Row {
                input: 10,
                ..row(r#"{"id":""}"#, "/repo/a", 1_000)
            },
        ],
    );
    assert_eq!(tokens_by_model_in(&db, &dirs(&["/repo/a"]), None), None);
}

#[test]
fn an_all_zero_session_row_is_not_a_model_badge() {
    // OpenCode writes the `session` row at creation with every counter at its
    // `DEFAULT 0`; a launch that produced no turns must not publish a
    // zero-token badge for a model that was never actually called.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[row(
            &model_json("zai-org/GLM-5.3", "friendli"),
            "/repo/a",
            1_000,
        )],
    );
    assert_eq!(
        sessions_in(&db, &SessionFilter::directories(&dirs(&["/repo/a"])), None)
            .unwrap()
            .len(),
        1,
        "the raw reader still reports the row"
    );
    assert_eq!(
        tokens_by_model_in(&db, &dirs(&["/repo/a"]), None),
        None,
        "but a usage-free session contributes no totals"
    );
}

#[test]
fn a_database_with_no_matching_sessions_yields_none_not_an_empty_vec() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(&db, &[]);
    assert_eq!(tokens_by_model_in(&db, &dirs(&["/repo/a"]), None), None);
    assert_eq!(
        sessions_in(&db, &SessionFilter::directories(&dirs(&["/repo/a"])), None),
        Some(Vec::new()),
        "`opened fine, nothing matched` is distinct from `could not open`"
    );
    assert_eq!(
        sessions_in(
            Path::new("/nonexistent/opencode.db"),
            &SessionFilter::directories(&dirs(&["/repo/a"])),
            None
        ),
        None
    );
}

// --- Directory + time-window filtering ------------------------------------

#[test]
fn filters_out_sessions_from_a_directory_outside_the_attributed_set() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[
            Row {
                input: 10,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 1_000)
            },
            Row {
                input: 999,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/b", 1_000)
            },
        ],
    );
    let rows = tokens_by_model_in(&db, &dirs(&["/repo/a"]), None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].input, 10, "the other directory's session must not be folded in");
}

#[test]
fn attributes_every_directory_in_the_set_not_just_the_first() {
    // A sweep's set is {workspace root, its issue worktree} — both are the
    // same sweep's work and both must be folded in (see `usage_source`).
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[
            Row {
                input: 10,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 1_000)
            },
            Row {
                input: 20,
                ..row(
                    &model_json("zai-org/GLM-5.3", "friendli"),
                    "/repo/a/.loom/worktrees/issue-7",
                    2_000,
                )
            },
            Row {
                input: 999,
                ..row(
                    &model_json("zai-org/GLM-5.3", "friendli"),
                    "/repo/a/.loom/worktrees/issue-8",
                    2_000,
                )
            },
        ],
    );
    let rows =
        tokens_by_model_in(&db, &dirs(&["/repo/a", "/repo/a/.loom/worktrees/issue-7"]), None)
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].input, 30,
        "both attributed directories fold in; a SIBLING issue's worktree does not"
    );
}

#[test]
fn filters_by_time_window_inclusive_of_the_bounds() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[
            Row {
                input: 1,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 1_000)
            },
            Row {
                input: 2,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 2_000)
            },
            Row {
                input: 4,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 9_999_999)
            },
        ],
    );
    let start = Utc.timestamp_millis_opt(1_000).unwrap();
    let end = Utc.timestamp_millis_opt(2_000).unwrap();
    let rows = tokens_by_model_in(&db, &dirs(&["/repo/a"]), Some((start, end))).unwrap();
    assert_eq!(rows.len(), 1);
    // 1 (t=1000, on the lower bound) + 2 (t=2000, on the upper bound); the
    // t=9_999_999 row is excluded.
    assert_eq!(rows[0].input, 3);
}

#[test]
fn time_created_is_decoded_as_epoch_milliseconds_matching_the_live_schema() {
    // Live `opencode 1.18.31` writes e.g. 1790041222146 for 2026-09-22.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[Row {
            input: 7,
            ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 1_790_041_222_146)
        }],
    );
    let sessions =
        sessions_in(&db, &SessionFilter::directories(&dirs(&["/repo/a"])), None).unwrap();
    assert_eq!(sessions[0].created_at.to_rfc3339(), "2026-09-22T01:40:22.146+00:00");
}

// --- Discovery ------------------------------------------------------------

#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_finds_every_installed_version() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let opt = tmp.path().join(".loom").join("opt");
    for version in ["opencode-1.18.31", "opencode-2.0.10"] {
        let dir = opt.join(version).join("xdg").join("data").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.db"), b"").unwrap();
    }
    // A non-opencode sibling under .loom/opt must not be picked up.
    std::fs::create_dir_all(opt.join("pi-0.85.1")).unwrap();

    let found = discover_opencode_dbs(Some(tmp.path()));
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().all(|p| p.ends_with("opencode.db")));
}

#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_env_override_short_circuits_the_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("pinned.db");
    std::fs::write(&db, b"").unwrap();
    std::env::set_var(OPENCODE_DB_ENV, &db);
    let found = discover_opencode_dbs(Some(tmp.path()));
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    assert_eq!(found, vec![db]);
}

#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_yields_nothing_for_a_host_with_no_install() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    assert!(discover_opencode_dbs(Some(tmp.path())).is_empty());
}

/// Issue #8965: the operator's own OpenCode store at the XDG default is
/// discovered too — that is where live Z.ai GLM usage lands on a host with no
/// Loom-managed install.
#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_includes_the_xdg_default_store() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".local").join("share").join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("opencode.db"), b"").unwrap();
    let managed = tmp
        .path()
        .join(".loom/opt/opencode-2.0.10/xdg/data/opencode");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("opencode.db"), b"").unwrap();

    let found = discover_opencode_dbs(Some(tmp.path()));
    assert_eq!(found, vec![managed.join("opencode.db"), dir.join("opencode.db")]);
}

#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_honours_xdg_data_home() {
    std::env::remove_var(OPENCODE_DB_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("xdg-data");
    std::fs::create_dir_all(data.join("opencode")).unwrap();
    std::fs::write(data.join("opencode").join("opencode.db"), b"").unwrap();
    // A store at the home default is NOT read when XDG_DATA_HOME names another.
    let home_default = tmp.path().join(".local/share/opencode");
    std::fs::create_dir_all(&home_default).unwrap();
    std::fs::write(home_default.join("opencode.db"), b"").unwrap();

    std::env::set_var("XDG_DATA_HOME", &data);
    let found = discover_opencode_dbs(Some(tmp.path()));
    std::env::remove_var("XDG_DATA_HOME");
    assert_eq!(found, vec![data.join("opencode").join("opencode.db")]);
}

#[cfg(unix)]
#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_reads_a_store_reachable_two_ways_once() {
    std::env::remove_var(OPENCODE_DB_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let managed = tmp.path().join(".loom/opt/opencode-2.0.10/xdg/data");
    std::fs::create_dir_all(managed.join("opencode")).unwrap();
    std::fs::write(managed.join("opencode").join("opencode.db"), b"").unwrap();
    // XDG_DATA_HOME points (through a symlink) at the managed store's data dir.
    let link = tmp.path().join("data-link");
    std::os::unix::fs::symlink(&managed, &link).unwrap();

    std::env::set_var("XDG_DATA_HOME", &link);
    let found = discover_opencode_dbs(Some(tmp.path()));
    std::env::remove_var("XDG_DATA_HOME");
    assert_eq!(found, vec![managed.join("opencode").join("opencode.db")]);
}

/// Issue #11286: a guarded launch's per-launch `XDG_DATA_HOME` store under
/// the native-tools state dir is discovered too — that is where every
/// daemon-dispatched OpenCode sweep's sessions land.
#[test]
#[serial_test::serial(opencode_db_env)]
fn discover_opencode_dbs_includes_guarded_per_launch_stores() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".local/state/loom/native-tools");
    let mut launches = Vec::new();
    for (workspace, launch) in [("ws-a", "uuid-1"), ("ws-a", "uuid-2"), ("ws-b", "uuid-3")] {
        let dir = base.join(workspace).join(launch).join("data/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.db"), b"").unwrap();
        launches.push(dir.join("opencode.db"));
    }
    // A launch dir with no OpenCode store (a Pi launch), and a shared binding
    // tree beside the launches, contribute nothing.
    std::fs::create_dir_all(base.join("ws-a/uuid-pi/pi-agent")).unwrap();
    std::fs::create_dir_all(base.join("ws-a/bindings-abc")).unwrap();

    let mut found = discover_opencode_dbs(Some(tmp.path()));
    found.sort();
    launches.sort();
    assert_eq!(found, launches);
}

#[cfg(unix)]
#[test]
#[serial_test::serial(opencode_db_env)]
fn per_launch_stores_are_deduplicated_by_canonical_path() {
    std::env::remove_var(OPENCODE_DB_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp
        .path()
        .join(".local/state/loom/native-tools/ws/uuid-1/data");
    std::fs::create_dir_all(data.join("opencode")).unwrap();
    std::fs::write(data.join("opencode/opencode.db"), b"").unwrap();
    // An operator's XDG_DATA_HOME that is (through a symlink) that same dir.
    let link = tmp.path().join("data-link");
    std::os::unix::fs::symlink(&data, &link).unwrap();
    std::env::set_var("XDG_DATA_HOME", &link);
    let found = discover_opencode_dbs(Some(tmp.path()));
    std::env::remove_var("XDG_DATA_HOME");
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn the_per_launch_scan_is_bounded_and_prefers_the_most_recently_written() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let total = MAX_NATIVE_LAUNCH_DBS + 5;
    for i in 0..total {
        let dir = base
            .join("ws")
            .join(format!("uuid-{i:04}"))
            .join("data/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        std::fs::write(&db, b"").unwrap();
        let at = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(1_700_000_000 + i as u64);
        std::fs::File::options()
            .write(true)
            .open(&db)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }
    // The oldest store has a fresh WAL: a live launch writing through it.
    let oldest = base.join("ws/uuid-0000/data/opencode/opencode.db");
    std::fs::write(base.join("ws/uuid-0000/data/opencode/opencode.db-wal"), b"x").unwrap();

    let found = native_launch_dbs(base);
    assert_eq!(found.len(), MAX_NATIVE_LAUNCH_DBS);
    assert_eq!(found[0], oldest, "a fresh WAL counts as a recent write");
    assert!(
        !found.contains(&base.join("ws/uuid-0001/data/opencode/opencode.db")),
        "the stalest stores fall outside the bound"
    );
    assert!(native_launch_dbs(&base.join("absent")).is_empty());
}

/// Judge note on PR #11307: when the launch backlog exceeds the inspection
/// bound, the newest launch directories are inspected, not whichever ones
/// `read_dir` happened to yield first.
#[test]
fn a_launch_backlog_over_the_inspection_bound_keeps_the_newest_launches() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let total = 12;
    for i in 0..total {
        let launch = base.join("ws").join(format!("uuid-{i:04}"));
        let dir = launch.join("data/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.db"), b"").unwrap();
        let at = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(1_700_000_000 + i as u64);
        std::fs::File::open(&launch)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }
    let found = native_launch_dbs_bounded(base, 3);
    let mut expected: Vec<_> = (total - 3..total)
        .map(|i| base.join(format!("ws/uuid-{i:04}/data/opencode/opencode.db")))
        .collect();
    expected.sort();
    let mut got = found.clone();
    got.sort();
    assert_eq!(got, expected, "{found:?}");
}

/// End to end: a sweep's tokens in a guarded per-launch store reach the
/// per-sweep usage reader (and, through the same discovery, the burn sampler).
#[test]
#[serial_test::serial(opencode_db_env)]
fn tokens_by_model_reads_a_guarded_per_launch_store() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join(".local/state/loom/native-tools/ws/uuid-1/data/opencode");
    std::fs::create_dir_all(&dir).unwrap();
    seed_db(
        &dir.join("opencode.db"),
        &[Row {
            input: 11,
            output: 2,
            ..row(&model_json("glm-5.3-flash", "zai-coding-plan"), "/repo/wt", 1_000)
        }],
    );
    let rows = tokens_by_model(&dirs(&["/repo/wt"]), None, Some(tmp.path())).unwrap();
    assert_eq!((rows[0].model.as_str(), rows[0].input), ("glm-5.3-flash", 11));
}

#[test]
#[serial_test::serial(opencode_db_env)]
fn tokens_by_model_reads_the_xdg_default_store() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".local").join("share").join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    seed_db(
        &dir.join("opencode.db"),
        &[Row {
            input: 7,
            output: 1,
            ..row(&model_json("glm-5.3-flash", "zai-coding-plan"), "/repo/a", 1_000)
        }],
    );
    let rows = tokens_by_model(&dirs(&["/repo/a"]), None, Some(tmp.path())).unwrap();
    assert_eq!((rows[0].model.as_str(), rows[0].input), ("glm-5.3-flash", 7));
}

// --- tokens_by_model / sessions: merged across every installed version -----

#[test]
#[serial_test::serial(opencode_db_env)]
fn tokens_by_model_merges_matching_rows_across_every_installed_db() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    for (version, input) in [("opencode-1.18.31", 10), ("opencode-2.0.10", 20)] {
        let dir = tmp
            .path()
            .join(".loom")
            .join("opt")
            .join(version)
            .join("xdg")
            .join("data")
            .join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        seed_db(
            &dir.join("opencode.db"),
            &[Row {
                input,
                output: 1,
                ..row(&model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 1_000)
            }],
        );
    }
    let rows = tokens_by_model(&dirs(&["/repo/a"]), None, Some(tmp.path())).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].input, 30, "both installed versions' matching sessions are summed");

    let listed = sessions(&dirs(&["/repo/a"]), None, Some(tmp.path()));
    assert_eq!(listed.len(), 2, "the listing surface sees both too");
}

#[test]
#[serial_test::serial(opencode_db_env)]
fn tokens_by_model_returns_none_when_no_db_is_installed() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(tokens_by_model(&dirs(&["/repo/a"]), None, Some(tmp.path())), None);
    assert!(sessions(&dirs(&["/repo/a"]), None, Some(tmp.path())).is_empty());
}

/// Test-only convenience: [`fold_sessions`] over ONE fixture database,
/// bypassing [`discover_opencode_dbs`] so a test can point at a file directly
/// without touching the process-global env override. Uses the SAME production
/// fold [`tokens_by_model`] uses — no second copy of the arithmetic.
fn tokens_by_model_in(
    db: &Path,
    directories: &[PathBuf],
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<ModelUsageTotals>> {
    fold_sessions(sessions_in(db, &SessionFilter::directories(directories), window)?)
}

// --- Exact-id attribution (#14048) ---------------------------------------

/// A `Row` builder with an explicit session id, for the by-id tests —
/// [`seed_db`]'s positional `session-{i}` ids stay available through it too.
fn id_row<'a>(id: &'a str, model: &'a str, directory: &'a str, input: i64) -> Row<'a> {
    Row {
        model,
        input,
        output: 0,
        reasoning: 0,
        cache_read: 0,
        cache_write: 0,
        directory,
        created_ms: 1_000,
        id: Some(id),
    }
}

/// The exact ids a step's own log names select exactly those sessions —
/// including one whose `directory` is NOT in any caller-supplied set, because
/// the id is the whole key (#14048). A neighbouring session in the same store
/// under a different id stays out.
#[test]
#[serial_test::serial(opencode_db_env)]
fn an_exact_id_set_selects_only_the_sessions_it_names() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[
            id_row("ses_step", &model_json("zai-org/GLM-5.3", "friendli"), "/elsewhere", 500),
            id_row("ses_other", &model_json("zai-org/GLM-5.3", "friendli"), "/repo/a", 7),
        ],
    );
    std::env::set_var(OPENCODE_DB_ENV, &db);

    let sessions = sessions_for_ids(&["ses_step".to_string()], None);
    std::env::remove_var(OPENCODE_DB_ENV);
    assert_eq!(sessions.len(), 1, "the unnamed neighbour must stay out");
    assert_eq!(sessions[0].session_id.as_deref(), Some("ses_step"));
    // The id-keyed read ignores the window: a session created outside it is
    // still this launch's (the id cannot name another launch's session).
    assert_eq!(sessions[0].input, 500);

    std::env::set_var(OPENCODE_DB_ENV, &db);
    let totals = tokens_by_model_for_ids(&["ses_step".to_string()], None).unwrap();
    std::env::remove_var(OPENCODE_DB_ENV);
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].input, 500);
}

/// `tokens_by_model_for_ids` holds the module's "unknown != zero" contract:
/// ids no store holds answer `None`, never an empty breakdown.
#[test]
#[serial_test::serial(opencode_db_env)]
fn ids_no_store_holds_answer_none_not_an_empty_breakdown() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[id_row(
            "ses_x",
            &model_json("zai-org/GLM-5.3", "friendli"),
            "/repo/a",
            10,
        )],
    );
    std::env::set_var(OPENCODE_DB_ENV, &db);
    let totals = tokens_by_model_for_ids(&["ses_absent".to_string()], None);
    std::env::remove_var(OPENCODE_DB_ENV);
    assert_eq!(totals, None);
}

/// [`SessionFilter`]'s empty-match rule, pinned: a filter with neither
/// directories nor ids attributes NOTHING — "every session on this host" is
/// never a legitimate reading.
#[test]
fn an_entirely_empty_filter_matches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    seed_db(
        &db,
        &[id_row(
            "ses_x",
            &model_json("zai-org/GLM-5.3", "friendli"),
            "/repo/a",
            10,
        )],
    );
    assert!(sessions_in(&db, &SessionFilter::default(), None)
        .expect("db opened")
        .is_empty());
}

// --- Session-id selection (#14018) ---------------------------------------

#[test]
fn session_id_selection_counts_only_the_named_sessions_not_their_directory_neighbours() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    let model = model_json("m", "p");
    let mine = Row {
        input: 100,
        output: 10,
        ..row(&model, "/repo", 1_000)
    };
    let neighbour = Row {
        input: 9_999,
        ..row(&model, "/repo", 1_000)
    };
    seed_db(&db, &[mine, neighbour]);
    // `seed_db` names its rows `session-<index>`.
    let ids = ["session-0"].into_iter().collect();
    let sessions = sessions_by_id_in(&db, &ids).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id.as_deref(), Some("session-0"));
    let rows = fold_sessions(sessions).unwrap();
    assert_eq!((rows[0].input, rows[0].output), (100, 10));
    // No directory or window narrows an id: a session elsewhere, long ago, is
    // still the one the log named.
    let elsewhere = Row {
        input: 7,
        ..row(&model, "/somewhere/else", 5)
    };
    seed_db(&tmp.path().join("other.db"), &[elsewhere]);
    let ids = ["session-0"].into_iter().collect();
    assert_eq!(
        sessions_by_id_in(&tmp.path().join("other.db"), &ids)
            .unwrap()
            .len(),
        1
    );
    // An id the store does not hold selects nothing (an empty read, not a failure).
    let ids = ["ses_unknown"].into_iter().collect();
    assert_eq!(sessions_by_id_in(&db, &ids).unwrap(), Vec::new());
}

#[test]
fn no_session_ids_yield_no_totals() {
    assert_eq!(tokens_by_session_ids(&[], None), None);
}

// --- Id branch is the whole key (#14278) ---------------------------------

fn raw_row(id: &str, directory: &str, time_created: Option<i64>) -> RawSessionRow {
    RawSessionRow {
        id: Some(id.to_string()),
        model: Some(model_json("zai-org/GLM-5.3", "friendli")),
        input: Some(5),
        output: Some(1),
        reasoning: None,
        cache_read: None,
        cache_write: None,
        directory: Some(directory.to_string()),
        time_created,
    }
}

/// An id-keyed decode attributes a row whose `time_created` is NULL or out of
/// range — the id is the whole key, so the window-only column must not drop
/// it. The directory+window branch still drops it (an unattributable instant
/// is never assumed to be inside the window).
#[test]
fn an_id_keyed_row_without_a_decodable_time_created_is_still_attributed() {
    let ids = SessionFilter::ids(&["ses_a".to_string()]);
    for time_created in [None, Some(i64::MAX)] {
        let decoded = raw_row("ses_a", "/repo", time_created)
            .decode(&ids, None)
            .unwrap_or_else(|| panic!("id-matched row dropped for time_created={time_created:?}"));
        assert_eq!(decoded.session_id.as_deref(), Some("ses_a"));
        assert_eq!(decoded.input, 5);
    }
    // Pin the sibling branch so the asymmetry stays deliberate.
    let dirs_only = SessionFilter::directories(&dirs(&["/repo"]));
    assert!(raw_row("ses_a", "/repo", None)
        .decode(&dirs_only, None)
        .is_none());
}

/// One session id held by two installed stores (a cross-version copy) is one
/// session: BOTH id-keyed entry points count it once.
#[test]
#[serial_test::serial(opencode_db_env)]
fn one_id_in_two_stores_is_counted_once_by_both_id_entry_points() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    for version in ["opencode-1.18.31", "opencode-2.0.10"] {
        let dir = tmp
            .path()
            .join(".loom")
            .join("opt")
            .join(version)
            .join("xdg")
            .join("data")
            .join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        seed_db(
            &dir.join("opencode.db"),
            &[id_row(
                "ses_dup",
                &model_json("zai-org/GLM-5.3", "friendli"),
                "/repo/a",
                10,
            )],
        );
    }
    let ids = ["ses_dup".to_string()];

    assert_eq!(sessions_for_ids(&ids, Some(tmp.path())).len(), 1);
    for totals in [
        tokens_by_model_for_ids(&ids, Some(tmp.path())),
        tokens_by_session_ids(&ids, Some(tmp.path())),
    ] {
        let totals = totals.unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].input, 10, "the duplicated id must not be summed twice");
    }

    // Directory-keyed reads are unchanged: they are not id-deduped.
    let listed = sessions(&dirs(&["/repo/a"]), None, Some(tmp.path()));
    assert_eq!(listed.len(), 2);
}

/// The cross-store dedupe runs over usage-bearing rows only: when the
/// first-discovered copy of an id is the all-zero creation row OpenCode writes
/// before any usage lands, it must not claim the id slot and shadow the other
/// store's copy that holds the usage (#14440). Both id-keyed entry points report
/// that usage exactly once. The seeded store names sort so the zero row is
/// discovered first (loom-managed stores are visited in sorted order).
#[test]
#[serial_test::serial(opencode_db_env)]
fn an_all_zero_first_copy_does_not_shadow_the_usage_bearing_copy() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let model = model_json("zai-org/GLM-5.3", "friendli");
    for (version, input) in [("opencode-1.18.31", 0), ("opencode-2.0.10", 10)] {
        let dir = tmp
            .path()
            .join(".loom")
            .join("opt")
            .join(version)
            .join("xdg")
            .join("data")
            .join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        seed_db(&dir.join("opencode.db"), &[id_row("ses_dup", &model, "/repo/a", input)]);
    }
    let ids = ["ses_dup".to_string()];
    let dbs = discover_opencode_dbs(Some(tmp.path()));
    assert!(
        dbs[0].to_string_lossy().contains("opencode-1.18.31"),
        "fixture precondition: the all-zero store must be discovered first, got {dbs:?}"
    );

    let listed = sessions_for_ids(&ids, Some(tmp.path()));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].input, 10);
    for totals in [
        tokens_by_model_for_ids(&ids, Some(tmp.path())),
        tokens_by_session_ids(&ids, Some(tmp.path())),
    ] {
        let totals = totals.expect("the usage-bearing copy must be counted, not shadowed");
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].input, 10);
    }
}

/// An empty id set matches nothing through every id-keyed entry point, even
/// when a store holding sessions is installed.
#[test]
#[serial_test::serial(opencode_db_env)]
fn an_empty_id_set_selects_nothing_even_with_a_populated_store() {
    std::env::remove_var(OPENCODE_DB_ENV);
    std::env::remove_var("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join(".loom")
        .join("opt")
        .join("opencode-1.18.31")
        .join("xdg")
        .join("data")
        .join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    seed_db(
        &dir.join("opencode.db"),
        &[id_row(
            "ses_a",
            &model_json("zai-org/GLM-5.3", "friendli"),
            "/repo/a",
            10,
        )],
    );
    assert_eq!(sessions_for_ids(&[], Some(tmp.path())), Vec::new());
    assert_eq!(tokens_by_model_for_ids(&[], Some(tmp.path())), None);
    assert_eq!(tokens_by_session_ids(&[], Some(tmp.path())), None);
}
