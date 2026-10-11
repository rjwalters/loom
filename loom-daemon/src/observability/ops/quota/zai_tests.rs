use super::*;

const BODY: &str = r#"{"code":200,"msg":"ok","success":true,"data":{"level":"max","limits":[
 {"type":"CREDIT_LIMIT","unit":3,"usage":1000,"currentValue":250,"nextResetTime":1760000000000},
 {"type":"CREDIT_LIMIT","unit":6,"usage":4000,"currentValue":1000,"nextResetTime":1760500000000},
 {"type":"TIME_LIMIT","unit":5,"usage":100,"currentValue":100,"nextResetTime":1}]}}"#;

fn at(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

#[test]
fn parses_both_windows() {
    let q = parse_quota(BODY).unwrap();
    let five = q.five_hour.unwrap();
    let week = q.weekly.unwrap();
    assert_eq!(five.fraction, Some(0.25));
    assert_eq!(five.resets_at, at(1_760_000_000_000));
    assert_eq!(week.fraction, Some(0.25));
    assert_eq!(week.resets_at, at(1_760_500_000_000));
}

#[test]
fn fraction_is_clamped_and_zero_cap_is_unknown() {
    let body = r#"{"success":true,"data":{"limits":[
 {"type":"CREDIT_LIMIT","unit":3,"usage":100,"currentValue":250,"nextResetTime":1},
 {"type":"CREDIT_LIMIT","unit":6,"usage":0,"currentValue":0,"nextResetTime":2}]}}"#;
    let q = parse_quota(body).unwrap();
    assert_eq!(q.five_hour.unwrap().fraction, Some(1.0));
    assert_eq!(q.weekly.unwrap().fraction, None);
}

#[test]
fn failures_are_errors_never_zero() {
    assert_eq!(parse_quota(r#"{"success":false,"msg":"bad key"}"#), Err(ZaiError::Rejected));
    assert_eq!(parse_quota("<html>"), Err(ZaiError::Malformed));
    assert_eq!(parse_quota(r#"{"success":true,"data":{}}"#), Err(ZaiError::Malformed));
}

#[test]
fn missing_window_is_unknown_not_zero() {
    let body = r#"{"success":true,"data":{"limits":[
 {"type":"CREDIT_LIMIT","unit":6,"usage":10,"currentValue":5,"nextResetTime":3}]}}"#;
    let q = parse_quota(body).unwrap();
    let row = account_state("a", Some(&q), false, None);
    assert_eq!(row.usage_fraction, None);
    assert_eq!(row.usage_fraction_weekly, Some(0.5));
    assert!(!row.exhausted);
}

#[test]
fn unknown_reading_row_has_no_fractions() {
    let row = account_state("seat", None, false, None);
    assert_eq!(row.account, "seat");
    assert_eq!(row.provider, "zai");
    assert_eq!(row.usage_fraction, None);
    assert_eq!(row.usage_fraction_weekly, None);
    assert!(!row.exhausted);
}

#[test]
fn binding_window_is_weekly_when_spent_else_five_hour() {
    let spent_week = r#"{"success":true,"data":{"limits":[
 {"type":"CREDIT_LIMIT","unit":3,"usage":10,"currentValue":1,"nextResetTime":1000},
 {"type":"CREDIT_LIMIT","unit":6,"usage":10,"currentValue":10,"nextResetTime":2000}]}}"#;
    let row = account_state("a", Some(&parse_quota(spent_week).unwrap()), false, None);
    assert!(row.exhausted);
    assert_eq!(row.limit_window_reset_at, at(2000));
    let healthy = parse_quota(BODY).unwrap();
    let row = account_state("a", Some(&healthy), false, None);
    assert!(!row.exhausted);
    assert_eq!(row.limit_window_reset_at, at(1_760_000_000_000));
}

#[test]
fn bad_mark_marks_exhausted_even_without_reading() {
    let row = account_state("a", None, true, at(5000));
    assert!(row.exhausted);
    assert_eq!(row.limit_window_reset_at, at(5000));
}

#[test]
fn cache_serves_within_ttl_and_backs_off_failures() {
    let mut cache = ZaiCache::default();
    let t0 = Instant::now();
    assert!(cache.cached("a", t0).is_none());
    cache.store("a", t0, Ok(parse_quota(BODY).unwrap()));
    assert!(cache.cached("a", t0 + TTL / 2).unwrap().is_some());
    assert!(cache.cached("a", t0 + TTL).is_none());

    cache.store("b", t0, Err(ZaiError::Status(500)));
    assert_eq!(cache.cached("b", t0 + TTL), Some(None));
    cache.store("b", t0, Err(ZaiError::Timeout));
    assert_eq!(cache.cached("b", t0 + TTL * 2), Some(None));
    assert!(cache.cached("b", t0 + TTL * 5).is_none());
    // A success resets the backoff.
    cache.store("b", t0, Ok(ZaiQuota::default()));
    assert!(cache.cached("b", t0 + TTL).is_none());
}

#[test]
fn error_display_never_carries_key_or_body() {
    for e in [
        ZaiError::Transport,
        ZaiError::Timeout,
        ZaiError::Status(401),
        ZaiError::Rejected,
        ZaiError::Malformed,
    ] {
        let text = format!("{e} {e:?}");
        assert!(!text.contains("http"), "{text}");
    }
}

#[tokio::test]
async fn no_seats_means_no_rows_and_no_request() {
    assert!(sample(Vec::new()).await.is_empty());
}

// --- real fetch path against a controlled loopback endpoint ---------------

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One-shot loopback server. Returns its URL and a handle yielding the raw
/// request text. `reply` is the full raw response; `None` never answers.
async fn serve_once(reply: Option<String>) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/quota", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let mut req = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        match reply {
            Some(r) => {
                let _ = sock.write_all(r.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
            None => tokio::time::sleep(Duration::from_secs(5)).await,
        }
        String::from_utf8_lossy(&req).into_owned()
    });
    (url, handle)
}

fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder().timeout(timeout).build().unwrap()
}

#[tokio::test]
async fn fetch_success_parses_and_sends_key_only_in_authorization_header() {
    let key = "fake-zai-key-abc123";
    let (url, server) = serve_once(Some(http("200 OK", BODY))).await;
    let q = fetch_from(&client(Duration::from_secs(5)), &url, key)
        .await
        .unwrap();
    assert_eq!(q.five_hour.unwrap().fraction, Some(0.25));
    assert_eq!(q.weekly.unwrap().fraction, Some(0.25));

    let request = server.await.unwrap();
    let (request_line, headers) = request.split_once("\r\n").unwrap();
    assert!(request_line.starts_with("GET /quota "));
    assert!(!request_line.contains(key), "key must not be in the URL");
    let carrying: Vec<&str> = headers.lines().filter(|l| l.contains(key)).collect();
    assert_eq!(carrying.len(), 1, "key appears in exactly one header");
    assert!(carrying[0]
        .to_ascii_lowercase()
        .starts_with("authorization:"));
}

#[tokio::test]
async fn fetch_non_2xx_is_a_status_error_without_leaking() {
    let key = "fake-zai-key-abc123";
    let (url, _server) = serve_once(Some(http("503 Service Unavailable", "{}"))).await;
    let err = fetch_from(&client(Duration::from_secs(5)), &url, key)
        .await
        .unwrap_err();
    assert_eq!(err, ZaiError::Status(503));
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains(key) && !shown.contains("127.0.0.1"));
}

#[tokio::test]
async fn fetch_timeout_is_a_timeout_error() {
    let (url, server) = serve_once(None).await;
    let err = fetch_from(&client(Duration::from_millis(200)), &url, "fake-zai-key-abc123")
        .await
        .unwrap_err();
    assert_eq!(err, ZaiError::Timeout);
    server.abort();
}
