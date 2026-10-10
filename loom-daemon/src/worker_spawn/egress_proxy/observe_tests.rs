//! Coverage for observe mode (issue #11300, slice 1): header fidelity, passive
//! capture of streaming and non-streaming responses, what is (and is not)
//! emitted, and the default-off gate.

use super::*;
use crate::observability::ops::capture::{capture, Captured};
use crate::worker_spawn::egress_proxy::{
    server, HeaderStyle, Placeholder, ProfileProxy, Record, Registry, Upstream,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CREDENTIAL: &str = "sk-fake-real-credential";
const SECRET_TEXT: &str = "PROMPT-SECRET-TEXT-DO-NOT-EXPORT";

/// One scripted upstream response: `(delay before, bytes)` pieces, sent as
/// HTTP chunks (or a single content-length body when `chunked` is false).
struct Script {
    status: u16,
    content_type: &'static str,
    chunked: bool,
    pieces: Vec<(u64, Vec<u8>)>,
}

/// Fake provider on loopback that records each request head, then plays `script`.
async fn upstream(script: Script) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let sink = heads.clone();
    let script = Arc::new(script);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let (sink, script) = (sink.clone(), script.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, len) = loop {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len: usize = text[..i]
                            .to_ascii_lowercase()
                            .split("content-length:")
                            .nth(1)
                            .and_then(|r| r.split("\r\n").next())
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            break (text[..i].to_string(), len);
                        }
                    }
                };
                let _ = len;
                sink.lock().unwrap().push(head);
                let mut out = format!(
                    "HTTP/1.1 {} \r\ncontent-type: {}\r\nconnection: close\r\n",
                    script.status, script.content_type
                );
                if script.chunked {
                    out.push_str("transfer-encoding: chunked\r\n\r\n");
                } else {
                    let total: usize = script.pieces.iter().map(|(_, b)| b.len()).sum();
                    out.push_str(&format!("content-length: {total}\r\n\r\n"));
                }
                let _ = stream.write_all(out.as_bytes()).await;
                for (delay, bytes) in &script.pieces {
                    if *delay > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(*delay)).await;
                    }
                    if script.chunked {
                        let _ = stream
                            .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                            .await;
                        let _ = stream.write_all(bytes).await;
                        let _ = stream.write_all(b"\r\n").await;
                    } else {
                        let _ = stream.write_all(bytes).await;
                    }
                    let _ = stream.flush().await;
                }
                if script.chunked {
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), heads)
}

fn context() -> ObserveContext {
    ObserveContext {
        seat: "seat-a".into(),
        tap: "opencode:zai-flash".into(),
        profile: "zai-flash".into(),
        runtime: "opencode".into(),
        role: Some("builder".into()),
        issue: Some(11300),
        pr: None,
        sweep_id: Some("sweep-issue-11300-1".into()),
    }
}

/// Send `request` through a fresh proxy in front of `upstream_addr` and return
/// the raw reply. `observe` selects whether the record carries a context.
async fn through_proxy(
    upstream_addr: &str,
    observe: bool,
    request: impl Fn(&str, &str) -> String,
) -> String {
    let registry = Registry::new();
    let placeholder = Placeholder::generate();
    let mut record = Record::new(
        "launch-obs",
        "zai",
        Upstream::parse(&format!("http://{upstream_addr}")).unwrap(),
        HeaderStyle::AuthorizationBearer,
        CREDENTIAL,
    );
    if observe {
        record = record.with_observe(context());
    }
    registry.insert(&placeholder, record);
    let bound = server::Bound::bind(std::net::Ipv4Addr::LOCALHOST.into()).unwrap();
    let addr = bound.addr();
    tokio::spawn(server::serve(bound.into_tokio().unwrap(), registry));
    let raw = request(placeholder.as_str(), &addr.to_string());
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

fn chat_request(placeholder: &str, host: &str) -> String {
    let body = "{\"model\":\"glm\"}";
    format!(
        "POST /api/coding/paas/v4/chat/completions HTTP/1.1\r\nhost: {host}\r\n\
         authorization: Bearer {placeholder}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Run `f` on a current-thread runtime inside [`capture`], so tasks the proxy
/// spawns run on the capturing thread.
fn observed<T>(f: impl std::future::Future<Output = T>) -> (T, Captured) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    capture(|| rt.block_on(f))
}

fn sse(events: &[&str]) -> Vec<u8> {
    events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn tokens(captured: &Captured, kind: &str) -> Option<i64> {
    use crate::telemetry::ops::MetricValue;
    captured
        .metrics
        .iter()
        .find(|p| {
            p.name == MetricName::EgressTokens
                && p.labels.get("kind").map(String::as_str) == Some(kind)
        })
        .map(|p| match p.value {
            MetricValue::Int(v) => v,
            MetricValue::Double(v) => v as i64,
        })
}

fn single_span(captured: &Captured) -> &SpanRecord {
    assert_eq!(captured.spans.len(), 1, "{:?}", captured.spans);
    &captured.spans[0]
}

// ------------------------------------------------------------ header fidelity

#[test]
fn upstream_sees_every_client_header_untouched_and_nothing_added() {
    for observe in [false, true] {
        let (_, _) = observed(async move {
            let (addr, heads) = upstream(Script {
                status: 200,
                content_type: "application/json",
                chunked: false,
                pieces: vec![(0, b"{}".to_vec())],
            })
            .await;
            let client_headers = [
                ("user-agent", "opencode/1.4.2 (linux; x64)"),
                ("accept", "text/event-stream"),
                ("accept-language", "*"),
                ("content-type", "application/json"),
                ("x-session-affinity", "ses_abc123"),
                ("x-stainless-lang", "js"),
                ("x-custom-Mixed", "Value With  Spaces"),
            ];
            let reply = through_proxy(&addr, observe, |placeholder, host| {
                let body = "{\"model\":\"glm\"}";
                let mut raw = format!(
                    "POST /api/coding/paas/v4/chat/completions HTTP/1.1\r\nhost: {host}\r\n"
                );
                raw.push_str(&format!("authorization: Bearer {placeholder}\r\n"));
                for (k, v) in client_headers {
                    raw.push_str(&format!("{k}: {v}\r\n"));
                }
                raw.push_str(&format!("content-length: {}\r\n\r\n{body}", body.len()));
                raw
            })
            .await;
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

            let head = heads.lock().unwrap()[0].clone();
            let mut lines = head.split("\r\n");
            assert_eq!(lines.next().unwrap(), "POST /api/coding/paas/v4/chat/completions HTTP/1.1");
            let seen: Vec<(String, String)> = lines
                .map(|l| {
                    let (k, v) = l.split_once(':').unwrap();
                    (k.trim().to_ascii_lowercase(), v.trim().to_string())
                })
                .collect();
            for (k, v) in client_headers {
                let got: Vec<_> = seen
                    .iter()
                    .filter(|(n, _)| *n == k.to_ascii_lowercase())
                    .collect();
                assert_eq!(got.len(), 1, "{k} must arrive exactly once: {seen:?}");
                assert_eq!(got[0].1, v, "{k} must arrive byte-identical");
            }
            let auth = seen.iter().find(|(n, _)| n == "authorization").unwrap();
            assert_eq!(auth.1, format!("Bearer {CREDENTIAL}"), "only the credential is swapped");
            // The only names allowed beyond the client's own: the rewritten
            // host and the recomputed content-length.
            let allowed: Vec<String> = client_headers
                .iter()
                .map(|(k, _)| k.to_ascii_lowercase())
                .chain(["authorization", "host", "content-length"].map(String::from))
                .collect();
            for (name, _) in &seen {
                assert!(allowed.contains(name), "proxy added header {name}: {seen:?}");
            }
            let lowered = head.to_ascii_lowercase();
            assert!(!lowered.contains("loom"), "nothing Loom-identifying may be added: {head}");
            assert_eq!(seen.iter().find(|(n, _)| n == "host").unwrap().1, addr);
        });
    }
}

#[test]
fn a_client_that_sends_no_user_agent_gets_none_added() {
    let _ = observed(async {
        let (addr, heads) = upstream(Script {
            status: 200,
            content_type: "application/json",
            chunked: false,
            pieces: vec![(0, b"{}".to_vec())],
        })
        .await;
        through_proxy(&addr, true, chat_request).await;
        let head = heads.lock().unwrap()[0].to_ascii_lowercase();
        for name in [
            "user-agent",
            "accept-encoding",
            "via:",
            "x-forwarded-for",
            "forwarded",
        ] {
            assert!(!head.contains(name), "{name} must not be invented: {head}");
        }
        // The one thing the outbound HTTP library supplies for a client that
        // sent no `Accept` is `*/*` (it cannot be suppressed through the
        // builder). RFC 9110 defines an absent `Accept` as exactly that, so it
        // changes no semantics; a client's own `Accept` is never touched (see
        // the fidelity test above).
        let accepts: Vec<&str> = head.lines().filter(|l| l.starts_with("accept:")).collect();
        assert!(accepts.iter().all(|l| l.trim() == "accept: */*"), "{accepts:?}");
    });
}

// ------------------------------------------------------------------- capture

#[test]
fn openai_style_sse_yields_counts_model_ttft_and_is_relayed_whole() {
    let (reply, captured) = observed(async {
        let events = sse(&[
            &format!("{{\"model\":\"glm-4.6\",\"choices\":[{{\"delta\":{{\"content\":\"{SECRET_TEXT}\"}}}}]}}"),
            "{\"model\":\"glm-4.6\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":30}}}",
            "[DONE]",
        ]);
        // Split mid-line so the reader must reassemble across chunks.
        let (first, rest) = events.split_at(17);
        let (addr, _) = upstream(Script {
            status: 200,
            content_type: "text/event-stream",
            chunked: true,
            pieces: vec![(0, first.to_vec()), (120, rest.to_vec())],
        })
        .await;
        through_proxy(&addr, true, chat_request).await
    });
    assert!(reply.contains(SECRET_TEXT), "the stream must be relayed untouched");
    assert!(reply.contains("[DONE]"));
    assert_eq!(tokens(&captured, "input"), Some(70), "uncached = prompt - cached");
    assert_eq!(tokens(&captured, "output"), Some(20));
    assert_eq!(tokens(&captured, "cache_read"), Some(30));
    let span = single_span(&captured);
    assert_eq!(span.attributes["loom.model"], "glm-4.6");
    assert_eq!(span.attributes["loom.egress.stream"], "true");
    assert_eq!(span.attributes["loom.egress.status"], "200");
    let ttft: u128 = span.attributes["loom.egress.ttft_ms"].parse().unwrap();
    let latency: u128 = span.attributes["loom.egress.latency_ms"].parse().unwrap();
    assert!(latency >= 120, "latency covers the whole stream: {latency}");
    assert!(ttft < latency, "ttft {ttft} must precede total latency {latency}");
}

#[test]
fn anthropic_style_sse_merges_start_and_delta_usage() {
    let (_, captured) = observed(async {
        let events = sse(&[
            "{\"type\":\"message_start\",\"message\":{\"model\":\"claude-x\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":2,\"output_tokens\":1}}}",
            "{\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}",
            "{\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}",
        ]);
        let (addr, _) = upstream(Script {
            status: 200,
            content_type: "text/event-stream; charset=utf-8",
            chunked: true,
            pieces: vec![(0, events)],
        })
        .await;
        through_proxy(&addr, true, chat_request).await
    });
    assert_eq!(tokens(&captured, "input"), Some(10));
    assert_eq!(tokens(&captured, "output"), Some(42));
    assert_eq!(tokens(&captured, "cache_read"), Some(5));
    assert_eq!(tokens(&captured, "cache_write"), Some(2));
}

#[test]
fn non_streaming_json_yields_counts() {
    let (reply, captured) = observed(async {
        let body = json!({"model":"glm-4.6","choices":[{"message":{"content":SECRET_TEXT}}],
            "usage":{"prompt_tokens":7,"completion_tokens":3}})
        .to_string();
        let (addr, _) = upstream(Script {
            status: 200,
            content_type: "application/json",
            chunked: false,
            pieces: vec![(0, body.into_bytes())],
        })
        .await;
        through_proxy(&addr, true, chat_request).await
    });
    assert!(reply.contains(SECRET_TEXT));
    assert_eq!(tokens(&captured, "input"), Some(7));
    assert_eq!(tokens(&captured, "output"), Some(3));
    assert_eq!(single_span(&captured).attributes["loom.egress.stream"], "false");
}

#[test]
fn a_429_is_classified_to_a_code_and_its_body_is_not_emitted() {
    let (reply, captured) = observed(async {
        let body = format!("{{\"error\":{{\"message\":\"slow down {SECRET_TEXT}\"}}}}");
        let (addr, _) = upstream(Script {
            status: 429,
            content_type: "application/json",
            chunked: false,
            pieces: vec![(0, body.into_bytes())],
        })
        .await;
        through_proxy(&addr, true, chat_request).await
    });
    assert!(reply.starts_with("HTTP/1.1 429"), "{reply}");
    let span = single_span(&captured);
    assert_eq!(span.attributes["loom.egress.error_code"], "rate_limited");
    assert_eq!(span.status, SpanStatus::Error);
    let request = captured
        .metrics
        .iter()
        .find(|p| p.name == MetricName::EgressRequests)
        .unwrap();
    assert_eq!(request.labels["outcome"], "error");
    assert_eq!(request.labels["reason"], "rate_limited");
    assert_eq!(tokens(&captured, "input"), None);
}

#[test]
fn malformed_streams_never_affect_the_relay() {
    let (reply, captured) = observed(async {
        let mut junk = b"data: {not json at all\n\ndata: \xff\xfe\n\n:comment\n\n".to_vec();
        junk.extend(sse(&["{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}"]));
        let (addr, _) = upstream(Script {
            status: 200,
            content_type: "text/event-stream",
            chunked: true,
            pieces: vec![(0, junk)],
        })
        .await;
        through_proxy(&addr, true, chat_request).await
    });
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("{not json at all"), "garbage is relayed verbatim");
    // Parsing recovers on the next good line; the point is nothing broke.
    assert_eq!(tokens(&captured, "output"), Some(2));
}

#[test]
fn an_oversize_sse_line_is_skipped_and_parsing_resumes() {
    let mut tap = Tap::new(Instant::now(), true);
    tap.feed(b"data: ");
    let filler = vec![b'x'; 64 * 1024];
    for _ in 0..5 {
        tap.feed(&filler);
    }
    tap.feed(b"\n\ndata: {\"usage\":{\"input_tokens\":4,\"output_tokens\":6}}\n\n");
    assert!(tap.carry.len() <= MAX_LINE_BYTES);
    let obs = tap.finish(200, None);
    assert_eq!((obs.usage.input, obs.usage.output), (Some(4), Some(6)));
}

/// An oversized `data:` line whose usage would be parsed, sized just past the
/// bound (so the stale-usage check below proves it was skipped, not parsed).
fn oversized_usage_line() -> Vec<u8> {
    let mut line =
        b"data: {\"usage\":{\"input_tokens\":999,\"output_tokens\":999},\"pad\":\"".to_vec();
    line.resize(MAX_LINE_BYTES + 1, b'x');
    line.extend_from_slice(b"\"}\n");
    line
}

#[test]
fn a_whole_oversize_line_in_one_feed_is_skipped_and_parsing_resumes() {
    let mut tap = Tap::new(Instant::now(), true);
    let mut chunk = oversized_usage_line();
    chunk.extend_from_slice(b"\ndata: {\"usage\":{\"output_tokens\":6}}\n\n");
    tap.feed(&chunk);
    assert!(tap.carry.len() <= MAX_LINE_BYTES);
    let obs = tap.finish(200, None);
    assert_eq!((obs.usage.input, obs.usage.output), (None, Some(6)), "oversize line parsed");
}

#[test]
fn an_oversize_line_completed_by_the_next_feed_is_skipped_and_parsing_resumes() {
    let line = oversized_usage_line();
    // The first feed stays under the bound, so it is carried; the next feed
    // completes the line past it.
    let (head, tail) = line.split_at(MAX_LINE_BYTES / 2);
    let mut tap = Tap::new(Instant::now(), true);
    tap.feed(head);
    assert_eq!(tap.carry.len(), head.len(), "partial line under the bound is carried");
    let mut next = tail.to_vec();
    next.extend_from_slice(b"\ndata: {\"usage\":{\"output_tokens\":6}}\n\n");
    tap.feed(&next);
    assert!(tap.carry.len() <= MAX_LINE_BYTES);
    let obs = tap.finish(200, None);
    assert_eq!((obs.usage.input, obs.usage.output), (None, Some(6)), "oversize line parsed");
}

// -------------------------------------------------------- emission hygiene

#[test]
fn emitted_records_carry_the_tags_and_never_a_body_or_credential() {
    let (_, captured) = observed(async {
        let events = sse(&[
            &format!("{{\"model\":\"glm-4.6\",\"choices\":[{{\"delta\":{{\"content\":\"{SECRET_TEXT}\"}}}}]}}"),
            "{\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}",
        ]);
        let (addr, _) = upstream(Script {
            status: 200,
            content_type: "text/event-stream",
            chunked: true,
            pieces: vec![(0, events)],
        })
        .await;
        through_proxy(&addr, true, chat_request).await
    });
    let span = single_span(&captured);
    for (key, want) in [
        ("loom.egress.seat", "seat-a"),
        ("loom.egress.tap", "opencode:zai-flash"),
        ("loom.egress.profile", "zai-flash"),
        ("loom.egress.launch_id", "launch-obs"),
        ("loom.runtime", "opencode"),
        ("loom.role", "builder"),
        ("loom.issue", "11300"),
        ("loom.sweep_id", "sweep-issue-11300-1"),
    ] {
        assert_eq!(span.attributes[key], want, "{key}");
    }
    let request = captured
        .metrics
        .iter()
        .find(|p| p.name == MetricName::EgressRequests)
        .unwrap();
    assert_eq!(request.labels["account"], "seat-a");
    assert_eq!(request.labels["role"], "builder");
    assert_eq!(request.labels["outcome"], "ok");
    let rendered = format!("{captured:?}");
    for forbidden in [SECRET_TEXT, CREDENTIAL, "loom-placeholder-", "glm\"}"] {
        assert!(!rendered.contains(forbidden), "{forbidden} leaked into telemetry: {rendered}");
    }
    // Every attribute key the span uses is exportable.
    for key in span
        .attributes
        .keys()
        .filter(|k| k.starts_with("loom.egress."))
    {
        assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()), "{key}");
    }
}

#[test]
fn observe_mode_off_emits_nothing_and_changes_nothing() {
    let (reply, captured) = observed(async {
        let (addr, _) = upstream(Script {
            status: 200,
            content_type: "application/json",
            chunked: false,
            pieces: vec![(0, b"{\"usage\":{\"prompt_tokens\":1}}".to_vec())],
        })
        .await;
        through_proxy(&addr, false, chat_request).await
    });
    assert!(reply.starts_with("HTTP/1.1 200"));
    assert!(captured.metrics.is_empty() && captured.spans.is_empty(), "{captured:?}");
}

#[test]
fn an_unreachable_upstream_is_a_classified_502() {
    let (reply, captured) = observed(async {
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("127.0.0.1:{}", l.local_addr().unwrap().port())
        };
        through_proxy(&dead, true, chat_request).await
    });
    assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");
    assert_eq!(single_span(&captured).attributes["loom.egress.error_code"], "upstream_error");
}

// ---------------------------------------------------------- gate and bounds

#[test]
fn observe_is_off_by_default_and_needs_profile_and_config() {
    let _g = super::super::tests::env_lock();
    std::env::remove_var("LOOM_EGRESS_PROXY_OBSERVE");
    assert!(!enabled(&json!({})));
    assert!(!enabled(&json!({"runtimes":{"containment":{"credentialProxy":true}}})));
    assert!(enabled(&json!({"runtimes":{"containment":{"credentialProxyObserve":true}}})));
    std::env::set_var("LOOM_EGRESS_PROXY_OBSERVE", "0");
    assert!(!enabled(&json!({"runtimes":{"containment":{"credentialProxyObserve":true}}})));
    std::env::set_var("LOOM_EGRESS_PROXY_OBSERVE", "1");
    assert!(enabled(&json!({})));
    std::env::remove_var("LOOM_EGRESS_PROXY_OBSERVE");

    let off: ProfileProxy = serde_json::from_value(
        json!({"upstream":"https://api.z.ai","header":"authorization-bearer"}),
    )
    .unwrap();
    assert!(!off.observe, "a profile must opt in explicitly");
    let on: ProfileProxy =
        serde_json::from_value(json!({"upstream":"https://api.z.ai","observe":true})).unwrap();
    assert!(on.observe);
}

/// The stated bound: reading a chunk costs well under 1 ms on average, even in
/// an unoptimised test build, so the tee cannot add perceptible latency to a
/// token stream (the relay never awaits the tap).
#[test]
fn the_tee_adds_less_than_a_millisecond_per_chunk() {
    let delta =
        b"data: {\"model\":\"glm-4.6\",\"choices\":[{\"delta\":{\"content\":\"tok\"}}]}\n\n";
    let mut tap = Tap::new(Instant::now(), true);
    let chunks = 5000u32;
    let began = Instant::now();
    for _ in 0..chunks {
        tap.feed(delta);
    }
    tap.feed(b"data: {\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":8}}\n\n");
    let per_chunk = began.elapsed() / chunks;
    assert!(per_chunk < std::time::Duration::from_millis(1), "{per_chunk:?} per chunk");
    assert_eq!(tap.finish(200, None).usage.output, Some(8));
}

#[test]
fn error_codes_are_a_closed_vocabulary() {
    assert_eq!(error_code(429, b"{}"), "rate_limited");
    assert_eq!(error_code(401, b"nope"), "credential");
    assert_eq!(error_code(500, b"boom"), "http_5xx");
    assert_eq!(error_code(404, b"missing"), "http_4xx");
}

// ------------------------------------------- runtime attribution (launcher)

/// Resolve the runtime exactly as the launcher does, run `prepare` with it in
/// observe mode, and return the `(runtime, tap)` the launch record carries.
fn attributed_runtime(config: &serde_json::Value) -> (String, String) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(tmp.path().join(".loom/config.json"), config.to_string()).unwrap();
    let effective = crate::config_resolver::resolve_effective_config(tmp.path());
    let (runtime, _source) =
        crate::worker_spawn::resolve_launch_runtime(tmp.path(), &effective).unwrap();
    let mut selection = crate::worker_spawn::profiles::Selection {
        provider: "anthropic".into(),
        model: "test-model".into(),
        effort: None,
        profile: Some("test-profile".into()),
        credentials: vec![("LOOM_TEST_OBSERVE_KEY_11300".into(), "ANTHROPIC_AUTH_TOKEN".into())],
        credential_sources: vec!["LOOM_TEST_OBSERVE_KEY_11300".into()],
        provider_options: None,
        provider_definition: None,
        credential_pool: None,
        credential_proxy: None,
    };
    selection.credential_proxy = Some(ProfileProxy {
        upstream: "https://api.anthropic.com".into(),
        header: HeaderStyle::AuthorizationBearer,
        base_url_env: vec!["ANTHROPIC_BASE_URL".into()],
        observe: true,
    });
    std::env::set_var("LOOM_TEST_OBSERVE_KEY_11300", CREDENTIAL);
    let prepared = super::super::prepare(tmp.path(), &selection, &effective, &runtime)
        .unwrap()
        .expect("substitution should apply");
    std::env::remove_var("LOOM_TEST_OBSERVE_KEY_11300");
    let placeholder = prepared
        .injection
        .assignments
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_AUTH_TOKEN")
        .map(|(_, v)| v.clone())
        .unwrap();
    let record = prepared
        .registry
        .authorize("POST", &[placeholder], None)
        .unwrap();
    let ctx = record.observe().expect("observe mode is on").clone();
    (ctx.runtime, ctx.tap)
}

/// Clear every variable the launcher's runtime resolution reads.
fn clear_runtime_env() {
    for key in [
        "LOOM_ROLE",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_BUILDER",
        "LOOM_EGRESS_PROXY_OBSERVE",
    ] {
        std::env::remove_var(key);
    }
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
}

const OBSERVE_ON: &str = r#"{"credentialProxy":true,"credentialProxyObserve":true}"#;

#[test]
#[serial_test::serial]
fn observe_attribution_uses_the_config_selected_runtime_not_a_native_fallback() {
    let _g = super::super::tests::env_lock();
    clear_runtime_env();
    let containment: serde_json::Value = serde_json::from_str(OBSERVE_ON).unwrap();
    let config = json!({"runtimes": {"default": "opencode", "containment": containment}});
    let (runtime, tap) = attributed_runtime(&config);
    clear_runtime_env();
    assert_eq!(runtime, "opencode");
    assert_eq!(tap, "opencode:test-profile");
}

#[test]
#[serial_test::serial]
fn observe_attribution_follows_a_role_binding_over_an_inherited_runtime() {
    let _g = super::super::tests::env_lock();
    clear_runtime_env();
    // The parent exported LOOM_RUNTIME=claude; this launch's role binding
    // (LOOM_ROLE=builder + LOOM_RUNTIME_BUILDER) puts it on opencode.
    std::env::set_var("LOOM_RUNTIME", "claude");
    std::env::set_var("LOOM_ROLE", "builder");
    std::env::set_var("LOOM_RUNTIME_BUILDER", "opencode");
    let containment: serde_json::Value = serde_json::from_str(OBSERVE_ON).unwrap();
    let config = json!({"runtimes": {"containment": containment}});
    let (runtime, tap) = attributed_runtime(&config);
    clear_runtime_env();
    assert_eq!(runtime, "opencode");
    assert_eq!(tap, "opencode:test-profile");
}
