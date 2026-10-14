//! The SigNoz read client: URL binding, the request shape against a loopback
//! listener (no credential header), endpoint resolution, and [`FileRows`]
//! paging.

use super::*;
use chrono::TimeZone;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn ns(secs: i64) -> i64 {
    secs * 1_000_000_000
}

fn reader(endpoint: &str) -> TelemetryReader {
    TelemetryReader {
        endpoint: endpoint.to_string(),
        timeout: std::time::Duration::from_secs(10),
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
    let http = reader("https://reader.example:14341");
    let q = query(&[("repo", "acme/app"), ("host", "h1")], Some((42, "rec-01".into())), 7);
    let url = http.url(&q).unwrap();
    assert_eq!(url.path(), QUERY_PATH);
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
    // Only `param_<name>` keys: the reader refuses any other.
    assert!(pairs.keys().all(|k| k.starts_with("param_")), "{pairs:?}");

    let first = http.url(&query(&[], None, 1)).unwrap();
    let pairs: std::collections::BTreeMap<String, String> =
        first.query_pairs().into_owned().collect();
    assert_eq!(pairs["param_after_ns"], "0");
    assert_eq!(pairs["param_after_id"], "");

    let arbitrary = http
        .url_with(&[("as_of_ns".to_string(), "5".to_string())])
        .unwrap();
    assert_eq!(arbitrary.as_str(), "https://reader.example:14341/v1/query?param_as_of_ns=5");
    // A trailing slash is still the bare base; no parameters, no `?`.
    let bare = reader("http://reader.example:14341/")
        .url_with(&[])
        .unwrap();
    assert_eq!(bare.as_str(), "http://reader.example:14341/v1/query");
}

#[test]
fn an_endpoint_that_is_not_a_bare_http_base_is_refused_and_never_echoed() {
    for endpoint in [
        "file:///etc/passwd",
        "not a url",
        "http://reader.example:14341/v1/query",
        "http://reader.example:14341/?user=x",
        "http://reader.example:14341/#frag",
        "http://u:s3cret@reader.example:14341",
    ] {
        let Err(ReadError::Unavailable(why)) = reader(endpoint).url_with(&[]) else {
            panic!("{endpoint} must be refused");
        };
        assert!(why.starts_with("invalid reader endpoint"), "{why}");
        assert!(!why.contains("s3cret"), "{why}");
    }
    assert!(reader("not a url").post_with("SELECT 1", &[]).is_err());
}

/// One request as a local listener received it.
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Serve one request on a loopback port with `status` and `reply`; the
/// listener's base URL and a handle yielding what it received.
fn one_shot(status: &'static str, reply: &'static str) -> (String, std::thread::JoinHandle<Seen>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (k, v) = line.split_once(':').unwrap();
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        let len: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map_or(0, |(_, v)| v.parse().unwrap());
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        Seen {
            request_line: request_line.trim_end().to_string(),
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    });
    (base, handle)
}

#[test]
fn a_query_posts_the_sql_as_the_body_and_params_in_the_url_with_no_credential() {
    let (base, server) = one_shot("200 OK", "{\"n\":1}\n");
    let sql = "SELECT count() AS n FROM t WHERE x = {v:String} FORMAT JSONEachRow";
    let got = reader(&base)
        .post_with(sql, &[("v".to_string(), "1; DROP TABLE x".to_string())])
        .unwrap();
    assert_eq!(got, "{\"n\":1}\n");
    let seen = server.join().unwrap();
    assert_eq!(seen.request_line, "POST /v1/query?param_v=1%3B+DROP+TABLE+x HTTP/1.1");
    assert_eq!(seen.body, sql, "the statement is the body, unaltered");
    for (name, _) in &seen.headers {
        assert!(
            !name.starts_with("x-clickhouse") && name != "authorization",
            "no credential header may be sent: {:?}",
            seen.headers
        );
    }
}

#[test]
fn a_reader_error_status_is_refused_with_its_answer() {
    let (base, server) = one_shot("403 Forbidden", "unknown_caller");
    let Err(ReadError::Refused(why)) = reader(&base).post_with("SELECT 1", &[]) else {
        panic!("a 403 must be Refused");
    };
    assert!(why.contains("403") && why.contains("unknown_caller"), "{why}");
    server.join().unwrap();
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

#[test]
fn endpoint_config_reads_the_env_then_the_config_and_names_retired_keys() {
    let config = serde_json::json!({
        "telemetry": {"signoz": {
            "endpoint": " https://reader.example:14341 ",
            "user": "reader",
            "keyFile": "/home/me/.config/loom/signoz.key"
        }},
        "autonomous": {"eta": {"fleetRefresh": {"signoz": {
            "endpoint": "https://old.example:8443"
        }}}}
    });
    let c = EndpointConfig::resolve(&config, None);
    assert_eq!(c.endpoint.as_deref(), Some("https://reader.example:14341"));
    assert_eq!(
        c.ignored,
        [
            "telemetry.signoz.user",
            "telemetry.signoz.keyFile",
            "autonomous.eta.fleetRefresh.signoz"
        ]
    );
    // The environment wins; a blank one is unset.
    let env = EndpointConfig::resolve(&config, Some("http://env.example:14341"));
    assert_eq!(env.endpoint.as_deref(), Some("http://env.example:14341"));
    let blank = EndpointConfig::resolve(&config, Some("  "));
    assert_eq!(blank.endpoint.as_deref(), Some("https://reader.example:14341"));

    // The retired ETA block is never read as an endpoint.
    let retired_only = EndpointConfig::resolve(
        &serde_json::json!({"autonomous": {"eta": {"fleetRefresh": {"signoz": {
            "endpoint": "https://old.example:8443"}}}}}),
        None,
    );
    assert_eq!(retired_only.endpoint, None);
    assert_eq!(EndpointConfig::resolve(&serde_json::json!({}), None), EndpointConfig::default());
}
