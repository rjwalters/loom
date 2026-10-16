//! Tests for the state mirror's discovery reads and wakes (2am#4367 P5).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use serial_test::serial;

use super::client::{Freshness, MirrorError, Watermark};
use super::wake::{self, Cursors, Nudge};
use super::*;

const BOUND: Duration = Duration::from_secs(600);
const REPO: &str = "acme/widgets";

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    t("2026-10-13T12:00:00Z")
}

fn mark(event: Option<&str>, reconciled: Option<&str>, served: &str) -> Watermark {
    Watermark {
        repo: Some(REPO.to_string()),
        known: true,
        last_event_at: event.map(str::to_string),
        last_reconciled_at: reconciled.map(str::to_string),
        latest_seq: 7,
        served_at: Some(served.to_string()),
        archived: false,
        gone: None,
    }
}

// ---------------------------------------------------------------- config

fn cfg(mirror: Value) -> Value {
    json!({ "forge": { "mirror": mirror } })
}

#[test]
fn an_absent_or_null_block_is_off_with_no_warning() {
    for effective in [json!({}), json!({"forge": {}}), cfg(Value::Null)] {
        let (c, w) = parse(&effective);
        assert!(c.is_none());
        assert!(w.is_empty(), "{w:?}");
    }
    // A configured gateway alone does not turn the mirror on.
    let (c, _) = parse(&json!({"forge": {"gateway": {"origin": "gw.example.com", "keyRefs": {}}}}));
    assert!(c.is_none());
}

#[test]
fn a_full_block_parses_with_defaults_and_aliases() {
    let (c, w) = parse(&cfg(json!({
        "origin": "https://Events.Example.com/",
        "key_ref": "ssm:///events/keys/host-a",
        "region": "example-region-1"
    })));
    assert!(w.is_empty(), "{w:?}");
    let c = c.unwrap();
    assert_eq!(c.host, "events.example.com");
    assert_eq!(c.key_ref, KeyRef::Ssm("/events/keys/host-a".into()));
    assert_eq!(c.region.as_deref(), Some("example-region-1"));
    assert_eq!(c.max_staleness, Duration::from_secs(DEFAULT_MAX_STALENESS_SECS));
    assert_eq!(c.change_poll, Duration::from_secs(DEFAULT_CHANGE_POLL_SECS));

    let (c, w) = parse(&cfg(json!({
        "origin": "events.example.com",
        "keyRef": "file:/home/u/.loom/mirror.key",
        "maxStalenessSecs": 120,
        "changePollSecs": 15
    })));
    assert!(w.is_empty(), "{w:?}");
    let c = c.unwrap();
    assert_eq!(c.max_staleness, Duration::from_secs(120));
    assert_eq!(c.change_poll, Duration::from_secs(15));
}

#[test]
fn bounds_are_clamped_never_widened_past_the_adr_0021_ceiling() {
    let (c, w) = parse(&cfg(json!({
        "origin": "events.example.com", "keyRef": "file:/k",
        "maxStalenessSecs": 86_400, "changePollSecs": 1
    })));
    let c = c.unwrap();
    assert_eq!(c.max_staleness.as_secs(), MAX_STALENESS_CEILING_SECS);
    assert_eq!(c.max_staleness.as_secs(), 900);
    assert_eq!(c.change_poll.as_secs(), MIN_CHANGE_POLL_SECS);
    assert_eq!(w.len(), 2, "{w:?}");
    let (c, w) = parse(&cfg(json!({
        "origin": "events.example.com", "keyRef": "file:/k", "maxStalenessSecs": "soon"
    })));
    assert_eq!(c.unwrap().max_staleness.as_secs(), DEFAULT_MAX_STALENESS_SECS);
    assert_eq!(w.len(), 1);
}

#[test]
fn an_unusable_block_stays_off_and_says_why() {
    for (block, why) in [
        (json!("events.example.com"), "must be an object"),
        (json!({"keyRef": "file:/k"}), "origin is required"),
        (json!({"origin": "http://events.example.com", "keyRef": "file:/k"}), "https"),
        (json!({"origin": "api.github.com", "keyRef": "file:/k"}), "not GitHub"),
        (json!({"origin": "events.example.com"}), "keyRef is required"),
        (json!({"origin": "events.example.com", "keyRef": "env:KEY"}), "keyRef"),
        (json!({"origin": "events.example.com", "keyRef": 7}), "must be a string"),
    ] {
        let (c, w) = parse(&cfg(block.clone()));
        assert!(c.is_none(), "{block}");
        assert!(w.iter().any(|m| m.contains(why)), "{block}: {w:?}");
    }
    let (c, w) = parse(&cfg(json!({
        "origin": "events.example.com", "keyRef": "file:/k", "region": "Not A Region"
    })));
    assert!(c.unwrap().region.is_none());
    assert_eq!(w.len(), 1);
}

/// Break-glass: with `forge.mirror` unset nothing is live, so every
/// discovery listing returns `None` (today's path) without resolving a repo
/// or making a request, and setting then unsetting it restores exactly that.
#[test]
#[serial]
fn break_glass_unset_is_todays_path() {
    set_active(None);
    let mut r = Refresher::default();
    let t0 = Instant::now();
    let reads = std::cell::Cell::new(0);
    let read_key = |_: &KeyRef, _: &crate::aws_ssm::AwsAccess<'_>| {
        reads.set(reads.get() + 1);
        Ok("2ge_0123456789abcdef0123456789abcdef".to_string())
    };
    assert_eq!(r.pass_with(&json!({}), t0, &read_key), PassOutcome::Off);
    assert!(!is_active());
    assert_eq!(reads.get(), 0, "no key is read while unset");

    let on = cfg(json!({"origin": "events.example.com", "keyRef": "file:/k"}));
    assert_eq!(r.pass_with(&on, t0, &read_key), PassOutcome::Active);
    let a = super::active().unwrap();
    assert_eq!(a.base_url, "https://events.example.com");
    assert!(!format!("{a:?}").contains("0123456789abcdef"), "Debug redacts the key");
    assert_eq!(
        r.pass_with(&on, t0 + Duration::from_secs(60), &read_key),
        PassOutcome::Unchanged
    );
    assert_eq!(reads.get(), 1);
    // A 401 makes the next pass re-read the key.
    note_key_rejected();
    assert_eq!(r.pass_with(&on, t0 + Duration::from_secs(61), &read_key), PassOutcome::Active);
    assert_eq!(reads.get(), 2);

    assert_eq!(r.pass_with(&json!({}), t0, &read_key), PassOutcome::Off);
    assert!(super::active().is_none());

    // A key that cannot be read leaves nothing live.
    let fail = |_: &KeyRef, _: &crate::aws_ssm::AwsAccess<'_>| {
        Err::<String, String>("no such file".into())
    };
    assert!(matches!(r.pass_with(&on, t0, &fail), PassOutcome::KeyFailed(_)));
    assert!(!is_active());
    set_active(None);
}

// ---------------------------------------------------------------- watermark

#[test]
fn freshness_needs_a_known_reconciled_recent_repo() {
    let served = "2026-10-13T12:00:00Z";
    let fresh = mark(Some("2026-10-13T11:58:00Z"), Some("2026-10-13T09:00:00Z"), served);
    assert_eq!(fresh.freshness(REPO, BOUND, now()), Freshness::Fresh);
    // A recent reconcile alone is fresh too.
    let recon = mark(None, Some("2026-10-13T11:55:00Z"), served);
    assert_eq!(recon.freshness(REPO, BOUND, now()), Freshness::Fresh);
    // Newest evidence older than the bound.
    let old = mark(Some("2026-10-13T11:40:00Z"), Some("2026-10-13T09:00:00Z"), served);
    assert_eq!(old.freshness(REPO, BOUND, now()), Freshness::Stale);
    // Never reconciled: its holdings may be only what webhooks named.
    let never = mark(Some("2026-10-13T11:59:00Z"), None, served);
    assert_eq!(never.freshness(REPO, BOUND, now()), Freshness::Unknown);
    // An answer served long ago (or a skewed clock) is stale.
    let late = mark(
        Some("2026-10-13T11:20:00Z"),
        Some("2026-10-13T11:20:00Z"),
        "2026-10-13T11:21:00Z",
    );
    assert_eq!(late.freshness(REPO, BOUND, now()), Freshness::Stale);
    // Unknown, moved, or about another repo.
    let mut unknown = fresh.clone();
    unknown.known = false;
    assert_eq!(unknown.freshness(REPO, BOUND, now()), Freshness::Unknown);
    let mut gone = fresh.clone();
    gone.gone = Some("renamed:acme/gadgets".into());
    assert_eq!(gone.freshness(REPO, BOUND, now()), Freshness::Unknown);
    let mut other = fresh.clone();
    other.repo = Some("acme/other".into());
    assert_eq!(other.freshness(REPO, BOUND, now()), Freshness::Unknown);
    let mut upper = fresh;
    upper.repo = Some("Acme/Widgets".into());
    assert_eq!(upper.freshness(REPO, BOUND, now()), Freshness::Fresh);
}

/// poll_gate invariant 3 at the call sites: the mirror serves discovery only.
/// No claim, label-transition or merge module names it, so no such decision
/// can read it.
#[test]
fn only_the_discovery_and_wake_modules_name_the_mirror() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut users = std::collections::BTreeSet::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "forge_mirror.rs"
                || rel.starts_with("forge_mirror/")
                || rel == "observability/ops/forge_mirror.rs"
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            if text.contains("forge_mirror::") {
                users.insert(rel);
            }
        }
    }
    let expected: std::collections::BTreeSet<String> = [
        // The fleet worklist backend (#12306): one discovery read per tick,
        // never a claim, label-transition or merge read (invariant 3). Its
        // tests drive the mirror's client and `set_active`.
        // The chain (#12311) reads `forge.mirror` to know whether a mirror is
        // configured; it makes no read.
        "discovery/chain.rs",
        "discovery/mirror.rs",
        "discovery/tests.rs",
        "discovery/worklist.rs",
        // The reconciliation readers resolve a workspace's repo slug to find
        // its part of the tick's snapshot (#12307); they make no read.
        "discovery/published.rs",
        // The task spawn, the wake seam's source check, and the telemetry drain.
        "daemon_service/observer_tasks.rs",
        "forge_events/wake.rs",
        "observability/ops/ratelimit.rs",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(
        users, expected,
        "a new forge_mirror user needs review against poll_gate invariant 3"
    );
}

// ---------------------------------------------------------------- the HTTP client

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn serve(responses: Vec<(u16, String)>) -> (SocketAddr, Seen) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let seen_t = seen.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { break };
            let head = {
                use std::io::Read as _;
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while buf.len() < 16_384 && !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.push(b[0]),
                    }
                }
                String::from_utf8_lossy(&buf).into_owned()
            };
            let mut lines = head.lines();
            let line = lines.next().unwrap_or_default().to_string();
            let auth = lines
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l[14..].trim().to_string());
            seen_t.lock().unwrap().push((line, auth));
            let (status, body) = responses
                .get(i)
                .or_else(|| responses.last())
                .cloned()
                .unwrap_or((500, "{}".into()));
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (addr, seen)
}

#[test]
#[serial]
fn the_client_sends_the_key_as_a_bearer_header_only() {
    let body = json!({ "items": [{ "number": 4 }] }).to_string();
    let (addr, seen) = serve(vec![(200, body), (401, "{}".into())]);
    let a = Active::for_test(&format!("http://{addr}"), BOUND);
    let q = vec![
        ("repo".to_string(), REPO.to_string()),
        ("label".to_string(), "loom:issue".to_string()),
    ];
    let v = client::get_json_blocking(&a, "/v1/mirror/items", &q, Duration::from_secs(5)).unwrap();
    assert_eq!(v["items"][0]["number"], 4);
    let err =
        client::get_json_blocking(&a, "/v1/mirror/items", &q, Duration::from_secs(5)).unwrap_err();
    assert_eq!(err, MirrorError::Auth(401));
    assert!(!err.to_string().contains(a.key()));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[0]
            .0
            .starts_with("GET /v1/mirror/items?repo=acme%2Fwidgets&label=loom%3Aissue "),
        "{}",
        seen[0].0
    );
    assert!(!seen[0].0.contains(a.key()), "the key is never in the URL");
    assert_eq!(seen[0].1.as_deref(), Some(format!("Bearer {}", a.key()).as_str()));
    // The 401 asked the refresher to re-read the key; consume the flag.
    assert!(KEY_REJECTED.swap(false, std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn an_unreachable_mirror_is_a_bounded_error() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let a = Active::for_test(&format!("http://{addr}"), BOUND);
    let started = Instant::now();
    let err =
        client::get_json_blocking(&a, "/v1/mirror/items", &[], Duration::from_secs(3)).unwrap_err();
    assert!(matches!(err, MirrorError::Transport(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

// ---------------------------------------------------------------- wakes

fn changes_page(changes: Value, repos: Value, more: bool) -> client::ChangesPage {
    serde_json::from_value(json!({ "changes": changes, "repos": repos, "more": more })).unwrap()
}

fn rc(next_after: u64, latest: u64, truncated: bool) -> Value {
    json!({ "next_after": next_after, "more": false, "truncated": truncated,
            "watermark": { "repo": REPO, "known": true, "latest_seq": latest } })
}

#[test]
fn change_kinds_map_onto_the_consumers_webhook_types() {
    use crate::forge_events::wake::{
        payload_qualifies, CLAIM_RECONCILE_EVENT_TYPES, IN_FLIGHT_PR_EVENT_TYPES,
        WORK_FINDER_EVENT_TYPES,
    };
    assert_eq!(wake::webhook_type("issue", None), Some("issues"));
    assert_eq!(wake::webhook_type("label", Some("issue")), Some("issues"));
    assert_eq!(wake::webhook_type("label", Some("pr")), Some("pull_request"));
    assert_eq!(wake::webhook_type("pr", None), Some("pull_request"));
    assert_eq!(wake::webhook_type("pr_merged", None), Some("pull_request"));
    assert_eq!(wake::webhook_type("check", None), Some("check_run"));
    assert_eq!(wake::webhook_type("future_kind", None), None);

    let mut cursors = Cursors::new("https://m");
    cursors.repos.insert(REPO.into(), 3);
    let mut nudge = Nudge::default();
    let p = changes_page(
        json!([{"seq": 4, "repo": REPO, "kind": "label", "item_type": "issue", "number": 12, "at": "2026-10-13T11:59:58Z"}]),
        json!({ REPO: rc(4, 4, false) }),
        false,
    );
    wake::apply(&mut cursors, &[REPO.to_string()], &p, now(), &mut nudge);
    let payload = nudge.payload();
    assert!(payload_qualifies(&payload, WORK_FINDER_EVENT_TYPES));
    assert!(!payload_qualifies(&payload, IN_FLIGHT_PR_EVENT_TYPES));
    assert!(!payload_qualifies(&payload, CLAIM_RECONCILE_EVENT_TYPES));
    // Routing hints only: no number, label, title or state.
    let text = payload.to_string();
    assert!(!text.contains("12") && !text.contains("loom:"), "{text}");
    assert_eq!(payload["repos"], json!([REPO]));
    assert_eq!(nudge.latencies, vec![(REPO.to_string(), "label".to_string(), 2.0)]);
    assert_eq!(cursors.repos[REPO], 4);
}

#[test]
fn cursors_advance_and_a_truncated_or_reset_log_prompts_a_reread() {
    let asked = vec![REPO.to_string()];
    let mut cursors = Cursors::new("https://m");
    // Baseline: a new repo starts at its latest change, without a prompt.
    wake::baseline(
        &mut cursors,
        &changes_page(json!([]), json!({ REPO: rc(0, 41, false) }), false),
    );
    assert_eq!(cursors.repos[REPO], 41);

    let mut nudge = Nudge::default();
    wake::apply(
        &mut cursors,
        &asked,
        &changes_page(json!([]), json!({ REPO: rc(41, 41, false) }), false),
        now(),
        &mut nudge,
    );
    assert!(!nudge.prompts(), "nothing new is no prompt");

    // The log restarted below our cursor (a recreated mirror).
    let mut nudge = Nudge::default();
    wake::apply(
        &mut cursors,
        &asked,
        &changes_page(json!([]), json!({ REPO: rc(41, 2, false) }), false),
        now(),
        &mut nudge,
    );
    assert!(nudge.prompts() && nudge.missed);
    assert_eq!(cursors.repos[REPO], 2);

    // Truncated: changes were dropped from retention before we read them.
    let mut nudge = Nudge::default();
    wake::apply(
        &mut cursors,
        &asked,
        &changes_page(json!([]), json!({ REPO: rc(90, 100, true) }), false),
        now(),
        &mut nudge,
    );
    assert!(nudge.missed);
    assert_eq!(cursors.repos[REPO], 100);

    // A repo not asked about is ignored; a cursor never moves backwards.
    let mut nudge = Nudge::default();
    wake::apply(
        &mut cursors,
        &asked,
        &changes_page(
            json!([{"seq": 1, "repo": "acme/other", "kind": "issue"}]),
            json!({ REPO: rc(50, 100, false), "acme/other": rc(1, 1, false) }),
            false,
        ),
        now(),
        &mut nudge,
    );
    assert!(!nudge.prompts());
    assert_eq!(cursors.repos[REPO], 100);
    assert!(!cursors.repos.contains_key("acme/other"));
}

#[test]
fn cursors_persist_per_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forge-mirror").join("cursors.json");
    let mut c = Cursors::new("https://a.example.com");
    c.repos.insert(REPO.into(), 9);
    wake::save_cursors(&path, &c).unwrap();
    assert_eq!(wake::load_cursors(Some(&path), "https://a.example.com"), c);
    // Another origin's cursors are never applied: every repo re-baselines.
    assert_eq!(
        wake::load_cursors(Some(&path), "https://b.example.com"),
        Cursors::new("https://b.example.com")
    );
    assert_eq!(
        wake::load_cursors(None, "https://a.example.com"),
        Cursors::new("https://a.example.com")
    );
    std::fs::write(&path, b"{not json").unwrap();
    assert_eq!(
        wake::load_cursors(Some(&path), "https://a.example.com"),
        Cursors::new("https://a.example.com")
    );
}

#[tokio::test]
async fn a_pass_baselines_new_repos_then_reads_and_publishes_changes() {
    let baseline =
        json!({"changes": [], "cursor": "", "more": false, "repos": { REPO: rc(0, 10, false) }})
            .to_string();
    let next = json!({
        "changes": [{"seq": 11, "repo": REPO, "kind": "pr", "number": 3, "at": "2026-10-13T11:59:00Z"}],
        "cursor": "", "more": false, "repos": { REPO: rc(11, 11, false) }
    })
    .to_string();
    let (addr, seen) = serve(vec![(200, baseline), (200, next)]);
    let a = Active::for_test(&format!("http://{addr}"), BOUND);
    let http = client::build_http().unwrap();
    let repos = vec![REPO.to_string()];
    let mut cursors = Cursors::new(&a.base_url);

    let (first, r) = wake::pass(&http, &a, &repos, &mut cursors).await;
    r.unwrap();
    assert!(!first.prompts(), "a baseline never prompts");
    assert_eq!(cursors.repos[REPO], 10);

    let bus = crate::event_bus::EventBus::new();
    let mut sub = bus.subscribe([crate::forge_events::BUS_TOPIC]);
    let (second, r) = wake::pass(&http, &a, &repos, &mut cursors).await;
    r.unwrap();
    assert_eq!(cursors.repos[REPO], 11);
    assert!(second.prompts());
    wake::publish(&bus, &second);
    let event = sub.recv().await.unwrap();
    assert!(crate::forge_events::wake::event_qualifies(
        &event,
        crate::forge_events::wake::IN_FLIGHT_PR_EVENT_TYPES
    ));

    let seen = seen.lock().unwrap().clone();
    assert!(seen[0].0.contains("after=0") && seen[0].0.contains("limit=1"), "{}", seen[0].0);
    assert!(seen[1].0.contains("after=acme%2Fwidgets%3A10"), "{}", seen[1].0);
}

// ---------------------------------------------------------------- review fixes

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_mirror_read_from_a_spawned_task_on_a_multi_thread_runtime_works() {
    // The blocking call, from a task.
    let (addr, _) = serve(vec![(200, "{\"ok\":1}".into())]);
    let a = Active::for_test(&format!("http://{addr}"), BOUND);
    let v = tokio::spawn(async move {
        client::get_json_blocking(&a, "/v1/mirror/watermark", &[], Duration::from_secs(5))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(v["ok"], 1);
}

#[test]
fn a_watermark_that_names_no_repo_is_unknown() {
    let mut w = mark(
        Some("2026-10-13T11:58:00Z"),
        Some("2026-10-13T09:00:00Z"),
        "2026-10-13T12:00:00Z",
    );
    w.repo = None;
    assert_eq!(w.freshness(REPO, BOUND, now()), Freshness::Unknown);
}

#[test]
#[serial]
fn a_key_that_cannot_be_read_backs_off_exponentially_to_fifteen_minutes() {
    assert_eq!(key_retry_delay(1), Duration::from_secs(60));
    assert_eq!(key_retry_delay(2), Duration::from_secs(120));
    assert_eq!(key_retry_delay(4), Duration::from_secs(480));
    assert_eq!(key_retry_delay(5), KEY_RETRY_CAP);
    assert_eq!(key_retry_delay(40), KEY_RETRY_CAP);

    set_active(None);
    let on = cfg(json!({"origin": "events.example.com", "keyRef": "file:/k"}));
    let reads = std::cell::Cell::new(0);
    let fail = |_: &KeyRef, _: &crate::aws_ssm::AwsAccess<'_>| {
        reads.set(reads.get() + 1);
        Err::<String, String>("no such file".into())
    };
    let mut r = Refresher::default();
    let t0 = Instant::now();
    assert!(matches!(r.pass_with(&on, t0, &fail), PassOutcome::KeyFailed(_)));
    // Every-minute refreshes inside the backoff read nothing.
    assert_eq!(r.pass_with(&on, t0 + Duration::from_secs(59), &fail), PassOutcome::Unchanged);
    assert_eq!(reads.get(), 1);
    assert!(matches!(
        r.pass_with(&on, t0 + Duration::from_secs(60), &fail),
        PassOutcome::KeyFailed(_)
    ));
    assert_eq!(r.pass_with(&on, t0 + Duration::from_secs(179), &fail), PassOutcome::Unchanged);
    assert_eq!(reads.get(), 2);
    // A config change retries at once.
    let other = cfg(json!({"origin": "events.example.com", "keyRef": "file:/k2"}));
    assert!(matches!(
        r.pass_with(&other, t0 + Duration::from_secs(61), &fail),
        PassOutcome::KeyFailed(_)
    ));
    assert_eq!(reads.get(), 3);
    // An ssm:// ref with no region names this block, and reads nothing.
    let ssm = cfg(json!({"origin": "events.example.com", "keyRef": "ssm:///k"}));
    let old = (std::env::var_os("AWS_REGION"), std::env::var_os("AWS_DEFAULT_REGION"));
    std::env::remove_var("AWS_REGION");
    std::env::remove_var("AWS_DEFAULT_REGION");
    let real = |k: &KeyRef, a: &crate::aws_ssm::AwsAccess<'_>| k.read(a, CONFIG_KEY);
    let PassOutcome::KeyFailed(why) = r.pass_with(&ssm, t0, &real) else {
        panic!("no region")
    };
    if let Some(v) = old.0 {
        std::env::set_var("AWS_REGION", v)
    }
    if let Some(v) = old.1 {
        std::env::set_var("AWS_DEFAULT_REGION", v)
    }
    assert!(why.contains("forge.mirror.region") && !why.contains("gateway"), "{why}");
    assert_eq!(reads.get(), 3);
    set_active(None);
}

#[test]
fn a_missed_changes_prompt_wakes_every_consumer() {
    use crate::forge_events::wake::{
        payload_qualifies, CLAIM_RECONCILE_EVENT_TYPES, IN_FLIGHT_PR_EVENT_TYPES,
        WORK_FINDER_EVENT_TYPES,
    };
    let mut cursors = Cursors::new("https://m");
    cursors.repos.insert(REPO.into(), 50);
    let mut nudge = Nudge::default();
    wake::apply(
        &mut cursors,
        &[REPO.to_string()],
        &changes_page(json!([]), json!({ REPO: rc(60, 100, true) }), false),
        now(),
        &mut nudge,
    );
    assert!(nudge.missed && nudge.count == 0);
    let payload = nudge.payload();
    for wanted in [
        WORK_FINDER_EVENT_TYPES,
        CLAIM_RECONCILE_EVENT_TYPES,
        IN_FLIGHT_PR_EVENT_TYPES,
    ] {
        assert!(payload_qualifies(&payload, wanted), "{wanted:?}: {payload}");
    }
}

#[test]
fn a_repo_the_mirror_does_not_know_is_not_baselined() {
    let mut cursors = Cursors::new("https://m");
    let unknown =
        json!({ "next_after": 0, "watermark": { "repo": REPO, "known": false, "latest_seq": 0 } });
    wake::baseline(&mut cursors, &changes_page(json!([]), json!({ REPO: unknown }), false));
    assert!(cursors.repos.is_empty(), "baselined again once the mirror knows it");
}

#[tokio::test]
async fn a_pass_that_fails_part_way_still_returns_what_it_read() {
    let first = json!({
        "changes": [{"seq": 11, "repo": REPO, "kind": "issue", "number": 3, "at": "2026-10-13T11:59:00Z"}],
        "cursor": "", "more": true, "repos": { REPO: rc(11, 12, false) }
    })
    .to_string();
    let (addr, _) = serve(vec![(200, first), (500, "{}".into())]);
    let a = Active::for_test(&format!("http://{addr}"), BOUND);
    let http = client::build_http().unwrap();
    let mut cursors = Cursors::new(&a.base_url);
    cursors.repos.insert(REPO.into(), 10);
    let (nudge, result) = wake::pass(&http, &a, &[REPO.to_string()], &mut cursors).await;
    assert_eq!(result, Err(MirrorError::Status(500)));
    assert_eq!(cursors.repos[REPO], 11, "the cursor moved past the change read");
    assert!(nudge.prompts(), "so that change must still be published");
}

#[test]
fn the_mirror_reads_aws_profile_like_the_gateway() {
    let (c, w) = parse(&cfg(
        json!({"origin": "events.example.com", "keyRef": "ssm:///k", "awsProfile": "relay-reader"}),
    ));
    assert!(w.is_empty());
    assert_eq!(c.unwrap().aws_profile, crate::aws_ssm::AwsProfile::Named("relay-reader".into()));
    let (c, w) = parse(&cfg(
        json!({"origin": "events.example.com", "keyRef": "ssm:///k", "awsProfile": "-x"}),
    ));
    assert_eq!(c.unwrap().aws_profile, crate::aws_ssm::AwsProfile::Invalid);
    assert!(w[0].contains("forge.mirror.awsProfile"), "{w:?}");
    // The mirror's refusals name the mirror.
    let (_, w) = parse(&cfg(json!({"origin": "github.com", "keyRef": "file:/k"})));
    assert!(w[0].contains("the mirror, not GitHub"), "{w:?}");
}
