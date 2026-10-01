//! OTLP delivery for published evidence bundles (#9910): POST the
//! pre-built payload body (`otlp-payload.json`) to the configured
//! collector endpoint.
//!
//! # Contract
//!
//! Delivery failures are **counted and surfaced, never silent**: every
//! attempt is recorded — endpoint host (never the full URL, and never any
//! key material), HTTP status or transport-error kind, latency, body size —
//! into a `delivery-log.jsonl` next to the published bundle, and the CLI
//! fails loudly when the push does not deliver. This mirrors the
//! counted-skips contract of the record builder itself (#9786).
//!
//! The HTTP path mirrors [`crate::observability::exporter::HttpsExporter`]:
//! JSON body, `Authorization: Bearer <ingest_key>`, rustls TLS. The sink is
//! a one-method trait so the delivery-log logic is testable without a live
//! collector; the live SigNoz read-back stays #9786's acceptance item.

use serde::Serialize;

/// One delivery attempt, serialized into `delivery-log.jsonl`. Deliberately
/// records the endpoint **host only** — never the full URL and never key
/// material — so the log itself is safe to paste into an issue.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryAttempt {
    /// Host (and port, if non-default) extracted from the endpoint URL.
    pub endpoint_host: String,
    /// HTTP status when the request completed; `None` on transport failure.
    pub status: Option<u16>,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u128,
    /// Transport-error kind, or `HTTP {status}` for a non-2xx response.
    pub error: Option<String>,
    /// Payload body size in bytes.
    pub body_bytes: usize,
}

/// The result of pushing one payload body.
#[derive(Debug)]
pub struct DeliveryOutcome {
    pub attempts: Vec<DeliveryAttempt>,
}

impl DeliveryOutcome {
    /// Attempts that got a 2xx response.
    pub fn delivered(&self) -> usize {
        self.attempts.iter().filter(|a| a.delivered()).count()
    }

    /// Attempts that did not deliver (transport error or non-2xx).
    pub fn failed(&self) -> usize {
        self.attempts.len() - self.delivered()
    }
}

impl DeliveryAttempt {
    /// True when the request completed with a 2xx status.
    pub fn delivered(&self) -> bool {
        matches!(self.status, Some(s) if (200..300).contains(&s))
    }
}

/// The transport boundary: POST a JSON body, return the HTTP status, or the
/// transport-error kind as a string. One method — the whole point is that
/// the delivery-log logic around it is testable with a fake.
pub trait Sink {
    /// POST `body` to `endpoint` with an optional `Authorization: Bearer`
    /// header. `Ok(status)` means the HTTP round trip completed (any
    /// status); `Err` means it did not (DNS, TLS, connect, timeout).
    fn post_json(
        &self,
        endpoint: &str,
        bearer_key: Option<&str>,
        body: &[u8],
    ) -> std::result::Result<u16, String>;
}

/// The real sink: a current-thread tokio runtime driving one reqwest
/// (rustls) POST, mirroring [`crate::observability::exporter::HttpsExporter`].
pub struct HttpSink;

impl Sink for HttpSink {
    fn post_json(
        &self,
        endpoint: &str,
        bearer_key: Option<&str>,
        body: &[u8],
    ) -> std::result::Result<u16, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| format!("client build: {e}"))?;
            let mut req = client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json");
            if let Some(key) = bearer_key {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
            }
            let resp = req.body(body.to_vec()).send().await.map_err(|e| {
                // reqwest error Display names the failure kind; strip any
                // URL detail to keep host-only discipline in logs.
                format!("transport: {}", e.without_url())
            })?;
            Ok(resp.status().as_u16())
        })
    }
}

/// Push one payload body with exactly one attempt, recording the attempt.
/// Retries are deliberately out of scope here — a re-push of the same
/// bundle is the operator's idempotent retry, and the deterministic record
/// ids (#9786) keep the collector side dedupable.
pub fn push_payload(
    sink: &dyn Sink,
    endpoint: &str,
    bearer_key: Option<&str>,
    body: &[u8],
) -> DeliveryOutcome {
    let endpoint_host = endpoint_host(endpoint);
    let started = std::time::Instant::now();
    let (status, error) = match sink.post_json(endpoint, bearer_key, body) {
        Ok(status) if (200..300).contains(&status) => (Some(status), None),
        Ok(status) => (Some(status), Some(format!("HTTP {status}"))),
        Err(e) => (None, Some(e)),
    };
    DeliveryOutcome {
        attempts: vec![DeliveryAttempt {
            endpoint_host,
            status,
            latency_ms: started.elapsed().as_millis(),
            error,
            body_bytes: body.len(),
        }],
    }
}

/// Extract `host[:port]` from an endpoint URL for the delivery log. Falls
/// back to `"<unparseable>"` rather than leaking the raw URL.
fn endpoint_host(endpoint: &str) -> String {
    match reqwest::Url::parse(endpoint) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => "<no-host>".to_string(),
        },
        Err(_) => "<unparseable-endpoint>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake sink delivering a scripted result per call.
    struct FakeSink {
        result: std::result::Result<u16, String>,
    }

    impl Sink for FakeSink {
        fn post_json(
            &self,
            _endpoint: &str,
            _bearer_key: Option<&str>,
            _body: &[u8],
        ) -> std::result::Result<u16, String> {
            self.result.clone()
        }
    }

    const SECRET: &str = "super-secret-ingest-key";

    fn assert_no_secret_in_log(attempt: &DeliveryAttempt) {
        let serialized = serde_json::to_string(attempt).expect("serialize");
        assert!(!serialized.contains(SECRET), "delivery log leaked key material: {serialized}");
    }

    #[test]
    fn success_is_counted_as_delivered() {
        let outcome = push_payload(
            &FakeSink { result: Ok(200u16) },
            "https://collector.example.com/v1/logs",
            Some(SECRET),
            b"{\"resourceLogs\":[]}",
        );
        assert_eq!(outcome.delivered(), 1);
        assert_eq!(outcome.failed(), 0);
        let a = &outcome.attempts[0];
        assert_eq!(a.status, Some(200));
        assert_eq!(a.endpoint_host, "collector.example.com");
        assert_eq!(a.error, None);
        assert_eq!(a.body_bytes, 21);
        assert_no_secret_in_log(a);
    }

    #[test]
    fn server_error_is_counted_as_failed_with_status() {
        let outcome = push_payload(
            &FakeSink { result: Ok(503u16) },
            "https://collector.example.com/v1/logs",
            None,
            b"{}",
        );
        assert_eq!(outcome.delivered(), 0);
        assert_eq!(outcome.failed(), 1);
        let a = &outcome.attempts[0];
        assert_eq!(a.status, Some(503));
        assert_eq!(a.error.as_deref(), Some("HTTP 503"));
        assert_no_secret_in_log(a);
    }

    #[test]
    fn transport_error_is_counted_as_failed_without_status() {
        let outcome = push_payload(
            &FakeSink {
                result: Err("transport: dns error".to_string()),
            },
            "https://collector.example.com/v1/logs",
            None,
            b"{}",
        );
        assert_eq!(outcome.failed(), 1);
        let a = &outcome.attempts[0];
        assert_eq!(a.status, None);
        assert_eq!(a.error.as_deref(), Some("transport: dns error"));
        assert_no_secret_in_log(a);
    }

    #[test]
    fn endpoint_host_handles_ports_and_garbage() {
        assert_eq!(endpoint_host("http://localhost:4318/v1/logs"), "localhost:4318");
        assert_eq!(endpoint_host("not a url"), "<unparseable-endpoint>");
    }
}
