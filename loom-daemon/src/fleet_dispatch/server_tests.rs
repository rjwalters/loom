#![allow(clippy::unwrap_used)]

use super::*;
use crate::discovery::chain::Tier;
use crate::discovery::snapshot::{RepoSnapshot, RepoStatus, Source};
use crate::fleet_dispatch::identity::{StaticIdentity, TailnetNode};
use crate::fleet_dispatch::proto::RefusalReason;
use crate::fleet_dispatch::roster::RosterHost;
use crate::forge_listing::RestIssue;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};

const A: &str = "worker-a";
const B: &str = "worker-b";

/// A hand-driven monotonic clock.
#[derive(Default)]
struct ManualClock(Mutex<Mono>);

impl ManualClock {
    fn set(&self, secs: u64) {
        *self.0.lock().unwrap() = Mono(Duration::from_secs(secs));
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Mono {
        *self.0.lock().unwrap()
    }
}

struct DownIdentity;

impl Identity for DownIdentity {
    async fn whois(&self, _peer: SocketAddr) -> Result<TailnetNode, IdentityError> {
        Err(IdentityError::Unavailable("socket gone".into()))
    }
}

fn roster() -> Roster {
    let host = |node: &str, fleet_mode: bool| RosterHost {
        tailnet_node: Some(node.to_string()),
        fleet_mode,
    };
    Roster::new(BTreeMap::from([
        (A.to_string(), host("node-a", true)),
        (B.to_string(), host("node-b", true)),
    ]))
}

fn node(name: &str) -> TailnetNode {
    TailnetNode {
        name: format!("{name}.example.ts.net."),
        addresses: vec![],
        tags: vec![],
    }
}

/// Loopback callers resolve to `node_name`.
fn identity(node_name: &str) -> StaticIdentity {
    StaticIdentity(HashMap::from([(IpAddr::V4(Ipv4Addr::LOCALHOST), node(node_name))]))
}

fn server<I: Identity>(identity: I, clock: Arc<ManualClock>) -> Arc<Server<I>> {
    Arc::new(Server::new(identity, clock, roster(), Policy::default(), "epoch-test".into()))
}

/// A server past its grace window.
fn open_server(node_name: &str) -> Arc<Server<StaticIdentity>> {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity(node_name), Arc::clone(&clock));
    clock.set(91);
    s
}

async fn listen<I: Identity>(server: Arc<Server<I>>) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(run(listener, server, Options::default()));
    addr
}

async fn raw(addr: SocketAddr, request: String) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

async fn post(addr: SocketAddr, path: &str, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    raw(
        addr,
        format!(
            "POST {path} HTTP/1.1\r\nHost: captain\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

fn claim_body(host: &str, item: u64, step: &str) -> Value {
    json!({"host_id": host, "repo": "acme/w", "item": item, "step": step})
}

fn hb_body(host: &str, live: Value, free: u32) -> Value {
    json!({"host_id": host, "live": live, "free_slots": free,
           "roles_ok": ["builder", "judge", "doctor", "curator", "champion"]})
}

#[tokio::test]
async fn grant_then_held_over_http_with_the_epoch_on_every_answer() {
    let s_a = open_server("node-a");
    let addr_a = listen(Arc::clone(&s_a)).await;
    let (status, hb) = post(addr_a, "/v1/heartbeat", &hb_body(A, json!([]), 4)).await;
    assert_eq!(status, 200);
    assert_eq!(hb, json!({"server_epoch": "epoch-test", "grace": false}));

    let (status, grant) = post(addr_a, "/v1/claim", &claim_body(A, 7, "builder")).await;
    assert_eq!(status, 200);
    assert_eq!(grant["granted"], json!(true));
    assert_eq!(grant["ttl_s"], json!(1200));
    assert_eq!(grant["server_epoch"], json!("epoch-test"));
    let id = grant["assignment_id"].as_str().unwrap().to_string();

    // The same host again: the same assignment.
    let (_, again) = post(addr_a, "/v1/claim", &claim_body(A, 7, "builder")).await;
    assert_eq!(again["assignment_id"], json!(id));

    // B is refused `held` by A (the table answers B as it would over HTTP).
    let (reply, call) =
        s_a.handle(B, "/v1/claim", claim_body(B, 7, "judge").to_string().as_bytes());
    let refusal: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(refusal["granted"], json!(false));
    assert_eq!(refusal["reason"], json!("held"));
    assert_eq!(refusal["held_by"], json!(A));
    assert_eq!((call.result, call.reason.as_deref()), ("refused", Some("held")));

    // Release frees it; a second release is unknown, not an error.
    let release = json!({"host_id": A, "assignment_id": id, "outcome": "verdict"});
    let (status, ack) = post(addr_a, "/v1/release", &release).await;
    assert_eq!((status, ack), (200, json!({"server_epoch": "epoch-test", "known": true})));
    let (_, ack) = post(addr_a, "/v1/release", &release).await;
    assert_eq!(ack["known"], json!(false));
}

#[tokio::test]
async fn a_node_not_in_the_roster_is_refused_403() {
    // A tailnet peer whose node no dispatcher entry names, including one
    // whose node name happens to equal a host id.
    for name in ["laptop", A] {
        let addr = listen(open_server(name)).await;
        let (status, body) = post(addr, "/v1/claim", &claim_body(A, 1, "builder")).await;
        assert_eq!(status, 403, "{name}");
        assert_eq!(body["server_epoch"], json!("epoch-test"));
    }
    // Not a tailnet peer at all.
    let clock = Arc::new(ManualClock::default());
    let addr = listen(server(StaticIdentity::default(), clock)).await;
    let (status, _) = post(addr, "/v1/heartbeat", &hb_body(A, json!([]), 1)).await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn a_body_naming_another_host_is_refused_403() {
    let s = open_server("node-a");
    let addr = listen(Arc::clone(&s)).await;
    for (path, body) in [
        ("/v1/heartbeat", hb_body(B, json!([]), 4)),
        ("/v1/claim", claim_body(B, 1, "builder")),
        (
            "/v1/refuse",
            json!({"host_id": B, "assignment_id": "a-1", "reason": "claim_lost"}),
        ),
        ("/v1/release", json!({"host_id": B, "assignment_id": "a-1", "outcome": "noop"})),
    ] {
        let (status, reply) = post(addr, path, &body).await;
        assert_eq!(status, 403, "{path}");
        assert_eq!(reply["server_epoch"], json!("epoch-test"));
    }
    // Nothing a spoofed heartbeat said reached the table.
    assert!(s.table.lock().unwrap().host(B).is_none());
}

#[tokio::test]
async fn an_unavailable_identity_backend_is_503() {
    let addr = listen(server(DownIdentity, Arc::new(ManualClock::default()))).await;
    let (status, body) = post(addr, "/v1/claim", &claim_body(A, 1, "builder")).await;
    assert_eq!(status, 503);
    assert_eq!(body["server_epoch"], json!("epoch-test"));
}

#[tokio::test]
async fn malformed_requests_get_their_status_and_never_end_the_listener() {
    let addr = listen(open_server("node-a")).await;
    let cases = [
        ("GET /v1/claim HTTP/1.1\r\n\r\n".to_string(), 405),
        ("POST /v1/nope HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_string(), 404),
        ("POST /v1/claim HTTP/1.1\r\n\r\n".to_string(), 411),
        (
            format!("POST /v1/claim HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1),
            413,
        ),
        ("POST /v1/claim HTTP/1.1\r\nContent-Length: 8\r\n\r\nnot json".to_string(), 400),
        ("POST /v1/claim HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_string(), 400),
        ("garbage\r\n\r\n".to_string(), 400),
        ("POST /v1/claim HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_string(), 400),
    ];
    for (request, want) in cases {
        let (status, body) = raw(addr, request.clone()).await;
        assert_eq!(status, want, "{request:?}");
        assert_eq!(body["server_epoch"], json!("epoch-test"));
    }
    // Still serving.
    let (status, _) = post(addr, "/v1/heartbeat", &hb_body(A, json!([]), 1)).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_restart_takes_a_new_epoch_refuses_with_grace_and_rebuilds_from_heartbeats() {
    // The first process grants A an item.
    let first = open_server("node-a");
    let (reply, _) =
        first.handle(A, "/v1/claim", claim_body(A, 7, "builder").to_string().as_bytes());
    let grant: Value = serde_json::from_str(&reply.body).unwrap();
    let id = grant["assignment_id"].as_str().unwrap().to_string();

    // The restarted process.
    let clock = Arc::new(ManualClock::default());
    let s = Arc::new(Server::new(
        identity("node-a"),
        Arc::clone(&clock) as Arc<dyn Clock>,
        roster(),
        Policy::default(),
        crate::fleet_dispatch::new_epoch(),
    ));
    assert_ne!(s.epoch(), first.epoch());
    clock.set(5);
    let (reply, call) =
        s.handle(B, "/v1/claim", claim_body(B, 7, "builder").to_string().as_bytes());
    let refusal: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(refusal["reason"], json!(RefusalReason::Grace.as_str()));
    assert_eq!(refusal["retry_after_s"], json!(85));
    assert_eq!(refusal["server_epoch"], json!(s.epoch()));
    assert_eq!(call.reason.as_deref(), Some("grace"));

    // A reports its live set, with the old grant's id; the heartbeat says grace.
    let live = json!([{"repo": "acme/w", "item": 7, "step": "builder", "assignment_id": id}]);
    let (reply, _) = s.handle(A, "/v1/heartbeat", hb_body(A, live, 3).to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["grace"], json!(true));
    // B reports too: every fleet-mode host has, so the window closes early.
    let (reply, _) = s.handle(B, "/v1/heartbeat", hb_body(B, json!([]), 2).to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["grace"], json!(false));

    // The rebuilt row refuses B, and A's old id still releases it.
    let (reply, _) = s.handle(B, "/v1/claim", claim_body(B, 7, "builder").to_string().as_bytes());
    let refusal: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(
        (refusal["reason"].clone(), refusal["held_by"].clone()),
        (json!("held"), json!(A))
    );
    let release = json!({"host_id": A, "assignment_id": id, "outcome": "verdict"});
    let (reply, _) = s.handle(A, "/v1/release", release.to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], json!(true));
}

#[tokio::test]
async fn the_captain_never_refuses_cap() {
    // B reports 1 free slot; the captain still grants every claim it makes,
    // because the worker bounds its own claims (#12864).
    let s = open_server("node-b");
    s.handle(B, "/v1/heartbeat", hb_body(B, json!([]), 1).to_string().as_bytes());
    for item in 1..=5 {
        let (reply, call) =
            s.handle(B, "/v1/claim", claim_body(B, item, "builder").to_string().as_bytes());
        let body = serde_json::from_str::<Value>(&reply.body).unwrap();
        assert_eq!(body["granted"], json!(true), "item {item}: {body}");
        assert_ne!(call.reason.as_deref(), Some("cap"));
    }
}

#[tokio::test]
async fn claim_lost_holds_the_item_and_is_counted_once_flushed() {
    let s = open_server("node-a");
    let (reply, _) = s.handle(A, "/v1/claim", claim_body(A, 9, "judge").to_string().as_bytes());
    let id = serde_json::from_str::<Value>(&reply.body).unwrap()["assignment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let refuse = json!({"host_id": A, "assignment_id": id, "reason": "claim_lost"});
    let (reply, call) = s.handle(A, "/v1/refuse", refuse.to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], json!(true));
    assert_eq!(call.item, Some(("acme/w".to_string(), 9, "judge".to_string())));
    let (reply, _) = s.handle(B, "/v1/claim", claim_body(B, 9, "judge").to_string().as_bytes());
    let refusal: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(refusal["reason"], json!("held"));
    assert!(refusal.get("held_by").is_none(), "the owner is outside fleet mode");
    let c = s.counters.lock().unwrap().clone();
    assert_eq!(c.claim_lost_after_grant, 1);
    assert_eq!(c.refusals.get(&("refuse", "claim_lost".to_string())), Some(&1));
    assert_eq!(c.refusals.get(&("claim", "held".to_string())), Some(&1));
}

#[tokio::test]
async fn a_reload_replaces_the_roster() {
    let s = open_server("node-c");
    let addr = listen(Arc::clone(&s)).await;
    assert_eq!(
        post(addr, "/v1/heartbeat", &hb_body("worker-c", json!([]), 1))
            .await
            .0,
        403
    );
    let mut hosts = BTreeMap::new();
    hosts.insert(
        "worker-c".to_string(),
        RosterHost {
            tailnet_node: Some("node-c".into()),
            fleet_mode: false,
        },
    );
    s.set_roster(Roster::new(hosts));
    assert_eq!(
        post(addr, "/v1/heartbeat", &hb_body("worker-c", json!([]), 1))
            .await
            .0,
        200
    );
}

/// #12541: a release that did not move the item holds its `(repo, item,
/// step)` fleet-wide; the next host's claim is refused `held`, counted and
/// span-reasoned as the no-progress cooldown, until the hold ends.
#[tokio::test]
async fn a_no_progress_release_refuses_every_hosts_next_claim_until_the_cooldown_ends() {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    clock.set(100);
    let (reply, _) =
        s.handle(A, "/v1/claim", claim_body(A, 9, "champion_merge").to_string().as_bytes());
    let id = serde_json::from_str::<Value>(&reply.body).unwrap()["assignment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let release = json!({"host_id": A, "assignment_id": id, "outcome": "noop"});
    let (reply, call) = s.handle(A, "/v1/release", release.to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], json!(true));
    assert_eq!(call.item, Some(("acme/w".to_string(), 9, "champion_merge".to_string())));

    for host in [B, A] {
        let (reply, call) = s.handle(
            host,
            "/v1/claim",
            claim_body(host, 9, "champion_merge").to_string().as_bytes(),
        );
        let refusal: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(refusal["granted"], json!(false), "{host}");
        assert_eq!(refusal["reason"], json!("cooldown"), "a hold is not a live claim (`held`)");
        assert_eq!(refusal["retry_after_s"], json!(1800));
        assert_eq!(call.reason.as_deref(), Some(NO_PROGRESS_REASON));
        assert_eq!(call.retry_after_s, Some(1800));
        let hold = call.hold.expect("a hold refusal names its hold");
        assert_eq!((hold.rung, hold.set_by_outcome.as_str()), (1, "noop"));
        assert!(!hold.set_by_assignment_id.is_empty());
    }
    let c = s.counters.lock().unwrap().clone();
    assert_eq!(c.refusals.get(&("claim", NO_PROGRESS_REASON.to_string())), Some(&2));
    assert_eq!(c.refusals.get(&("claim", "held".to_string())), None, "counted once");

    clock.set(100 + 1800);
    let (reply, _) =
        s.handle(B, "/v1/claim", claim_body(B, 9, "champion_merge").to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["granted"], json!(true));
}

// Queue candidates are discovery fixtures only; every refusal still goes through
// the server's authenticated-host/body check and the real assignment table.
fn refusal_queue_snapshot(updated_at: &str) -> RepoSnapshot {
    RepoSnapshot {
        slug: "acme/w".to_string(),
        source: Source::Worklist,
        tier: Tier::Mirror,
        status: RepoStatus::Ok,
        rows: vec![RestIssue {
            number: 7,
            title: None,
            labels: vec!["loom:issue".to_string()],
            created_at: Some("2026-10-19T10:00:00Z".to_string()),
            updated_at: Some(updated_at.to_string()),
            closed_at: None,
            state: "open".to_string(),
            body: None,
            author: None,
            author_association: None,
            is_pull_request: false,
            comments: 0,
            open_sub_issues: None,
            labels_at: None,
            draft: false,
        }],
        pulls: None,
        extras: BTreeMap::new(),
        watermark: None,
    }
}

fn queue_assignment(s: &Server<StaticIdentity>) -> String {
    let (reply, _) = s.handle(A, "/v1/claim", claim_body(A, 7, "builder").to_string().as_bytes());
    let grant: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(grant["granted"], json!(true));
    grant["assignment_id"].as_str().unwrap().to_string()
}

fn refuse_assignment(s: &Server<StaticIdentity>, host: &str, id: &str, reason: &str) -> bool {
    let request = json!({"host_id": host, "assignment_id": id, "reason": reason});
    let (reply, _) = s.handle(host, "/v1/refuse", request.to_string().as_bytes());
    assert_eq!(reply.status, 200);
    serde_json::from_str::<Value>(&reply.body).unwrap()["known"]
        .as_bool()
        .unwrap()
}

#[test]
fn unknown_refusal_preserves_queue_and_same_revision_refresh() {
    for foreign in [true, false] {
        for reason in ["stale", "claim_lost"] {
            let s = open_server("node-a");
            let snapshot = refusal_queue_snapshot("2026-10-19T10:00:00Z");
            s.queue().refresh_repos([&snapshot]);
            let id = queue_assignment(&s);
            let before = s.queue().repo_entries("acme/w").to_vec();
            assert_eq!(before.len(), 1);
            let refused_id = if foreign {
                id.as_str()
            } else {
                "nonexistent-assignment"
            };
            assert!(!refuse_assignment(&s, B, refused_id, reason));
            assert_eq!(s.queue().repo_entries("acme/w"), before);
            assert_eq!(s.queue().tombstones(), 0);
            s.queue().refresh_repos([&snapshot]);
            assert_eq!(s.queue().repo_entries("acme/w"), before);
            assert_eq!(s.queue().tombstones(), 0);
            // A still owns the exact assignment; B's refusal changed neither half.
            assert_eq!(queue_assignment(&s), id);
        }
    }
}

#[test]
fn already_held_refusal_preserves_refreshed_queue_candidate() {
    for host in [A, B] {
        for reason in ["stale", "claim_lost"] {
            let s = open_server("node-a");
            let old = refusal_queue_snapshot("2026-10-19T10:00:00Z");
            s.queue().refresh_repos([&old]);
            let id = queue_assignment(&s);
            assert!(refuse_assignment(&s, A, &id, "claim_lost"));
            assert_eq!((s.queue_len(), s.queue().tombstones()), (0, 1));
            let newer = refusal_queue_snapshot("2026-10-19T11:00:00Z");
            s.queue().refresh_repos([&newer]);
            let before = s.queue().repo_entries("acme/w").to_vec();
            assert_eq!(before.len(), 1);
            assert_eq!(s.queue().tombstones(), 0);
            assert!(!refuse_assignment(&s, host, &id, reason));
            assert_eq!(s.queue().repo_entries("acme/w"), before);
            assert_eq!(s.queue().tombstones(), 0);
            s.queue().refresh_repos([&newer]);
            assert_eq!(s.queue().repo_entries("acme/w"), before);
            assert_eq!(s.queue().tombstones(), 0);
        }
    }
}

#[test]
fn accepted_owner_refusal_tombstones_only_the_current_revision() {
    for reason in ["stale", "claim_lost"] {
        let s = open_server("node-a");
        let old = refusal_queue_snapshot("2026-10-19T10:00:00Z");
        s.queue().refresh_repos([&old]);
        let id = queue_assignment(&s);
        assert!(refuse_assignment(&s, A, &id, reason));
        assert_eq!((s.queue_len(), s.queue().tombstones()), (0, 1));
        s.queue().refresh_repos([&old]);
        assert_eq!((s.queue_len(), s.queue().tombstones()), (0, 1));
        let newer = refusal_queue_snapshot("2026-10-19T11:00:00Z");
        s.queue().refresh_repos([&newer]);
        assert_eq!((s.queue_len(), s.queue().tombstones()), (1, 0));
    }
}

/// #12873: a sweep's points carry the table's gauges beside the counter
/// deltas, from a heartbeat with a step start; a roster host that never
/// heartbeated has no point at all.
#[tokio::test]
async fn a_sweep_emits_the_tables_gauges_with_the_counters() {
    use crate::telemetry::ops::{MetricName, MetricValue};
    let s = open_server("node-a");
    let live = json!([{"repo": "acme/w", "item": 3, "step": "judge",
                       "started_at": "2026-10-20T11:00:00Z"}]);
    s.handle(A, "/v1/heartbeat", hb_body(A, live, 2).to_string().as_bytes());
    let now: chrono::DateTime<chrono::Utc> = "2026-10-20T12:00:00Z".parse().unwrap();
    let points = s.sweep_points(now);
    let find = |name: MetricName, host: Option<&str>, state: Option<&str>| {
        points
            .iter()
            .find(|p| {
                p.name == name
                    && p.labels.get("host").map(String::as_str) == host
                    && p.labels.get("state").map(String::as_str) == state
            })
            .map(|p| match p.value {
                MetricValue::Int(v) => v,
                MetricValue::Double(_) => panic!("int gauges"),
            })
    };
    assert_eq!(find(MetricName::FleetDispatchRows, Some(A), Some("live")), Some(1));
    assert_eq!(find(MetricName::FleetDispatchRows, Some(A), Some("granted")), Some(0));
    assert_eq!(find(MetricName::FleetDispatchRows, None, Some("held")), Some(0));
    assert_eq!(find(MetricName::FleetDispatchFreeSlotsReported, Some(A), None), Some(2));
    assert_eq!(find(MetricName::FleetDispatchLiveReported, Some(A), None), Some(1));
    assert_eq!(find(MetricName::FleetDispatchOldestLiveAgeSec, Some(A), None), Some(3600));
    assert!(
        !points
            .iter()
            .any(|p| p.labels.get("host").map(String::as_str) == Some(B)),
        "a host never heard from has no point, never 0"
    );
}

// ---------------------------------------------------------------- the queue feed's startup path

#[test]
fn the_queue_refresh_setting_is_positive_whole_seconds_or_the_default() {
    let secs = |v: Value| queue_refresh_in(&v);
    let default = Duration::from_secs(DEFAULT_REFRESH_SECS);
    assert_eq!(secs(json!({})), (default, None));
    assert_eq!(
        secs(json!({"fleetDispatch": {"queueRefreshSecs": 120}})).0,
        Duration::from_secs(120)
    );
    for bad in [json!(0), json!("soon"), json!(1.5), json!(-3), json!(null)] {
        let (got, warning) = secs(json!({"fleetDispatch": {"queueRefreshSecs": bad}}));
        assert_eq!(got, default, "{bad}");
        if !bad.is_null() {
            assert!(warning.is_some_and(|w| w.contains(QUEUE_REFRESH_KEY)), "{bad}");
        }
    }
}

fn mirror_config() -> Value {
    json!({"forge": {"mirror": {"origin": "mirror.example.com", "keyRef": "file:/k"}}})
}

#[tokio::test]
async fn startup_applies_the_refresh_period_and_stays_off_without_a_usable_mirror() {
    let key_ok = |_: &crate::key_ref::KeyRef, _: &crate::aws_ssm::AwsAccess<'_>| {
        Ok("2ge_test_key_0123456789abcdef".to_string())
    };
    let key_bad = |_: &crate::key_ref::KeyRef, _: &crate::aws_ssm::AwsAccess<'_>| {
        Err("no such key".to_string())
    };
    let due_after = |s: &Server<StaticIdentity>, clock: &ManualClock, at: u64| {
        s.queue().not_modified(Mono(Duration::from_secs(0)));
        clock.set(at);
        s.queue_refresh_due()
    };
    let cases: [(Value, bool, &crate::forge_mirror::KeyReader<'_>); 3] = [
        (json!({"fleetDispatch": {"queueRefreshSecs": 120}}), false, &key_ok),
        (merge(mirror_config(), 120), false, &key_bad),
        (merge(mirror_config(), 120), true, &key_ok),
    ];
    for (effective, feed_runs, read_key) in cases {
        let clock = Arc::new(ManualClock::default());
        let s = server(identity(A), Arc::clone(&clock));
        let handle = start_queue_feed(&s, &effective, read_key);
        assert_eq!(handle.is_some(), feed_runs);
        if let Some(h) = handle {
            h.abort();
        }
        assert!(!due_after(&s, &clock, 119), "not due before the configured period");
        assert!(due_after(&s, &clock, 120), "due at the configured period");
        assert_eq!(s.queue_len(), 0, "the queue starts empty");
    }
}

fn merge(mut base: Value, refresh: u64) -> Value {
    base["fleetDispatch"] = json!({"queueRefreshSecs": refresh});
    base
}

/// A loopback mirror serving a worklist (ETag-conditional) and a change stream.
#[derive(Default)]
struct MirrorState {
    version: u32,
    worklist: Value,
    changes: Value,
    /// `(path, status)` per request.
    log: Vec<(String, u16)>,
}

type Mirror = Arc<std::sync::Mutex<MirrorState>>;

fn fake_mirror(state: Mirror) -> SocketAddr {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut b = [0u8; 1];
            while buf.len() < 16_384 && !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.push(b[0]),
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split(' ').nth(1))
                .unwrap_or_default()
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            let none_match = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                .map(|l| l[14..].trim().to_string());
            let mut st = state.lock().unwrap();
            let etag = format!("\"v{}\"", st.version);
            let (status, body) = if path == "/v1/mirror/worklist" {
                if none_match.as_deref() == Some(etag.as_str()) {
                    (304, String::new())
                } else {
                    (200, st.worklist.to_string())
                }
            } else if path == "/v1/changes" {
                (200, st.changes.to_string())
            } else {
                (404, "{}".to_string())
            };
            st.log.push((path, status));
            drop(st);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nETag: {etag}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    addr
}

fn wl_repo(slug: &str, numbers: &[u64]) -> Value {
    let items: Vec<Value> = numbers
        .iter()
        .map(|n| {
            json!({"number": n, "type": "issue", "state": "open", "labels": ["loom:issue"],
                   "created_at": "2026-10-19T10:00:00Z", "updated_at": "2026-10-19T10:00:00Z"})
        })
        .collect();
    json!({"watermark": {"known": true, "repo": slug, "latest_seq": 10,
                         "last_reconciled_at": "2026-10-20T00:00:00Z"}, "items": items})
}

fn worklist(w: &[u64], x: &[u64]) -> Value {
    json!({"repos": {"acme/w": wl_repo("acme/w", w), "acme/x": wl_repo("acme/x", x)}})
}

fn stream_page(changes: Value, w: (u64, bool)) -> Value {
    let rc = |slug: &str, latest: u64, truncated: bool| {
        json!({"next_after": latest, "more": false, "truncated": truncated,
               "watermark": {"repo": slug, "known": true, "latest_seq": latest}})
    };
    json!({"changes": changes, "cursor": "", "more": false,
           "repos": {"acme/w": rc("acme/w", w.0, w.1), "acme/x": rc("acme/x", 10, false)}})
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feed_follows_changes_refreshes_in_full_on_period_and_recovers_a_lossy_stream() {
    let state: Mirror = Arc::new(std::sync::Mutex::new(MirrorState {
        version: 1,
        worklist: worklist(&[7], &[8]),
        changes: stream_page(json!([]), (10, false)),
        log: Vec::new(),
    }));
    let addr = fake_mirror(Arc::clone(&state));
    let clock = Arc::new(ManualClock::default());
    let s = server(identity(A), Arc::clone(&clock));
    s.set_queue_refresh(Duration::from_secs(120));
    let active = crate::forge_mirror::Active::with_key(
        format!("http://{addr}"),
        "2ge_captain_key_0123456789abcdef".to_string(),
        Duration::from_secs(600),
        Duration::from_secs(1),
    );
    let feed = spawn_queue_feed(
        Arc::clone(&s),
        active,
        crate::forge_mirror::client::build_http().unwrap(),
        format!("http://{addr}"),
    );
    let worklist_calls = |status: Option<u16>| {
        let st = state.lock().unwrap();
        st.log
            .iter()
            .filter(|(p, c)| p == "/v1/mirror/worklist" && status.is_none_or(|s| s == *c))
            .count()
    };

    // Restart: the first pass is the full refresh.
    until("the first full refresh", || s.queue_len() == 2).await;

    // A change names only acme/w: only acme/w is re-read, though the worklist
    // also grew acme/x. The change-driven read must not eat the full read's 200.
    {
        let mut st = state.lock().unwrap();
        st.version = 2;
        st.worklist = worklist(&[7, 9], &[8, 10]);
        st.changes = stream_page(
            json!([{"seq": 11, "repo": "acme/w", "kind": "issue", "number": 9,
                    "at": "2026-10-20T00:00:00Z"}]),
            (11, false),
        );
    }
    until("the change-driven refresh", || s.queue_len() == 3).await;
    state.lock().unwrap().changes = stream_page(json!([]), (11, false));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(s.queue_len(), 3, "a quiet stream re-reads nothing");

    // The period elapses: the full refresh picks up what the stream never named.
    clock.set(200);
    until("the full-refresh backstop", || s.queue_len() == 4).await;

    // The next period finds the worklist unchanged: a 304 only advances the age.
    clock.set(400);
    until("the conditional full read", || worklist_calls(Some(304)) >= 1).await;
    assert_eq!(s.queue_len(), 4);
    assert!(!s.queue_refresh_due(), "a 304 advances the age");

    // A truncated cursor means changes were missed: the next full refresh
    // recovers them with no period elapsing.
    {
        let mut st = state.lock().unwrap();
        st.version = 3;
        st.worklist = worklist(&[7], &[8, 10]);
        st.changes = stream_page(json!([]), (5, true));
    }
    until("the lossy-stream recovery", || s.queue_len() == 3).await;
    feed.abort();
}

// ---------------------------------------------------------------------------
// `next` (#12817)
// ---------------------------------------------------------------------------

/// `slug`'s open `loom:issue` items `numbers` (each a Builder candidate).
fn next_snapshot(slug: &str, numbers: &[u32]) -> RepoSnapshot {
    let mut snap = refusal_queue_snapshot("2026-10-19T10:00:00Z");
    snap.slug = slug.to_string();
    let template = snap.rows[0].clone();
    snap.rows = numbers
        .iter()
        .map(|n| RestIssue {
            number: *n,
            ..template.clone()
        })
        .collect();
    snap
}

fn next_body(host: &str, free: u32, roles: &[&str]) -> Value {
    json!({"host_id": host, "free_slots": free, "roles_ok": roles})
}

/// Ask `next` as `host`; the parsed answer.
fn ask_next(s: &Server<StaticIdentity>, host: &str, free: u32, roles: &[&str]) -> Value {
    let (reply, call) =
        s.handle(host, "/v1/next", next_body(host, free, roles).to_string().as_bytes());
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(call.endpoint, "/v1/next");
    serde_json::from_str(&reply.body).unwrap()
}

fn offered(answer: &Value) -> Vec<(String, u64)> {
    answer["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["repo"].as_str().unwrap().to_string(), a["item"].as_u64().unwrap()))
        .collect()
}

#[test]
fn a_local_seat_refusal_leaves_the_same_pull_request_offerable_to_a_peer() {
    use crate::session_exec::refusal::{REFUSAL_EXIT_CODE, SEAT_BUSY_REASON};
    use crate::step_role::StepRole;
    use crate::sweep_registry::step_spawn::capture::{
        parse_outcome_line, seat_busy_exit_reason, StepEnd,
    };

    let mut snapshot = refusal_queue_snapshot("2026-10-19T10:00:00Z");
    snapshot.rows[0].is_pull_request = true;
    snapshot.rows[0].labels = vec!["loom:review-requested".into()];
    for early in [true, false] {
        let s = open_server("node-a");
        s.queue().refresh_repos([&snapshot]);
        let first = ask_next(&s, A, 1, &["judge"]);
        assert_eq!(offered(&first), [("acme/w".into(), 7)]);
        let first_id = first["assignments"][0]["assignment_id"].as_str().unwrap();
        assert!(offered(&ask_next(&s, B, 1, &["judge"])).is_empty());
        if early {
            assert!(refuse_assignment(&s, A, first_id, SEAT_BUSY_REASON));
            // Counted by name, not folded into `other` (#14512).
            let c = s.counters.lock().unwrap().clone();
            assert_eq!(c.refusals.get(&("refuse", "seat_busy".to_string())), Some(&1));
            assert_eq!(c.refusals.get(&("refuse", "other".to_string())), None);
        } else {
            let line = parse_outcome_line(
                "LOOM_STEP_OUTCOME role=judge target=7 class=refused reason=seat_busy",
            );
            let outcome = seat_busy_exit_reason(
                StepRole::Judge,
                7,
                StepEnd::Exited(Some(REFUSAL_EXIT_CODE)),
                line.as_ref(),
            )
            .unwrap();
            let release = json!({"host_id": A, "assignment_id": first_id, "outcome": outcome});
            let (reply, _) = s.handle(A, "/v1/release", release.to_string().as_bytes());
            assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], true);
        }
        assert_eq!((s.queue_len(), s.queue().tombstones()), (1, 0));
        s.queue().refresh_repos([&snapshot]);
        let second = ask_next(&s, B, 1, &["judge"]);
        assert_eq!(offered(&second), [("acme/w".into(), 7)]);
        assert_ne!(second["assignments"][0]["assignment_id"], first_id);
    }
}

#[test]
fn next_grants_up_to_free_slots_in_rounds_of_one_item_per_repo() {
    let s = open_server("node-a");
    let (w, x) = (next_snapshot("acme/w", &[1, 2, 3]), next_snapshot("acme/x", &[10, 11]));
    s.queue().refresh_repos([&w, &x]);
    let answer = ask_next(&s, A, 3, &["builder"]);
    assert_eq!(answer["server_epoch"], "epoch-test");
    // Round one takes one per repo, round two the next of the first.
    assert_eq!(
        offered(&answer),
        [
            ("acme/w".into(), 1),
            ("acme/x".into(), 10),
            ("acme/w".into(), 2)
        ]
    );
    for a in answer["assignments"].as_array().unwrap() {
        assert_eq!(a["step"], "builder");
        assert!(a["ttl_s"].as_u64().unwrap() > 0);
        assert!(!a["assignment_id"].as_str().unwrap().is_empty());
    }
    // The grants are rows: another host's claim is held, and A's own claim
    // returns the same assignment id.
    let (reply, _) = s.handle(
        B,
        "/v1/claim",
        json!({"host_id": B, "repo": "acme/w",
        "item": 1, "step": "builder"})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["reason"], "held");
    // Nothing offered twice: the rest of the queue comes next.
    let more = ask_next(&s, A, 10, &["builder"]);
    assert_eq!(offered(&more), [("acme/w".into(), 3), ("acme/x".into(), 11)]);
    assert!(offered(&ask_next(&s, A, 10, &["builder"])).is_empty());
}

#[test]
fn next_answers_empty_without_an_error_when_nothing_fits() {
    let s = open_server("node-a");
    // An empty queue.
    let empty = ask_next(&s, A, 4, &["builder"]);
    assert!(offered(&empty).is_empty());
    assert_eq!(empty["server_epoch"], "epoch-test");
    // Queue present, but no slots, or the role is not runnable.
    s.queue().refresh_repos([&next_snapshot("acme/w", &[1])]);
    assert!(offered(&ask_next(&s, A, 0, &["builder"])).is_empty());
    assert!(offered(&ask_next(&s, A, 4, &["judge"])).is_empty());
    assert!(offered(&ask_next(&s, A, 4, &[])).is_empty());
    assert_eq!(s.queue_len(), 1, "an empty answer drops nothing");
    assert_eq!(offered(&ask_next(&s, A, 1, &["builder"])), [("acme/w".into(), 1)]);
}

#[test]
fn next_skips_items_another_host_holds_or_a_hold_covers() {
    let s = open_server("node-a");
    s.queue()
        .refresh_repos([&next_snapshot("acme/w", &[1, 2, 3])]);
    let (reply, _) = s.handle(
        B,
        "/v1/claim",
        json!({"host_id": B, "repo": "acme/w",
        "item": 1, "step": "builder"})
        .to_string()
        .as_bytes(),
    );
    let id = serde_json::from_str::<Value>(&reply.body).unwrap()["assignment_id"]
        .as_str()
        .unwrap()
        .to_string();
    // B's item 1 is skipped; a no-progress release holds item 2 fleet-wide.
    let (reply, _) = s.handle(A, "/v1/claim", claim_body(A, 2, "builder").to_string().as_bytes());
    let id2 = serde_json::from_str::<Value>(&reply.body).unwrap()["assignment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let rel = json!({"host_id": A, "assignment_id": id2, "outcome": "failed"});
    s.handle(A, "/v1/release", rel.to_string().as_bytes());
    assert_eq!(offered(&ask_next(&s, A, 5, &["builder"])), [("acme/w".into(), 3)]);
    // B's own `next` does not re-offer its running item either.
    let _ = id;
    assert!(offered(&ask_next(&s, B, 5, &["builder"])).is_empty());
}

#[test]
fn next_is_empty_during_grace_and_a_restart_forgets_the_queue() {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    s.queue().refresh_repos([&next_snapshot("acme/w", &[1])]);
    clock.set(10);
    assert!(offered(&ask_next(&s, A, 3, &["builder"])).is_empty(), "grace: nothing granted");
    clock.set(91);
    assert_eq!(offered(&ask_next(&s, A, 3, &["builder"])).len(), 1);
    // A restarted captain: a new epoch on the answer, soft state gone.
    let restarted = Arc::new(Server::new(
        identity("node-a"),
        Arc::new(ManualClock::default()),
        roster(),
        Policy::default(),
        "epoch-after".into(),
    ));
    let answer = ask_next(&restarted, A, 3, &["builder"]);
    assert_eq!(answer["server_epoch"], "epoch-after");
    assert!(offered(&answer).is_empty());
    assert!(answer.get("queue_age_s").is_none(), "no full read yet");
}

#[test]
fn next_reports_the_queue_age_once_a_full_read_answered() {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    clock.set(100);
    s.queue().not_modified(clock.now());
    clock.set(145);
    assert_eq!(ask_next(&s, A, 1, &["builder"])["queue_age_s"], 45);
}

#[tokio::test]
async fn next_over_http_rejects_a_body_naming_another_host() {
    let s = open_server("node-a");
    s.queue().refresh_repos([&next_snapshot("acme/w", &[5])]);
    let addr = listen(Arc::clone(&s)).await;
    let (status, body) = post(addr, "/v1/next", &next_body(B, 1, &["builder"])).await;
    assert_eq!((status, body["server_epoch"].as_str()), (403, Some("epoch-test")));
    let (status, body) = post(addr, "/v1/next", &next_body(A, 1, &["builder"])).await;
    assert_eq!(status, 200);
    assert_eq!(offered(&body), [("acme/w".into(), 5)]);
    let (status, _) = post(addr, "/v1/next", &json!({"host_id": A})).await;
    assert_eq!(status, 400);
}

/// Claim `item` for `host` at `clock`, release it with `outcome`; the answer.
fn claim_release(
    s: &Server<StaticIdentity>,
    clock: &ManualClock,
    at: u64,
    host: &str,
    outcome: &str,
) -> Value {
    clock.set(at);
    let hb = hb_body(host, json!([]), 4);
    s.handle(host, "/v1/heartbeat", hb.to_string().as_bytes());
    let (reply, _) =
        s.handle(host, "/v1/claim", claim_body(host, 11, "builder").to_string().as_bytes());
    let id = serde_json::from_str::<Value>(&reply.body).unwrap()["assignment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let rel = json!({"host_id": host, "assignment_id": id, "outcome": outcome});
    let (reply, _) = s.handle(host, "/v1/release", rel.to_string().as_bytes());
    serde_json::from_str(&reply.body).unwrap()
}

#[tokio::test]
async fn the_third_fast_fail_release_answers_park_once_over_the_wire() {
    // The #11930 sequence: one item fast-failing across hosts, each draw past
    // the previous no-progress hold.
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    let first = claim_release(&s, &clock, 100, A, "fast_fail");
    assert!(first.get("park").is_none(), "no order on the first: {first}");
    let second = claim_release(&s, &clock, 2_000, B, "fast_fail");
    assert!(second.get("park").is_none());
    let third = claim_release(&s, &clock, 9_300, A, "fast_fail");
    assert_eq!(third["park"], json!(true));
    assert_eq!(third["known"], json!(true));
    // Refused afterwards, whoever asks.
    clock.set(9_400);
    let (reply, _) = s.handle(B, "/v1/claim", claim_body(B, 11, "builder").to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["reason"], json!("held"));
    // The park ended on a worker (TTL expiry or an operator's clear, from any
    // host): the hold ends with it. Asked as `judge`, since the Builder
    // step keeps its own no-progress hold.
    let judge = claim_body(B, 11, "judge").to_string();
    let (reply, _) = s.handle(B, "/v1/claim", judge.as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["reason"], json!("held"));
    // No `cause` means a TTL expiry: it ends the order, never the hold.
    let ended = json!({"host_id": B, "repo": "acme/w", "item": 11}).to_string();
    let (reply, call) = s.handle(B, "/v1/park_ended", ended.as_bytes());
    assert_eq!(reply.status, 200);
    assert_eq!(call.endpoint, "/v1/park_ended");
    let ack: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(ack["known"], json!(true));
    assert!(ack.get("park").is_none());
    let (reply, _) = s.handle(B, "/v1/claim", judge.as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["granted"], json!(true));
    // A second notice finds nothing; another host's id in the body is refused.
    let (reply, _) = s.handle(B, "/v1/park_ended", ended.as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], json!(false));
    let (reply, _) = s.handle(A, "/v1/park_ended", ended.as_bytes());
    assert_eq!(reply.status, 403);
}

// ---------------------------------------------------------------- the filing lock (#13918)

fn filing_body(host: &str, holder: &str) -> Value {
    json!({"host_id": host, "holder": holder, "label": "architect"})
}

fn filing_lock(s: &Server<StaticIdentity>, host: &str, holder: &str) -> Value {
    let (reply, call) =
        s.handle(host, "/v1/filing-lock", filing_body(host, holder).to_string().as_bytes());
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(call.endpoint, "/v1/filing-lock");
    serde_json::from_str(&reply.body).unwrap()
}

fn filing_unlock(s: &Server<StaticIdentity>, host: &str, holder: &str) -> bool {
    let body = json!({"host_id": host, "holder": holder}).to_string();
    let (reply, _) = s.handle(host, "/v1/filing-unlock", body.as_bytes());
    assert_eq!(reply.status, 200, "{}", reply.body);
    let v: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(v["server_epoch"], "epoch-test");
    v["known"].as_bool().unwrap()
}

/// Two hosts at once: exactly one is granted, the other is held with a retry
/// hint, and after the first unlocks the second is granted.
#[test]
fn two_hosts_filing_at_once_serialise_through_the_captain() {
    let s = open_server("node-a");
    let a = filing_lock(&s, A, "a-1");
    assert_eq!(a["granted"], true);
    assert_eq!(a["ttl_s"], 60);
    let b = filing_lock(&s, B, "b-1");
    assert_eq!(b["granted"], false);
    assert_eq!(b["held_by"], A);
    assert!(b["retry_after_s"].as_u64().unwrap() >= 1);
    assert!(b.get("ttl_s").is_none());

    assert!(filing_unlock(&s, A, "a-1"));
    assert_eq!(filing_lock(&s, B, "b-1")["granted"], true);
}

#[test]
fn an_unlock_from_a_non_holder_does_not_free_the_slot() {
    let s = open_server("node-a");
    filing_lock(&s, A, "a-1");
    assert!(!filing_unlock(&s, B, "a-1"), "another host naming the holder id");
    assert!(!filing_unlock(&s, A, "stale"), "a stale holder id");
    assert_eq!(filing_lock(&s, B, "b-1")["granted"], false, "still held");
    assert!(filing_unlock(&s, A, "a-1"));
    assert!(!filing_unlock(&s, A, "a-1"), "a second unlock is a no-op");
}

/// A holder that never unlocks loses the slot at its TTL, and a refresh under
/// the same holder id extends it.
#[test]
fn a_holder_that_never_unlocks_expires_and_a_refresh_extends_the_ttl() {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    clock.set(100);
    assert_eq!(filing_lock(&s, A, "a-1")["granted"], true);
    clock.set(150);
    let refreshed = filing_lock(&s, A, "a-1");
    assert_eq!(refreshed["granted"], true, "the same holder is re-granted");
    clock.set(200);
    assert_eq!(filing_lock(&s, B, "b-1")["granted"], false, "refreshed to 210");
    clock.set(210);
    assert_eq!(filing_lock(&s, B, "b-1")["granted"], true, "A never unlocked: expired");
    assert!(!filing_unlock(&s, A, "a-1"), "A's late unlock must not free B's hold");
    assert_eq!(filing_lock(&s, A, "a-2")["granted"], false);
}

/// The sweep expires an abandoned hold without a request arriving.
#[test]
fn the_sweep_expires_an_abandoned_filing_hold() {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    clock.set(100);
    filing_lock(&s, A, "a-1");
    clock.set(161);
    s.sweep_points(chrono::Utc::now());
    s.expire_filing();
    assert_eq!(filing_lock(&s, B, "b-1")["granted"], true);
}

/// A restart empties the slot (soft state): the holder's next refresh is
/// granted again, it does not wedge.
#[test]
fn a_restarted_captain_grants_a_holders_refresh_again() {
    let first = open_server("node-a");
    filing_lock(&first, A, "a-1");
    let second = open_server("node-a");
    assert_eq!(filing_lock(&second, A, "a-1")["granted"], true);
}

/// Identity is the same `whois` as for claims, and the holder id is bounded.
#[tokio::test]
async fn filing_calls_need_a_roster_host_and_a_bounded_holder() {
    let clock = Arc::new(ManualClock::default());
    let addr = listen(server(StaticIdentity::default(), Arc::clone(&clock))).await;
    let (status, body) = post(addr, "/v1/filing-lock", &filing_body(A, "a-1")).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["server_epoch"], "epoch-test");

    let addr = listen(open_server("node-a")).await;
    let (status, body) = post(addr, "/v1/filing-lock", &filing_body(B, "b-1")).await;
    assert_eq!(status, 403, "host_id is not the caller's: {body}");
    let (status, _) = post(addr, "/v1/filing-lock", &filing_body(A, "")).await;
    assert_eq!(status, 400);
    let long = "x".repeat(129);
    let (status, _) = post(addr, "/v1/filing-unlock", &json!({"host_id": A, "holder": long})).await;
    assert_eq!(status, 400);
    let (status, body) = post(addr, "/v1/filing-lock", &filing_body(A, "a-1")).await;
    assert_eq!((status, &body["granted"]), (200, &json!(true)));
}

/// #14047, end to end: the Builder step's no-progress hold survives a TTL
/// expiry (`cause: expired`) and ends on an operator's clear (`cause:
/// cleared`).
#[tokio::test]
async fn an_operator_clear_lifts_the_no_progress_hold_but_a_ttl_expiry_does_not() {
    let clock = Arc::new(ManualClock::default());
    let s = server(identity("node-a"), Arc::clone(&clock));
    claim_release(&s, &clock, 100, A, "fast_fail");
    claim_release(&s, &clock, 2_000, B, "fast_fail");
    claim_release(&s, &clock, 9_300, A, "fast_fail");
    // Past the park order's hold, inside the 6 h no-progress hold.
    clock.set(9_300 + 3_600 + 600);
    let builder = claim_body(B, 11, "builder").to_string();
    let reason = |s: &Server<StaticIdentity>| {
        let (reply, _) = s.handle(B, "/v1/claim", builder.as_bytes());
        serde_json::from_str::<Value>(&reply.body).unwrap()
    };
    assert_eq!(reason(&s)["reason"], json!("cooldown"), "refused before any park-ended call");

    let expired = json!({"host_id": B, "repo": "acme/w", "item": 11, "cause": "expired"});
    let (reply, _) = s.handle(B, "/v1/park_ended", expired.to_string().as_bytes());
    assert_eq!(reply.status, 200);
    assert_eq!(reason(&s)["reason"], json!("cooldown"), "a TTL expiry leaves the hold");

    let cleared = json!({"host_id": B, "repo": "acme/w", "item": 11, "cause": "cleared"});
    let (reply, _) = s.handle(B, "/v1/park_ended", cleared.to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], json!(true));
    assert_eq!(reason(&s)["granted"], json!(true), "granted right after the clear");

    // A clear on an item with nothing to forget: acknowledged, `known` false.
    let (reply, _) = s.handle(B, "/v1/park_ended", cleared.to_string().as_bytes());
    assert_eq!(reply.status, 200);
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["known"], json!(false));
}

// -----------------------------------------------------------------
// #13942: the heartbeat answer publishes the captain's token probe.
// -----------------------------------------------------------------

/// A server with a token-probe pool dir holding one ranking row.
fn probing_server(
    node_name: &str,
    ranking: &str,
) -> (Arc<Server<StaticIdentity>>, tempfile::TempDir) {
    let pool = tempfile::tempdir().unwrap();
    std::fs::write(pool.path().join(".ranking"), ranking).unwrap();
    let clock = Arc::new(ManualClock::default());
    let s = Arc::new(
        Server::new(
            identity(node_name),
            clock.clone(),
            roster(),
            Policy::default(),
            "epoch-test".into(),
        )
        .with_token_probe(pool.path().to_path_buf()),
    );
    clock.set(91);
    (s, pool)
}

#[test]
fn the_heartbeat_answer_carries_the_captains_snapshot() {
    let (s, _pool) = probing_server(
        "node-a",
        "agent-1|available|0.42\nagent-2|exhausted||2026-11-20T03:00:00Z\n",
    );
    let (reply, _) = s.handle(A, "/v1/heartbeat", hb_body(A, json!([]), 1).to_string().as_bytes());
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    let probe = &body["token_probe"];
    assert!(probe.is_object(), "the snapshot rides the answer: {body}");
    assert_eq!(probe["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(probe["accounts"][0]["name"], "agent-1");
    assert_eq!(probe["accounts"][0]["util_5h"], 0.42);
    assert_eq!(probe["accounts"][1]["limit_reset"], "2026-11-20T03:00:00Z");
    // The probe instant is recorded, not sampled: the worker's freshness
    // comparison needs the captain's probe time, not this answer's.
    assert!(probe["probed_at"].is_string());
}

/// A server built without a pool dir omits the field entirely — the exact
/// body an old captain sent.
#[test]
fn a_server_without_a_pool_dir_omits_the_field() {
    let s = open_server("node-a");
    let (reply, _) = s.handle(A, "/v1/heartbeat", hb_body(A, json!([]), 1).to_string().as_bytes());
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert!(body.get("token_probe").is_none(), "absent, not null: {body}");
}

/// The captain-side per-account record is one per distinct probe cycle, not
/// one per heartbeat: a second heartbeat over the same `.ranking` emits
/// nothing, a rewritten `.ranking` emits the new cycle.
#[test]
fn the_per_account_cycle_record_fires_once_per_cycle() {
    let (s, pool) = probing_server("node-a", "agent-1|available|0.42\n");
    let beat = hb_body(A, json!([]), 1).to_string();
    let (_, first) = crate::observability::ops::capture::capture(|| {
        let _ = s.handle(A, "/v1/heartbeat", beat.as_bytes());
    });
    let cycle = |captured: &crate::observability::ops::capture::Captured| {
        captured
            .metrics
            .iter()
            .filter(|p| p.name == crate::telemetry::ops::MetricName::TokensFleetProbeAccounts)
            .count()
    };
    assert_eq!(cycle(&first), 1, "one point per account of the new cycle");

    let (_, again) = crate::observability::ops::capture::capture(|| {
        let _ = s.handle(A, "/v1/heartbeat", beat.as_bytes());
    });
    assert_eq!(cycle(&again), 0, "the same cycle is not re-emitted");

    std::fs::write(pool.path().join(".ranking"), "agent-1|available|0.10\nagent-2|error\n")
        .unwrap();
    let (_, next) = crate::observability::ops::capture::capture(|| {
        let _ = s.handle(A, "/v1/heartbeat", beat.as_bytes());
    });
    assert_eq!(cycle(&next), 2, "a new probe cycle records every account");
}

// -----------------------------------------------------------------
// #13943: `POST /v1/tokens-refresh` — the on-demand fleet probe
// (`tokens refresh --fleet`, part 3 of #13893).
// -----------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::token_ranking_refresh::RefreshOutcome;

/// A probe's `.ranking` rewrite with a pinned mtime, so the probe instant
/// is a fact the test names rather than a race with the filesystem clock.
fn write_ranking_at(dir: &std::path::Path, rows: &str, mtime: SystemTime) {
    let path = dir.join(".ranking");
    std::fs::write(&path, rows).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

/// A captain that can run an on-demand probe over `pool` (#13943): the
/// clock is returned so a test can order arrivals and completions.
fn refreshing_server(
    probe: std::sync::Arc<dyn Fn() -> RefreshOutcome + Send + Sync>,
    ranking: &str,
) -> (Arc<Server<StaticIdentity>>, tempfile::TempDir, Arc<ManualClock>) {
    let pool = tempfile::tempdir().unwrap();
    write_ranking_at(pool.path(), ranking, UNIX_EPOCH);
    let clock = Arc::new(ManualClock::default());
    let s = Arc::new(
        Server::new(
            identity("node-a"),
            clock.clone(),
            roster(),
            Policy::default(),
            "epoch-test".into(),
        )
        .with_token_probe(pool.path().to_path_buf())
        .with_token_refresh(probe),
    );
    clock.set(91);
    (s, pool, clock)
}

/// The probe the request names runs now, the answer carries the probe
/// instant (`probed_at`), and the very next heartbeat publishes that same
/// instant to the fleet (#13943 acceptance: the snapshot workers receive
/// carries the new `probed_at`).
#[tokio::test]
async fn a_tokens_refresh_runs_the_captains_probe_and_answers_its_instant() {
    let runs = Arc::new(AtomicUsize::new(0));
    let pool = tempfile::tempdir().unwrap();
    write_ranking_at(pool.path(), "agent-1|available|0.42\n", UNIX_EPOCH);
    let clock = Arc::new(ManualClock::default());
    let dir = pool.path().to_path_buf();
    let seen_runs = Arc::clone(&runs);
    let probe: std::sync::Arc<dyn Fn() -> RefreshOutcome + Send + Sync> =
        std::sync::Arc::new(move || {
            seen_runs.fetch_add(1, Ordering::SeqCst);
            // What `tokens check --ranking` leaves behind: a fresh ranking
            // with a later mtime.
            write_ranking_at(
                &dir,
                "agent-1|exhausted||2026-11-21T03:00:00Z\n",
                UNIX_EPOCH + Duration::from_secs(1_000_000_500),
            );
            RefreshOutcome::Success
        });
    let s = Arc::new(
        Server::new(identity("node-a"), clock, roster(), Policy::default(), "epoch-test".into())
            .with_token_probe(pool.path().to_path_buf())
            .with_token_refresh(probe),
    );
    let addr = listen(Arc::clone(&s)).await;
    let (status, body) = post(addr, "/v1/tokens-refresh", &json!({"host_id": A})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["coalesced"], json!(false));
    let probed_at = body["probed_at"].as_str().expect("the probe instant");
    let expected = DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_secs(1_000_000_500));
    assert_eq!(
        probed_at.parse::<DateTime<Utc>>().unwrap(),
        expected,
        "the answer names the probe the captain just ran"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one probe ran");

    // The snapshot the fleet receives carries the same new probe instant
    // and the probe's verdicts (AC: workers' snapshot carries new probed_at).
    let (_, hb) = post(addr, "/v1/heartbeat", &hb_body(A, json!([]), 1)).await;
    let probe_json = &hb["token_probe"];
    assert_eq!(probe_json["probed_at"].as_str().unwrap(), probed_at);
    assert_eq!(probe_json["accounts"][0]["status"], json!("exhausted"));
}

/// A request arriving while a probe runs joins that probe: one probe run,
/// both requests answered with its instant, the second marked coalesced
/// (#13943 acceptance: the coalescing rate-limit).
#[tokio::test]
async fn a_request_while_a_probe_runs_joins_it_without_a_second_probe() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (probe_started, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let pool = tempfile::tempdir().unwrap();
    write_ranking_at(pool.path(), "agent-1|available|0.42\n", UNIX_EPOCH);
    let dir = pool.path().to_path_buf();
    let seen_runs = Arc::clone(&runs);
    let probe: std::sync::Arc<dyn Fn() -> RefreshOutcome + Send + Sync> =
        std::sync::Arc::new(move || {
            seen_runs.fetch_add(1, Ordering::SeqCst);
            let _ = probe_started.send(());
            // Hold the probe until the test has a second request arrive.
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            write_ranking_at(
                &dir,
                "agent-1|exhausted||2026-11-21T03:00:00Z\n",
                UNIX_EPOCH + Duration::from_secs(1_000_000_500),
            );
            RefreshOutcome::Success
        });
    let (s, _pool, clock) = refreshing_server(probe, "agent-1|available|0.42\n");
    let body = json!({"host_id": A}).to_string();
    clock.set(100);
    let first = {
        let s = Arc::clone(&s);
        let body = body.clone();
        tokio::spawn(async move { s.tokens_refresh(A, body.as_bytes()).await })
    };
    // Wait for the probe to be running, yielding to the runtime: the
    // current-thread test runtime only polls the spawned task while this
    // main task awaits.
    let mut probe_running = false;
    for _ in 0..500 {
        if started_rx.try_recv().is_ok() {
            probe_running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(probe_running, "the probe is running");
    // The second operator arrives mid-probe; the probe completes after
    // that, so the second answer must be the first probe's.
    clock.set(101);
    let second = {
        let s = Arc::clone(&s);
        let body = body.clone();
        tokio::spawn(async move { s.tokens_refresh(A, body.as_bytes()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    clock.set(102);
    let _ = release_tx.send(());

    let (reply_a, call_a) = first.await.unwrap();
    let (reply_b, call_b) = second.await.unwrap();
    assert_eq!(reply_a.status, 200);
    assert_eq!(reply_b.status, 200);
    let a: Value = serde_json::from_str(&reply_a.body).unwrap();
    let b: Value = serde_json::from_str(&reply_b.body).unwrap();
    assert_eq!(a["coalesced"], json!(false), "the first request ran the probe");
    assert_eq!(b["coalesced"], json!(true), "the second joined it");
    assert_eq!(a["probed_at"], b["probed_at"], "one probe, one instant");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "no second probe was started");
    assert_eq!(call_a.reason.as_deref(), Some("probed"));
    assert_eq!(call_b.reason.as_deref(), Some("coalesced"));
}

/// A captain built without a probe hook answers 503 with a reason — the
/// worker exits non-zero, nothing is probed (#13943).
#[tokio::test]
async fn a_server_without_a_probe_hook_refuses_the_refresh() {
    let (s, _pool) = probing_server("node-a", "agent-1|available|0.42\n");
    let (reply, call) = s
        .tokens_refresh(A, json!({"host_id": A}).to_string().as_bytes())
        .await;
    assert_eq!(reply.status, 503);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert!(body["error"].as_str().unwrap().contains("probe"), "{body}");
    assert_eq!(call.result, "error");
}

/// A probe that ran and failed is an answer carrying the failure, not a
/// protocol error: the worker exits non-zero on the captain's own reason
/// (#13943).
#[tokio::test]
async fn a_failed_probe_is_answered_with_its_reason() {
    let probe: std::sync::Arc<dyn Fn() -> RefreshOutcome + Send + Sync> =
        std::sync::Arc::new(|| RefreshOutcome::Failure("every account errored".into()));
    let (s, _pool, _clock) = refreshing_server(probe, "agent-1|available|0.42\n");
    let (reply, call) = s
        .tokens_refresh(A, json!({"host_id": A}).to_string().as_bytes())
        .await;
    assert_eq!(reply.status, 200, "the request was served");
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["error"], json!("every account errored"));
    assert!(body.get("probed_at").is_none(), "no instant for a failed probe: {body}");
    assert_eq!(call.result, "error", "the span reports the probe's failure");
}

/// A body naming another host is the same 403 every endpoint answers.
#[tokio::test]
async fn a_refresh_naming_another_host_is_refused() {
    let probe: std::sync::Arc<dyn Fn() -> RefreshOutcome + Send + Sync> =
        std::sync::Arc::new(|| RefreshOutcome::Success);
    let (s, _pool, _clock) = refreshing_server(probe, "agent-1|available|0.42\n");
    let (reply, _) = s
        .tokens_refresh(A, json!({"host_id": B}).to_string().as_bytes())
        .await;
    assert_eq!(reply.status, 403);
}
