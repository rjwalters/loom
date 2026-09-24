//! Coverage for credential substitution (issue #8674).
//!
//! The four request-level acceptance criteria are asserted against a **real
//! listener talking to a real (local) upstream**, not a mocked seam: a fake
//! provider records the headers it was handed, so "the swap happened" and "the
//! placeholder never reached the upstream" are the same assertion, made on
//! bytes that crossed a socket.
//!
//! What is deliberately NOT here: a live container. `env` inside a running
//! `docker run` is asserted at the argv level (the placeholder is assigned,
//! the real variable is never forwarded by name), which is the property a
//! reviewer can check without docker, a provider key and a fleet host. The
//! end-to-end run is the issue's own out-of-band acceptance criterion.

use super::*;
use crate::worker_spawn::containment;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serialize env mutation — `enabled` reads process-global env.
pub(super) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- enablement

#[test]
fn off_by_default() {
    let _g = env_lock();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    assert!(!enabled(&json!({})));
    assert!(!enabled(&json!({"runtimes":{"containment":{"native":"ephemeral"}}})));
}

#[test]
fn config_and_env_enable_it_with_env_winning() {
    let _g = env_lock();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    assert!(enabled(&on));
    assert!(enabled(&json!({"runtimes":{"containment":{"credentialProxy":"yes"}}})));
    std::env::set_var("LOOM_NATIVE_CREDENTIAL_PROXY", "0");
    assert!(!enabled(&on), "env must be able to turn a config-on workspace off");
    std::env::set_var("LOOM_NATIVE_CREDENTIAL_PROXY", "1");
    assert!(enabled(&json!({})));
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
}

// ----------------------------------------------------------------- upstreams

#[test]
fn upstream_parsing_rejects_every_way_to_smuggle_a_destination() {
    for bad in [
        "api.anthropic.com",                    // no scheme
        "ftp://api.anthropic.com",              // not http(s)
        "https://user:pw@api.anthropic.com",    // userinfo
        "https://api.anthropic.com?x=evil.com", // query
        "https://api.anthropic.com#evil.com",   // fragment
        "https://",                             // no host
        "https://api.anthropic.com:not-a-port",
    ] {
        assert!(Upstream::parse(bad).is_err(), "should refuse {bad:?}");
    }
    let upstream = Upstream::parse("https://API.Moonshot.ai/v1/").unwrap();
    assert_eq!(upstream.authority(), "api.moonshot.ai");
    assert_eq!(
        upstream.url_for("/chat/completions?stream=true"),
        "https://api.moonshot.ai/v1/chat/completions?stream=true"
    );
}

#[test]
fn host_pinning_allows_only_the_pin_and_the_proxy_itself() {
    let upstream = Upstream::parse("https://api.anthropic.com").unwrap();
    for allowed in [
        "api.anthropic.com",
        "API.ANTHROPIC.COM:443",
        "127.0.0.1:38111",
        "localhost:38111",
        "host.docker.internal:38111",
        "172.17.0.1:38111",
    ] {
        assert!(upstream.pinned(allowed), "should allow {allowed}");
    }
    for refused in [
        "evil.example",
        "api.anthropic.com.evil.example",
        "8.8.8.8:443",
        "api.openai.com",
    ] {
        assert!(!upstream.pinned(refused), "should refuse {refused}");
    }
}

// ------------------------------------------------------------ secret hygiene

#[test]
fn neither_the_placeholder_nor_the_credential_is_renderable() {
    let placeholder = Placeholder::generate();
    assert!(placeholder.as_str().starts_with(PLACEHOLDER_PREFIX));
    assert!(!format!("{placeholder:?}").contains(placeholder.as_str()));
    let record = Record::new(
        "launch-1",
        "anthropic",
        Upstream::parse("https://api.anthropic.com").unwrap(),
        HeaderStyle::AuthorizationBearer,
        "sk-fake-real-credential",
    );
    let rendered = format!("{record:?}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains("sk-fake-real-credential"), "{rendered}");
}

#[test]
fn two_placeholders_are_never_the_same() {
    let a = Placeholder::generate();
    let b = Placeholder::generate();
    assert_ne!(a.as_str(), b.as_str());
    assert!(a.as_str().len() > 60, "placeholder must not be guessable");
}

// ---------------------------------------------------------- registry refusal

fn registry_with(upstream: &str) -> (Registry, Placeholder) {
    let registry = Registry::new();
    let placeholder = Placeholder::generate();
    registry.insert(
        &placeholder,
        Record::new(
            "launch-1",
            "anthropic",
            Upstream::parse(upstream).unwrap(),
            HeaderStyle::AuthorizationBearer,
            "sk-fake-real-credential",
        ),
    );
    (registry, placeholder)
}

#[test]
fn authorization_refuses_everything_that_is_not_a_live_pinned_request() {
    let (registry, placeholder) = registry_with("https://api.anthropic.com");
    let live = vec![placeholder.as_str().to_string()];

    assert_eq!(registry.authorize("POST", &[], None).unwrap_err(), Refusal::MissingCredential);
    assert_eq!(
        registry
            .authorize("POST", &["not-a-real-placeholder".into()], None)
            .unwrap_err(),
        Refusal::UnknownPlaceholder
    );
    assert_eq!(
        registry
            .authorize("CONNECT", &live, Some("api.anthropic.com"))
            .unwrap_err(),
        Refusal::MethodNotAllowed
    );
    assert_eq!(
        registry
            .authorize("POST", &live, Some("evil.example"))
            .unwrap_err(),
        Refusal::HostNotPinned
    );
    assert_eq!(
        registry
            .authorize("POST", &live, Some("api.anthropic.com"))
            .unwrap()
            .launch_id,
        "launch-1"
    );

    // AC4: closing the launch invalidates the placeholder immediately, and the
    // refusal is distinguishable from "never existed".
    registry.close_all();
    assert_eq!(
        registry
            .authorize("POST", &live, Some("api.anthropic.com"))
            .unwrap_err(),
        Refusal::ClosedLaunch
    );
}

#[test]
fn refusal_statuses_match_the_acceptance_criteria() {
    assert_eq!(Refusal::UnknownPlaceholder.status().0, 401);
    assert_eq!(Refusal::ClosedLaunch.status().0, 401);
    assert_eq!(Refusal::MissingCredential.status().0, 401);
    assert_eq!(Refusal::HostNotPinned.status().0, 403);
    assert_eq!(Refusal::MethodNotAllowed.status().0, 405);
}

// -------------------------------------------------------- end-to-end over TCP

/// Headers one fake-upstream request was handed, plus its body.
#[derive(Clone, Debug, Default)]
pub(super) struct Seen {
    pub(super) head: String,
    pub(super) body: String,
}

/// A one-shot fake provider on loopback. Returns `(host:port, seen)`.
pub(super) async fn fake_upstream(reply: &'static str) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len: usize = text
                            .to_ascii_lowercase()
                            .split("content-length:")
                            .nth(1)
                            .and_then(|rest| rest.split("\r\n").next())
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            sink.lock().unwrap().push(Seen {
                                head: text[..i].to_string(),
                                body: text[i + 4..].to_string(),
                            });
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), seen)
}

/// Minimal client: send `request` verbatim, read the whole reply.
pub(super) async fn raw_request(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// Start a proxy on loopback in front of `upstream`, with one live record.
async fn start_proxy(upstream: &str) -> (std::net::SocketAddr, Registry, Placeholder) {
    let (registry, placeholder) = registry_with(upstream);
    let bound = server::Bound::bind(std::net::Ipv4Addr::LOCALHOST.into()).unwrap();
    let addr = bound.addr();
    let listener = bound.into_tokio().unwrap();
    tokio::spawn(server::serve(listener, registry.clone()));
    (addr, registry, placeholder)
}

fn post(placeholder: Option<&str>, host: &str) -> String {
    let auth = placeholder
        .map(|p| format!("authorization: Bearer {p}\r\n"))
        .unwrap_or_default();
    format!(
        "POST /v1/messages HTTP/1.1\r\nhost: {host}\r\n{auth}content-type: application/json\r\ncontent-length: 13\r\n\r\n{{\"model\":\"x\"}}"
    )
}

#[tokio::test]
async fn a_live_placeholder_is_swapped_for_the_real_credential() {
    let (upstream_addr, seen) = fake_upstream("{\"ok\":true}").await;
    let (addr, _registry, placeholder) = start_proxy(&format!("http://{upstream_addr}")).await;

    let response = raw_request(addr, &post(Some(placeholder.as_str()), &addr.to_string())).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("{\"ok\":true}"), "{response}");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "upstream should have seen exactly one request");
    let head = seen[0].head.to_ascii_lowercase();
    assert!(
        head.contains("authorization: bearer sk-fake-real-credential"),
        "the real credential must reach the upstream: {}",
        seen[0].head
    );
    assert!(
        !seen[0].head.contains(placeholder.as_str()),
        "the placeholder must NEVER reach the upstream: {}",
        seen[0].head
    );
    assert!(head.contains("post /v1/messages"), "{}", seen[0].head);
    assert_eq!(seen[0].body, "{\"model\":\"x\"}");
}

#[tokio::test]
async fn an_unknown_placeholder_is_refused_401_and_never_reaches_the_upstream() {
    let (upstream_addr, seen) = fake_upstream("{\"ok\":true}").await;
    let (addr, _registry, _placeholder) = start_proxy(&format!("http://{upstream_addr}")).await;

    let response =
        raw_request(addr, &post(Some("loom-placeholder-bogus"), &addr.to_string())).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(response.contains("unknown_placeholder"), "{response}");
    assert!(seen.lock().unwrap().is_empty(), "nothing may be forwarded");

    // A request with no credential at all is refused the same way.
    let response = raw_request(addr, &post(None, &addr.to_string())).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(response.contains("missing_credential"), "{response}");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_request_naming_another_host_is_refused_403_no_open_relay() {
    let (upstream_addr, seen) = fake_upstream("{\"ok\":true}").await;
    let (addr, _registry, placeholder) = start_proxy(&format!("http://{upstream_addr}")).await;

    // Absolute-form target at a foreign origin — the classic open-relay probe.
    let request = format!(
        "GET http://evil.example/steal HTTP/1.1\r\nhost: evil.example\r\nauthorization: Bearer {}\r\n\r\n",
        placeholder.as_str()
    );
    let response = raw_request(addr, &request).await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("host_not_pinned"), "{response}");

    // Tunnelling is refused outright rather than pinned.
    let request = format!(
        "CONNECT evil.example:443 HTTP/1.1\r\nhost: evil.example:443\r\nauthorization: Bearer {}\r\n\r\n",
        placeholder.as_str()
    );
    let response = raw_request(addr, &request).await;
    assert!(response.starts_with("HTTP/1.1 405"), "{response}");
    assert!(seen.lock().unwrap().is_empty(), "nothing may be forwarded");
}

#[tokio::test]
async fn a_placeholder_is_dead_the_moment_its_launch_closes() {
    let (upstream_addr, seen) = fake_upstream("{\"ok\":true}").await;
    let (addr, registry, placeholder) = start_proxy(&format!("http://{upstream_addr}")).await;

    let response = raw_request(addr, &post(Some(placeholder.as_str()), &addr.to_string())).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    registry.close_all();

    let response = raw_request(addr, &post(Some(placeholder.as_str()), &addr.to_string())).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(response.contains("closed_launch"), "{response}");
    assert_eq!(seen.lock().unwrap().len(), 1, "only the pre-close request forwarded");
}

#[tokio::test]
async fn a_second_credential_header_cannot_ride_along_upstream() {
    let (upstream_addr, seen) = fake_upstream("{\"ok\":true}").await;
    let (addr, _registry, placeholder) = start_proxy(&format!("http://{upstream_addr}")).await;

    let request = format!(
        "POST /v1/messages HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {}\r\nx-api-key: sk-smuggled-by-the-worker\r\ncontent-length: 2\r\n\r\n{{}}",
        placeholder.as_str()
    );
    let response = raw_request(addr, &request).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen[0].head.contains("sk-smuggled-by-the-worker"),
        "a second credential header must be stripped: {}",
        seen[0].head
    );
}

// ----------------------------------------------------- container argv (AC1)

fn containment_profile() -> containment::Profile {
    containment::Profile {
        image: "img:test".to_string(),
        cpus: None,
        memory: Some("512m".to_string()),
        launch_id: "deadbeef".to_string(),
    }
}

fn argv(command: &std::process::Command) -> Vec<String> {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// AC1 at the dispatch level: the docker invocation assigns a placeholder and
/// never forwards the credential's variables by NAME — a by-name `-e VAR` is
/// exactly what would make docker read the REAL value out of this process.
#[test]
fn the_container_gets_the_placeholder_and_never_the_real_variable() {
    let _g = env_lock();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("ZHIPU_API_KEY", "sk-fake-real-credential");
    let placeholder = Placeholder::generate();
    let injection = containment::Injection {
        assignments: vec![
            ("ZAI_API_KEY".into(), placeholder.as_str().to_string()),
            ("ZHIPU_API_KEY".into(), placeholder.as_str().to_string()),
            ("ANTHROPIC_BASE_URL".into(), "http://host.docker.internal:38111".into()),
        ],
        withheld: vec!["ZAI_API_KEY".into(), "ZHIPU_API_KEY".into()],
        mask_dirs: vec![std::path::PathBuf::from("/ws/.loom/api-keys")],
        add_host_gateway: true,
    };
    let command = containment::docker_command(
        &containment_profile(),
        std::path::Path::new("/ws"),
        std::path::Path::new("/ws"),
        None,
        &[],
        &["ZAI_API_KEY", "ZHIPU_API_KEY"],
        Some(&injection),
    )
    .unwrap();
    let argv = argv(&command);
    std::env::remove_var("ZHIPU_API_KEY");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");

    assert!(argv.contains(&format!("ZHIPU_API_KEY={}", placeholder.as_str())), "{argv:?}");
    assert!(argv.contains(&format!("ZAI_API_KEY={}", placeholder.as_str())), "{argv:?}");
    // The bare by-name forms are what leak the real value; neither may appear.
    assert!(!argv.iter().any(|a| a == "ZHIPU_API_KEY"), "{argv:?}");
    assert!(!argv.iter().any(|a| a == "ZAI_API_KEY"), "{argv:?}");
    // And of course the value itself is nowhere in argv.
    assert!(!argv.iter().any(|a| a.contains("sk-fake-real-credential")), "{argv:?}");
    assert!(argv.contains(&"host.docker.internal:host-gateway".to_string()), "{argv:?}");
    assert!(
        argv.iter()
            .any(|a| a == "type=tmpfs,destination=/ws/.loom/api-keys,tmpfs-mode=0555"),
        "a per-repo API-key pool under the mounted workspace must be masked: {argv:?}"
    );
}

/// Without an injection the dispatch is byte-for-byte what #8403 shipped.
#[test]
fn no_injection_leaves_the_contained_dispatch_unchanged() {
    let _g = env_lock();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("ZHIPU_API_KEY", "sk-fake-real-credential");
    let command = containment::docker_command(
        &containment_profile(),
        std::path::Path::new("/ws"),
        std::path::Path::new("/ws"),
        None,
        &[],
        &["ZHIPU_API_KEY"],
        None,
    )
    .unwrap();
    let argv = argv(&command);
    std::env::remove_var("ZHIPU_API_KEY");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
    assert!(argv.iter().any(|a| a == "ZHIPU_API_KEY"), "{argv:?}");
    assert!(!argv.iter().any(|a| a.starts_with("ZHIPU_API_KEY=")), "{argv:?}");
    assert!(!argv.iter().any(|a| a.contains("host-gateway")), "{argv:?}");
    assert!(!argv.iter().any(|a| a.starts_with("type=tmpfs")), "{argv:?}");
}

// ------------------------------------------------------------ prepare() path

fn selection_with(proxy: Option<ProfileProxy>) -> crate::worker_spawn::profiles::Selection {
    crate::worker_spawn::profiles::Selection {
        provider: "anthropic".into(),
        model: "test-model".into(),
        effort: None,
        profile: Some("test-profile".into()),
        credentials: vec![("LOOM_TEST_PROXY_KEY_8674".into(), "ANTHROPIC_AUTH_TOKEN".into())],
        credential_sources: vec!["LOOM_TEST_PROXY_KEY_8674".into()],
        provider_options: None,
        provider_definition: None,
        credential_pool: None,
        credential_proxy: proxy,
    }
}

fn anthropic_proxy() -> ProfileProxy {
    ProfileProxy {
        upstream: "https://api.anthropic.com".into(),
        header: HeaderStyle::AuthorizationBearer,
        base_url_env: vec!["ANTHROPIC_BASE_URL".into()],
    }
}

#[test]
#[serial_test::serial]
fn prepare_is_a_no_op_when_the_feature_is_off_or_the_profile_opts_out() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    assert!(prepare(tmp.path(), &selection_with(Some(anthropic_proxy())), &json!({}))
        .unwrap()
        .is_none());
    assert!(prepare(tmp.path(), &selection_with(None), &on)
        .unwrap()
        .is_none());
}

#[test]
#[serial_test::serial]
fn prepare_fails_closed_when_no_credential_resolves() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    std::env::remove_var("LOOM_TEST_PROXY_KEY_8674");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let error = prepare(tmp.path(), &selection_with(Some(anthropic_proxy())), &on).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error.message.contains("no credential resolved"), "{}", error.message);
}

#[test]
#[serial_test::serial]
fn prepare_binds_a_listener_and_hands_the_container_only_a_placeholder() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    std::env::set_var("LOOM_TEST_PROXY_KEY_8674", "sk-fake-real-credential");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let prepared = prepare(tmp.path(), &selection_with(Some(anthropic_proxy())), &on)
        .unwrap()
        .expect("substitution should apply");
    std::env::remove_var("LOOM_TEST_PROXY_KEY_8674");

    let assignments = &prepared.injection.assignments;
    let key = assignments
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_AUTH_TOKEN")
        .expect("target variable must be assigned");
    assert!(key.1.starts_with(PLACEHOLDER_PREFIX), "{key:?}");
    assert!(
        assignments
            .iter()
            .all(|(_, v)| v != "sk-fake-real-credential"),
        "the real credential must never be an assignment: {assignments:?}"
    );
    let base = assignments
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
        .expect("base URL must be re-pointed at the proxy");
    assert!(base.1.starts_with("http://host.docker.internal:") || base.1.starts_with("http://1"));
    assert!(prepared
        .injection
        .withheld
        .contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
    assert!(prepared
        .injection
        .withheld
        .contains(&"LOOM_TEST_PROXY_KEY_8674".to_string()));
    assert!(prepared
        .injection
        .mask_dirs
        .iter()
        .any(|d| d.ends_with("api-keys")));
    // The marker is log-safe.
    let marker = prepared.dispatch_marker();
    assert!(marker.contains("upstream=https://api.anthropic.com"), "{marker}");
    assert!(!marker.contains("sk-fake-real-credential"), "{marker}");
    assert!(!marker.contains(PLACEHOLDER_PREFIX), "{marker}");
}

#[test]
fn a_multi_variable_profile_cannot_be_proxied() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let mut selection = selection_with(Some(anthropic_proxy()));
    selection
        .credentials
        .push(("LOOM_TEST_OTHER_8674".into(), "OTHER".into()));
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let error = prepare(tmp.path(), &selection, &on).unwrap_err();
    assert!(error.message.contains("exactly one credential variable"), "{}", error.message);
}

/// Regression for the Judge's blocking finding on #8701: an array-form
/// `credentialEnv` with a `credentialTargets` map that only covers ONE of
/// its declared names still produces exactly one `credentials` pair, so the
/// check above alone would let it through — then `credential_sources` (the
/// full declared set) would forward the unmapped variable into the
/// container by name, unproxied and unwithheld, leaking the real host
/// value straight past the substitution this module exists to enforce.
#[test]
fn a_declared_but_unmapped_variable_cannot_be_proxied() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let mut selection = selection_with(Some(anthropic_proxy()));
    // Only one pair is mapped ...
    assert_eq!(selection.credentials.len(), 1);
    // ... but a second variable was declared in `credentialEnv` and never
    // mapped, exactly like `"credentialEnv": ["A", "B"]` with
    // `"credentialTargets": {"opencode": {"A": "A"}}`.
    selection
        .credential_sources
        .push("LOOM_TEST_UNMAPPED_8701".into());
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let error = prepare(tmp.path(), &selection, &on).unwrap_err();
    assert!(
        error.message.contains("additional credentialEnv variables"),
        "{}",
        error.message
    );
}

#[test]
fn a_malformed_credential_proxy_block_is_refused_at_validation() {
    let bad = ProfileProxy {
        upstream: "not-a-url".into(),
        header: HeaderStyle::XApiKey,
        base_url_env: Vec::new(),
    };
    assert!(bad.validate().is_err());
    let bad_env = ProfileProxy {
        upstream: "https://api.anthropic.com".into(),
        header: HeaderStyle::XApiKey,
        base_url_env: vec!["not a var name".into()],
    };
    assert!(bad_env.validate().is_err());
    assert!(anthropic_proxy().validate().is_ok());
}

#[test]
fn the_bundled_proxied_example_profile_parses_and_validates() {
    let bundled = crate::worker_spawn::profiles::bundled();
    let profile = bundled
        .get("example-proxied-anthropic")
        .expect("bundled example must exist");
    let proxy = profile
        .credential_proxy
        .as_ref()
        .expect("example must declare credentialProxy");
    let upstream = proxy.validate().unwrap();
    assert_eq!(upstream.host(), "api.anthropic.com");
    assert_eq!(proxy.base_url_env, vec!["ANTHROPIC_BASE_URL".to_string()]);
}
