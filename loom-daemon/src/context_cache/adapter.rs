//! The retrieval adapter boundary (#9783 steps 2 & 4): a bounded, versioned
//! provider seam. Every call runs under a [`Budget`]; every outcome is
//! explicit — success, empty-success, partial/truncated, unavailable,
//! malformed. Errors never become permanent empty-success cache entries.
//!
//! Two implementations ship:
//!
//! * [`FakeAdapter`] — deterministic, seeded; drives every cache test and the
//!   offline replay/demo path (recorded responses).
//! * [`AugmentAdapter`] — the real provider surface, env-gated. Without
//!   `AUGMENT_API_TOKEN` (and optionally `AUGMENT_BASE_URL`) it reports
//!   [`AdapterOutcome::Unavailable`] — clean unavailable evidence for
//!   consumers, never fabricated risk. The DirectContext contract (supplied
//!   file contents in, search text out) means returned snippets carry **no
//!   git provenance** — the caller must bind results to the pinned source
//!   revision and record the index manifest; unknown provenance stays
//!   explicit.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One raw retrieved location, before validation/normalization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawSnippet {
    /// Repo-relative path as the provider returned it (validated later
    /// against the pinned source).
    pub path: String,
    /// 1-based inclusive line ranges the provider surfaced (may be empty =
    /// file-level evidence).
    pub ranges: Vec<(u32, u32)>,
    /// The returned text (search text per the DirectContext contract).
    pub text: String,
    /// Provider-side reference for later offline replay (index/doc id).
    pub source_ref: String,
}

/// Per-session caps (#9783 step 2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    /// Maximum adapter calls for one key.
    pub max_calls: u32,
    /// Wall-clock cap for the whole session, in seconds.
    pub max_seconds: u64,
    /// Maximum response bytes accepted across the session.
    pub max_bytes: u64,
    /// Maximum retries per query on transient failure.
    pub max_retries: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_calls: 8,
            max_seconds: 120,
            max_bytes: 4 * 1024 * 1024,
            max_retries: 2,
        }
    }
}

/// A single retrieval query with its exact recorded options.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuerySpec {
    /// The query text (recorded verbatim).
    pub text: String,
    /// Query class: implementation | consumer | test | configuration.
    pub class: String,
    /// Options the adapter was invoked with (recorded verbatim).
    pub options: serde_json::Value,
}

/// Provider identity recorded verbatim into every artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderIdentity {
    pub name: String,
    pub version: String,
}

/// The explicit adapter outcome taxonomy (#9783 step 2). `Unavailable` and
/// `Malformed` are terminal for the query; neither is ever cached as an
/// empty success.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AdapterOutcome {
    /// Successful non-empty output.
    Results(Vec<RawSnippet>),
    /// The provider answered successfully with no locations — a real,
    /// cacheable "nothing found".
    EmptySuccess,
    /// Output arrived but was truncated/partial: recorded with what arrived
    /// and why it is incomplete.
    Partial {
        snippets: Vec<RawSnippet>,
        reason: String,
    },
    /// Provider not configured/reachable/out of quota.
    Unavailable { reason: String },
    /// Output arrived but could not be parsed into the contract.
    Malformed { reason: String },
}

/// The provider seam. `query` must respect the budget's caps and report
/// truncation rather than silently dropping content.
pub trait RetrievalAdapter {
    fn identity(&self) -> ProviderIdentity;
    fn query(&self, spec: &QuerySpec, budget: &Budget) -> AdapterOutcome;
}

/// Deterministic fake provider: answers each query text from a seeded map
/// (empty vec → EmptySuccess). Counts every provider call so tests can
/// prove a cache hit made **zero** provider calls. Tests and offline demos
/// only.
#[derive(Default)]
pub struct FakeAdapter {
    seeded: std::collections::BTreeMap<String, Vec<RawSnippet>>,
    /// When true, every query reports Unavailable (drives the fallback tests).
    pub unavailable: bool,
    /// Total query invocations (shared across clones).
    pub calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeAdapter {
    pub fn seeded(map: Vec<(String, RawSnippet)>) -> Self {
        let mut seeded: std::collections::BTreeMap<String, Vec<RawSnippet>> =
            std::collections::BTreeMap::new();
        for (q, s) in map {
            seeded.entry(q).or_default().push(s);
        }
        Self {
            seeded,
            unavailable: false,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn unavailable() -> Self {
        Self {
            seeded: Default::default(),
            unavailable: true,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl RetrievalAdapter for FakeAdapter {
    fn identity(&self) -> ProviderIdentity {
        ProviderIdentity {
            name: "fake".into(),
            version: "fake-1".into(),
        }
    }

    fn query(&self, spec: &QuerySpec, _budget: &Budget) -> AdapterOutcome {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.unavailable {
            return AdapterOutcome::Unavailable {
                reason: "fake adapter configured unavailable".into(),
            };
        }
        match self.seeded.get(&spec.text) {
            Some(snippets) if snippets.is_empty() => AdapterOutcome::EmptySuccess,
            Some(snippets) => AdapterOutcome::Results(snippets.clone()),
            None => AdapterOutcome::EmptySuccess,
        }
    }
}

/// The real Augment provider adapter (env-gated, bounded).
///
/// Configuration (never stored in artifacts): `AUGMENT_API_TOKEN`,
/// optional `AUGMENT_BASE_URL` (default `https://api.augmentcode.com`),
/// optional `AUGMENT_TIMEOUT_SECS`. When unconfigured the adapter returns
/// [`AdapterOutcome::Unavailable`] — consumers see explicit unavailable
/// evidence, never fabricated results.
pub struct AugmentAdapter {
    token: Option<String>,
    base_url: String,
    timeout: Duration,
}

impl AugmentAdapter {
    pub fn from_env() -> Self {
        Self {
            token: std::env::var("AUGMENT_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            base_url: std::env::var("AUGMENT_BASE_URL")
                .unwrap_or_else(|_| "https://api.augmentcode.com".into()),
            timeout: Duration::from_secs(
                std::env::var("AUGMENT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60),
            ),
        }
    }

    pub fn configured(&self) -> bool {
        self.token.is_some()
    }

    fn request_body(spec: &QuerySpec) -> serde_json::Value {
        serde_json::json!({
            "query": spec.text,
            "options": spec.options,
        })
    }
}

impl RetrievalAdapter for AugmentAdapter {
    fn identity(&self) -> ProviderIdentity {
        ProviderIdentity {
            name: "augment".into(),
            version: format!("direct-context-v1/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    fn query(&self, spec: &QuerySpec, budget: &Budget) -> AdapterOutcome {
        let Some(token) = &self.token else {
            return AdapterOutcome::Unavailable {
                reason: "AUGMENT_API_TOKEN not set — provider not configured".into(),
            };
        };
        let url = format!("{}/context/direct", self.base_url.trim_end_matches('/'));
        let body = match serde_json::to_vec(&Self::request_body(spec)) {
            Ok(b) => b,
            Err(e) => {
                return AdapterOutcome::Malformed {
                    reason: format!("request encoding failed: {e}"),
                }
            }
        };
        let mut attempt = 0u32;
        loop {
            // reqwest::blocking cannot be created or dropped on a tokio
            // executor thread (the CLI's `main` is async) — run the whole
            // exchange on a dedicated OS thread and join it.
            let exchange = std::thread::spawn({
                let url = url.clone();
                let token = token.clone();
                let body = body.clone();
                let timeout = self.timeout;
                move || -> Result<Vec<u8>, String> {
                    let resp = reqwest::blocking::Client::builder()
                        .timeout(timeout)
                        .build()
                        .and_then(|c| {
                            c.post(&url)
                                .bearer_auth(&token)
                                .header("content-type", "application/json")
                                .body(body)
                                .send()
                        })
                        .map_err(|e| format!("__TRANSPORT__{e}"))?;
                    let status = resp.status();
                    let mut payload = Vec::new();
                    let mut resp = resp;
                    use std::io::Read;
                    resp.read_to_end(&mut payload)
                        .map_err(|e| format!("__TRANSPORT__body read failed: {e}"))?;
                    if status.is_success() {
                        Ok(payload)
                    } else if status.as_u16() == 429 || status.as_u16() >= 500 {
                        Err(format!("__RETRY__{status}"))
                    } else {
                        Err(format!("__STATUS__{status}"))
                    }
                }
            });
            let exchanged = match exchange.join() {
                Ok(r) => r,
                Err(e) => {
                    return AdapterOutcome::Unavailable {
                        reason: format!("adapter thread panicked: {e:?}"),
                    }
                }
            };
            match exchanged {
                Err(msg) if msg.starts_with("__RETRY__") => {
                    attempt += 1;
                    if attempt > budget.max_retries {
                        return AdapterOutcome::Unavailable {
                            reason: format!(
                                "provider {} after {attempt} attempt(s)",
                                &msg["__RETRY__".len()..]
                            ),
                        };
                    }
                    continue;
                }
                Err(msg) if msg.starts_with("__STATUS__") => {
                    return AdapterOutcome::Unavailable {
                        reason: format!("provider returned {}", &msg["__STATUS__".len()..]),
                    };
                }
                Err(msg) => {
                    attempt += 1;
                    if attempt > budget.max_retries {
                        return AdapterOutcome::Unavailable {
                            reason: format!("{msg} after {attempt} attempt(s)"),
                        };
                    }
                    continue;
                }
                Ok(payload) => {
                    // Byte cap: truncate-detect before parsing.
                    if payload.len() as u64 > budget.max_bytes {
                        return AdapterOutcome::Partial {
                            snippets: vec![],
                            reason: format!(
                                "response {} bytes exceeds budget {} — refusing partial parse",
                                payload.len(),
                                budget.max_bytes
                            ),
                        };
                    }
                    return parse_augment_payload(&payload, budget.max_bytes);
                }
            }
        }
    }
}

/// Parse the DirectContext response shape: `{"chunks":[{"content": "...",
/// "filePath"?: "...", "index"?: N}, ...]}`. The provider supplies search
/// text; file provenance is only present when the provider includes it —
/// absent provenance is preserved as an unknown path, never guessed.
fn parse_augment_payload(payload: &[u8], max_bytes: u64) -> AdapterOutcome {
    let v: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            return AdapterOutcome::Malformed {
                reason: format!("response is not JSON: {e}"),
            }
        }
    };
    let Some(chunks) = v.get("chunks").and_then(|c| c.as_array()) else {
        return AdapterOutcome::Malformed {
            reason: "response missing `chunks` array".into(),
        };
    };
    if chunks.is_empty() {
        return AdapterOutcome::EmptySuccess;
    }
    let mut snippets = Vec::new();
    for c in chunks {
        let Some(text) = c.get("content").and_then(|t| t.as_str()) else {
            return AdapterOutcome::Malformed {
                reason: "chunk missing `content`".into(),
            };
        };
        let path = c
            .get("filePath")
            .and_then(|p| p.as_str())
            .unwrap_or("<unknown-provenance>")
            .to_string();
        // Ranges are not part of the demonstrated contract; a provider that
        // supplies them (startLine/endLine, 1-based) is honored, otherwise
        // the snippet is file/unknown-provenance level.
        let ranges = match (
            c.get("startLine").and_then(|l| l.as_u64()),
            c.get("endLine").and_then(|l| l.as_u64()),
        ) {
            (Some(s), Some(e)) if s >= 1 && e >= s => vec![(s as u32, e as u32)],
            _ => vec![],
        };
        snippets.push(RawSnippet {
            path,
            ranges,
            text: text.to_string(),
            source_ref: c.get("index").map(|i| i.to_string()).unwrap_or_default(),
        });
    }
    if payload.len() as u64 >= max_bytes {
        return AdapterOutcome::Partial {
            snippets,
            reason: "response at byte budget — may be truncated".into(),
        };
    }
    AdapterOutcome::Results(snippets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_answers_seeded_queries_only() {
        let a = FakeAdapter::seeded(vec![(
            "q".into(),
            RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 2)],
                text: "t".into(),
                source_ref: "0".into(),
            },
        )]);
        let b = Budget::default();
        assert!(matches!(
            a.query(
                &QuerySpec {
                    text: "q".into(),
                    class: "implementation".into(),
                    options: serde_json::json!({})
                },
                &b
            ),
            AdapterOutcome::Results(_)
        ));
        assert!(matches!(
            a.query(
                &QuerySpec {
                    text: "other".into(),
                    class: "test".into(),
                    options: serde_json::json!({})
                },
                &b
            ),
            AdapterOutcome::EmptySuccess
        ));
        assert!(matches!(
            FakeAdapter::unavailable().query(
                &QuerySpec {
                    text: "q".into(),
                    class: "x".into(),
                    options: serde_json::json!({})
                },
                &b
            ),
            AdapterOutcome::Unavailable { .. }
        ));
    }

    #[test]
    fn augment_adapter_reports_unavailable_without_token() {
        // The test process does not carry AUGMENT_API_TOKEN (credential policy:
        // secrets never enter the environment of a test run).
        if std::env::var("AUGMENT_API_TOKEN").is_ok() {
            return; // provisioned host — skip the unconfigured-path assertion
        }
        let a = AugmentAdapter::from_env();
        assert!(!a.configured());
        let out = a.query(
            &QuerySpec {
                text: "q".into(),
                class: "implementation".into(),
                options: serde_json::json!({}),
            },
            &Budget::default(),
        );
        assert!(matches!(out, AdapterOutcome::Unavailable { .. }));
    }

    #[test]
    fn payload_parsing_outcomes() {
        let ok = br#"{"chunks":[{"content":"fn a(){}","filePath":"src/a.rs","startLine":1,"endLine":2,"index":0}]}"#;
        assert!(matches!(parse_augment_payload(ok, u64::MAX), AdapterOutcome::Results(_)));
        let empty = br#"{"chunks":[]}"#;
        assert!(matches!(parse_augment_payload(empty, u64::MAX), AdapterOutcome::EmptySuccess));
        let malformed = br#"{"nope":true}"#;
        assert!(matches!(
            parse_augment_payload(malformed, u64::MAX),
            AdapterOutcome::Malformed { .. }
        ));
        let not_json = b"hello";
        assert!(matches!(
            parse_augment_payload(not_json, u64::MAX),
            AdapterOutcome::Malformed { .. }
        ));
    }

    #[test]
    fn unknown_provenance_is_preserved_never_guessed() {
        let payload = br#"{"chunks":[{"content":"x"}]}"#;
        match parse_augment_payload(payload, u64::MAX) {
            AdapterOutcome::Results(snippets) => {
                assert_eq!(snippets[0].path, "<unknown-provenance>");
                assert!(snippets[0].ranges.is_empty());
            }
            other => panic!("expected results, got {other:?}"),
        }
    }
}
