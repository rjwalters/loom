//! The worker's HTTP client for the fleet-dispatch server (#12450): one
//! bounded `POST` per call, and the `loom.fleet_dispatch.call` span.
//!
//! Every request has a short deadline ([`CALL_TIMEOUT`]), follows no
//! redirect and uses no proxy (the server is on the tailnet). The calls run
//! on a small runtime of their own, the same bridge `forge_mirror::client`
//! uses for its synchronous callers: it is only built and spawned onto, never
//! entered with `block_on`, so a call is safe from any thread, inside a tokio
//! runtime or not.
//!
//! A claim waits for its answer ([`post_blocking`]). `refuse`, `release` and
//! the heartbeat do not ([`post_detached`]): the step path never waits on
//! them.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// One call's deadline: short, so a dead server costs a spawn little.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a call produced no answer. Every one of these fails open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No connection, or no answer within the deadline.
    Transport(String),
    /// A non-2xx status.
    Status(u16),
    /// A 2xx whose body is not the contract's shape.
    Malformed(String),
}

impl CallError {
    /// The HTTP status, when the server answered one.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status(s) => Some(*s),
            Self::Malformed(_) => Some(200),
            Self::Transport(_) => None,
        }
    }

    /// The closed `loom.fleet_dispatch.reason` token of a failed call.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::Status(s) if *s >= 500 => "status_5xx",
            Self::Status(_) => "status_4xx",
            Self::Malformed(_) => "malformed",
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Status(s) => write!(f, "HTTP {s}"),
            Self::Malformed(e) => write!(f, "malformed answer: {e}"),
        }
    }
}

fn build_http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(CALL_TIMEOUT)
        .connect_timeout(CALL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| format!("could not construct the fleet-dispatch HTTP client: {e}"))
}

fn runtime() -> Option<&'static (tokio::runtime::Runtime, reqwest::Client)> {
    static RT: OnceLock<Option<(tokio::runtime::Runtime, reqwest::Client)>> = OnceLock::new();
    RT.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("loom-fleet-dispatch")
            .enable_all()
            .build()
            .map_err(|e| log::warn!("fleet_dispatch: no runtime for the client ({e})"))
            .ok()?;
        let http = build_http()
            .map_err(|e| log::warn!("fleet_dispatch: {e}"))
            .ok()?;
        Some((rt, http))
    })
    .as_ref()
}

/// `POST <base><path>` with `body`, and parse the JSON answer.
async fn post<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    body: &Req,
) -> Result<Resp, CallError> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let res = http
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| CallError::Transport(classify(&e)))?;
    let status = res.status();
    if !status.is_success() {
        return Err(CallError::Status(status.as_u16()));
    }
    let bytes = res
        .bytes()
        .await
        .map_err(|e| CallError::Transport(classify(&e)))?;
    serde_json::from_slice(&bytes).map_err(|e| CallError::Malformed(e.to_string()))
}

/// A transport error's class, never its URL.
fn classify(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connect".into()
    } else if e.is_body() || e.is_decode() {
        "body".into()
    } else {
        "request".into()
    }
}

/// Wait for `rx`, telling a multi-thread tokio runtime the worker is about
/// to block; a current-thread runtime simply blocks, bounded by `deadline`.
fn wait<T>(rx: &std::sync::mpsc::Receiver<T>, deadline: Duration) -> Option<T> {
    let multi = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi {
        tokio::task::block_in_place(|| rx.recv_timeout(deadline).ok())
    } else {
        rx.recv_timeout(deadline).ok()
    }
}

/// [`post`] from synchronous code, bounded by [`CALL_TIMEOUT`] (plus a
/// little) whatever the network does.
///
/// # Errors
///
/// [`CallError`]; a missed deadline is [`CallError::Transport`].
pub fn post_blocking<Req, Resp>(base: &str, path: &str, body: &Req) -> Result<Resp, CallError>
where
    Req: Serialize + Clone + Send + Sync + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    let Some((rt, http)) = runtime() else {
        return Err(CallError::Transport("no runtime".into()));
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (http, base, path, body) = (http.clone(), base.to_string(), path.to_string(), body.clone());
    rt.spawn(async move {
        let _ = tx.send(post::<Req, Resp>(&http, &base, &path, &body).await);
    });
    wait(&rx, CALL_TIMEOUT + Duration::from_millis(500))
        .unwrap_or_else(|| Err(CallError::Transport("deadline".into())))
}

/// [`post`] without waiting: `done` gets the answer and the call's latency
/// on the client's runtime.
pub fn post_detached<Req, Resp>(
    base: &str,
    path: &str,
    body: Req,
    done: impl FnOnce(Result<Resp, CallError>, Duration) + Send + 'static,
) where
    Req: Serialize + Send + Sync + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    let Some((rt, http)) = runtime() else {
        done(Err(CallError::Transport("no runtime".into())), Duration::ZERO);
        return;
    };
    let (http, base, path) = (http.clone(), base.to_string(), path.to_string());
    rt.spawn(async move {
        let started = Instant::now();
        let answer = post::<Req, Resp>(&http, &base, &path, &body).await;
        done(answer, started.elapsed());
    });
}

// ---------------------------------------------------------------------------
// The call span
// ---------------------------------------------------------------------------

/// `loom.fleet_dispatch.result` of a granted claim.
pub const RESULT_GRANTED: &str = "granted";
/// `loom.fleet_dispatch.result` of a refused claim.
pub const RESULT_REFUSED: &str = "refused";
/// `loom.fleet_dispatch.result` of an answered heartbeat, refuse or release.
pub const RESULT_OK: &str = "ok";
/// `loom.fleet_dispatch.result` of a call with no usable answer.
pub const RESULT_FAILED: &str = "failed";

/// One call's facts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallFacts {
    /// The path called (`/v1/claim`, ...), as the server names it.
    pub endpoint: &'static str,
    pub latency: Duration,
    /// [`RESULT_GRANTED`], [`RESULT_REFUSED`], [`RESULT_OK`] or
    /// [`RESULT_FAILED`].
    pub result: &'static str,
    /// The refusal reason, the refuse reason, the release outcome, or a
    /// failed call's [`CallError::token`].
    pub reason: Option<String>,
    pub server_epoch: Option<String>,
    /// The HTTP status, when the server answered.
    pub status: Option<u16>,
    pub repo: Option<String>,
    pub item: Option<u64>,
    pub step: Option<String>,
    pub assignment_id: Option<String>,
}

/// The span for `facts`, ended at `at`. Pure.
#[must_use]
pub fn span(facts: &CallFacts, at: DateTime<Utc>) -> SpanRecord {
    let name = SpanName::FleetDispatchCall;
    let instant = crate::telemetry::trace::instant(at);
    let item = facts.item.map(|n| n.to_string()).unwrap_or_default();
    let context = TraceContext::derived(
        name.as_str(),
        &[
            facts.endpoint,
            facts.assignment_id.as_deref().unwrap_or(""),
            facts.repo.as_deref().unwrap_or(""),
            &item,
            &instant,
        ],
    );
    let mut attributes = TraceAttributes::new();
    let mut put = |k: &str, v: String| {
        attributes.insert(k.to_string(), v);
    };
    put("loom.fleet_dispatch.side", "worker".to_string());
    put("loom.fleet_dispatch.host", crate::host_identity::host_identity());
    put("loom.fleet_dispatch.endpoint", facts.endpoint.to_string());
    put("loom.fleet_dispatch.latency_ms", facts.latency.as_millis().to_string());
    put("loom.fleet_dispatch.result", facts.result.to_string());
    put(
        "loom.fleet_dispatch.status",
        facts
            .status
            .map_or_else(|| "0".to_string(), |s| s.to_string()),
    );
    if let Some(reason) = &facts.reason {
        put("loom.fleet_dispatch.reason", reason.clone());
    }
    if let Some(epoch) = &facts.server_epoch {
        put("loom.fleet_dispatch.server_epoch", epoch.clone());
    }
    if let Some(repo) = &facts.repo {
        put("loom.repo", repo.clone());
    }
    if let Some(n) = facts.item {
        put("loom.step.item", n.to_string());
    }
    if let Some(step) = &facts.step {
        put("loom.step.role", step.clone());
    }
    if let Some(id) = &facts.assignment_id {
        put("loom.assignment.id", id.clone());
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    let started_at =
        at - chrono::Duration::from_std(facts.latency).unwrap_or_else(|_| chrono::Duration::zero());
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at,
        ended_at: at,
        status: if facts.result == RESULT_FAILED {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Emit `facts`' span; a no-op without an exporter.
pub fn record(facts: &CallFacts) {
    if !crate::observability::ops::spans_exported() {
        return;
    }
    crate::observability::ops::emit_span(span(facts, Utc::now()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::proto::{ClaimRequest, ClaimResponse, CLAIM_PATH};
    use super::super::test_server::TestServer;
    use super::*;
    use serde_json::json;

    #[test]
    fn a_claim_round_trips_through_a_real_server() {
        let server = TestServer::start(|path, body| {
            assert_eq!(path, CLAIM_PATH);
            assert_eq!(body["step"], "judge");
            (
                200,
                json!({"server_epoch":"e1","granted":true,"assignment_id":"a-9","ttl_s":1080}),
            )
        });
        let req = ClaimRequest {
            host_id: "h".into(),
            repo: "o/r".into(),
            item: 9,
            step: "judge".into(),
        };
        let answer: ClaimResponse = post_blocking(&server.url(), CLAIM_PATH, &req).unwrap();
        assert_eq!(answer.assignment_id.as_deref(), Some("a-9"));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn a_5xx_a_bad_body_and_a_dead_port_are_errors() {
        let req = json!({});
        let five = TestServer::start(|_, _| (503, json!({})));
        let e = post_blocking::<_, ClaimResponse>(&five.url(), CLAIM_PATH, &req).unwrap_err();
        assert_eq!(e, CallError::Status(503));
        assert_eq!(e.token(), "status_5xx");
        let bad = TestServer::start(|_, _| (200, json!({"nope": 1})));
        let e = post_blocking::<_, ClaimResponse>(&bad.url(), CLAIM_PATH, &req).unwrap_err();
        assert_eq!(e.token(), "malformed");
        let dead = TestServer::dead_url();
        let e = post_blocking::<_, ClaimResponse>(&dead, CLAIM_PATH, &req).unwrap_err();
        assert_eq!(e.token(), "transport");
    }

    #[test]
    fn a_server_that_never_answers_is_bounded_by_the_deadline() {
        let silent = TestServer::silent();
        let started = Instant::now();
        let e =
            post_blocking::<_, ClaimResponse>(&silent.url(), CLAIM_PATH, &json!({})).unwrap_err();
        assert_eq!(e.token(), "transport");
        assert!(
            started.elapsed() < CALL_TIMEOUT + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_call_span_carries_its_facts_and_passes_the_allowlist() {
        let facts = CallFacts {
            endpoint: CLAIM_PATH,
            latency: Duration::from_millis(12),
            result: RESULT_REFUSED,
            reason: Some("held".into()),
            server_epoch: Some("e1".into()),
            status: Some(200),
            repo: Some("o/r".into()),
            item: Some(9),
            step: Some("judge".into()),
            assignment_id: None,
        };
        let span = span(&facts, Utc::now());
        assert_eq!(span.name.as_str(), "loom.fleet_dispatch.call");
        let attr = |k: &str| span.attributes.get(k).cloned();
        assert_eq!(attr("loom.fleet_dispatch.endpoint").as_deref(), Some("/v1/claim"));
        assert_eq!(attr("loom.fleet_dispatch.latency_ms").as_deref(), Some("12"));
        assert_eq!(attr("loom.fleet_dispatch.result").as_deref(), Some("refused"));
        assert_eq!(attr("loom.fleet_dispatch.reason").as_deref(), Some("held"));
        assert_eq!(attr("loom.fleet_dispatch.server_epoch").as_deref(), Some("e1"));
        assert_eq!(attr("loom.fleet_dispatch.side").as_deref(), Some("worker"));
        assert_eq!(attr("loom.fleet_dispatch.status").as_deref(), Some("200"));
        for key in span
            .attributes
            .keys()
            .filter(|k| k.starts_with("loom.fleet_dispatch.") || k.starts_with("loom.step."))
        {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key} is dropped at export"
            );
        }
        let failed = super::span(
            &CallFacts {
                result: RESULT_FAILED,
                assignment_id: Some("a-1".into()),
                ..facts
            },
            Utc::now(),
        );
        assert_eq!(failed.status, SpanStatus::Error);
        assert_eq!(failed.attributes["loom.assignment.id"], "a-1");
    }
}
