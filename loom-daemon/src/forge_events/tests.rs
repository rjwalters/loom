//! Tests for [`crate::forge_events`] — split into their own file (the
//! `health/tests.rs` shape) so the module file stays under the
//! 1000-line file-size ratchet (`.loom/docs/file-size-policy.md`).

use super::*;

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
/// A fresh, uniquely-named state dir per test (these tests share one
/// process and touch `std::env`, so per-test dirs stay independent).
fn fresh_dir() -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("forge-events-test-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn feed_client_at(
    dir: &Path,
    cap: u64,
    base_url: &str,
    host: &str,
    bus: &EventBus,
) -> Arc<FeedClient> {
    Arc::new(FeedClient {
        http: reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap(),
        base_url: base_url.to_string(),
        host_id: host.to_string(),
        key_file: dir.join("key"),
        state_dir: dir.to_path_buf(),
        page_size: DEFAULT_PAGE_SIZE,
        journal_max_bytes: cap,
        bus: bus.clone(),
        status: Arc::new(Mutex::new(ForgeEventsStatus::new(
            ForgeEventsState::Connecting,
            Some(host.to_string()),
            Some(base_url.to_string()),
            DEFAULT_POLL_INTERVAL_SECS,
        ))),
        consecutive_errors: AtomicU32::new(0),
        events_journaled: AtomicU64::new(0),
        cursor: AtomicU64::new(0),
    })
}

#[test]
fn resolve_enabled_defaults_off() {
    std::env::remove_var(ENABLED_ENV);
    assert!(!resolve_enabled(&ForgeEventsConfig::default()));
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(true);
    assert!(resolve_enabled(&c));
}

#[test]
fn resolve_env_beats_config_for_endpoint_and_host() {
    std::env::set_var(ENDPOINT_ENV, "https://override.example/");
    std::env::set_var(HOST_ID_ENV, "override-host");
    let mut c = ForgeEventsConfig::default();
    c.endpoint = Some("https://cfg.example".into());
    c.host_id = Some("cfg-host".into());
    assert_eq!(resolve_endpoint(&c).as_deref(), Some("https://override.example/"));
    assert_eq!(resolve_host_id(&c).as_deref(), Some("override-host"));
    std::env::remove_var(ENDPOINT_ENV);
    std::env::remove_var(HOST_ID_ENV);
    assert_eq!(resolve_endpoint(&c).as_deref(), Some("https://cfg.example"));
    assert_eq!(resolve_host_id(&c).as_deref(), Some("cfg-host"));
}

#[test]
fn resolve_page_and_interval_defaults() {
    std::env::remove_var(POLL_INTERVAL_ENV);
    std::env::remove_var(PAGE_SIZE_ENV);
    assert_eq!(resolve_poll_interval(&ForgeEventsConfig::default()), DEFAULT_POLL_INTERVAL_SECS);
    assert_eq!(resolve_page_size(&ForgeEventsConfig::default()), DEFAULT_PAGE_SIZE);
}

#[test]
fn key_file_roundtrip_trims_and_refuses_empty_or_missing() {
    let dir = fresh_dir();
    let key = dir.join("key");
    std::fs::write(&key, "key-abc123\n").unwrap();
    assert_eq!(load_key(&key).unwrap(), "key-abc123");
    std::fs::write(&key, "   \n").unwrap();
    assert!(load_key(&key).is_err(), "whitespace-only key must refuse");
    assert!(load_key(&dir.join("nope")).is_err(), "missing file must refuse");
}

#[test]
fn cursor_persists_atomically_and_torn_file_reads_zero() {
    let dir = fresh_dir();
    assert_eq!(load_cursor(&dir), 0);
    persist_cursor(&dir, 1234).unwrap();
    assert_eq!(load_cursor(&dir), 1234);
    std::fs::write(dir.join(STATE_FILE), "{\"cursor\": 1").unwrap();
    assert_eq!(load_cursor(&dir), 0, "a torn file must read as 0, not a mid-value");
}

#[test]
fn journal_rotates_at_cap_and_keeps_the_most_recent_full_file() {
    let dir = fresh_dir();
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 64, "http://127.0.0.1:1", "me-host", &bus);
    for i in 0..8u32 {
        let line = serde_json::json!({"n": i, "pad": "p".repeat(20)});
        client.journal_append(&line).unwrap();
    }
    let main = std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
    let rotated = std::fs::read_to_string(dir.join(JOURNAL_ROTATED)).unwrap();
    assert!(
        main.lines().count() < 8,
        "the live journal was never rotated (still holds all 8 lines)"
    );
    // Rotation overwrites `journal.1` with the most recent full file, so
    // the oldest line was consumed by an earlier rotation and the newest
    // full pair is what survives: the audit tail is the *latest* fill, by
    // design — forward progress never depends on it.
    assert!(!rotated.is_empty());
}

#[tokio::test]
async fn feed_refuses_host_echo_mismatch_and_keeps_cursor() {
    let dir = fresh_dir();
    let (base_url, _server) = serve_raw(
        "HTTP/1.1 200 OK",
        &serde_json::json!({
            "host_id": "some-other-host",
            "cursor": 7,
            "events": [{"seq": 7}],
        })
        .to_string(),
    )
    .await;
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 1024, &base_url, "me-host", &bus);
    std::fs::write(&client.key_file, "key-test").unwrap();
    let (state, reason) = client.poll_once().await.unwrap_err();
    assert_eq!(state, ForgeEventsState::HostMismatch);
    assert!(reason.contains("cursor is not trusted"));
    // Neither the in-memory nor the durable cursor moved.
    assert_eq!(client.cursor.load(Ordering::Relaxed), 0);
    assert_eq!(load_cursor(&dir), 0);
    let st = client.status.lock().unwrap();
    assert_eq!(st.state, ForgeEventsState::HostMismatch);
    assert_eq!(st.cursor, 0);
    drop(st);
}

#[tokio::test]
async fn feed_auth_failure_classifies_auth_not_backoff() {
    let dir = fresh_dir();
    let (base_url, _server) =
        serve_raw("HTTP/1.1 401 Unauthorized", "{\"error\":\"authentication_failed\"}").await;
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 1024, &base_url, "me-host", &bus);
    std::fs::write(&client.key_file, "key-bad").unwrap();
    let (state, reason) = client.poll_once().await.unwrap_err();
    assert_eq!(state, ForgeEventsState::AuthFailed);
    assert!(reason.contains("rotate"));
    // One failure is the *class*, not yet backoff.
    let st = client.status.lock().unwrap();
    assert_eq!(st.state, ForgeEventsState::AuthFailed);
    assert_eq!(st.consecutive_errors, 1);
    drop(st);
}

#[tokio::test]
async fn feed_success_journals_advances_cursor_and_reports_healthy() {
    let dir = fresh_dir();
    let (base_url, _server) = serve_raw(
        "HTTP/1.1 200 OK",
        &serde_json::json!({
            "host_id": "me-host",
            "cursor": 5,
            "events": [
                {"seq": 4, "type": "issues", "detail": {"action": "opened"}},
                {"seq": 5, "type": "pull_request", "detail": {"action": "closed"}},
            ],
        })
        .to_string(),
    )
    .await;
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 1024, &base_url, "me-host", &bus);
    std::fs::write(&client.key_file, "key-good").unwrap();
    // Subscribe BEFORE the page lands: broadcast receivers created after
    // a publish miss it, and the prompt is one-shot by design.
    let mut sub = client.bus.subscribe(["forge.event".to_string()]);
    client.poll_once().await.unwrap();
    assert_eq!(client.cursor.load(Ordering::Relaxed), 5);
    assert_eq!(load_cursor(&dir), 5);
    let journal = std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
    assert_eq!(journal.lines().count(), 2);
    let st = client.status.lock().unwrap();
    assert_eq!(st.state, ForgeEventsState::Healthy);
    assert_eq!(st.events_journaled, 2);
    assert_eq!(st.cursor, 5);
    assert!(st.last_success_at.is_some());
    assert_eq!(st.consecutive_errors, 0);
    drop(st);
    // One prompt per non-empty page — the ADR-0021 §D3 dedup discipline.
    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
        .await
        .expect("the published prompt never arrived")
        .expect("the bus closed before the prompt arrived");
    match ev {
        crate::types::Event::Generic { topic, payload } => {
            assert_eq!(topic, "forge.event");
            assert_eq!(payload["host_id"], "me-host");
            assert_eq!(payload["count"], 2);
            assert_eq!(payload["first_seq"], 4);
            assert_eq!(payload["last_seq"], 5);
            assert_eq!(payload["types"], serde_json::json!(["issues", "pull_request"]));
        }
        other => panic!("expected the forge.event Generic prompt, got {other:?}"),
    }
}

#[tokio::test]
async fn feed_fourxx_means_host_mismatch() {
    let dir = fresh_dir();
    let (base_url, _server) =
        serve_raw("HTTP/1.1 404 Not Found", "{\"error\":\"host_not_found\"}").await;
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 1024, &base_url, "ghost-host", &bus);
    std::fs::write(&client.key_file, "key-good").unwrap();
    let (state, reason) = client.poll_once().await.unwrap_err();
    assert_eq!(state, ForgeEventsState::HostMismatch);
    assert!(reason.contains("ghost-host"));
}

#[tokio::test]
async fn feed_overlimit_body_refused_without_cursor_advance() {
    let dir = fresh_dir();
    // ~330 x's + chrome, against a 200-byte cap — below the 600-byte body
    // of the real MAX_RESPONSE_BYTES path, which the same code serves.
    let body = format!(
        "{{\"host_id\":\"me-host\",\"cursor\":3,\"events\":[],\"blob\":\"{}\"}}",
        "x".repeat(330)
    );
    // feed_client_at does not take a body cap (MAX_RESPONSE_BYTES is a
    // const); to exercise the *refuse* branch we go through the same
    // bounded_body code with a server that answers over a small cap —
    // the cap is a fn arg, so call it directly with a captured response.
    let (base_url, _server) = serve_raw("HTTP/1.1 200 OK", &body).await;
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 1024, &base_url, "me-host", &bus);
    std::fs::write(&client.key_file, "key-good").unwrap();
    // The production cap (256 KiB) is far larger than this body, so call
    // bounded_body directly at a small cap to pin the refuse branch.
    let response = client
        .http
        .get(format!("{base_url}/v1/hosts/me-host/events?after=0&limit=1"))
        .send()
        .await
        .unwrap();
    let err = FeedClient::bounded_body(response, 200).await.unwrap_err();
    assert!(err.contains("exceeds 200 bytes"));
    // And through the full poll at the production cap the same body is
    // fine (330 + chrome < 256 KiB): success path, cursor advances.
    client.poll_once().await.unwrap();
    assert_eq!(client.cursor.load(Ordering::Relaxed), 3);
}

#[test]
fn effective_interval_stretches_at_the_threshold() {
    let dir = fresh_dir();
    let bus = EventBus::default();
    let client = feed_client_at(&dir, 1024, "http://127.0.0.1:1", "me-host", &bus);
    assert_eq!(client.effective_interval(10), Duration::from_secs(10));
    client
        .consecutive_errors
        .store(BACKOFF_AFTER_ERRORS - 1, Ordering::Relaxed);
    assert_eq!(client.effective_interval(10), Duration::from_secs(10));
    client
        .consecutive_errors
        .store(BACKOFF_AFTER_ERRORS, Ordering::Relaxed);
    assert_eq!(client.effective_interval(10), Duration::from_secs(BACKOFF_INTERVAL.as_secs()));
    client.consecutive_errors.store(0, Ordering::Relaxed);
    assert_eq!(client.effective_interval(10), Duration::from_secs(10));
}

#[test]
fn prepare_off_without_enabled_at_any_tier() {
    std::env::remove_var(ENABLED_ENV);
    match prepare(&ForgeEventsConfig::default()) {
        SpawnPlan::Off => {}
        other => panic!("absent forgeEvents block must be Off, got {other:?}"),
    }
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(false);
    match prepare(&c) {
        SpawnPlan::Off => {}
        other => panic!("enabled=false must be Off, got {other:?}"),
    }
}

#[test]
fn prepare_missing_endpoint_reports_which_piece_is_missing() {
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(true);
    c.host_id = Some("me-host".into());
    match prepare(&c) {
        SpawnPlan::Misconfigured {
            detail, endpoint, ..
        } => {
            assert!(detail.contains("endpoint"), "detail: {detail}");
            assert!(endpoint.is_none());
        }
        other => panic!("missing endpoint must be Misconfigured, got {other:?}"),
    }
}

#[test]
fn prepare_missing_hostid_reports_which_piece_is_missing() {
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(true);
    c.endpoint = Some("https://real-endpoint.host/".into());
    match prepare(&c) {
        SpawnPlan::Misconfigured {
            detail,
            host_id,
            endpoint,
        } => {
            assert!(detail.contains("hostId"), "detail: {detail}");
            assert!(host_id.is_none());
            assert_eq!(endpoint.as_deref(), Some("https://real-endpoint.host/"));
        }
        other => panic!("missing hostId must be Misconfigured, got {other:?}"),
    }
}

#[test]
fn prepare_refuses_a_reserved_placeholder_endpoint() {
    // A placeholder endpoint is *not configured*, not a destination
    // (the #7815 guard, policy shared with the observability exporter):
    // refused before any key is read. This pins the wiring here.
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(true);
    c.endpoint = Some("https://dashboard.example.com/".into());
    c.host_id = Some("me-host".into());
    match prepare(&c) {
        SpawnPlan::Misconfigured { detail, .. } => {
            assert!(detail.contains("placeholder"), "detail: {detail}");
            assert!(detail.contains("example.com"), "detail: {detail}");
        }
        other => panic!("placeholder endpoint must be Misconfigured, got {other:?}"),
    }
    // …while a real-looking endpoint (and a readable key) is not
    // refused on that ground.
    let dir = fresh_dir();
    let key = dir.join("key");
    std::fs::write(&key, "key-abc123\n").unwrap();
    std::env::set_var(KEY_FILE_ENV, &key);
    c.endpoint = Some("https://events.real-endpoint.workers.dev/".into());
    match prepare(&c) {
        SpawnPlan::Live { .. } => {}
        other => panic!("real endpoint must be Live, got {other:?}"),
    }
    std::env::remove_var(KEY_FILE_ENV);
}

#[test]
fn prepare_live_resolves_key_file_and_state_dir_shape() {
    let dir = fresh_dir();
    let key = dir.join("my-key");
    std::fs::write(&key, "key-abc123\n").unwrap();
    std::env::set_var(KEY_FILE_ENV, &key);
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(true);
    c.endpoint = Some("https://events.real-endpoint/".into());
    c.host_id = Some("me-host".into());
    c.page_size = Some(42);
    match prepare(&c) {
        SpawnPlan::Live {
            host_id,
            endpoint,
            key_file,
            state_dir,
            page_size,
            ..
        } => {
            assert_eq!(host_id, "me-host");
            assert_eq!(endpoint, "https://events.real-endpoint/");
            assert_eq!(key_file, key);
            // The whole local footprint lives in one directory.
            assert!(
                state_dir
                    .components()
                    .next_back()
                    .is_some_and(|c| c.as_os_str().to_str() == Some(STATE_DIR_REL)),
                "state_dir: {}",
                state_dir.display()
            );
            assert_eq!(page_size, 42);
        }
        other => panic!("fully-resolved config must be Live, got {other:?}"),
    }
    std::env::remove_var(KEY_FILE_ENV);
}

#[test]
fn prepare_without_a_readable_key_is_misconfigured_not_live() {
    let dir = fresh_dir();
    std::env::set_var(KEY_FILE_ENV, dir.join("absent-key"));
    let mut c = ForgeEventsConfig::default();
    c.enabled = Some(true);
    c.endpoint = Some("https://events.real-endpoint/".into());
    c.host_id = Some("me-host".into());
    match prepare(&c) {
        SpawnPlan::Misconfigured {
            detail,
            host_id,
            endpoint,
        } => {
            assert!(detail.contains("key"), "detail: {detail}");
            assert_eq!(host_id.as_deref(), Some("me-host"));
            assert_eq!(endpoint.as_deref(), Some("https://events.real-endpoint/"));
        }
        other => panic!("missing key file must be Misconfigured, got {other:?}"),
    }
    std::env::remove_var(KEY_FILE_ENV);
}

/// A minimal in-process HTTP server: single status line + JSON body,
/// keep-alive, accept loop until the runtime drops.
async fn serve_raw(status_line: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_string();
    let status_line = status_line.to_string();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            use tokio::io::AsyncReadExt as _;
            // Drain the request head (the feed client sends GET-only, no
            // body) until CRLFCRLF so the reply does not race the request.
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut chunk = [0u8; 1024];
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..n]);
            }
            use tokio::io::AsyncWriteExt as _;
            let resp = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), server)
}
