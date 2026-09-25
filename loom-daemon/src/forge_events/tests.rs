//! Tests for the forge event-feed consumer (#8765).
//!
//! Split from `forge_events.rs` per the `health/tests.rs` precedent so the
//! file-size ratchet measures production code on its own terms.
//!
//! The feed is exercised against a real loopback HTTP/1.1 server rather than
//! a mocked `reqwest::Client`: the behaviours under test — the 256 KiB read
//! cap, the status-code classification, redirects being refused, the Bearer
//! header — are properties of the *transport*, and a mock would assert the
//! test's model of reqwest rather than reqwest itself.

use super::*;

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};

use serial_test::serial;

// ============================================================================
// Fixtures
// ============================================================================

/// A canned HTTP response.
#[derive(Clone)]
struct Canned {
    status: u16,
    body: String,
}

impl Canned {
    fn ok(body: &str) -> Self {
        Canned {
            status: 200,
            body: body.to_string(),
        }
    }
    fn code(status: u16) -> Self {
        Canned {
            status,
            body: "{}".to_string(),
        }
    }
}

/// One observed request: its request line and its `Authorization` header.
type SeenRequest = (String, Option<String>);

/// A loopback feed server. Answers `responses` in order (the last repeats),
/// and records every request line + `Authorization` header it saw so a test
/// can assert on what the client actually sent.
struct TestFeed {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    served: Arc<AtomicUsize>,
}

impl TestFeed {
    fn start(responses: Vec<Canned>) -> Self {
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        assert!(addr.ip().is_loopback());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));
        let seen_task = seen.clone();
        let served_task = served.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let request = read_request_head(&mut stream);
                let index = served_task.fetch_add(1, Ordering::SeqCst);
                seen_task
                    .lock()
                    .expect("seen mutex")
                    .push(parse_request(&request));
                let canned = responses
                    .get(index)
                    .or_else(|| responses.last())
                    .cloned()
                    .unwrap_or_else(|| Canned::code(500));
                let reason = if canned.status == 200 { "OK" } else { "ERR" };
                let head = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    canned.status,
                    canned.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(canned.body.as_bytes());
                let _ = stream.flush();
            }
        });
        TestFeed { addr, seen, served }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen mutex").clone()
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

fn read_request_head(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 8192 {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn parse_request(head: &str) -> SeenRequest {
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let auth = lines
        .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        .map(|line| {
            line[line.find(':').map_or(0, |i| i + 1)..]
                .trim()
                .to_string()
        });
    (request_line, auth)
}

/// A provisioned feed rooted in a temp directory, with a key file written.
struct Fixture {
    dir: tempfile::TempDir,
    feed: ResolvedFeed,
}

impl Fixture {
    fn new(endpoint: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let key_file = dir.path().join("key");
        std::fs::write(&key_file, "super-secret-key\n").expect("write key");
        let feed = ResolvedFeed {
            endpoint: endpoint.to_string(),
            host_id: "mac-studio".to_string(),
            key_file,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            page_size: 100,
            paths: FeedPaths::in_dir(dir.path().to_path_buf()),
        };
        Fixture { dir, feed }
    }

    fn client(&self, bus: &EventBus) -> (FeedClient, Arc<FeedStatus>) {
        self.client_with(bus, self.feed.clone())
    }

    fn client_with(&self, bus: &EventBus, feed: ResolvedFeed) -> (FeedClient, Arc<FeedStatus>) {
        let status = Arc::new(FeedStatus::started(&feed, 0));
        let client = FeedClient::new(feed, bus.clone(), status.clone()).expect("construct client");
        (client, status)
    }
}

fn page_json(host: &str, cursor: u64, events: &str) -> String {
    format!(
        r#"{{"host_id":"{host}","cursor":{cursor},"clamped":false,"events":[{events}],"has_more":false}}"#
    )
}

const TWO_EVENTS: &str = r#"{"seq":7,"type":"pull_request","repo":"rjwalters/loom"},
                            {"seq":8,"type":"issues","repo":"rjwalters/loom"}"#;

/// Clear every `LOOM_FORGE_EVENTS_*` override. Resolution tests are
/// `#[serial]` because this crate links all tests into one binary.
fn clear_env() {
    for key in [
        ENABLED_ENV,
        ENDPOINT_ENV,
        HOST_ID_ENV,
        EVENT_KEY_FILE_ENV,
        POLL_INTERVAL_SECS_ENV,
        PAGE_SIZE_ENV,
    ] {
        std::env::remove_var(key);
    }
}

fn enabled_config() -> ForgeEventsConfig {
    ForgeEventsConfig {
        enabled: Some(true),
        endpoint: Some("https://events.internal".to_string()),
        host_id: Some("mac-studio".to_string()),
        ..ForgeEventsConfig::default()
    }
}

// ============================================================================
// Resolve-tier precedence (env > config > default)
// ============================================================================

#[test]
#[serial]
fn enabled_resolves_env_over_config_over_default() {
    clear_env();
    assert!(!resolve_enabled(&ForgeEventsConfig::default()));
    let config = ForgeEventsConfig {
        enabled: Some(true),
        ..ForgeEventsConfig::default()
    };
    assert!(resolve_enabled(&config));
    std::env::set_var(ENABLED_ENV, "false");
    assert!(!resolve_enabled(&config), "env must beat config");
    std::env::set_var(ENABLED_ENV, "1");
    assert!(resolve_enabled(&ForgeEventsConfig::default()));
    clear_env();
}

#[test]
#[serial]
fn endpoint_and_host_id_resolve_env_over_config_with_no_default() {
    clear_env();
    let config = enabled_config();
    assert_eq!(resolve_endpoint(&config).as_deref(), Some("https://events.internal"));
    assert_eq!(resolve_host_id(&config).as_deref(), Some("mac-studio"));
    std::env::set_var(ENDPOINT_ENV, "https://feed.internal-host.net");
    std::env::set_var(HOST_ID_ENV, "linux-box");
    assert_eq!(resolve_endpoint(&config).as_deref(), Some("https://feed.internal-host.net"));
    assert_eq!(resolve_host_id(&config).as_deref(), Some("linux-box"));
    clear_env();
    // No default tier for either: unset means "not provisioned", which
    // `prepare` reports by name rather than guessing a value.
    assert_eq!(resolve_endpoint(&ForgeEventsConfig::default()), None);
    assert_eq!(resolve_host_id(&ForgeEventsConfig::default()), None);
}

#[test]
#[serial]
fn cadence_and_page_size_resolve_env_over_config_over_default() {
    clear_env();
    let default = ForgeEventsConfig::default();
    assert_eq!(resolve_poll_interval_secs(&default), DEFAULT_POLL_INTERVAL_SECS);
    assert_eq!(resolve_page_size(&default), DEFAULT_PAGE_SIZE);
    let config = ForgeEventsConfig {
        poll_interval_secs: Some(45),
        page_size: Some(25),
        ..ForgeEventsConfig::default()
    };
    assert_eq!(resolve_poll_interval_secs(&config), 45);
    assert_eq!(resolve_page_size(&config), 25);
    std::env::set_var(POLL_INTERVAL_SECS_ENV, "90");
    std::env::set_var(PAGE_SIZE_ENV, "7");
    assert_eq!(resolve_poll_interval_secs(&config), 90);
    assert_eq!(resolve_page_size(&config), 7);
    // A zero/garbage override falls through rather than being fatal —
    // malformed input never blocks daemon startup anywhere else in this tree.
    std::env::set_var(POLL_INTERVAL_SECS_ENV, "0");
    std::env::set_var(PAGE_SIZE_ENV, "not-a-number");
    assert_eq!(resolve_poll_interval_secs(&config), 45);
    assert_eq!(resolve_page_size(&config), 25);
    clear_env();
}

#[test]
#[serial]
fn key_file_resolves_env_over_config_over_host_relative_default() {
    clear_env();
    let config = ForgeEventsConfig {
        event_key_file: Some("/etc/loom/forge.key".to_string()),
        ..ForgeEventsConfig::default()
    };
    assert_eq!(resolve_event_key_file(&config).as_deref(), Some("/etc/loom/forge.key"));
    std::env::set_var(EVENT_KEY_FILE_ENV, "/run/secrets/forge.key");
    assert_eq!(resolve_event_key_file(&config).as_deref(), Some("/run/secrets/forge.key"));
    clear_env();
    // The default tier is host-relative by construction (#5336): a path
    // copied from another host's $HOME into shared config is unreadable here.
    let home = Path::new("/home/someone");
    assert_eq!(key_file_in(&state_dir_under(home)), "/home/someone/.loom/forge-events/key");
}

#[test]
#[serial]
fn read_config_maps_every_camel_case_key() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".loom")).expect("mkdir");
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"forgeEvents":{"enabled":true,"endpoint":"https://feed.internal",
            "hostId":"mac-studio","eventKeyFile":"/etc/loom/k","pollIntervalSecs":30,
            "pageSize":250}}"#,
    )
    .expect("write config");
    let config = read_config(dir.path());
    assert_eq!(config.enabled, Some(true));
    assert_eq!(config.endpoint.as_deref(), Some("https://feed.internal"));
    assert_eq!(config.host_id.as_deref(), Some("mac-studio"));
    assert_eq!(config.event_key_file.as_deref(), Some("/etc/loom/k"));
    assert_eq!(config.poll_interval_secs, Some(30));
    assert_eq!(config.page_size, Some(250));
}

#[test]
#[serial]
fn a_repo_with_no_block_reads_as_all_none_and_stays_off() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".loom")).expect("mkdir");
    std::fs::write(dir.path().join(".loom/config.json"), r#"{"terminals":[]}"#)
        .expect("write config");
    assert_eq!(read_config(dir.path()), ForgeEventsConfig::default());
    assert!(!resolve_enabled(&read_config(dir.path())));
}

// ============================================================================
// Spawn decision (`prepare`)
// ============================================================================

#[test]
#[serial]
fn off_is_disabled_without_reading_anything_else() {
    clear_env();
    // Deliberately absurd config: nothing below `enabled` may be consulted,
    // so a disabled daemon can never report itself misconfigured.
    let config = ForgeEventsConfig {
        enabled: Some(false),
        endpoint: Some("https://dashboard.example.com".to_string()),
        host_id: Some("../../etc".to_string()),
        ..ForgeEventsConfig::default()
    };
    assert!(matches!(prepare(&config, None), Prepared::Disabled));
}

#[test]
#[serial]
fn missing_endpoint_is_misconfigured_and_names_the_key() {
    clear_env();
    let config = ForgeEventsConfig {
        enabled: Some(true),
        host_id: Some("mac-studio".to_string()),
        ..ForgeEventsConfig::default()
    };
    let Prepared::Misconfigured { endpoint, detail } = prepare(&config, None) else {
        panic!("expected misconfigured");
    };
    assert_eq!(endpoint, None);
    assert!(detail.contains("forgeEvents.endpoint"), "{detail}");
}

#[test]
#[serial]
fn missing_host_id_is_misconfigured_and_keeps_the_resolved_endpoint() {
    clear_env();
    let config = ForgeEventsConfig {
        enabled: Some(true),
        endpoint: Some("https://events.internal".to_string()),
        ..ForgeEventsConfig::default()
    };
    let Prepared::Misconfigured { endpoint, detail } = prepare(&config, None) else {
        panic!("expected misconfigured");
    };
    assert_eq!(endpoint.as_deref(), Some("https://events.internal"));
    assert!(detail.contains("forgeEvents.hostId"), "{detail}");
}

#[test]
#[serial]
fn a_host_id_that_is_not_url_safe_is_refused_rather_than_encoded() {
    clear_env();
    for bad in ["../../etc", "has space", "a/b"] {
        let config = ForgeEventsConfig {
            host_id: Some(bad.to_string()),
            ..enabled_config()
        };
        let Prepared::Misconfigured { detail, .. } = prepare(&config, None) else {
            panic!("expected misconfigured for {bad:?}");
        };
        assert!(detail.contains("URL-safe"), "{bad:?}: {detail}");
    }
}

#[test]
#[serial]
fn an_endpoint_carrying_credentials_or_a_query_is_refused() {
    clear_env();
    for bad in [
        "https://user:secret@events.internal",
        "https://events.internal?key=secret",
        "ftp://events.internal",
        "not a url",
    ] {
        let config = ForgeEventsConfig {
            endpoint: Some(bad.to_string()),
            ..enabled_config()
        };
        let Prepared::Misconfigured { detail, .. } = prepare(&config, None) else {
            panic!("expected misconfigured for {bad:?}");
        };
        assert!(detail.contains("Authorization header"), "{bad:?}: {detail}");
    }
}

#[test]
#[serial]
fn a_reserved_placeholder_endpoint_is_refused_before_the_key_is_read() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    // A perfectly readable key sits right there: the point is that the
    // placeholder guard fires first, so the key is never opened, let alone
    // sent to a documentation domain (#7815).
    std::fs::write(dir.path().join("key"), "real-key").expect("write key");
    let config = ForgeEventsConfig {
        endpoint: Some("https://feed.example.com/v1".to_string()),
        ..enabled_config()
    };
    let Prepared::Misconfigured { detail, .. } = prepare(&config, Some(dir.path().to_path_buf()))
    else {
        panic!("expected misconfigured");
    };
    assert!(detail.contains("reserved placeholder"), "{detail}");
    assert!(detail.contains("example.com"), "{detail}");
    assert!(detail.contains("never sent there"), "{detail}");
}

#[test]
#[serial]
fn an_unreadable_or_empty_key_file_is_misconfigured_by_path_only() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    // Missing entirely.
    let Prepared::Misconfigured { detail, .. } =
        prepare(&enabled_config(), Some(dir.path().to_path_buf()))
    else {
        panic!("expected misconfigured");
    };
    assert!(detail.contains("could not read event key file"), "{detail}");
    assert!(detail.contains("key"), "{detail}");
    // Present but whitespace-only.
    std::fs::write(dir.path().join("key"), "   \n").expect("write key");
    let Prepared::Misconfigured { detail, .. } =
        prepare(&enabled_config(), Some(dir.path().to_path_buf()))
    else {
        panic!("expected misconfigured");
    };
    assert!(detail.contains("empty after trimming"), "{detail}");
}

#[test]
#[serial]
fn a_fully_provisioned_config_resolves_live_and_touches_no_disk() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("key"), "the-key\n").expect("write key");
    let config = ForgeEventsConfig {
        poll_interval_secs: Some(15),
        page_size: Some(64),
        ..enabled_config()
    };
    let Prepared::Ready(feed) = prepare(&config, Some(dir.path().to_path_buf())) else {
        panic!("expected ready");
    };
    assert_eq!(feed.endpoint, "https://events.internal");
    assert_eq!(feed.host_id, "mac-studio");
    assert_eq!(feed.poll_interval_secs, 15);
    assert_eq!(feed.page_size, 64);
    assert_eq!(feed.paths.journal, dir.path().join("journal.jsonl"));
    assert_eq!(feed.paths.state, dir.path().join("state.json"));
    assert!(!feed.paths.journal.exists());
    assert!(!feed.paths.state.exists());
}

// ============================================================================
// Durable cursor
// ============================================================================

#[test]
fn cursor_persist_is_atomic_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state.json");
    persist_cursor(&state, "mac-studio", 512).expect("persist");
    assert_eq!(read_cursor(&state, "mac-studio"), 512);
    persist_cursor(&state, "mac-studio", 900).expect("persist again");
    assert_eq!(read_cursor(&state, "mac-studio"), 900);
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
}

#[test]
fn a_torn_absent_or_foreign_cursor_file_reads_as_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state.json");
    // Absent.
    assert_eq!(read_cursor(&state, "mac-studio"), 0);
    // Torn: a half-written object, which is what a non-atomic writer leaves
    // behind after a crash.
    std::fs::write(&state, r#"{"host_id":"mac-studio","cur"#).expect("write torn");
    assert_eq!(read_cursor(&state, "mac-studio"), 0);
    // Empty.
    std::fs::write(&state, "").expect("write empty");
    assert_eq!(read_cursor(&state, "mac-studio"), 0);
    // Another host's cursor is never adopted (invariant 3, applied to disk).
    persist_cursor(&state, "linux-box", 77).expect("persist");
    assert_eq!(read_cursor(&state, "linux-box"), 77);
    assert_eq!(read_cursor(&state, "mac-studio"), 0);
}

// ============================================================================
// Journal
// ============================================================================

#[test]
fn journal_appends_one_line_per_page_and_creates_its_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = FeedPaths::in_dir(dir.path().join("nested"));
    append_journal(&paths, r#"{"a":1}"#).expect("append");
    append_journal(&paths, r#"{"a":2}"#).expect("append");
    let body = std::fs::read_to_string(&paths.journal).expect("read journal");
    assert_eq!(body.lines().count(), 2);
    assert!(body.starts_with(r#"{"a":1}"#));
}

#[test]
fn journal_rotation_keeps_the_most_recent_full_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = FeedPaths::in_dir(dir.path().to_path_buf());
    let line = "x".repeat(64 * 1024);
    let mut n = 0;
    loop {
        let candidate = format!("{n}-{line}");
        let current = std::fs::metadata(&paths.journal)
            .map(|m| m.len())
            .unwrap_or(0);
        if current + candidate.len() as u64 + 1 > JOURNAL_MAX_BYTES {
            break;
        }
        append_journal(&paths, &candidate).expect("append");
        n += 1;
    }
    let before = std::fs::metadata(&paths.journal).expect("meta").len();
    assert!(before > JOURNAL_MAX_BYTES / 2, "fixture did not fill up");
    assert!(!paths.journal_prev.exists(), "rotated too early");

    append_journal(&paths, &format!("{n}-{line}")).expect("append past cap");
    // The full file is retained as journal.1 — the newest history is never
    // the thing discarded — and the live journal restarts with the new line.
    assert!(paths.journal_prev.exists(), "no rotation happened");
    assert_eq!(std::fs::metadata(&paths.journal_prev).expect("meta").len(), before);
    let live = std::fs::read_to_string(&paths.journal).expect("read journal");
    assert_eq!(live.lines().count(), 1);
    assert!(live.starts_with(&format!("{n}-")));
}

// ============================================================================
// Bus payload shape
// ============================================================================

#[test]
fn the_bus_payload_is_routing_hints_only() {
    let events = vec![
        serde_json::json!({"seq": 7, "type": "pull_request", "repo": "rjwalters/loom",
                           "title": "a private title", "number": 4321}),
        serde_json::json!({"seq": 9, "type": "issues", "repo": "rjwalters/loom"}),
        serde_json::json!({"seq": 11, "type": "pull_request", "repo": "other/repo"}),
    ];
    let payload = page_payload("mac-studio", &events);
    assert_eq!(payload["source"], PAYLOAD_SOURCE);
    assert_eq!(payload["source"], "forge-event-feed");
    assert_eq!(payload["host_id"], "mac-studio");
    assert_eq!(payload["count"], 3);
    assert_eq!(payload["first_seq"], 7);
    assert_eq!(payload["last_seq"], 11);
    // Sorted + deduped type names, nothing else.
    assert_eq!(payload["types"], serde_json::json!(["issues", "pull_request"]));
    let keys: Vec<&String> = payload.as_object().expect("object").keys().collect();
    assert_eq!(
        keys,
        vec![
            "source",
            "host_id",
            "count",
            "first_seq",
            "last_seq",
            "types"
        ]
    );
    // The events themselves are NEVER copied onto the bus: a subscriber that
    // wants forge state has to go ask the forge (ADR-0014 invariant 1).
    let rendered = payload.to_string();
    assert!(!rendered.contains("a private title"), "{rendered}");
    assert!(!rendered.contains("4321"), "{rendered}");
    assert!(!rendered.contains("rjwalters/loom"), "{rendered}");
}

// ============================================================================
// poll_once — one poll's whole effect
// ============================================================================

#[tokio::test]
async fn a_successful_page_journals_persists_and_publishes() {
    let feed = TestFeed::start(vec![Canned::ok(&page_json("mac-studio", 8, TWO_EVENTS))]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let mut subscription = bus.subscribe([BUS_TOPIC]);
    let (mut client, status) = fixture.client(&bus);

    let outcome = client.poll_once().await;
    assert_eq!(
        outcome,
        PollOutcome::Page {
            count: 2,
            cursor: 8
        }
    );

    // Journal: one line per page, carrying the page verbatim.
    let journal = std::fs::read_to_string(&fixture.feed.paths.journal).expect("journal");
    assert_eq!(journal.lines().count(), 1);
    let record: serde_json::Value = serde_json::from_str(journal.trim()).expect("journal json");
    assert_eq!(record["host_id"], "mac-studio");
    assert_eq!(record["after"], 0);
    assert_eq!(record["cursor"], 8);
    assert_eq!(record["count"], 2);
    assert_eq!(record["events"].as_array().expect("events").len(), 2);

    // Cursor: durable, and durable before the prompt went out.
    assert_eq!(read_cursor(&fixture.feed.paths.state, "mac-studio"), 8);
    assert_eq!(client.cursor(), 8);

    // Bus: exactly one Generic on the documented topic, summary shape only.
    let event = subscription.recv().await.expect("bus event");
    let Event::Generic { topic, payload } = event else {
        panic!("expected a Generic event");
    };
    assert_eq!(topic, BUS_TOPIC);
    assert_eq!(topic, "forge.event");
    assert_eq!(payload["count"], 2);
    assert_eq!(payload["first_seq"], 7);
    assert_eq!(payload["last_seq"], 8);
    assert_eq!(payload["types"], serde_json::json!(["issues", "pull_request"]));

    let snapshot = status.snapshot();
    assert_eq!(snapshot.state, ForgeEventsState::Healthy);
    assert_eq!(snapshot.cursor, 8);
    assert_eq!(snapshot.pages_observed, 1);
    assert_eq!(snapshot.events_observed, 2);
    assert!(snapshot.last_page_at.is_some());
    assert_eq!(snapshot.last_error, None);
}

#[tokio::test]
async fn the_request_carries_the_key_as_a_bearer_header_and_the_cursor_as_after() {
    let feed = TestFeed::start(vec![
        Canned::ok(&page_json("mac-studio", 8, TWO_EVENTS)),
        Canned::ok(&page_json("mac-studio", 8, "")),
    ]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, _status) = fixture.client(&bus);

    client.poll_once().await;
    // The second poll must resume from the cursor the first one persisted.
    client.poll_once().await;

    let requests = feed.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .0
            .contains("/v1/hosts/mac-studio/events?after=0&limit=100"),
        "{:?}",
        requests[0].0
    );
    assert!(requests[1].0.contains("after=8"), "{:?}", requests[1].0);
    assert_eq!(requests[0].1.as_deref(), Some("Bearer super-secret-key"));
}

#[tokio::test]
async fn the_key_is_re_read_every_poll_so_a_rotation_is_a_file_swap() {
    let feed = TestFeed::start(vec![Canned::ok(&page_json("mac-studio", 1, ""))]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, _status) = fixture.client(&bus);

    client.poll_once().await;
    std::fs::write(&fixture.feed.key_file, "rotated-key\n").expect("rotate key");
    client.poll_once().await;

    let requests = feed.requests();
    assert_eq!(requests[0].1.as_deref(), Some("Bearer super-secret-key"));
    assert_eq!(
        requests[1].1.as_deref(),
        Some("Bearer rotated-key"),
        "a rotated key must be picked up without a daemon restart"
    );
}

#[tokio::test]
async fn an_empty_page_is_a_successful_poll() {
    let feed = TestFeed::start(vec![Canned::ok(&page_json("mac-studio", 0, ""))]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let mut subscription = bus.subscribe([BUS_TOPIC]);
    let (mut client, status) = fixture.client(&bus);

    assert_eq!(client.poll_once().await, PollOutcome::Empty { cursor: 0 });
    assert_eq!(status.snapshot().state, ForgeEventsState::Healthy);
    assert_eq!(status.snapshot().pages_observed, 0);
    // Nothing journaled and nothing published: a quiet feed is not an event.
    assert!(!fixture.feed.paths.journal.exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), subscription.recv())
            .await
            .is_err(),
        "an empty page must not publish a prompt"
    );
}

#[tokio::test]
async fn an_empty_page_with_a_forward_cursor_still_persists_it() {
    // The feed clamped us past expired events: that range is gone, and
    // re-requesting it forever would be the only alternative.
    let feed = TestFeed::start(vec![Canned::ok(
        r#"{"host_id":"mac-studio","cursor":500,"clamped":true,"events":[],"has_more":false}"#,
    )]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, _status) = fixture.client(&bus);

    assert_eq!(client.poll_once().await, PollOutcome::Empty { cursor: 500 });
    assert_eq!(read_cursor(&fixture.feed.paths.state, "mac-studio"), 500);
}

#[tokio::test]
async fn a_401_or_403_is_auth_failed_and_never_names_the_key() {
    for code in [401u16, 403] {
        let feed = TestFeed::start(vec![Canned::code(code)]);
        let fixture = Fixture::new(&feed.base_url());
        let bus = EventBus::new();
        let (mut client, status) = fixture.client(&bus);
        assert_eq!(
            client.poll_once().await,
            PollOutcome::Failed(FeedErrorClass::AuthFailed),
            "HTTP {code}"
        );
        let snapshot = status.snapshot();
        assert_eq!(snapshot.state, ForgeEventsState::AuthFailed);
        assert_eq!(snapshot.last_error.as_deref(), Some("auth_failed"));
        // The detail names the key's PATH; the key's contents never appear.
        let detail = snapshot.last_error_detail.unwrap_or_default();
        assert!(detail.contains("key file"), "{detail}");
        assert!(!detail.contains("super-secret-key"), "{detail}");
    }
}

#[tokio::test]
async fn a_404_for_our_host_is_a_host_mismatch_and_never_advances_the_cursor() {
    let feed = TestFeed::start(vec![Canned::code(404)]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, status) = fixture.client(&bus);

    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::HostMismatch));
    assert_eq!(status.snapshot().state, ForgeEventsState::HostMismatch);
    assert_eq!(client.cursor(), 0);
    assert!(!fixture.feed.paths.state.exists());
}

#[tokio::test]
async fn a_200_echoing_another_host_is_refused_along_with_its_cursor() {
    let feed = TestFeed::start(vec![Canned::ok(&page_json("linux-box", 9000, TWO_EVENTS))]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let mut subscription = bus.subscribe([BUS_TOPIC]);
    let (mut client, status) = fixture.client(&bus);

    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::HostMismatch));
    let snapshot = status.snapshot();
    assert_eq!(snapshot.state, ForgeEventsState::HostMismatch);
    assert_eq!(snapshot.last_error.as_deref(), Some("host_mismatch"));
    // Nothing from a feed that is not ours is applied, journaled or published.
    assert_eq!(client.cursor(), 0);
    assert!(!fixture.feed.paths.state.exists());
    assert!(!fixture.feed.paths.journal.exists());
    assert!(tokio::time::timeout(Duration::from_millis(100), subscription.recv())
        .await
        .is_err());
}

#[tokio::test]
async fn a_page_with_no_host_id_is_a_mismatch_not_an_unchecked_pass() {
    let feed = TestFeed::start(vec![Canned::ok(
        r#"{"cursor":9,"events":[{"seq":1,"type":"issues"}]}"#,
    )]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, _status) = fixture.client(&bus);
    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::HostMismatch));
    assert_eq!(client.cursor(), 0);
}

#[tokio::test]
async fn any_other_non_2xx_including_a_redirect_is_a_protocol_failure() {
    // A 302 lands here precisely because redirects are disabled: a redirect
    // is an instruction to re-send the Bearer header somewhere unconfigured.
    for code in [302u16, 429, 500] {
        let feed = TestFeed::start(vec![Canned::code(code)]);
        let fixture = Fixture::new(&feed.base_url());
        let bus = EventBus::new();
        let (mut client, status) = fixture.client(&bus);
        assert_eq!(
            client.poll_once().await,
            PollOutcome::Failed(FeedErrorClass::Protocol),
            "HTTP {code}"
        );
        assert_eq!(status.snapshot().state, ForgeEventsState::Failing);
        assert!(status
            .snapshot()
            .last_error_detail
            .unwrap_or_default()
            .contains(&code.to_string()));
    }
}

#[tokio::test]
async fn an_over_limit_response_is_refused_not_truncated_and_leaves_the_cursor_alone() {
    let padding = "y".repeat(MAX_RESPONSE_BYTES + 1024);
    let body = format!(
        r#"{{"host_id":"mac-studio","cursor":99,"events":[{{"seq":1,"type":"issues","pad":"{padding}"}}]}}"#
    );
    let feed = TestFeed::start(vec![Canned::ok(&body)]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, status) = fixture.client(&bus);

    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::Protocol));
    assert!(status
        .snapshot()
        .last_error_detail
        .unwrap_or_default()
        .contains("refusing"));
    assert_eq!(client.cursor(), 0, "an over-cap page must not advance");
    assert!(!fixture.feed.paths.state.exists());
    assert!(!fixture.feed.paths.journal.exists());
}

#[tokio::test]
async fn a_cursor_that_moves_backwards_is_refused() {
    let feed = TestFeed::start(vec![
        Canned::ok(&page_json("mac-studio", 100, TWO_EVENTS)),
        Canned::ok(&page_json("mac-studio", 4, TWO_EVENTS)),
    ]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, status) = fixture.client(&bus);

    assert_eq!(
        client.poll_once().await,
        PollOutcome::Page {
            count: 2,
            cursor: 100
        }
    );
    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::Protocol));
    assert!(status
        .snapshot()
        .last_error_detail
        .unwrap_or_default()
        .contains("backwards"));
    assert_eq!(client.cursor(), 100, "the good cursor must survive");
    assert_eq!(read_cursor(&fixture.feed.paths.state, "mac-studio"), 100);
}

#[tokio::test]
async fn a_body_that_is_not_a_page_is_a_protocol_failure() {
    let feed = TestFeed::start(vec![Canned::ok("<html>proxy interstitial</html>")]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, _status) = fixture.client(&bus);
    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::Protocol));
}

#[tokio::test]
async fn an_unreachable_feed_is_a_transport_failure() {
    // Bind and immediately drop, so the port is closed.
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let fixture = Fixture::new(&format!("http://{addr}"));
    let bus = EventBus::new();
    let (mut client, status) = fixture.client(&bus);
    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::Transport));
    assert_eq!(status.snapshot().last_error.as_deref(), Some("transport"));
}

#[tokio::test]
async fn a_key_file_that_vanishes_between_polls_is_auth_failed_and_never_hits_the_network() {
    let feed = TestFeed::start(vec![Canned::ok(&page_json("mac-studio", 3, ""))]);
    let fixture = Fixture::new(&feed.base_url());
    let bus = EventBus::new();
    let (mut client, status) = fixture.client(&bus);

    client.poll_once().await;
    assert_eq!(feed.served(), 1);
    std::fs::remove_file(&fixture.feed.key_file).expect("remove key");
    assert_eq!(client.poll_once().await, PollOutcome::Failed(FeedErrorClass::AuthFailed));
    assert_eq!(feed.served(), 1, "no request may be made without a key");
    assert_eq!(status.snapshot().state, ForgeEventsState::AuthFailed);
}

#[tokio::test]
async fn an_unwritable_state_directory_does_not_stop_the_page_advancing() {
    // The journal and the cursor file are diagnostic: a write failure is
    // logged and the page still advances. A restart then re-renders from the
    // last durable cursor, still inside the feed's retention window.
    let feed = TestFeed::start(vec![Canned::ok(&page_json("mac-studio", 12, TWO_EVENTS))]);
    let fixture = Fixture::new(&feed.base_url());
    let blocked = fixture.dir.path().join("blocked");
    std::fs::write(&blocked, "a regular file, not a directory").expect("write blocker");
    let unwritable = ResolvedFeed {
        paths: FeedPaths::in_dir(blocked.join("sub")),
        ..fixture.feed.clone()
    };
    let bus = EventBus::new();
    let mut subscription = bus.subscribe([BUS_TOPIC]);
    let (mut client, status) = fixture.client_with(&bus, unwritable);

    assert_eq!(
        client.poll_once().await,
        PollOutcome::Page {
            count: 2,
            cursor: 12
        }
    );
    assert_eq!(client.cursor(), 12);
    assert_eq!(status.snapshot().state, ForgeEventsState::Healthy);
    // The prompt still goes out — the durability failure is diagnostic.
    assert!(subscription.recv().await.is_ok());
}

// ============================================================================
// Backoff promotion
// ============================================================================

fn status_cell(poll_interval_secs: u64) -> (tempfile::TempDir, FeedStatus) {
    let dir = tempfile::tempdir().expect("tempdir");
    let feed = ResolvedFeed {
        endpoint: "https://events.internal".to_string(),
        host_id: "mac-studio".to_string(),
        key_file: dir.path().join("key"),
        poll_interval_secs,
        page_size: 100,
        paths: FeedPaths::in_dir(dir.path().to_path_buf()),
    };
    let status = FeedStatus::started(&feed, 0);
    (dir, status)
}

#[test]
fn the_backoff_promotion_is_symmetric_and_keeps_the_error_class() {
    let (_dir, status) = status_cell(DEFAULT_POLL_INTERVAL_SECS);
    assert_eq!(status.snapshot().state, ForgeEventsState::Connecting);
    assert_eq!(status.current_interval_secs(), DEFAULT_POLL_INTERVAL_SECS);

    for n in 1..BACKOFF_FAILURE_STREAK {
        status.record_failure(FeedErrorClass::AuthFailed, format!("attempt {n}"));
        let snapshot = status.snapshot();
        assert_eq!(snapshot.state, ForgeEventsState::AuthFailed);
        assert_eq!(
            status.current_interval_secs(),
            DEFAULT_POLL_INTERVAL_SECS,
            "not stretched below the streak"
        );
        assert_eq!(snapshot.consecutive_failures, n);
    }
    status.record_failure(FeedErrorClass::AuthFailed, "attempt 3".to_string());
    let snapshot = status.snapshot();
    assert_eq!(snapshot.state, ForgeEventsState::Backoff);
    assert_eq!(status.current_interval_secs(), BACKOFF_POLL_INTERVAL_SECS);
    // Backoff narrows the cadence question; it does not erase the cause.
    assert_eq!(snapshot.last_error.as_deref(), Some("auth_failed"));
    assert_eq!(snapshot.last_error_detail.as_deref(), Some("attempt 3"));

    // One success is enough to come back — in both state and cadence.
    status.record_success(1, 42);
    let snapshot = status.snapshot();
    assert_eq!(snapshot.state, ForgeEventsState::Healthy);
    assert_eq!(snapshot.consecutive_failures, 0);
    assert_eq!(snapshot.last_error, None);
    assert_eq!(status.current_interval_secs(), DEFAULT_POLL_INTERVAL_SECS);
    assert_eq!(snapshot.cursor, 42);
}

#[test]
fn a_successful_empty_poll_does_not_count_as_a_page() {
    let (_dir, status) = status_cell(DEFAULT_POLL_INTERVAL_SECS);
    status.record_success(0, 5);
    let snapshot = status.snapshot();
    assert_eq!(snapshot.state, ForgeEventsState::Healthy);
    assert_eq!(snapshot.pages_observed, 0);
    assert_eq!(snapshot.events_observed, 0);
    assert_eq!(snapshot.last_page_at, None);
    assert!(snapshot.last_poll_at.is_some());
}

#[test]
fn each_error_class_maps_to_its_own_state_and_token() {
    assert_eq!(FeedErrorClass::Transport.state(), ForgeEventsState::Failing);
    assert_eq!(FeedErrorClass::Protocol.state(), ForgeEventsState::Failing);
    assert_eq!(FeedErrorClass::AuthFailed.state(), ForgeEventsState::AuthFailed);
    assert_eq!(FeedErrorClass::HostMismatch.state(), ForgeEventsState::HostMismatch);
    assert_eq!(FeedErrorClass::Transport.token(), "transport");
    assert_eq!(FeedErrorClass::Protocol.token(), "protocol");
    assert_eq!(FeedErrorClass::AuthFailed.token(), "auth_failed");
    assert_eq!(FeedErrorClass::HostMismatch.token(), "host_mismatch");
}

// ============================================================================
// Status surface
// ============================================================================

#[test]
fn a_daemon_with_no_block_answers_disabled_rather_than_falling_silent() {
    let status = ForgeEventsStatus::disabled();
    assert_eq!(status.state, ForgeEventsState::Disabled);
    assert_eq!(serde_json::to_value(&status).expect("serialize")["state"], "disabled");
    assert!(!ForgeEventsState::Disabled.is_problem());
}

#[test]
fn every_state_serializes_to_its_documented_snake_case_token() {
    let expected = [
        (ForgeEventsState::Disabled, "disabled"),
        (ForgeEventsState::Misconfigured, "misconfigured"),
        (ForgeEventsState::Connecting, "connecting"),
        (ForgeEventsState::Failing, "failing"),
        (ForgeEventsState::AuthFailed, "auth_failed"),
        (ForgeEventsState::HostMismatch, "host_mismatch"),
        (ForgeEventsState::Backoff, "backoff"),
        (ForgeEventsState::Healthy, "healthy"),
    ];
    for (state, token) in expected {
        assert_eq!(serde_json::to_value(state).expect("serialize"), token);
        assert_eq!(
            serde_json::from_value::<ForgeEventsState>(serde_json::json!(token))
                .expect("deserialize"),
            state
        );
    }
    // A newer daemon's unknown state must not fail the whole status parse.
    assert_eq!(
        serde_json::from_value::<ForgeEventsState>(serde_json::json!("quantum"))
            .expect("deserialize"),
        ForgeEventsState::Unrecognized
    );
}

#[test]
fn misconfigured_carries_the_resolved_endpoint_and_the_named_detail() {
    let status = ForgeEventsStatus::misconfigured(
        Some("https://events.internal".to_string()),
        "forgeEvents.hostId not configured".to_string(),
    );
    assert_eq!(status.state, ForgeEventsState::Misconfigured);
    assert_eq!(status.endpoint.as_deref(), Some("https://events.internal"));
    assert_eq!(status.last_error_detail.as_deref(), Some("forgeEvents.hostId not configured"));
    assert!(ForgeEventsState::Misconfigured.is_problem());
}

// ============================================================================
// spawn_task
// ============================================================================

#[test]
#[serial]
fn spawn_is_inert_when_disabled() {
    clear_env();
    let bus = EventBus::new();
    assert!(spawn_task(&ForgeEventsConfig::default(), &bus).is_none());
}

#[test]
#[serial]
fn spawn_refuses_when_under_configured() {
    clear_env();
    let bus = EventBus::new();
    // `default_state_dir()` is `None` under cfg(test), so an enabled config
    // can never accidentally resolve a real `$HOME` here.
    let config = ForgeEventsConfig {
        enabled: Some(true),
        ..ForgeEventsConfig::default()
    };
    assert!(spawn_task(&config, &bus).is_none());
}

// ============================================================================
// EventBus::Clone — the piece this module needed (#8765)
// ============================================================================

#[tokio::test]
async fn a_cloned_bus_publishes_to_the_original_subscribers() {
    let bus = EventBus::new();
    let mut subscription = bus.subscribe([BUS_TOPIC]);
    let clone = bus.clone();
    drop(bus);
    clone
        .publish_generic(BUS_TOPIC, serde_json::json!({"count": 1}))
        .expect("publish through the clone");
    let Event::Generic { topic, payload } = subscription.recv().await.expect("event") else {
        panic!("expected Generic");
    };
    assert_eq!(topic, BUS_TOPIC);
    assert_eq!(payload["count"], 1);
}
