//! Static + in-process contract: the trace *shape* Loom exports decides which
//! SigNoz product views can ever hold data for it. No Docker, network, backend,
//! credential or trial host is used — the exported payload is captured from the
//! real `OtlpExporter` through a loopback sink in this process.
//!
//! Why this exists (#8528, scope item 5 — "document which views work for Loom's
//! actual span kinds"). `evidence.md`'s "UI view matrix" section answered two of
//! the three SigNoz product pages against a live trial backend and left the
//! third **confounded**: `POST /api/v1/dependency_graph` returned `[]`, and that
//! session could not say whether the empty Service Map was a Loom-shape
//! limitation or merely an artifact of the trial's render (no topology connector)
//! and of the shared fixture being single-service. It wrote: "This row cannot be
//! called a Loom-specific limitation from this evidence; it needs either a
//! service-graph connector or a multi-service fixture to mean anything."
//!
//! That confound is resolvable without the trial host, because it is a property
//! of the emitting code rather than of the deployment. A topology edge needs one
//! of exactly three things:
//!
//!   1. a parent/child span pair carrying two **different** `service.name`
//!      values (SigNoz's dependency graph),
//!   2. a CLIENT/SERVER (or PRODUCER/CONSUMER) span-kind pair, or
//!   3. a single span carrying a peer/virtual-node attribute such as
//!      `peer.service` — how the OTel `servicegraph` connector synthesizes an
//!      edge when the remote side never reports.
//!
//! The tests below establish that Loom's exporter produces none of the three,
//! for every record family the shared fixture generates, and that the neutral
//! gateway would strip the attributes of (3) even if a later change started
//! emitting them. So an empty Service Map is a permanent consequence of Loom's span
//! shape: configuring a topology connector on the trial, or pointing a
//! multi-service fixture at it, cannot change the answer *for Loom's data*.
//!
//! This is the regression guard the conclusion needs, not a restatement of it.
//! The day a Loom span becomes CLIENT-kind, or a second `service.name` appears,
//! the documented answer stops being true — and these tests fail in ordinary CI
//! instead of the `evidence.md` row quietly going stale.
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};
use loom_daemon::observability::exporter::Exporter;
use loom_daemon::observability::otlp::OtlpExporter;

const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const CASTING_LOCK: &str = include_str!("../../defaults/observability/signoz/casting.yaml.lock");

/// `SpanKind` discriminants from `opentelemetry/proto/trace/v1/trace.proto`.
/// Proto enum numbering is a wire-compatibility guarantee, so these are stable
/// external constants rather than a local convention. `opentelemetry-proto` is
/// an optional dependency of the library and not a dev-dependency, so an
/// integration test reads the wire form instead of the Rust enum.
const SPAN_KIND_UNSPECIFIED: i64 = 0;
const SPAN_KIND_INTERNAL: i64 = 1;
const SPAN_KIND_SERVER: i64 = 2;
const SPAN_KIND_CLIENT: i64 = 3;
const SPAN_KIND_PRODUCER: i64 = 4;
const SPAN_KIND_CONSUMER: i64 = 5;

/// Attribute keys a service-graph/topology view reads to synthesize an edge
/// when only one side of a call reports. `peer.service` is the OTel
/// `servicegraph` connector's primary virtual-node key; the rest are the
/// database/messaging/RPC/HTTP peer conventions that the same connector and
/// SigNoz's own external-call views fall back to. None of these is an operational
/// fact about a Loom sweep, which is why none is emitted — but "not emitted
/// today" is exactly the kind of claim that rots silently.
const TOPOLOGY_PEER_KEYS: &[&str] = &[
    "peer.service",
    "db.system",
    "db.name",
    "messaging.system",
    "messaging.destination.name",
    "rpc.system",
    "rpc.service",
    "net.peer.name",
    "net.sock.peer.addr",
    "server.address",
    "http.host",
    "http.url",
];

// ---------------------------------------------------------------------------
// A loopback OTLP/HTTP sink. Captures (path, body) and answers 200, so the real
// exporter's real payload — not a re-derivation of it — is what gets asserted.
// ---------------------------------------------------------------------------

/// Captured `(request path, request body)` pairs, shared with the sink thread.
type Captured = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Sink {
    base_url: String,
    captured: Captured,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sink {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let captured: Captured = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let captured = captured.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // macOS/BSD `accept()` hands back a socket that
                            // inherits the listener's O_NONBLOCK (Linux does
                            // not). A non-blocking read that races the client
                            // returns WouldBlock mid-request, which silently
                            // dropped the capture while the sink still answered
                            // 200 — so the stream must block, with the 5 s read
                            // timeout set in `read_request` as the bound.
                            stream.set_nonblocking(false).unwrap();
                            // A request the sink failed to read is answered 500,
                            // never 200: a dropped capture must surface as an
                            // export failure, not as a quietly smaller payload
                            // set that the assertions below then pass over.
                            let response = match read_request(&mut stream) {
                                Some(request) => {
                                    captured.lock().unwrap().push(request);
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                                }
                                None => "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            };
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Sink {
            base_url,
            captured,
            stop,
            thread: Some(thread),
        }
    }

    /// Every captured body posted to `path`, parsed as OTLP/HTTP+JSON.
    fn payloads(&self, path: &str) -> Vec<serde_json::Value> {
        self.captured
            .lock()
            .unwrap()
            .iter()
            .filter(|(captured_path, _)| captured_path == path)
            .map(|(_, body)| serde_json::from_slice(body).expect("sink body is not JSON"))
            .collect()
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];
    // Headers first, byte at a time: the body length is only known after them.
    while !buffer.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        buffer.push(byte[0]);
    }
    let headers = String::from_utf8_lossy(&buffer).to_string();
    let path = headers.split_whitespace().nth(1)?.to_string();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).ok()?;
    Some((path, body))
}

// ---------------------------------------------------------------------------
// The exported shared fixture, captured once per test.
// ---------------------------------------------------------------------------

/// Posts the whole shared fixture manifest (#8578 — the same generator the trial
/// host replays) through the real `OtlpExporter` and returns the sink plus the
/// generator's own manifest.
async fn export_shared_fixture() -> (Sink, serde_json::Value) {
    let start = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
    let bundle = loom_daemon::telemetry::fixture::build("topologyshape", start)
        .expect("shared fixture must build");
    let sink = Sink::start();
    let exporter = OtlpExporter::new(sink.base_url.clone(), "test-key".into()).unwrap();
    let outcome = exporter.emit_batch_outcome(&bundle.envelopes).await;
    assert!(outcome.error.is_none(), "loopback export failed: {:?}", outcome.error);
    assert_eq!(
        outcome.acknowledged,
        bundle.envelopes.len(),
        "the sink must have received every fixture envelope"
    );
    (sink, bundle.manifest)
}

/// Every `spans[]` element across every captured `/v1/traces` payload.
fn exported_spans(sink: &Sink) -> Vec<serde_json::Value> {
    sink.payloads("/v1/traces")
        .iter()
        .flat_map(|payload| {
            payload["resourceSpans"]
                .as_array()
                .expect("resourceSpans")
                .clone()
        })
        .flat_map(|resource| {
            resource["scopeSpans"]
                .as_array()
                .expect("scopeSpans")
                .clone()
        })
        .flat_map(|scope| scope["spans"].as_array().expect("spans").clone())
        .collect()
}

/// Every exported span, after asserting that there are **exactly** as many as
/// the generator's own manifest declares. Every test that reads spans goes
/// through this, so none can pass by quietly asserting over a subset — a sink
/// that dropped a request would otherwise shrink the evidence without failing
/// (the macOS non-blocking-accept flake that reached review on #9742).
fn exported_spans_exact(sink: &Sink, manifest: &serde_json::Value) -> Vec<serde_json::Value> {
    let spans = exported_spans(sink);
    let expected = manifest["spans"].as_array().expect("manifest spans").len();
    assert!(expected > 0, "the fixture must declare spans to assert on");
    assert_eq!(
        spans.len(),
        expected,
        "exported span count must equal the manifest's own declared span count"
    );
    spans
}

fn attribute_keys(attributes: &serde_json::Value) -> BTreeSet<String> {
    attributes
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|kv| kv["key"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 1. Span kind: the client/server precondition for a synthesized edge.
// ---------------------------------------------------------------------------

/// Precondition (2) for a topology edge: the span-kind pair. `kind` is absent
/// from the JSON when it is `SPAN_KIND_UNSPECIFIED` (proto3 omits defaults), so
/// a missing field is read as `0` rather than skipped — otherwise an exporter
/// regression that stopped setting the field at all would pass this test by
/// producing nothing to check.
#[tokio::test]
async fn every_exported_span_is_internal_kind_with_no_client_or_server_pair() {
    let (sink, manifest) = export_shared_fixture().await;
    let spans = exported_spans_exact(&sink, &manifest);

    let kinds: BTreeSet<i64> = spans
        .iter()
        .map(|span| span["kind"].as_i64().unwrap_or(SPAN_KIND_UNSPECIFIED))
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([SPAN_KIND_INTERNAL]),
        "Loom exports exactly one span kind, SPAN_KIND_INTERNAL; found {kinds:?}. \
         A CLIENT/SERVER/PRODUCER/CONSUMER span makes a service-graph edge \
         possible, which invalidates the 'Service Map is empty by construction' \
         row in defaults/observability/signoz/evidence.md"
    );
    for forbidden in [
        SPAN_KIND_SERVER,
        SPAN_KIND_CLIENT,
        SPAN_KIND_PRODUCER,
        SPAN_KIND_CONSUMER,
    ] {
        assert!(!kinds.contains(&forbidden), "span kind {forbidden} pairs into a topology edge");
    }
}

// ---------------------------------------------------------------------------
// 2. Service identity: the two-different-service precondition.
// ---------------------------------------------------------------------------

/// Precondition (1). The fixture deliberately spreads records over sweeps,
/// repos, roles and runtimes; the exporter additionally groups `ResourceSpans`
/// **per host id**, so this also proves that per-host fan-out does not mint a
/// second service identity (a distinct `service.instance.id` is not a distinct
/// `service.name`). The expected value is read out of the generator's own
/// manifest rather than restated here.
#[tokio::test]
async fn every_exported_resource_carries_one_shared_service_name() {
    let (sink, manifest) = export_shared_fixture().await;
    let declared = manifest["resource_attributes"]["service.name"]
        .as_str()
        .expect("the manifest declares the resource service.name")
        .to_string();

    // Every declared span must be present, so the service-name check below
    // covers every resource that carried one — not only the requests that
    // happened to be captured.
    exported_spans_exact(&sink, &manifest);
    let payloads = sink.payloads("/v1/traces");
    assert!(!payloads.is_empty(), "no trace payload reached the sink");
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut resources = 0_usize;
    for payload in &payloads {
        for resource in payload["resourceSpans"].as_array().expect("resourceSpans") {
            resources += 1;
            let attributes = resource["resource"]["attributes"]
                .as_array()
                .expect("resource attributes");
            let name = attributes
                .iter()
                .find(|kv| kv["key"] == "service.name")
                .and_then(|kv| kv["value"]["stringValue"].as_str())
                .expect("every exported resource must name its service");
            names.insert(name.to_string());
        }
    }
    assert!(resources > 0, "no resource was exported");
    assert_eq!(
        names,
        BTreeSet::from([declared.clone()]),
        "Loom exports a single service identity ({declared}); a second one makes \
         a SigNoz dependency-graph edge possible and invalidates the Service Map \
         row in defaults/observability/signoz/evidence.md"
    );
}

// ---------------------------------------------------------------------------
// 3. Peer attributes: the virtual-node precondition.
// ---------------------------------------------------------------------------

/// Precondition (3). A `servicegraph` connector will draw an edge to a
/// *virtual* node from one span alone if it carries a peer key, so span kind
/// (precondition 2) is not sufficient on its own.
#[tokio::test]
async fn no_exported_span_or_resource_carries_a_topology_peer_attribute() {
    let (sink, manifest) = export_shared_fixture().await;
    let spans = exported_spans_exact(&sink, &manifest);
    for span in &spans {
        let keys = attribute_keys(&span["attributes"]);
        for peer in TOPOLOGY_PEER_KEYS {
            assert!(
                !keys.contains(*peer),
                "span {} carries topology peer key {peer}",
                span["name"].as_str().unwrap_or("<unnamed>")
            );
        }
        for event in span["events"].as_array().unwrap_or(&vec![]).clone() {
            let keys = attribute_keys(&event["attributes"]);
            for peer in TOPOLOGY_PEER_KEYS {
                assert!(
                    !keys.contains(*peer),
                    "span event {} carries topology peer key {peer}",
                    event["name"].as_str().unwrap_or("<unnamed>")
                );
            }
        }
    }
    for payload in sink.payloads("/v1/traces") {
        for resource in payload["resourceSpans"].as_array().expect("resourceSpans") {
            let keys = attribute_keys(&resource["resource"]["attributes"]);
            for peer in TOPOLOGY_PEER_KEYS {
                assert!(!keys.contains(*peer), "resource carries peer key {peer}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. The gateway is a second, independent enforcement of the same conclusion.
// ---------------------------------------------------------------------------

/// Returns the single `keep_keys` allowlist declared for `context` in the
/// committed gateway config's `transform/privacy` processor. Parsed the same way
/// `collector_fanout.rs` parses it, so both read one authority.
fn gateway_keep_keys(context: &str) -> BTreeSet<String> {
    let mut current = String::new();
    for line in COLLECTOR_CONFIG.lines() {
        if let Some(name) = line.trim().strip_prefix("- context: ") {
            current = name.trim().to_string();
        }
        if current == context && line.contains("keep_keys(") {
            return line
                .split('"')
                .filter(|piece| piece.contains('.') && !piece.contains(','))
                .map(str::to_string)
                .collect();
        }
    }
    panic!("no keep_keys allowlist found for context {context}");
}

/// Even if a future change started emitting a peer key, the neutral gateway
/// drops it before SigNoz sees it: `transform/privacy` is an allowlist, so the
/// assertion is that none of the topology keys has been *added* to it. The
/// resource allowlist is additionally asserted to be exactly the four service
/// identity keys — that is what makes "one service identity" a property of the
/// pipeline and not only of today's exporter.
#[test]
fn the_gateway_allowlist_admits_no_topology_peer_attribute() {
    for context in ["span", "spanevent", "resource"] {
        let allowed = gateway_keep_keys(context);
        assert!(
            !allowed.is_empty(),
            "the {context} allowlist parsed empty; the parser has drifted from the config"
        );
        for peer in TOPOLOGY_PEER_KEYS {
            assert!(
                !allowed.contains(*peer),
                "gateway {context} keep_keys now forwards topology peer key {peer}; \
                 the Service Map row in defaults/observability/signoz/evidence.md \
                 assumes no such key reaches SigNoz"
            );
        }
    }
    assert_eq!(
        gateway_keep_keys("resource"),
        BTreeSet::from([
            "host.id".to_string(),
            "service.instance.id".to_string(),
            "service.name".to_string(),
            "service.version".to_string(),
        ]),
        "the resource allowlist is the pipeline-level guarantee that only one \
         service identity can reach either backend"
    );
}

// ---------------------------------------------------------------------------
// 5. The render: why APM populates and Service Map cannot.
// ---------------------------------------------------------------------------

/// `evidence.md` attributes the populated Service List/APM page to
/// `signozspanmetrics/delta` running on every span regardless of kind, and notes
/// the render configures no topology connector. Both halves are pinned here so
/// re-rendering with a newer Foundry release cannot silently change what the
/// evidence claims about the trial.
///
/// Note what this test does **not** claim: adding a topology connector is not
/// forbidden, and doing so would not be a bug. It would, however, mean the
/// evidence's reasoning needs rewriting — the connector would then exist and the
/// remaining reason for an empty Service Map would be Loom's shape alone, which
/// tests 1–4 own.
#[test]
fn the_rendered_ingester_aggregates_every_span_and_declares_no_topology_connector() {
    assert!(
        CASTING_LOCK.contains("signozspanmetrics/delta"),
        "the rendered ingester no longer runs signozspanmetrics/delta; the \
         evidence's explanation for a populated Service List/APM page (RED \
         metrics from unconditional root-span aggregation, not an HTTP \
         convention) no longer holds"
    );
    for connector in ["servicegraph", "service_graph", "dependencygraph"] {
        assert!(
            !CASTING_LOCK.contains(connector),
            "the render now declares a {connector} connector; the Service Map \
             row in defaults/observability/signoz/evidence.md was written \
             against a render that had none"
        );
    }
}
