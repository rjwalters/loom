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
                format!("session-{i}"),
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
        }],
    );
    let sessions = sessions_in(&db, &dirs(&["/repo/a"]), None).expect("db opened");
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
             model TEXT, tokens_input INTEGER, tokens_output INTEGER,
             tokens_reasoning INTEGER, tokens_cache_read INTEGER,
             tokens_cache_write INTEGER, directory TEXT, time_created INTEGER
         );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES (?1, 10, 20, 0, 5, 1, '/repo/a', 1000)",
        rusqlite::params![model_json("zai-org/GLM-5.3", "friendli")],
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
        }],
    );
    let sessions = sessions_in(&db, &dirs(&["/repo/a"]), None).unwrap();
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
        sessions_in(&db, &dirs(&["/repo/a"]), None).unwrap().len(),
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
        sessions_in(&db, &dirs(&["/repo/a"]), None),
        Some(Vec::new()),
        "`opened fine, nothing matched` is distinct from `could not open`"
    );
    assert_eq!(
        sessions_in(Path::new("/nonexistent/opencode.db"), &dirs(&["/repo/a"]), None),
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
    let sessions = sessions_in(&db, &dirs(&["/repo/a"]), None).unwrap();
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
    fold_sessions(sessions_in(db, directories, window)?)
}
