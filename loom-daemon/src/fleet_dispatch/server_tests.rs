#![allow(clippy::unwrap_used)]

use super::*;
use crate::fleet_dispatch::identity::{StaticIdentity, TailnetNode};
use crate::fleet_dispatch::proto::RefusalReason;
use crate::fleet_dispatch::roster::RosterHost;
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
    let host = |node: &str, max: u32, fleet_mode: bool| RosterHost {
        tailnet_node: Some(node.to_string()),
        max_concurrent: Some(max),
        fleet_mode,
    };
    Roster::new(BTreeMap::from([
        (A.to_string(), host("node-a", 40, true)),
        (B.to_string(), host("node-b", 2, true)),
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
async fn the_cap_uses_the_rosters_max_concurrent() {
    // B reports 100 free slots, but its fleet config caps it at 2.
    let s = open_server("node-b");
    s.handle(B, "/v1/heartbeat", hb_body(B, json!([]), 100).to_string().as_bytes());
    for item in [1, 2] {
        let (reply, _) =
            s.handle(B, "/v1/claim", claim_body(B, item, "builder").to_string().as_bytes());
        assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["granted"], json!(true));
    }
    let (reply, _) = s.handle(B, "/v1/claim", claim_body(B, 3, "builder").to_string().as_bytes());
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["reason"], json!("cap"));
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
            max_concurrent: None,
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
