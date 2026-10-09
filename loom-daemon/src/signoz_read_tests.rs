//! The SigNoz read client without a network: URL binding, credential
//! refusal, and [`FileRows`] paging.

use super::*;
use chrono::TimeZone;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn ns(secs: i64) -> i64 {
    secs * 1_000_000_000
}

fn reader(credential_file: Option<PathBuf>) -> ClickhouseHttp {
    ClickhouseHttp {
        // Unroutable on purpose: a credential check must fail first.
        endpoint: "http://127.0.0.1:9".to_string(),
        user: Some("reader".to_string()),
        credential_file,
        timeout: std::time::Duration::from_millis(200),
    }
}

fn query(filters: &[(&str, &str)], after: Option<RowCursor>, limit: u32) -> PageQuery {
    PageQuery {
        filters: filters
            .iter()
            .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
            .collect(),
        since: at(1_000),
        until: at(2_000),
        after,
        limit,
    }
}

#[test]
fn every_filter_and_the_keyset_are_bound_as_url_parameters() {
    let http = ClickhouseHttp {
        endpoint: "https://telemetry.example:8443".to_string(),
        ..reader(None)
    };
    let q = query(&[("repo", "acme/app"), ("host", "h1")], Some((42, "rec-01".into())), 7);
    let url = http.url(&q).unwrap();
    let pairs: std::collections::BTreeMap<String, String> =
        url.query_pairs().into_owned().collect();
    assert_eq!(pairs["param_repo"], "acme/app");
    assert_eq!(pairs["param_host"], "h1");
    assert_eq!(pairs["param_since_ns"], ns(1_000).to_string());
    assert_eq!(pairs["param_until_ns"], ns(2_000).to_string());
    assert_eq!(pairs["param_after_ns"], "42");
    assert_eq!(pairs["param_after_id"], "rec-01");
    assert_eq!(pairs["param_limit"], "7");
    assert_eq!(pairs.len(), 7, "{pairs:?}");

    let first = http.url(&query(&[], None, 1)).unwrap();
    let pairs: std::collections::BTreeMap<String, String> =
        first.query_pairs().into_owned().collect();
    assert_eq!(pairs["param_after_ns"], "0");
    assert_eq!(pairs["param_after_id"], "");

    let arbitrary = http
        .url_with(&[("as_of_ns".to_string(), "5".to_string())])
        .unwrap();
    assert_eq!(arbitrary.query(), Some("param_as_of_ns=5"));
}

#[test]
fn a_non_http_endpoint_is_refused() {
    let file = ClickhouseHttp {
        endpoint: "file:///etc/passwd".to_string(),
        ..reader(None)
    };
    assert!(matches!(file.url(&query(&[], None, 1)), Err(ReadError::Unavailable(_))));
    let garbage = ClickhouseHttp {
        endpoint: "not a url".to_string(),
        ..reader(None)
    };
    assert!(garbage.post_with("SELECT 1", &[]).is_err());
}

#[cfg(unix)]
#[test]
fn a_credential_file_readable_by_others_is_refused_and_never_echoed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("signoz-read.key");
    std::fs::write(&path, "s3cret-value\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut pages = SqlPages {
        http: reader(Some(path)),
        sql: "SELECT 1",
    };
    let Err(ReadError::Unavailable(why)) = pages.page(&query(&[], None, 1)) else {
        panic!("a group/world-readable credential must be refused");
    };
    assert!(why.contains("readable by group or others"), "{why}");
    assert!(!why.contains("s3cret"), "{why}");
}

#[cfg(unix)]
#[test]
fn an_empty_or_missing_credential_file_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.key");
    std::fs::write(&path, "  \n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let Err(ReadError::Unavailable(why)) = reader(Some(path)).post_with("SELECT 1", &[]) else {
        panic!("an empty credential must be refused");
    };
    assert!(why.ends_with(": empty"), "{why}");

    let missing = dir.path().join("absent.key");
    let Err(ReadError::Unavailable(why)) = reader(Some(missing)).post_with("SELECT 1", &[]) else {
        panic!("a missing credential must be refused");
    };
    assert!(why.ends_with(": unreadable"), "{why}");
}

#[cfg(unix)]
#[test]
fn an_owner_only_credential_is_read_and_trimmed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ok.key");
    std::fs::write(&path, "  pw\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(reader(Some(path)).password().unwrap().as_deref(), Some("pw"));
    assert_eq!(reader(None).password().unwrap(), None);
}

fn row(knowable_s: i64, id: &str, repo: &str) -> String {
    format!(
        r#"{{"knowable_time_ns":"{}","record_id":"{id}","repo":"{repo}"}}"#,
        ns(knowable_s)
    )
}

fn ids(page: &str) -> Vec<String> {
    page.lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["record_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn file_rows_page_in_keyset_order_under_the_window_and_filters() {
    let text = [
        row(1_500, "b", "acme/app"),
        row(1_500, "a", "ACME/App"),
        row(1_200, "z", "acme/app"),
        row(1_700, "c", "other/repo"),
        row(999, "early", "acme/app"),
        row(2_001, "late", "acme/app"),
        String::new(),
        row(1_900, "d", "acme/app"),
    ]
    .join("\n");
    let mut rows = FileRows::parse(&text).unwrap();
    let repo = [("repo", "acme/app")];

    let first = rows.page(&query(&repo, None, 2)).unwrap();
    assert_eq!(ids(&first), ["z", "a"]);
    let second = rows
        .page(&query(&repo, Some((ns(1_500), "a".into())), 2))
        .unwrap();
    assert_eq!(ids(&second), ["b", "d"]);
    let last = rows
        .page(&query(&repo, Some((ns(1_900), "d".into())), 2))
        .unwrap();
    assert_eq!(last, "");

    // No filter: every row in the window, whatever its repo.
    let all = rows.page(&query(&[], None, 100)).unwrap();
    assert_eq!(ids(&all), ["z", "a", "b", "c", "d"]);
    // A filter on a column a row lacks excludes it.
    let none = rows.page(&query(&[("host", "h1")], None, 100)).unwrap();
    assert_eq!(none, "");
}

#[test]
fn a_malformed_export_is_refused_with_its_line_number() {
    let missing_cursor = r#"{"record_id":"a"}"#;
    let err = FileRows::parse(&format!("{}\n{missing_cursor}", row(1, "x", "r"))).unwrap_err();
    assert!(err.starts_with("line 2:"), "{err}");
    assert!(FileRows::parse(r#"{"knowable_time_ns":1}"#)
        .unwrap_err()
        .contains("record_id"));
    assert!(FileRows::parse("not json")
        .unwrap_err()
        .contains("not JSON"));
    // A null record id is the empty id, not a malformed row.
    let rows = FileRows::parse(r#"{"knowable_time_ns":5,"record_id":null}"#).unwrap();
    assert_eq!(rows.rows[0].0, (5, String::new()));
}

#[test]
fn page_query_filter_looks_up_by_name() {
    let q = query(&[("repo", "acme/app")], None, 1);
    assert_eq!(q.filter("repo"), Some("acme/app"));
    assert_eq!(q.filter("host"), None);
}
