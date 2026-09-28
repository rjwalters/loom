//! Coverage for host-side account rotation (issue #8818).
//!
//! Three families, one per acceptance criterion that code can decide:
//!
//! - **AC1** — a real listener, a real (local) upstream that answers 429, the
//!   real in-container client (`request_rotation`) and the real [`HostPool`]
//!   over a temp `.loom/tokens`: the host pool's `.bad_tokens` is updated and
//!   the NEXT proxied request carries the other account's credential behind the
//!   same placeholder.
//! - **AC2** — the request body is a closed schema; anything that tries to name
//!   an account, a credential, an upstream or a model is a 400 and marks
//!   nothing.
//! - **AC3** — every byte the rotation produces that could reach the container
//!   or a log (response bodies, the rotation marker, `Debug` renderings, the
//!   child's environment) is grepped for both credential values.

use super::*;
use crate::worker_spawn::egress_proxy::rotate_client::request_rotation;
use crate::worker_spawn::egress_proxy::tests::raw_request;
use crate::worker_spawn::egress_proxy::{server, HeaderStyle, Placeholder, Record, Upstream};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const OLD: &str = "sk-ant-oat01-OLD-credential-8818";
const NEW: &str = "sk-ant-oat01-NEW-credential-8818";

// ------------------------------------------------------------------ fakes

/// Counts every pool call, so "refused" can be asserted as "nothing marked,
/// nothing selected". The host re-probe answers `verdict` (default: the host
/// confirms the credential dead) and records what it was asked to probe.
struct FakePool {
    marks: Mutex<Vec<(String, String, bool)>>,
    selects: Mutex<u32>,
    next: Mutex<Option<(String, String)>>,
    verdict: Mutex<AuthProbe>,
    probes: Mutex<Vec<(String, String)>>,
}

impl Default for FakePool {
    fn default() -> Self {
        Self {
            marks: Mutex::default(),
            selects: Mutex::default(),
            next: Mutex::default(),
            verdict: Mutex::new(AuthProbe::Dead),
            probes: Mutex::default(),
        }
    }
}

impl FakePool {
    fn offering(name: &str, credential: &str) -> Arc<Self> {
        Self::probing(name, credential, AuthProbe::Dead)
    }

    fn probing(name: &str, credential: &str, verdict: AuthProbe) -> Arc<Self> {
        let pool = Self::default();
        *pool.next.lock().unwrap() = Some((name.into(), credential.into()));
        *pool.verdict.lock().unwrap() = verdict;
        Arc::new(pool)
    }
}

impl AccountPool for FakePool {
    fn mark_bad(&self, account: &str, reason: &str, model_scoped: bool) -> Result<(), String> {
        self.marks
            .lock()
            .unwrap()
            .push((account.into(), reason.into(), model_scoped));
        Ok(())
    }

    fn select(&self) -> Result<Selected, String> {
        *self.selects.lock().unwrap() += 1;
        self.next
            .lock()
            .unwrap()
            .clone()
            .map(|(n, c)| Selected::new(n, c))
            .ok_or_else(|| "empty".to_string())
    }

    fn probe_auth(&self, account: &str, credential: &str) -> AuthProbe {
        self.probes
            .lock()
            .unwrap()
            .push((account.into(), credential.into()));
        *self.verdict.lock().unwrap()
    }
}

fn record(upstream: &str) -> Record {
    Record::new(
        "launch-8818",
        "claude",
        Upstream::parse(upstream).unwrap(),
        HeaderStyle::AuthorizationBearer,
        OLD,
    )
}

/// A registry with one live record on account `alpha`, rotation enabled.
fn armed(pool: Arc<dyn AccountPool>, upstream: &str, max: u32) -> (Registry, Placeholder) {
    let registry = Registry::new();
    let placeholder = Placeholder::generate();
    registry.insert(&placeholder, record(upstream));
    registry.set_account("alpha");
    registry.enable_rotation(pool, max);
    (registry, placeholder)
}

fn ask(
    registry: &Registry,
    placeholder: &Placeholder,
    reason: Reason,
) -> Result<Rotated, ControlRefusal> {
    registry.rotate(
        &[placeholder.as_str().to_string()],
        RotateRequest {
            reason,
            model_scoped: false,
        },
    )
}

fn current_credential(registry: &Registry) -> String {
    registry.lock().values().next().unwrap().credential.clone()
}

// ------------------------------------------------------- AC2: request shape

#[test]
fn the_request_can_only_carry_a_reason_and_a_scope_flag() {
    assert_eq!(
        RotateRequest::parse(br#"{"reason":"usage-limit"}"#).unwrap(),
        RotateRequest {
            reason: Reason::UsageLimit,
            model_scoped: false
        }
    );
    assert!(RotateRequest::parse(br#"{"reason":"auth-dead","modelScoped":true}"#).is_ok());
    for smuggle in [
        r#"{"reason":"usage-limit","account":"victim"}"#,
        r#"{"reason":"usage-limit","credential":"sk-ant-evil"}"#,
        r#"{"reason":"usage-limit","upstream":"https://evil.example"}"#,
        r#"{"reason":"usage-limit","model":"opus"}"#,
        r#"{"reason":"usage-limit","launch":"someone-else"}"#,
        r#"{"reason":"victim-account"}"#,
        r#"{"reason":"exhausted: free text\nvictim 2026-01-01"}"#,
        r#"{"account":"victim"}"#,
        r#"["usage-limit"]"#,
        "",
    ] {
        assert_eq!(
            RotateRequest::parse(smuggle.as_bytes()),
            Err(ControlRefusal::BadRequest),
            "must refuse {smuggle:?}"
        );
    }
    let big = format!(r#"{{"reason":"usage-limit","pad":"{}"}}"#, "x".repeat(MAX_CONTROL_BODY));
    assert_eq!(RotateRequest::parse(big.as_bytes()), Err(ControlRefusal::PayloadTooLarge));
}

#[test]
fn the_mark_is_composed_host_side_and_classifies_like_the_wrappers() {
    use crate::tokens_pool::bad_tokens::is_session_limit_reason;
    let auth = Reason::AuthDead.mark_text("l1").unwrap();
    let usage = Reason::UsageLimit.mark_text("l1").unwrap();
    let window = Reason::SessionWindow.mark_text("l1").unwrap();
    assert!(auth.starts_with("auth-dead: ") && auth.contains("401"), "{auth}");
    assert!(usage.starts_with("exhausted: ") && !is_session_limit_reason(&usage), "{usage}");
    assert!(
        window.starts_with("exhausted: ") && is_session_limit_reason(&window),
        "{window}"
    );
    assert_eq!(Reason::ConcurrentSession.mark_text("l1"), None);
}

// ------------------------------------------- trust boundary (registry level)

#[test]
fn no_mark_without_upstream_evidence_and_auth_dead_needs_a_401() {
    let pool = FakePool::offering("beta", NEW);
    let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 8);

    for reason in [Reason::UsageLimit, Reason::SessionWindow, Reason::AuthDead] {
        assert_eq!(ask(&registry, &placeholder, reason), Err(ControlRefusal::NoUpstreamEvidence));
    }
    // Request-shape answers the container can provoke on a healthy
    // credential are evidence of nothing (#8818 review, blocker 2).
    for status in [400, 403, 404, 405, 413, 422] {
        registry.observe("launch-8818", 0, status);
    }
    for reason in [Reason::UsageLimit, Reason::SessionWindow, Reason::AuthDead] {
        assert_eq!(ask(&registry, &placeholder, reason), Err(ControlRefusal::NoUpstreamEvidence));
    }
    // A 429 is evidence of exhaustion, NOT of a dead credential.
    registry.observe("launch-8818", 0, 429);
    assert_eq!(
        ask(&registry, &placeholder, Reason::AuthDead),
        Err(ControlRefusal::NoUpstreamEvidence)
    );
    assert!(pool.marks.lock().unwrap().is_empty(), "a refused rotation must mark nothing");
    assert_eq!(*pool.selects.lock().unwrap(), 0, "a refused rotation must select nothing");
    assert!(pool.probes.lock().unwrap().is_empty(), "no evidence, no host probe");
    assert_eq!(current_credential(&registry), OLD);

    registry.observe("launch-8818", 0, 401);
    let rotated = ask(&registry, &placeholder, Reason::AuthDead).unwrap();
    assert_eq!(rotated.from, "alpha");
    assert_eq!(rotated.account, "beta");
    let marks = pool.marks.lock().unwrap().clone();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].0, "alpha", "only the launch's OWN current account is ever marked");
    assert!(marks[0].1.starts_with("auth-dead: "), "{}", marks[0].1);
    // The host re-probed the launch's OWN current credential before marking.
    assert_eq!(
        pool.probes.lock().unwrap().clone(),
        vec![("alpha".to_string(), OLD.to_string())]
    );
}

/// #8818 review, blocker 2: the container shapes every request the proxy
/// forwards, so it can make the upstream 401 a HEALTHY credential and then ask
/// for `auth-dead`. The host's own probe disagrees, so nothing permanent is
/// written — the launch just moves on, bounded by the cap.
#[test]
fn an_induced_401_cannot_permanently_mark_a_credential_the_host_probe_finds_alive() {
    for verdict in [AuthProbe::Alive, AuthProbe::Inconclusive] {
        let pool = FakePool::probing("beta", NEW, verdict);
        let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 8);
        registry.observe("launch-8818", 0, 401);
        let rotated = ask(&registry, &placeholder, Reason::AuthDead).unwrap();
        assert!(!rotated.marked, "{verdict:?}: an unconfirmed auth death must not mark");
        assert!(pool.marks.lock().unwrap().is_empty(), "{verdict:?}");
        assert_eq!(pool.probes.lock().unwrap().len(), 1, "{verdict:?}");
        assert_eq!(current_credential(&registry), NEW, "{verdict:?}: the launch still moves on");
    }
}

#[test]
fn exhaustion_and_concurrent_session_rotations_never_probe() {
    let pool = FakePool::offering("beta", NEW);
    let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 8);
    registry.observe("launch-8818", 0, 429);
    assert!(
        ask(&registry, &placeholder, Reason::UsageLimit)
            .unwrap()
            .marked
    );
    ask(&registry, &placeholder, Reason::ConcurrentSession).unwrap();
    assert!(pool.probes.lock().unwrap().is_empty());
}

/// The production classifier over `tokens check`'s real probe: only the
/// probe's own 401 is `Dead`.
#[test]
fn the_host_probe_verdict_is_dead_only_on_the_probes_own_401() {
    use crate::tokens_pool::check::{
        probe_account, ProbeError, ProbeResponse, ProbeTransport, DEFAULT_PROBE_MODEL,
    };
    struct Answer(Result<u16, ()>);
    impl ProbeTransport for Answer {
        fn post(
            &self,
            _: &str,
            _: &[(String, String)],
            _: &str,
            _: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            self.0
                .map(|status| ProbeResponse {
                    status,
                    headers: Vec::new(),
                })
                .map_err(|()| ProbeError::Timeout)
        }
    }
    let verdict = |answer: Result<u16, ()>, token: &str| {
        AuthProbe::from_result(&probe_account(
            "alpha",
            token,
            DEFAULT_PROBE_MODEL,
            "hi",
            1.0,
            &Answer(answer),
        ))
    };
    assert_eq!(verdict(Ok(401), OLD), AuthProbe::Dead);
    assert_eq!(verdict(Ok(200), OLD), AuthProbe::Alive);
    assert_eq!(verdict(Ok(429), OLD), AuthProbe::Alive);
    for answer in [Ok(400), Ok(403), Ok(404), Ok(500), Ok(529), Err(())] {
        assert_eq!(verdict(answer, OLD), AuthProbe::Inconclusive, "{answer:?}");
    }
    // An empty token reports `blocked` without a 401: not evidence.
    assert_eq!(verdict(Ok(401), ""), AuthProbe::Inconclusive);
}

#[test]
fn a_rotation_swaps_in_place_and_resets_evidence_for_the_new_credential() {
    let pool = FakePool::offering("beta", NEW);
    let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 8);
    registry.observe("launch-8818", 0, 429);
    let rotated = ask(&registry, &placeholder, Reason::UsageLimit).unwrap();
    assert!(rotated.marked);
    assert_eq!(current_credential(&registry), NEW);

    // The same placeholder still authorizes, now for the new account.
    let record = registry
        .authorize("POST", &[placeholder.as_str().to_string()], None)
        .unwrap();
    assert_eq!(record.upstream_header().1, format!("Bearer {NEW}"));
    assert_eq!(record.account.as_deref(), Some("beta"));

    // Evidence was about the OLD credential: a second marking rotation needs
    // new evidence, and a late status from an old-generation request is not it.
    assert_eq!(
        ask(&registry, &placeholder, Reason::UsageLimit),
        Err(ControlRefusal::NoUpstreamEvidence)
    );
    registry.observe("launch-8818", 0, 429);
    assert_eq!(
        ask(&registry, &placeholder, Reason::UsageLimit),
        Err(ControlRefusal::NoUpstreamEvidence)
    );
    registry.observe("launch-8818", 1, 429);
    assert!(ask(&registry, &placeholder, Reason::UsageLimit).is_ok());
}

#[test]
fn concurrent_session_swaps_without_marking_or_evidence() {
    let pool = FakePool::offering("beta", NEW);
    let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 8);
    let rotated = ask(&registry, &placeholder, Reason::ConcurrentSession).unwrap();
    assert!(!rotated.marked);
    assert!(pool.marks.lock().unwrap().is_empty());
    assert_eq!(current_credential(&registry), NEW);
}

#[test]
fn model_scope_is_honored_for_exhaustion_and_ignored_for_auth_death() {
    let pool = FakePool::offering("beta", NEW);
    let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 8);
    registry.observe("launch-8818", 0, 429);
    let scoped = RotateRequest {
        reason: Reason::UsageLimit,
        model_scoped: true,
    };
    registry
        .rotate(&[placeholder.as_str().to_string()], scoped)
        .unwrap();
    registry.observe("launch-8818", 1, 401);
    let auth = RotateRequest {
        reason: Reason::AuthDead,
        model_scoped: true,
    };
    registry
        .rotate(&[placeholder.as_str().to_string()], auth)
        .unwrap();
    let marks = pool.marks.lock().unwrap().clone();
    assert!(marks[0].2, "exhaustion mark keeps the scope flag");
    assert!(!marks[1].2, "an auth death is always account-wide");
}

#[test]
fn the_per_launch_cap_bounds_how_far_a_container_can_walk_the_pool() {
    let pool = FakePool::offering("beta", NEW);
    let (registry, placeholder) = armed(pool.clone(), "https://api.anthropic.com", 2);
    for _ in 0..2 {
        ask(&registry, &placeholder, Reason::ConcurrentSession).unwrap();
    }
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::RotationLimit)
    );
    assert_eq!(*pool.selects.lock().unwrap(), 2);
}

#[test]
fn rotation_is_refused_when_not_enabled_unknown_account_closed_or_foreign_placeholder() {
    // Never enabled (the native-harness path).
    let registry = Registry::new();
    let placeholder = Placeholder::generate();
    registry.insert(&placeholder, record("https://api.anthropic.com"));
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::RotationUnavailable)
    );

    // Enabled but the host never learned the account.
    let pool = FakePool::offering("beta", NEW);
    registry.enable_rotation(pool.clone(), 8);
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::RotationUnavailable)
    );

    registry.set_account("alpha");
    assert_eq!(
        ask(&registry, &Placeholder::generate(), Reason::ConcurrentSession),
        Err(ControlRefusal::Auth(Refusal::UnknownPlaceholder))
    );
    registry.close_all();
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::Auth(Refusal::ClosedLaunch))
    );
    assert_eq!(*pool.selects.lock().unwrap(), 0);
}

/// #8699's `pool_account` / `bad_marked` assume a record's credential never
/// changes, so a record carrying that attribution is never rotated.
#[test]
fn a_record_with_api_keys_pool_attribution_is_never_rotated() {
    let pool = FakePool::offering("beta", NEW);
    let registry = Registry::new();
    let placeholder = Placeholder::generate();
    registry.insert(
        &placeholder,
        record("https://api.anthropic.com").with_pool_account(
            std::path::PathBuf::from("/nonexistent"),
            "keyed",
            None,
        ),
    );
    registry.set_account("alpha");
    registry.enable_rotation(pool.clone(), 8);
    assert!(registry.lock().values().next().unwrap().account.is_none());
    // Even if the account were somehow set, rotate refuses.
    registry.lock().values_mut().next().unwrap().account = Some("alpha".into());
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::RotationUnavailable)
    );
    assert_eq!(*pool.selects.lock().unwrap(), 0);
    assert_eq!(current_credential(&registry), OLD);
}

#[test]
fn an_empty_pool_or_a_placeholder_shaped_entry_is_never_swapped_in() {
    let empty = Arc::new(FakePool::default());
    let (registry, placeholder) = armed(empty, "https://api.anthropic.com", 8);
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::PoolExhausted)
    );
    assert_eq!(current_credential(&registry), OLD);

    let nested = FakePool::offering("beta", Placeholder::generate().as_str());
    let (registry, placeholder) = armed(nested, "https://api.anthropic.com", 8);
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::PoolExhausted)
    );

    let bad_name = FakePool::offering("beta'; rm -rf / #", NEW);
    let (registry, placeholder) = armed(bad_name, "https://api.anthropic.com", 8);
    assert_eq!(
        ask(&registry, &placeholder, Reason::ConcurrentSession),
        Err(ControlRefusal::PoolExhausted)
    );
}

// ------------------------------------------------ over the wire (AC1 + AC3)

/// A fake provider that answers each request with the next status in
/// `statuses` (then 200) and records the credential header it was handed.
async fn status_upstream(statuses: Vec<u16>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(statuses)));
    let sink = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            let auth = head
                .lines()
                .find(|l| l.starts_with("authorization:"))
                .unwrap_or("")
                .to_string();
            sink.lock().unwrap().push(auth);
            let status = queue.lock().unwrap().pop_front().unwrap_or(200);
            let body = "{}";
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

fn get(placeholder: &str, addr: std::net::SocketAddr) -> String {
    format!(
        "GET /v1/messages HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {placeholder}\r\nconnection: close\r\n\r\n"
    )
}

fn control(
    placeholder: &str,
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &str,
) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {placeholder}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A temp workspace whose `.loom/tokens` holds `alpha` (OLD) and `beta` (NEW).
fn host_pool() -> tempfile::TempDir {
    let ws = tempfile::tempdir().unwrap();
    let dir = ws.path().join(".loom/tokens");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.token"), OLD).unwrap();
    std::fs::write(dir.join("beta.token"), NEW).unwrap();
    ws
}

async fn serve(registry: &Registry) -> std::net::SocketAddr {
    let bound = server::Bound::bind(std::net::Ipv4Addr::LOCALHOST.into()).unwrap();
    let addr = bound.addr();
    tokio::spawn(server::serve(bound.into_tokio().unwrap(), registry.clone()));
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_account_is_marked_in_the_host_pool_and_the_launch_continues_on_another() {
    let ws = host_pool();
    let (upstream, seen) = status_upstream(vec![429]).await;
    let (registry, placeholder) = armed(
        Arc::new(HostPool {
            workspace: ws.path().to_path_buf(),
            model: None,
        }),
        &upstream,
        8,
    );
    let addr = serve(&registry).await;
    let mut container_saw = Vec::new();

    // 1. The launch hits its limit: the proxy sees the upstream's 429.
    let first = raw_request(addr, &get(placeholder.as_str(), addr)).await;
    assert!(first.starts_with("HTTP/1.1 429"), "{first}");
    container_saw.push(first);

    // 2. The in-container client asks for a rotation — the real client code,
    //    blocking, exactly as the wrapper runs it.
    let base = format!("http://{addr}");
    let ph = placeholder.as_str().to_string();
    let account =
        tokio::task::spawn_blocking(move || request_rotation(&base, &ph, "usage-limit", false))
            .await
            .unwrap()
            .expect("rotation should succeed");
    assert_eq!(account, "beta");

    // 3. The HOST pool now bad-marks alpha, with a host-composed reason.
    let bad = std::fs::read_to_string(ws.path().join(".loom/tokens/.bad_tokens")).unwrap();
    assert!(bad.contains(" alpha exhausted: usage limit"), "{bad}");
    assert!(!bad.contains(OLD) && !bad.contains(NEW), "{bad}");

    // 4. Same placeholder, no restart: the next request goes out as beta.
    let next = raw_request(addr, &get(placeholder.as_str(), addr)).await;
    assert!(next.starts_with("HTTP/1.1 200"), "{next}");
    container_saw.push(next);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], format!("authorization: bearer {}", OLD.to_ascii_lowercase()));
    assert_eq!(seen[1], format!("authorization: bearer {}", NEW.to_ascii_lowercase()));

    // AC3: nothing the container received carries either credential.
    for bytes in &container_saw {
        assert!(!bytes.contains(OLD) && !bytes.contains(NEW), "{bytes}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_response_bodies_markers_and_renderings_never_carry_a_credential() {
    let pool = FakePool::offering("beta", NEW);
    let (upstream, _) = status_upstream(vec![401]).await;
    let (registry, placeholder) = armed(pool, &upstream, 8);
    let addr = serve(&registry).await;

    let mut surfaces = Vec::new();
    // A refusal first — before the upstream has refused anything, so the claim
    // has no evidence behind it — then the upstream's 401, then the successful
    // rotation.
    let refused = raw_request(
        addr,
        &control(
            placeholder.as_str(),
            addr,
            "POST",
            ROTATE_PATH,
            r#"{"reason":"usage-limit","modelScoped":false}"#,
        ),
    )
    .await;
    assert!(refused.starts_with("HTTP/1.1 409"), "{refused}");
    surfaces.push(refused);
    surfaces.push(raw_request(addr, &get(placeholder.as_str(), addr)).await);
    let ok = raw_request(
        addr,
        &control(placeholder.as_str(), addr, "POST", ROTATE_PATH, r#"{"reason":"auth-dead"}"#),
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(ok.contains(r#""account":"beta""#), "{ok}");
    surfaces.push(ok);
    surfaces.push(format!("{:?}", registry.lock().values().collect::<Vec<_>>()));
    surfaces.push(format!("{:?}", Selected::new("beta", NEW)));
    surfaces.push(
        Rotated {
            launch_id: "launch-8818".into(),
            from: "alpha".into(),
            account: "beta".into(),
            marked: true,
        }
        .marker(Reason::AuthDead),
    );
    for surface in &surfaces {
        assert!(!surface.contains(OLD), "old credential leaked: {surface}");
        assert!(!surface.contains(NEW), "new credential leaked: {surface}");
    }
}

/// #8818 review, blocker 2, end to end: the container drives the upstream to
/// refuse a healthy credential with requests it shapes itself, then asks for
/// every marking rotation. Nothing is marked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn container_induced_refusals_never_mark_a_healthy_account() {
    let pool = FakePool::probing("beta", NEW, AuthProbe::Alive);
    let (upstream, _) = status_upstream(vec![400, 404, 403, 401]).await;
    let (registry, placeholder) = armed(pool.clone(), &upstream, 8);
    let addr = serve(&registry).await;
    let ph = placeholder.as_str();
    for _ in 0..4 {
        raw_request(addr, &get(ph, addr)).await;
    }
    for reason in ["usage-limit", "session-window"] {
        let body = format!(r#"{{"reason":"{reason}"}}"#);
        let response = raw_request(addr, &control(ph, addr, "POST", ROTATE_PATH, &body)).await;
        assert!(response.starts_with("HTTP/1.1 409"), "{reason}: {response}");
    }
    let response =
        raw_request(addr, &control(ph, addr, "POST", ROTATE_PATH, r#"{"reason":"auth-dead"}"#))
            .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains(r#""marked":false"#), "{response}");
    assert!(pool.marks.lock().unwrap().is_empty(), "a healthy account must never be marked");
}

#[tokio::test]
async fn the_control_path_is_never_forwarded_upstream() {
    let pool = FakePool::offering("beta", NEW);
    let (upstream, seen) = status_upstream(vec![]).await;
    let (registry, placeholder) = armed(pool.clone(), &upstream, 8);
    let addr = serve(&registry).await;
    let ph = placeholder.as_str();
    let cases = [
        (control(Placeholder::generate().as_str(), addr, "POST", ROTATE_PATH, r#"{"reason":"concurrent-session"}"#), "401"),
        (control(ph, addr, "GET", ROTATE_PATH, ""), "405"),
        (control(ph, addr, "POST", "/.loom-egress-proxy/v1/anything", "{}"), "404"),
        (control(ph, addr, "POST", "/.LOOM-Egress-Proxy/v1/rotate", "{}"), "404"),
        (control(ph, addr, "POST", "/.loom-egress-proxy/v1/rotate?account=victim", "{}"), "404"),
        (control(ph, addr, "POST", ROTATE_PATH, r#"{"reason":"concurrent-session","account":"victim"}"#), "400"),
        (
            format!(
                "POST {ROTATE_PATH} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {ph}\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            "400",
        ),
        (
            format!(
                "POST {ROTATE_PATH} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {ph}\r\ncontent-length: 999999\r\n\r\n"
            ),
            "413",
        ),
        (control(ph, addr, "POST", "http://evil.example/.loom-egress-proxy/v1/rotate", "{}"), "403"),
    ];
    for (request, status) in cases {
        let response = raw_request(addr, &request).await;
        assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{status}: {response}");
    }
    assert!(seen.lock().unwrap().is_empty(), "no control request may reach the upstream");
    assert!(pool.marks.lock().unwrap().is_empty());
    assert_eq!(*pool.selects.lock().unwrap(), 0);
}

// ------------------------------------------------------------- the client

#[test]
fn the_client_never_sends_a_real_credential_or_talks_tls() {
    // A real credential in the variable: refused before any connection (the
    // address is unroutable on purpose — reaching it would hang, not pass).
    let err = request_rotation("http://192.0.2.1:9", OLD, "usage-limit", false).unwrap_err();
    assert_eq!(err.code, 78);
    assert!(!err.message.contains(OLD), "{}", err.message);
    let ph = Placeholder::generate();
    let err = request_rotation("https://api.anthropic.com", ph.as_str(), "usage-limit", false)
        .unwrap_err();
    assert_eq!(err.code, 78);
    assert!(!err.message.contains(ph.as_str()), "{}", err.message);
}

// ------------------------------------------------------ proxy-exec wiring

#[test]
fn proxy_exec_enables_rotation_only_with_a_workspace_and_a_known_account() {
    use crate::worker_spawn::egress_proxy::exec::{build, ExecArgs};
    use std::ffi::OsString;
    let _g = crate::worker_spawn::egress_proxy::tests::env_lock();
    std::env::set_var("LOOM_EGRESS_PROXY_BIND", "127.0.0.1");
    let ws = host_pool();
    let args = |workspace: Option<std::path::PathBuf>| ExecArgs {
        credential_env: "CLAUDE_CODE_OAUTH_TOKEN".into(),
        upstream: "https://api.anthropic.com".into(),
        header: HeaderStyle::AuthorizationBearer,
        base_url_env: vec!["ANTHROPIC_BASE_URL".into()],
        provider: "claude".into(),
        docker_workspace: workspace,
        forward_env: vec!["LOOM_TOKEN_NAME".into()],
        command: ["docker", "run", "img"]
            .iter()
            .map(OsString::from)
            .collect(),
    };
    let env = |pairs: &[(&str, &str)]| -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    };

    let (prepared, command) = build(
        &args(Some(ws.path().to_path_buf())),
        &env(&[
            ("CLAUDE_CODE_OAUTH_TOKEN", OLD),
            ("LOOM_TOKEN_NAME", "alpha"),
        ]),
    )
    .unwrap();
    assert!(prepared.registry.rotation.get().is_some());
    assert_eq!(
        prepared
            .registry
            .lock()
            .values()
            .next()
            .unwrap()
            .account
            .as_deref(),
        Some("alpha")
    );
    // AC3: the child's environment carries neither credential, before or
    // after a rotation (it is fixed at spawn; rotation never touches it).
    for (_, value) in command.get_envs() {
        let value = value
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(!value.contains(OLD) && !value.contains(NEW), "{value}");
    }

    for (workspace, account) in [(None, "alpha"), (Some(ws.path().to_path_buf()), "bad name")] {
        let (prepared, _) = build(
            &args(workspace),
            &env(&[
                ("CLAUDE_CODE_OAUTH_TOKEN", OLD),
                ("LOOM_TOKEN_NAME", account),
            ]),
        )
        .unwrap();
        assert!(prepared.registry.rotation.get().is_none(), "{account}");
    }
    std::env::remove_var("LOOM_EGRESS_PROXY_BIND");
}
