//! Tests for the loom-ui alert output (#11295): the contract's charsets, the
//! status classification, the outbox's order and bounds, and the drain loop
//! against a fake sink and a real loopback HTTP one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::outbox::{backoff, Next, Outbox, BACKOFF_CAP, MAX_ATTEMPTS};
use super::*;

fn alert(rule: &str, key: &str, state: State) -> Alert {
    Alert::new(rule, key, "title", Severity::Warning, state)
}

// ---------------------------------------------------------------- sanitize

#[test]
fn identifiers_are_fitted_to_the_contract_charset() {
    assert_eq!(sanitize::ident("fleet.dispatch-halted"), "fleet.dispatch-halted");
    assert_eq!(sanitize::ident("  -my host:1 "), "my-host-1");
    assert_eq!(sanitize::ident("…"), "unknown");
    let long = "a".repeat(100);
    let fitted = sanitize::ident(&long);
    assert_eq!(fitted.chars().count(), sanitize::IDENT_MAX);
    assert_eq!(fitted, sanitize::ident(&long), "stable for the same input");
}

#[test]
fn keys_keep_their_extra_characters_and_stay_distinct_when_cut() {
    assert_eq!(sanitize::key("owner/repo#12 x@y+z"), "owner/repo-12-x@y+z");
    let a = sanitize::key(&format!("{}a", "k".repeat(200)));
    let b = sanitize::key(&format!("{}b", "k".repeat(200)));
    assert_eq!(a.chars().count(), sanitize::KEY_MAX);
    assert_ne!(a, b, "the hash suffix keeps two long keys apart");
}

#[test]
fn titles_are_one_printable_line_of_at_most_200_characters() {
    assert_eq!(sanitize::title("a\nb\tc\u{7}", "r"), "a b c");
    assert_eq!(sanitize::title("  \n ", "fleet.x"), "fleet.x");
    let t = sanitize::title(&"é".repeat(300), "r");
    assert_eq!(t.chars().count(), sanitize::TITLE_MAX);
    assert!(t.ends_with('…'));
}

#[test]
fn detail_keeps_newlines_and_is_bounded() {
    assert_eq!(sanitize::detail("a\nb\u{1b}[0m"), "a\nb[0m");
    assert_eq!(sanitize::detail(&"x".repeat(5000)).chars().count(), sanitize::DETAIL_MAX);
}

#[test]
fn only_plain_https_links_are_sent() {
    assert_eq!(
        sanitize::link("https://github.com/o/r/issues/1").as_deref(),
        Some("https://github.com/o/r/issues/1")
    );
    assert_eq!(sanitize::link("http://github.com/x"), None);
    assert_eq!(sanitize::link("https://user:pw@github.com/x"), None);
    assert_eq!(sanitize::link(""), None);
    assert_eq!(sanitize::link(&format!("https://e.com/{}", "a".repeat(600))), None);
}

#[test]
fn the_payload_has_exactly_the_contract_fields() {
    let a = Alert::new(
        "fleet.capacity-limited",
        "fleet",
        "Cap held below max",
        Severity::Critical,
        State::Open,
    )
    .with_detail("why")
    .with_link("https://github.com/o/r/issues/2");
    let body = payload(&a, "host-1");
    assert_eq!(
        body,
        serde_json::json!({
            "source": "host-1",
            "rule": "fleet.capacity-limited",
            "key": "fleet",
            "title": "Cap held below max",
            "severity": "critical",
            "state": "open",
            "detail": "why",
            "link": "https://github.com/o/r/issues/2",
        })
    );
    let bare = payload(&alert("r", "k", State::Resolved).with_link("ftp://x"), "h");
    assert!(bare.get("detail").is_none() && bare.get("link").is_none());
    assert_eq!(bare["state"], "resolved");
}

// --------------------------------------------------------- config and status

#[test]
fn the_alerts_url_is_the_ingest_endpoint_with_its_path_replaced() {
    assert_eq!(
        alerts_url_from_ingest("https://loom-ui.example.org/ingest").as_deref(),
        Some("https://loom-ui.example.org/api/alerts/external")
    );
    assert_eq!(
        alerts_url_from_ingest("https://h.dev/prefix/ingest/?x=1").as_deref(),
        Some("https://h.dev/prefix/api/alerts/external")
    );
    assert_eq!(
        alerts_url_from_ingest("https://h.dev").as_deref(),
        Some("https://h.dev/api/alerts/external")
    );
    assert_eq!(alerts_url_from_ingest("not a url"), None);
}

#[test]
fn the_key_is_only_ever_sent_to_https_or_loopback() {
    assert!(acceptable_url("https://loom-ui.2amlogic.dev/api/alerts/external").is_ok());
    assert!(acceptable_url("http://127.0.0.1:9/api/alerts/external").is_ok());
    assert!(acceptable_url("http://loom-ui.2amlogic.dev/api/alerts/external").is_err());
    assert!(acceptable_url("https://u:p@loom-ui.2amlogic.dev/x").is_err());
    assert!(acceptable_url("https://dashboard.example.com/api/alerts/external").is_err());
}

#[test]
fn no_ingest_config_means_off() {
    let d = tempfile::tempdir().unwrap();
    assert!(resolve_target(d.path()).is_err());
}

#[test]
fn statuses_split_into_delivered_dropped_and_retry() {
    assert_eq!(classify_status(200), Delivery::Delivered);
    assert!(matches!(classify_status(400), Delivery::Dropped(_)));
    assert!(matches!(classify_status(403), Delivery::Dropped(_)));
    assert!(matches!(classify_status(409), Delivery::Dropped(_)));
    assert!(matches!(classify_status(429), Delivery::Retry(_)));
    assert!(matches!(classify_status(503), Delivery::Retry(_)));
}

#[test]
fn the_target_never_prints_its_key() {
    let t = Target::new("https://h/api/alerts/external".into(), "s3cret".into());
    assert!(!format!("{t:?}").contains("s3cret"));
}

// ------------------------------------------------------------------- outbox

#[test]
fn backoff_doubles_and_is_capped() {
    assert_eq!(backoff(1), Duration::from_secs(5));
    assert_eq!(backoff(2), Duration::from_secs(10));
    assert_eq!(backoff(30), BACKOFF_CAP);
}

#[test]
fn a_retried_head_blocks_everything_behind_it_so_order_holds() {
    let mut o = Outbox::new(8);
    let t0 = Instant::now();
    o.push(alert("r", "k", State::Open), t0);
    o.push(alert("r", "k", State::Resolved), t0);
    let Next::Ready(head) = o.take_ready(t0) else {
        panic!("head due")
    };
    assert_eq!(head.alert.state, State::Open);
    assert!(o.settle(head, &Delivery::Retry("503".into()), t0).is_none());
    // The resolve is NOT sent while the open waits.
    assert!(matches!(o.take_ready(t0), Next::Wait(_)));
    let Next::Ready(again) = o.take_ready(t0 + backoff(1)) else {
        panic!("due after backoff")
    };
    assert_eq!(again.alert.state, State::Open);
    assert_eq!(again.attempts, 1);
}

#[test]
fn delivered_and_dropped_both_leave_the_queue() {
    let mut o = Outbox::new(8);
    let t0 = Instant::now();
    o.push(alert("a", "k", State::Open), t0);
    o.push(alert("b", "k", State::Open), t0);
    let Next::Ready(e) = o.take_ready(t0) else {
        panic!()
    };
    assert!(o.settle(e, &Delivery::Dropped("400".into()), t0).is_none());
    let Next::Ready(e) = o.take_ready(t0) else {
        panic!()
    };
    assert_eq!(e.alert.rule, "b");
    assert!(o.settle(e, &Delivery::Delivered, t0).is_none());
    assert!(matches!(o.take_ready(t0), Next::Empty));
}

#[test]
fn retries_are_bounded() {
    let mut o = Outbox::new(8);
    let mut now = Instant::now();
    o.push(alert("r", "k", State::Open), now);
    for _ in 1..MAX_ATTEMPTS {
        let Next::Ready(e) = o.take_ready(now) else {
            panic!()
        };
        assert!(o.settle(e, &Delivery::Retry("x".into()), now).is_none());
        now += BACKOFF_CAP;
    }
    let Next::Ready(e) = o.take_ready(now) else {
        panic!()
    };
    let gave_up = o.settle(e, &Delivery::Retry("x".into()), now).unwrap();
    assert_eq!(gave_up.attempts, MAX_ATTEMPTS);
    assert!(o.is_empty());
}

#[test]
fn a_full_queue_drops_its_oldest_queued_alert() {
    let mut o = Outbox::new(2);
    let t0 = Instant::now();
    assert!(o.push(alert("a", "k", State::Open), t0).is_none());
    assert!(o.push(alert("b", "k", State::Open), t0).is_none());
    assert_eq!(o.push(alert("c", "k", State::Open), t0).unwrap().rule, "a");
    assert_eq!(o.len(), 2);
}

// -------------------------------------------------------- drain, fake sink

/// A fake loom-ui: records every body and answers from a script (then 200).
#[derive(Clone, Default)]
struct FakeSink {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
    script: Arc<Mutex<VecDeque<Delivery>>>,
}

impl Transport for FakeSink {
    async fn post(&self, body: &serde_json::Value) -> Delivery {
        self.seen.lock().unwrap().push(body.clone());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Delivery::Delivered)
    }
}

async fn wait_for(sink: &FakeSink, n: usize) -> Vec<serde_json::Value> {
    for _ in 0..1000 {
        if sink.seen.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sink.seen.lock().unwrap().clone()
}

#[tokio::test(start_paused = true)]
async fn the_drain_posts_in_order_and_retries_a_5xx_before_moving_on() {
    let sink = FakeSink::default();
    sink.script
        .lock()
        .unwrap()
        .extend([Delivery::Retry("HTTP 503".into())]);
    let shared = Shared::new(16, Some("host-1".into()));
    let task = tokio::spawn(drain(shared.clone(), sink.clone()));
    shared.push(alert("fleet.dispatch-halted", "fleet", State::Open));
    shared.push(alert("fleet.dispatch-halted", "fleet", State::Resolved));
    let seen = wait_for(&sink, 3).await;
    task.abort();
    let states: Vec<_> = seen.iter().map(|b| b["state"].as_str().unwrap()).collect();
    assert_eq!(states, ["open", "open", "resolved"], "retry the open, then resolve");
    assert!(seen.iter().all(|b| b["source"] == "host-1"));
    assert_eq!(shared.queued(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_refused_alert_is_dropped_and_the_next_one_still_goes() {
    let sink = FakeSink::default();
    sink.script
        .lock()
        .unwrap()
        .extend([Delivery::Dropped("HTTP 400".into())]);
    let shared = Shared::new(16, Some("h".into()));
    let task = tokio::spawn(drain(shared.clone(), sink.clone()));
    shared.push(alert("a", "k", State::Open));
    shared.push(alert("b", "k", State::Open));
    let seen = wait_for(&sink, 2).await;
    task.abort();
    let rules: Vec<_> = seen.iter().map(|b| b["rule"].as_str().unwrap()).collect();
    assert_eq!(rules, ["a", "b"]);
}

#[test]
fn post_without_start_is_a_no_op() {
    // The global is never started in the unit-test binary.
    assert!(!is_active());
    post(alert("r", "k", State::Open));
}

// ----------------------------------------------- real HTTP, loopback sink

/// One-request loopback HTTP server answering `status`; returns the raw
/// request text.
fn one_shot_server(status: u16) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/api/alerts/external", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(split) = text.find("\r\n\r\n") {
                let len = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if buf.len() >= split + 4 + len {
                    break;
                }
            }
        }
        let body = r#"{"error":"field: source"}"#;
        let reply = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes());
        String::from_utf8_lossy(&buf).to_string()
    });
    (url, handle)
}

#[test]
fn post_blocking_sends_the_bearer_and_json_body() {
    let (url, server) = one_shot_server(200);
    let target = Target::new(url, "ingest-key-1".into());
    let a = alert("daemon.down", "loom-daemon", State::Open);
    assert_eq!(post_blocking(&target, &a, Duration::from_secs(10)), Delivery::Delivered);
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /api/alerts/external "), "{request}");
    assert!(request.contains("authorization: Bearer ingest-key-1"), "{request}");
    assert!(request.contains(r#""rule":"daemon.down""#), "{request}");
    assert!(request.contains(r#""state":"open""#), "{request}");
}

#[test]
fn post_blocking_reports_a_403_as_dropped_with_loom_uis_reason() {
    let (url, server) = one_shot_server(403);
    let target = Target::new(url, "k".into());
    let got = post_blocking(&target, &alert("r", "k", State::Open), Duration::from_secs(10));
    server.join().unwrap();
    match got {
        Delivery::Dropped(why) => assert!(why.contains("403") && why.contains("field: source")),
        other => panic!("expected Dropped, got {other:?}"),
    }
}

#[test]
fn post_blocking_with_nothing_listening_is_a_retry() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let target = Target::new(format!("http://127.0.0.1:{port}/api/alerts/external"), "k".into());
    let got = post_blocking(&target, &alert("r", "k", State::Open), Duration::from_secs(10));
    assert!(matches!(got, Delivery::Retry(_)), "{got:?}");
}
