//! Tests for the OpenCode burn source (Issue #8930). The fixture schema is
//! the `message` table of `opencode.db` on a live host (2026-09-25).

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;

use super::{pool_provider, OpencodeSource, BURN_QUERY};
use crate::observability::ops::quota::burn::Burn;

fn store(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL, \
         time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
         CREATE TABLE credential (id text, secret text);
         INSERT INTO credential VALUES ('c', 'sk-PLANTED-SECRET');",
    )
    .unwrap();
    conn
}

/// Insert an assistant step as OpenCode does: zero tokens, no completion.
fn start(conn: &Connection, id: &str, provider: &str, at: DateTime<Utc>) {
    let data = serde_json::json!({
        "role": "assistant", "providerID": provider, "modelID": "glm-5.3-flash",
        "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "time": {"created": at.timestamp_millis()},
    });
    conn.execute(
        "INSERT INTO message VALUES (?1, 's', ?2, ?2, ?3)",
        rusqlite::params![id, at.timestamp_millis(), data.to_string()],
    )
    .unwrap();
}

/// Complete it in place, as OpenCode does.
fn complete(conn: &Connection, id: &str, at: DateTime<Utc>, tokens: (i64, i64, i64, i64)) {
    let (input, output, reasoning, read) = tokens;
    conn.execute(
        "UPDATE message SET time_updated = ?2, data = json_set(json_set(data, '$.tokens', \
         json(?3)), '$.time.completed', ?2) WHERE id = ?1",
        rusqlite::params![
            id,
            at.timestamp_millis(),
            serde_json::json!({"input": input, "output": output, "reasoning": reasoning,
                               "cache": {"read": read, "write": 0}})
            .to_string()
        ],
    )
    .unwrap();
}

#[test]
fn the_only_query_names_the_message_table_and_nothing_else() {
    let sql = BURN_QUERY.to_ascii_lowercase();
    assert!(sql.starts_with("select ") && sql.contains(" from message "));
    for forbidden in ["credential", "account", "session", "join", ";", "part"] {
        assert!(!sql.contains(forbidden), "BURN_QUERY must not mention {forbidden}");
    }
    assert!(!sql.contains("select data") && !sql.contains(", data "), "never the raw blob");
}

#[test]
fn opencode_provider_ids_map_to_the_pool_namespace() {
    assert_eq!(pool_provider("zai-coding-plan"), "zai");
    assert_eq!(pool_provider("zai"), "zai");
    assert_eq!(pool_provider("kimi-for-coding"), "kimi");
    assert_eq!(pool_provider("Cerebras"), "cerebras");
}

/// Acceptance (#8930): a session spanning two windows is split between them
/// with no double count — a step counts in the window its completion falls
/// in, however long it streamed and however often its row is updated.
#[test]
fn a_session_spanning_two_windows_is_split_with_no_double_count() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    let conn = store(&db);
    let base = Utc::now() - Duration::seconds(3600);
    start(&conn, "old", "zai-coding-plan", base - Duration::seconds(100));
    complete(&conn, "old", base - Duration::seconds(90), (999, 999, 0, 0));
    let source = OpencodeSource {
        dbs: Some(vec![db.clone()]),
        ..OpencodeSource::default()
    };
    let mut burn = Burn::new(vec![Box::new(source)]);
    assert!(burn.sample(base).is_none(), "anchor");

    start(&conn, "a", "zai-coding-plan", base + Duration::seconds(10));
    complete(&conn, "a", base + Duration::seconds(20), (100, 10, 5, 1000));
    // Started in window 1, completes in window 2.
    start(&conn, "b", "zai-coding-plan", base + Duration::seconds(200));
    // A failed request: no tokens, not usage.
    start(&conn, "err", "zai-coding-plan", base + Duration::seconds(30));
    complete(&conn, "err", base + Duration::seconds(31), (0, 0, 0, 0));
    let (_, end1, first) = burn.sample(base + Duration::seconds(300)).unwrap();
    complete(&conn, "b", end1 + Duration::seconds(5), (200, 20, 0, 2000));
    // A row touched again later is not counted again.
    conn.execute(
        "UPDATE message SET time_updated = ?1 WHERE id = 'a'",
        [(end1 + Duration::seconds(6)).timestamp_millis()],
    )
    .unwrap();
    let (_, _, second) = burn.sample(base + Duration::seconds(600)).unwrap();

    let key = ("zai".to_string(), "glm-5.3-flash".to_string());
    assert_eq!(
        (
            first[&key].input,
            first[&key].output,
            first[&key].cache_read,
            first[&key].requests
        ),
        (100, 15, 1000, 1),
        "reasoning is billed as output"
    );
    assert_eq!((second[&key].input, second[&key].output, second[&key].requests), (200, 20, 1));
}

#[test]
fn an_unreadable_store_keeps_its_cursor_and_is_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("opencode.db");
    let base = Utc::now() - Duration::seconds(3600);
    let source = OpencodeSource {
        dbs: Some(vec![db.clone()]),
        ..OpencodeSource::default()
    };
    let mut burn = Burn::new(vec![Box::new(source)]);
    burn.sample(base);
    assert!(
        burn.sample(base + Duration::seconds(300))
            .unwrap()
            .2
            .is_empty(),
        "no store yet"
    );
    let conn = store(&db);
    start(&conn, "a", "zai", base + Duration::seconds(10));
    complete(&conn, "a", base + Duration::seconds(20), (1, 1, 0, 0));
    drop(conn);
    // The step completed while the store could not be read: it still counts.
    let (_, _, window) = burn.sample(base + Duration::seconds(600)).unwrap();
    assert_eq!(window[&("zai".to_string(), "glm-5.3-flash".to_string())].requests, 1);
}
