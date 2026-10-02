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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::augment_direct::{
    ensure_index, retrieve, DirectClient, DirectTransport, HttpDirectTransport, PinnedBlobSet,
    WaitPolicy,
};

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
    /// Overrides the reported identity version (test seam; `None` = the
    /// constant `fake-1`, or the `LOOM_FAKE_ADAPTER_VERSION` env override).
    pub version: Option<String>,
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
            version: None,
        }
    }

    pub fn unavailable() -> Self {
        Self {
            seeded: Default::default(),
            unavailable: true,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            version: None,
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Override the reported identity version (test seam for cache
    /// re-keying assertions).
    pub fn with_version(mut self, version: &str) -> Self {
        self.version = Some(version.into());
        self
    }
}

impl RetrievalAdapter for FakeAdapter {
    fn identity(&self) -> ProviderIdentity {
        // Test seam (same pattern as LOOM_BRANCH_LANDED_GIT_VERSION): the
        // struct field, else the `LOOM_FAKE_ADAPTER_VERSION` env override,
        // lets tests vary the adapter's reported schema version through the
        // real binary and observe cache re-keying. Production runs never set
        // either, so the version is the constant `fake-1`.
        let version = self
            .version
            .clone()
            .or_else(|| {
                std::env::var("LOOM_FAKE_ADAPTER_VERSION")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "fake-1".into());
        ProviderIdentity {
            name: "fake".into(),
            version,
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
/// Configuration (never stored in artifacts — identity carries only name +
/// version): `AUGMENT_SESSION_FILE`, `AUGMENT_API_TOKEN`, optional
/// `AUGMENT_BASE_URL` (default `https://api.augmentcode.com`), optional
/// `AUGMENT_TIMEOUT_SECS`. When unconfigured the adapter returns
/// [`AdapterOutcome::Unavailable`] — consumers see explicit unavailable
/// evidence, never fabricated results.
///
/// Credential resolution (#9930): `AUGMENT_SESSION_FILE` — a mounted copy
/// of the auggie CLI session JSON (`accessToken`, usually `tenantURL`) —
/// takes precedence over `AUGMENT_API_TOKEN`, because the SSM-stored API
/// token is known to 401 on the context engine while the session
/// credential is the proven-working path. A session file that fails to
/// load is a hard unconfigured state: no silent fallback to an API token
/// that would send a known-broken credential. Failure reasons name the
/// failure mode but never quote file contents or token material.
pub struct AugmentAdapter {
    token: Option<String>,
    /// Why the adapter is unconfigured, when it is — surfaced verbatim as
    /// the Unavailable reason instead of a generic guess.
    config_reason: Option<String>,
    /// The pinned revision to index, as (repo checkout, revision) — set
    /// for production construction. The blob set is built lazily on the
    /// first query and memoized (a cache hit never pays for it).
    pinned_tree: Option<(std::path::PathBuf, String)>,
    /// Lazy blob-set result, memoized (including the failure reason).
    blob_set: OnceLock<Result<Arc<PinnedBlobSet>, String>>,
    /// Memoized checkpoint id once indexing succeeds (per instance).
    checkpoint: Mutex<Option<String>>,
    /// The built-in real transport (built once, from the credentials).
    http: HttpDirectTransport,
    /// Test seam: when set, used instead of `http`.
    transport: Option<Box<dyn DirectTransport>>,
}

/// Driver-parity caps for indexing a pinned tree (the experiment driver's
/// `--max-files` / 512k per-file char cap).
const MAX_INDEX_FILES: usize = 400;
const MAX_BLOB_BYTES: usize = 512_000;

/// Load an auggie CLI session credential (#9930): `path` points at a
/// session JSON with `accessToken` and usually `tenantURL`. Returns
/// `(bearer_token, tenant_url)`. Error reasons name the failure mode but
/// never quote file contents or token material.
fn load_session_credentials(path: &str) -> Result<(String, Option<String>), String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read session credential file: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|_| "session credential file is not valid JSON".to_string())?;
    let token = parsed
        .get("accessToken")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or("session credential file has no usable accessToken")?
        .to_string();
    let tenant = parsed
        .get("tenantURL")
        .and_then(|v| v.as_str())
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string());
    Ok((token, tenant))
}

impl AugmentAdapter {
    pub fn from_env() -> Self {
        let session_file = std::env::var("AUGMENT_SESSION_FILE")
            .ok()
            .filter(|p| !p.is_empty());
        let api_token = std::env::var("AUGMENT_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let (token, tenant_url, config_reason) = match session_file {
            Some(path) => match load_session_credentials(&path) {
                Ok((tok, tenant)) => (Some(tok), tenant, None),
                Err(reason) => {
                    (None, None, Some(format!("AUGMENT_SESSION_FILE ({path}) unusable: {reason}")))
                }
            },
            None => match api_token {
                Some(tok) => (Some(tok), None, None),
                None => (
                    None,
                    None,
                    Some(
                        "neither AUGMENT_SESSION_FILE nor AUGMENT_API_TOKEN set — \
                         provider not configured"
                            .into(),
                    ),
                ),
            },
        };
        // Explicit operator base wins; otherwise the session's tenant URL
        // (the credential's own API base); otherwise the generic default.
        let base_url = match std::env::var("AUGMENT_BASE_URL")
            .ok()
            .filter(|u| !u.is_empty())
        {
            Some(u) => u,
            None => tenant_url.unwrap_or_else(|| "https://api.augmentcode.com".into()),
        };
        let http = HttpDirectTransport {
            api_url: base_url.clone(),
            api_key: token.clone().unwrap_or_default(),
            session_id: uuid::Uuid::new_v4().to_string(),
            timeout: Duration::from_secs(
                std::env::var("AUGMENT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60),
            ),
        };
        Self {
            token,
            config_reason,
            pinned_tree: None,
            blob_set: OnceLock::new(),
            checkpoint: Mutex::new(None),
            http,
            transport: None,
        }
    }

    pub fn configured(&self) -> bool {
        self.token.is_some()
    }

    /// Point the adapter at the pinned revision's checkout so `query` can
    /// index + search it (#9930). The blob set is built lazily on the
    /// first query — a cache hit never pays for the read.
    pub fn with_pinned_tree(mut self, repo: std::path::PathBuf, revision: String) -> Self {
        self.pinned_tree = Some((repo, revision));
        self
    }

    fn resolve_blob_set(&self) -> Result<Arc<PinnedBlobSet>, String> {
        if let Some(ready) = self.blob_set.get() {
            return ready.clone();
        }
        let result = match &self.pinned_tree {
            Some((repo, revision)) => PinnedBlobSet::from_pinned_tree(
                repo,
                revision,
                MAX_INDEX_FILES,
                MAX_BLOB_BYTES,
            )
            .map_err(|e| e.to_string())
            .map(Arc::new),
            None => Err("no pinned tree configured — the context engine indexes a                          file set; construct with `with_pinned_tree`"
                .to_string()),
        };
        let _ = self.blob_set.set(result.clone());
        result
    }
}

impl RetrievalAdapter for AugmentAdapter {
    fn identity(&self) -> ProviderIdentity {
        ProviderIdentity {
            name: "augment".into(),
            version: format!("direct-context-v2/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    fn query(&self, spec: &QuerySpec, budget: &Budget) -> AdapterOutcome {
        let Some(token) = &self.token else {
            return AdapterOutcome::Unavailable {
                reason: self.config_reason.clone().unwrap_or_else(|| {
                    "AUGMENT_API_TOKEN not set — provider not configured".into()
                }),
            };
        };
        let _ = token;
        // Resolve the pinned blob set (lazy, memoized): the context engine
        // indexes a file set and searches a checkpoint — a stateful flow,
        // per the vendored SDK (#9930). No blob set → clean Unavailable.
        let blob_set = match self.resolve_blob_set() {
            Ok(b) => b,
            Err(reason) => return AdapterOutcome::Unavailable { reason },
        };
        let transport: &dyn DirectTransport = match &self.transport {
            Some(t) => t.as_ref(),
            None => &self.http,
        };
        let direct = DirectClient {
            transport,
            session_id: uuid::Uuid::new_v4().to_string(),
            backoff: std::time::Duration::from_secs(1),
            max_attempts: budget.max_retries.saturating_add(1).max(1),
            wait_policy: WaitPolicy::default(),
        };
        // Index once per adapter instance; the checkpoint is memoized. The
        // blob names are content-addressed, so a re-run is cheap: the
        // server already has the blobs, and only the checkpoint + the
        // indexing wait re-run.
        let mut checkpoint = self.checkpoint.lock().unwrap_or_else(|p| p.into_inner());
        if checkpoint.is_none() {
            match ensure_index(&direct, &blob_set) {
                Ok(id) => *checkpoint = Some(id),
                Err(e) => {
                    return AdapterOutcome::Unavailable {
                        reason: format!(
                            "direct-context indexing failed after {} attempt(s): {e}",
                            direct.max_attempts
                        ),
                    }
                }
            }
        }
        let checkpoint_id = checkpoint.as_deref().unwrap_or_default();
        // The server's own output cap doubles as the session byte budget's
        // enforcement point (default 20000 chars, max 80000).
        let max_output = u32::try_from(budget.max_bytes).unwrap_or(u32::MAX);
        match retrieve(&direct, checkpoint_id, &spec.text, Some(max_output)) {
            Ok(text) => {
                // Byte cap: truncate-detect before parsing.
                if text.len() as u64 > budget.max_bytes {
                    return AdapterOutcome::Partial {
                        snippets: vec![],
                        reason: format!(
                            "response {} bytes exceeds budget {} — refusing partial parse",
                            text.len(),
                            budget.max_bytes
                        ),
                    };
                }
                AdapterOutcome::Results(parse_formatted_retrieval(&text))
            }
            Err(e) => AdapterOutcome::Unavailable {
                reason: format!("direct-context retrieval failed: {e}"),
            },
        }
    }
}

/// Parse the server's `formatted_retrieval` text into raw snippets. The
/// documented shape (SDK search docstring) is "file paths, line numbers,
/// and code content in a structured, readable format"; the exact syntax is
/// not yet pinned by a live sample (#9930 slice 4), so this parser is
/// deliberately conservative: a line that reads like a `path:line` /
/// `path:start-end` location (optionally with one trailing colon) opens a
/// snippet; everything that does not fit — including anything before the
/// first location — is retained as one explicit `<unknown-provenance>`
/// snippet. Nothing is dropped, nothing is guessed; revisit against a live
/// sample.
pub(crate) fn parse_formatted_retrieval(text: &str) -> Vec<RawSnippet> {
    fn flush_unknown(buf: &mut String, out: &mut Vec<RawSnippet>) {
        if !buf.trim().is_empty() {
            out.push(RawSnippet {
                path: "<unknown-provenance>".into(),
                ranges: vec![],
                text: std::mem::take(buf),
                source_ref: String::new(),
            });
        }
    }
    let mut out = Vec::new();
    let mut unknown = String::new();
    let mut current: Option<RawSnippet> = None;
    for line in text.lines() {
        match parse_location_header(line) {
            Some((path, range)) => {
                flush_unknown(&mut unknown, &mut out);
                if let Some(cur) = current.take() {
                    out.push(cur);
                }
                let mut ranges = Vec::new();
                if let Some((s, e)) = range {
                    ranges.push((s, e));
                }
                current = Some(RawSnippet {
                    path,
                    ranges,
                    text: String::new(),
                    source_ref: String::new(),
                });
            }
            None => match current.as_mut() {
                Some(cur) => {
                    cur.text.push_str(line);
                    cur.text.push('\n');
                }
                None => {
                    unknown.push_str(line);
                    unknown.push('\n');
                }
            },
        }
    }
    flush_unknown(&mut unknown, &mut out);
    if let Some(cur) = current.take() {
        out.push(cur);
    }
    out
}

/// A header line that reads like a code location: `path:line` /
/// `path:start-end`, optionally with one trailing colon. Code lines almost
/// never qualify: a non-numeric tail after the final `:` fails
/// (`Foo::bar`), arithmetic never ends in a bare integer after `:`, and
/// prose with spaces is rejected outright. The exact server syntax gets
/// pinned by a live sample (#9930 slice 4).
fn parse_location_header(line: &str) -> Option<(String, Option<(u32, u32)>)> {
    fn is_num(s: &str) -> bool {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
    }
    let mut trimmed = line.trim();
    if trimmed.is_empty() || trimmed.contains(' ') {
        return None;
    }
    trimmed = trimmed.strip_suffix(':').unwrap_or(trimmed);
    let (base, range) = {
        let (base, tail) = trimmed.rsplit_once(':')?;
        let range = if is_num(tail) {
            let n: u32 = tail.parse().ok()?;
            Some((n, n))
        } else {
            match tail.split_once('-') {
                Some((a, b)) if is_num(a) && is_num(b) => {
                    let s: u32 = a.parse().ok()?;
                    let e: u32 = b.parse().ok()?;
                    Some((s, e.max(s)))
                }
                _ => return None,
            }
        };
        (base, range)
    };
    if base.is_empty() || !(base.contains('/') || base.contains('.')) {
        return None;
    }
    Some((base.to_string(), range))
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
    fn formatted_retrieval_parser_locations_with_ranges() {
        let text = "src/a.rs:1-10:\nfn a() {}\nsrc/b.py:42:\nx = 1\n";
        let out = parse_formatted_retrieval(text);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "src/a.rs");
        assert_eq!(out[0].ranges, vec![(1, 10)]);
        assert_eq!(out[0].text, "fn a() {}\n");
        assert_eq!(out[1].path, "src/b.py");
        assert_eq!(out[1].ranges, vec![(42, 42)]);
    }

    #[test]
    fn formatted_retrieval_parser_unknown_provenance_fallback() {
        // No location headers → one explicit unknown-provenance snippet,
        // never dropped.
        let out = parse_formatted_retrieval("prose and code\nwithout locations\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "<unknown-provenance>");
        assert!(out[0].text.contains("without locations"));
    }

    #[test]
    fn formatted_retrieval_parser_mixed_prose_then_locations() {
        let text = "Overview of findings:\n\nsrc/a.rs:3:\nfn a() {}\n";
        let out = parse_formatted_retrieval(text);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "<unknown-provenance>");
        assert!(out[0].text.contains("Overview"));
        assert_eq!(out[1].path, "src/a.rs");
        assert_eq!(out[1].ranges, vec![(3, 3)]);
    }

    #[test]
    fn direct_flow_indexes_once_then_reuses_checkpoint() {
        use crate::context_cache::augment_direct::{DirectError, DirectTransport, PinnedBlob};
        use std::collections::VecDeque;

        struct MockTransport {
            script: Mutex<VecDeque<Result<serde_json::Value, DirectError>>>,
            calls: Mutex<Vec<(String, serde_json::Value)>>,
        }
        impl MockTransport {
            fn new(script: Vec<Result<serde_json::Value, DirectError>>) -> Self {
                Self {
                    script: Mutex::new(script.into_iter().collect::<VecDeque<_>>()),
                    calls: Mutex::new(Vec::new()),
                }
            }
            fn find_missing_calls(&self) -> usize {
                self.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(e, _)| e == "find-missing")
                    .count()
            }
            fn calls_total(&self) -> usize {
                self.calls.lock().unwrap().len()
            }
        }
        impl DirectTransport for MockTransport {
            fn post(
                &self,
                endpoint: &str,
                payload: serde_json::Value,
                _request_id: &str,
            ) -> Result<serde_json::Value, DirectError> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((endpoint.to_string(), payload));
                self.script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("script exhausted")
            }
        }
        impl DirectTransport for Arc<MockTransport> {
            fn post(
                &self,
                endpoint: &str,
                payload: serde_json::Value,
                request_id: &str,
            ) -> Result<serde_json::Value, DirectError> {
                (**self).post(endpoint, payload, request_id)
            }
        }

        let blob_set = Arc::new(PinnedBlobSet {
            blobs: vec![PinnedBlob {
                blob_name: "blob-1".into(),
                path: "src/a.rs".into(),
                contents: "fn a() {}\n".into(),
            }],
            skipped_oversize: 0,
            skipped_unreadable: 0,
        });
        let mock = Arc::new(MockTransport::new(vec![
            Ok(
                serde_json::json!({"unknown_memory_names": ["blob-1"], "nonindexed_blob_names": []}),
            ),
            Ok(serde_json::json!({"blob_names": ["blob-1"]})),
            Ok(serde_json::json!({"new_checkpoint_id": "cp-1"})),
            Ok(serde_json::json!({"unknown_memory_names": [], "nonindexed_blob_names": []})),
            Ok(serde_json::json!({"formatted_retrieval": "src/a.rs:1-2:\nfn a(){}\n"})),
            // Second query: checkpoint reused — only a retrieval call.
            Ok(serde_json::json!({"formatted_retrieval": "src/a.rs:1-2:\nfn a(){}\n"})),
        ]));
        let adapter = AugmentAdapter {
            token: Some("t".into()),
            config_reason: None,
            pinned_tree: None,
            blob_set: OnceLock::new(),
            checkpoint: Mutex::new(None),
            http: HttpDirectTransport {
                api_url: "https://x".into(),
                api_key: "t".into(),
                session_id: "s".into(),
                timeout: Duration::from_secs(1),
            },
            transport: Some(Box::new(mock.clone())),
        };
        adapter
            .blob_set
            .set(Ok(blob_set))
            .unwrap_or_else(|_| panic!("preset blob set"));
        let b = Budget::default();
        let spec = QuerySpec {
            text: "q".into(),
            class: "implementation".into(),
            options: serde_json::json!({}),
        };
        let out = adapter.query(&spec, &b);
        assert!(
            matches!(&out, AdapterOutcome::Results(s) if !s.is_empty()),
            "expected parsed snippets, got {out:?}"
        );
        let fm_after_first = mock.find_missing_calls();
        let calls_after_first = mock.calls_total();
        // Second query: checkpoint memoized — no re-index (the find-missing
        // count must not move; only a retrieval call is added).
        let out2 = adapter.query(&spec, &b);
        assert!(matches!(out2, AdapterOutcome::Results(_)));
        assert_eq!(
            mock.find_missing_calls(),
            fm_after_first,
            "checkpoint memoized: the second query must not re-index"
        );
        assert_eq!(mock.calls_total(), calls_after_first + 1);
    }

    #[test]
    fn unknown_provenance_is_preserved_never_guessed() {
        // The parser retains unmatched retrieval text as an explicit
        // <unknown-provenance> snippet (the session keeps unknown
        // provenance honest; provenance validation bypasses `<` paths).
        let out = parse_formatted_retrieval("just some text with no locations\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "<unknown-provenance>");
        assert!(out[0].ranges.is_empty());
        assert!(out[0].text.contains("just some text"));
    }

    #[test]
    fn session_credential_loader_accepts_auggie_session_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        std::fs::write(
            &p,
            r#"{"accessToken":"tok-fixture","tenantURL":"https://tenant.example.invalid/","scopes":[]}"#,
        )
        .unwrap();
        let (tok, tenant) = super::load_session_credentials(p.to_str().unwrap()).unwrap();
        assert_eq!(tok, "tok-fixture");
        assert_eq!(tenant.as_deref(), Some("https://tenant.example.invalid/"));
    }

    #[test]
    fn session_credential_loader_failures_never_leak_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        let marker = "tok-secret-marker-please-not-in-errors";
        // Malformed JSON.
        std::fs::write(&p, format!("not json {marker}")).unwrap();
        let err = super::load_session_credentials(p.to_str().unwrap()).unwrap_err();
        assert!(err.contains("not valid JSON"));
        assert!(!err.contains(marker), "error text leaked file contents: {err}");
        // Empty token.
        std::fs::write(&p, r#"{"accessToken":""}"#).unwrap();
        let err = super::load_session_credentials(p.to_str().unwrap()).unwrap_err();
        assert!(err.contains("no usable accessToken"));
        // Missing file.
        let err =
            super::load_session_credentials(dir.path().join("missing.json").to_str().unwrap())
                .unwrap_err();
        assert!(err.contains("cannot read session credential file"));
    }
}
