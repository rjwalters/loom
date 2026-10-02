//! The real DirectContext HTTP contract, reverse-engineered from the
//! vendored `auggie_sdk` Python source (#9930) — the vendor knowledge that
//! was previously missing.
//!
//! # The discovered contract
//!
//! All calls are `POST {api_url}/{endpoint}` with `Authorization: Bearer
//! {key}`, `X-Request-Session-Id`/`X-Request-Id` (uuid), and a SDK-style
//! user agent. Retrieval is **stateful**, not the single-shot
//! `{base}/context/direct` shape this crate's adapter previously assumed
//! (that endpoint does not exist in the SDK — it was never live-validated):
//!
//! 1. `find-missing` `{"mem_object_names": [names]}` →
//!    `{"unknown_memory_names", "nonindexed_blob_names"}` — both lists are
//!    names the server wants uploaded;
//! 2. `batch-upload` `{"blobs": [{"blob_name", "path", "content"}]}`;
//! 3. `checkpoint-blobs` `{"blobs": {"checkpoint_id", "added_blobs",
//!    "deleted_blobs"}}` → `{"new_checkpoint_id"}`;
//! 4. `agents/codebase-retrieval` `{"information_request", "blobs",
//!    "dialog": [], "max_output_length"?}` → `{"formatted_retrieval"}`.
//!
//! A blob name is `sha256(path_utf8 ++ content_utf8)` hex — byte-for-byte
//! parity with the SDK's `BlobNameCalculator` (locked by a cross-language
//! test vector). Retriable statuses per the SDK's context retry policy:
//! 499, 503, and the 5xx range — deliberately **not** 429/504.
//!
//! Credentials are a *pair*: the SDK requires api key **and** api url from
//! the same source (env pair `AUGMENT_API_TOKEN`+`AUGMENT_API_URL`, or the
//! auggie session's `accessToken`+`tenantURL`). This explains the
//! experiments' 401: the SSM token was used without its matching tenant
//! URL. The proven-working pair is the auggie session's.
//!
//! # Status
//!
//! Transport + flow are implemented here with a mockable transport seam
//! (request shapes locked by tests against the SDK payloads). The adapter
//! (`super::adapter::AugmentAdapter`) is not yet wired to this contract —
//! swapping its `/context/direct` shape is a focused follow-up slice, since
//! it invalidates #9783's designed request shape and its tests. Retrieval
//! returns the server's `formatted_retrieval` text; parsing it into
//! structured (path, range) snippets needs one live response sample, so v1
//! callers treat it as explicitly unknown-provenance text (the session
//! contract handles that honestly) until then.

use serde_json::json;

use sha2::{Digest, Sha256};

/// A file from the pinned revision, with its computed blob name.
#[derive(Debug, Clone)]
pub struct PinnedBlob {
    pub path: String,
    pub contents: String,
    pub blob_name: String,
}

/// The pinned revision's file set, capped the way the experiment driver
/// capped it (`max_files`, per-file byte cap with counted skips).
#[derive(Debug, Clone, Default)]
pub struct PinnedBlobSet {
    pub blobs: Vec<PinnedBlob>,
    pub skipped_oversize: usize,
    pub skipped_unreadable: usize,
}

impl PinnedBlobSet {
    /// Read the pinned revision's tree via git. `max_blob_bytes` caps each
    /// file (the driver used 512 000); unreadable files are skipped and
    /// counted, never guessed at.
    pub fn from_pinned_tree(
        repo: &std::path::Path,
        revision: &str,
        max_files: usize,
        max_blob_bytes: usize,
    ) -> anyhow::Result<Self> {
        let listing = super::super::overlap_replay::patch::git(
            repo,
            &["ls-tree", "-r", "--name-only", revision],
        )?;
        let mut set = Self::default();
        for path in listing.lines().filter(|l| !l.is_empty()) {
            if set.blobs.len() >= max_files {
                break;
            }
            let contents = match super::super::overlap_replay::patch::git(
                repo,
                &["show", &format!("{revision}:{path}")],
            ) {
                Ok(c) => c,
                Err(_) => {
                    set.skipped_unreadable += 1;
                    continue;
                }
            };
            if contents.len() > max_blob_bytes {
                set.skipped_oversize += 1;
                continue;
            }
            set.blobs.push(PinnedBlob {
                blob_name: blob_name(path, &contents),
                path: path.to_string(),
                contents,
            });
        }
        Ok(set)
    }
}

/// `sha256(path_utf8 ++ content_utf8)` hex — byte-for-byte parity with the
/// SDK's `BlobNameCalculator._hash` (cross-language test vector locks it).
pub fn blob_name(path: &str, contents: &str) -> String {
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(contents.as_bytes());
    hex::encode(h.finalize())
}

/// Server-side indexing is asynchronous: after upload, blobs sit in an
/// "uploaded but not yet indexed" state until the backend processes them.
/// The SDK waits by polling `find-missing` with the non-indexed list
/// merged in until it comes back empty.
#[derive(Debug, Clone)]
pub struct WaitPolicy {
    /// Poll interval before the backoff threshold (SDK: 3s).
    pub initial: std::time::Duration,
    /// Elapsed time after which the poll interval widens (SDK: 60s).
    pub backoff_after: std::time::Duration,
    /// Poll interval after the threshold (SDK: 60s).
    pub backoff_interval: std::time::Duration,
    /// Total wait budget before giving up (SDK: 600s = 10 minutes).
    pub max_wait: std::time::Duration,
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self {
            initial: std::time::Duration::from_secs(3),
            backoff_after: std::time::Duration::from_secs(60),
            backoff_interval: std::time::Duration::from_secs(60),
            max_wait: std::time::Duration::from_secs(600),
        }
    }
}

/// The SDK batches `find-missing` at 1000 names per call.
pub const MAX_FIND_MISSING_BATCH: usize = 1000;

/// A failed DirectContext call. `status` is the HTTP status when the round
/// trip completed; `retryable()` mirrors the SDK's context retry policy:
/// 499, 503, and the 5xx range — deliberately not 429/504.
#[derive(Debug, Clone)]
pub struct DirectError {
    pub status: Option<u16>,
    pub message: String,
}

impl DirectError {
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
        }
    }

    pub fn retryable(&self) -> bool {
        match self.status {
            Some(s) => s == 499 || s == 503 || (500..600).contains(&s),
            None => false,
        }
    }
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => write!(f, "transport: {}", self.message),
        }
    }
}

impl std::error::Error for DirectError {}

/// The transport boundary: one JSON POST, one response JSON. Mocked in
/// tests to lock the wire shapes; the real impl mirrors the adapter's
/// dedicated-thread reqwest::blocking pattern.
pub trait DirectTransport {
    fn post(
        &self,
        endpoint: &str,
        payload: serde_json::Value,
        request_id: &str,
    ) -> Result<serde_json::Value, DirectError>;
}

/// The real transport: reqwest (rustls) on a dedicated OS thread —
/// `reqwest::blocking` cannot be created or dropped on a tokio executor
/// thread (same pattern as the adapter's existing exchange path).
pub struct HttpDirectTransport {
    pub api_url: String,
    pub api_key: String,
    pub session_id: String,
    pub timeout: std::time::Duration,
}

impl DirectTransport for HttpDirectTransport {
    fn post(
        &self,
        endpoint: &str,
        payload: serde_json::Value,
        request_id: &str,
    ) -> Result<serde_json::Value, DirectError> {
        let url = format!("{}/{}", self.api_url.trim_end_matches('/'), endpoint);
        let endpoint = endpoint.to_string();
        let auth = format!("Bearer {}", self.api_key);
        let session_id = self.session_id.clone();
        let request_id = request_id.to_string();
        let user_agent = format!("augment.sdk.context/{} (loom-daemon)", env!("CARGO_PKG_VERSION"));
        let timeout = self.timeout;
        let exchange = std::thread::spawn(move || -> Result<serde_json::Value, DirectError> {
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| DirectError::transport(format!("client build: {e}")))?;
            let resp = client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", auth)
                .header("X-Request-Session-Id", session_id)
                .header("X-Request-Id", request_id)
                .header("User-Agent", user_agent)
                .json(&payload)
                .send()
                .map_err(|e| DirectError::transport(e.to_string()))?;
            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(DirectError::http(status, format!("{endpoint} failed")));
            }
            resp.json::<serde_json::Value>()
                .map_err(|e| DirectError::transport(format!("response decode: {e}")))
        });
        exchange.join().map_err(|_| {
            DirectError::transport("direct-context exchange thread panicked".to_string())
        })?
    }
}

/// The DirectContext client: exact endpoint payloads per the SDK, with the
/// SDK's retry semantics (stable request id across retries of one call;
/// 499/503/5xx retriable) and bounded attempts.
pub struct DirectClient<'a> {
    pub transport: &'a dyn DirectTransport,
    pub session_id: String,
    /// Backoff between retries; zero in tests.
    pub backoff: std::time::Duration,
    pub max_attempts: u32,
    /// Indexing-wait policy; zeroed in tests.
    pub wait_policy: WaitPolicy,
}

impl<'a> DirectClient<'a> {
    pub fn new(transport: &'a dyn DirectTransport) -> Self {
        Self {
            transport,
            session_id: uuid::Uuid::new_v4().to_string(),
            backoff: std::time::Duration::from_secs(1),
            max_attempts: 3,
            wait_policy: WaitPolicy::default(),
        }
    }

    fn call(
        &self,
        endpoint: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, DirectError> {
        // One stable request id across this call's retries, mirroring the
        // SDK's `_call_api_with_retry`.
        let request_id = uuid::Uuid::new_v4().to_string();
        for attempt in 0..self.max_attempts {
            match self.transport.post(endpoint, payload.clone(), &request_id) {
                Ok(v) => return Ok(v),
                Err(e) if e.retryable() && attempt + 1 < self.max_attempts => {
                    std::thread::sleep(self.backoff);
                }
                Err(e) => return Err(e),
            }
        }
        Err(DirectError::transport(format!("{endpoint}: retry attempts exhausted")))
    }

    /// `find-missing`: which of these blob names does the server want?
    ///
    /// Mirrors the SDK's client-side semantics over the same endpoint: with
    /// `include_non_indexed = false` (the indexing step's need) only
    /// `unknown_memory_names` count as missing; with `true` (the
    /// indexing-wait step's need) `nonindexed_blob_names` — uploaded but
    /// not yet processed — merge in. Names are chunked at
    /// [`MAX_FIND_MISSING_BATCH`] like the SDK.
    pub fn find_missing(
        &self,
        blob_names: &[String],
        include_non_indexed: bool,
    ) -> Result<Vec<String>, DirectError> {
        let mut out = Vec::new();
        for chunk in blob_names.chunks(MAX_FIND_MISSING_BATCH) {
            let resp = self.call("find-missing", json!({ "mem_object_names": chunk }))?;
            let mut keys = vec!["unknown_memory_names"];
            if include_non_indexed {
                keys.push("nonindexed_blob_names");
            }
            for key in keys {
                if let Some(list) = resp.get(key).and_then(|v| v.as_array()) {
                    for name in list {
                        if let Some(n) = name.as_str() {
                            if !out.iter().any(|existing: &String| existing == n) {
                                out.push(n.to_string());
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Wait until the backend has indexed every named blob: poll
    /// `find-missing` with the non-indexed list merged in until it comes
    /// back empty, per the SDK's timing (3s polls, widening to 60s after a
    /// minute, 10-minute budget — see [`WaitPolicy`]).
    pub fn wait_for_indexing(&self, blob_names: &[String]) -> Result<(), DirectError> {
        let started = std::time::Instant::now();
        loop {
            let pending = self.find_missing(blob_names, true)?;
            if pending.is_empty() {
                return Ok(());
            }
            let elapsed = started.elapsed();
            if elapsed >= self.wait_policy.max_wait {
                return Err(DirectError::transport(format!(
                    "indexing timeout: backend did not finish indexing within {}s \
                     ({} blob(s) still pending)",
                    self.wait_policy.max_wait.as_secs(),
                    pending.len()
                )));
            }
            let interval = if elapsed < self.wait_policy.backoff_after {
                self.wait_policy.initial
            } else {
                self.wait_policy.backoff_interval
            };
            std::thread::sleep(interval);
        }
    }

    /// `batch-upload`: content-addressed blobs the server is missing.
    pub fn batch_upload(&self, blobs: &[&PinnedBlob]) -> Result<(), DirectError> {
        let payload = json!({
            "blobs": blobs
                .iter()
                .map(|b| json!({
                    "blob_name": b.blob_name,
                    "path": b.path,
                    "content": b.contents,
                }))
                .collect::<Vec<_>>(),
        });
        self.call("batch-upload", payload)?;
        Ok(())
    }

    /// `checkpoint-blobs`: persist the blob set, get the checkpoint id.
    pub fn checkpoint_blobs(
        &self,
        checkpoint_id: Option<&str>,
        added: &[String],
        deleted: &[String],
    ) -> Result<String, DirectError> {
        let resp = self.call(
            "checkpoint-blobs",
            json!({
                "blobs": {
                    "checkpoint_id": checkpoint_id,
                    "added_blobs": added,
                    "deleted_blobs": deleted,
                }
            }),
        )?;
        resp.get("new_checkpoint_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                DirectError::transport(
                    "checkpoint-blobs response missing new_checkpoint_id".to_string(),
                )
            })
    }

    /// `agents/codebase-retrieval`: the search. Returns the server's
    /// formatted retrieval text.
    pub fn agent_codebase_retrieval(
        &self,
        query: &str,
        checkpoint_id: &str,
        max_output_length: Option<u32>,
    ) -> Result<String, DirectError> {
        let mut payload = json!({
            "information_request": query,
            "blobs": {
                "checkpoint_id": checkpoint_id,
                "added_blobs": [],
                "deleted_blobs": [],
            },
            "dialog": [],
        });
        if let Some(max) = max_output_length {
            payload["max_output_length"] = json!(max);
        }
        let resp = self.call("agents/codebase-retrieval", payload)?;
        resp.get("formatted_retrieval")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                DirectError::transport(
                    "agents/codebase-retrieval response missing formatted_retrieval".to_string(),
                )
            })
    }
}

/// First-time indexing of a pinned blob set: find-missing → upload the
/// missing subset → checkpoint. Returns the checkpoint id searches
/// reference. (Incremental re-indexing of a changed revision is a
/// follow-up; v1 re-indexes a fresh revision from scratch.)
pub fn ensure_index(client: &DirectClient, set: &PinnedBlobSet) -> Result<String, DirectError> {
    let names: Vec<String> = set.blobs.iter().map(|b| b.blob_name.clone()).collect();
    // Indexing semantics: only blobs the server has never seen get
    // uploaded (unknown_memory_names — not the merged list).
    let missing = client.find_missing(&names, false)?;
    let to_upload: Vec<&PinnedBlob> = missing
        .iter()
        .filter_map(|n| set.blobs.iter().find(|b| &b.blob_name == n))
        .collect();
    if !to_upload.is_empty() {
        client.batch_upload(&to_upload)?;
    }
    let checkpoint = client.checkpoint_blobs(None, &names, &[])?;
    // Server-side indexing is async: block until every blob is searchable
    // (the SDK requires callers to wait before searching).
    client.wait_for_indexing(&names)?;
    Ok(checkpoint)
}

/// One retrieval against an indexed checkpoint.
pub fn retrieve(
    client: &DirectClient,
    checkpoint_id: &str,
    query: &str,
    max_output_length: Option<u32>,
) -> Result<String, DirectError> {
    client.agent_codebase_retrieval(query, checkpoint_id, max_output_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Records every (endpoint, payload) and replays scripted results.
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

        fn calls(&self) -> Vec<(String, serde_json::Value)> {
            self.calls.lock().unwrap().clone()
        }

        fn post_inner(
            &self,
            endpoint: &str,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, DirectError> {
            self.calls
                .lock()
                .unwrap()
                .push((endpoint.to_string(), payload));
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock script exhausted")
        }
    }

    impl DirectTransport for MockTransport {
        fn post(
            &self,
            endpoint: &str,
            payload: serde_json::Value,
            _request_id: &str,
        ) -> Result<serde_json::Value, DirectError> {
            self.post_inner(endpoint, payload)
        }
    }

    fn client<'a>(mock: &'a Arc<MockTransport>) -> DirectClient<'a> {
        let mut c = DirectClient::new(mock.as_ref());
        c.backoff = std::time::Duration::ZERO;
        c.wait_policy = WaitPolicy {
            initial: std::time::Duration::ZERO,
            backoff_after: std::time::Duration::ZERO,
            backoff_interval: std::time::Duration::ZERO,
            max_wait: std::time::Duration::ZERO,
        };
        c
    }

    fn blob(path: &str, contents: &str) -> PinnedBlob {
        PinnedBlob {
            blob_name: blob_name(path, contents),
            path: path.to_string(),
            contents: contents.to_string(),
        }
    }

    #[test]
    fn blob_name_matches_the_sdk_calculator_byte_for_byte() {
        // Vector generated from the vendored Python implementation:
        //   hashlib.sha256(b"src/a.rs" + b"fn main() {}\n").hexdigest()
        assert_eq!(
            blob_name("src/a.rs", "fn main() {}\n"),
            "8525ac7224a24e0387d184cda7694be003a53989528b2ce44437c9bda7e6e7a3"
        );
    }

    #[test]
    fn find_missing_sends_the_sdk_payload_and_merges_both_name_lists() {
        let mock = Arc::new(MockTransport::new(vec![Ok(json!({
            "unknown_memory_names": ["b1"],
            "nonindexed_blob_names": ["b2"],
        }))]));
        let c = client(&mock);
        // Indexing step: include_non_indexed = false -> unknown only.
        let missing = c.find_missing(&["b1".into(), "b2".into()], false).unwrap();
        assert_eq!(missing, vec!["b1"]);
        let calls = mock.calls();
        assert_eq!(calls[0].0, "find-missing");
        assert_eq!(calls[0].1.get("mem_object_names").unwrap(), &json!(["b1", "b2"]));
    }

    #[test]
    fn find_missing_wait_semantics_merge_nonindexed() {
        let mock = Arc::new(MockTransport::new(vec![Ok(json!({
            "unknown_memory_names": [],
            "nonindexed_blob_names": ["b2"],
        }))]));
        let c = client(&mock);
        // Indexing-wait step: both lists count as pending.
        let pending = c.find_missing(&["b2".into()], true).unwrap();
        assert_eq!(pending, vec!["b2"]);
    }

    #[test]
    fn ensure_index_uploads_only_missing_then_checkpoints_the_full_set() {
        let a = blob("src/a.rs", "fn a() {}\n");
        let b = blob("src/b.rs", "fn b() {}\n");
        let set = PinnedBlobSet {
            blobs: vec![a.clone(), b.clone()],
            skipped_oversize: 0,
            skipped_unreadable: 0,
        };
        // Server already has a's blob; wants b's upload AND an indexing wait.
        let mock = Arc::new(MockTransport::new(vec![
            Ok(json!({"unknown_memory_names": [b.blob_name], "nonindexed_blob_names": []})),
            Ok(json!({"blob_names": [b.blob_name]})),
            Ok(json!({"new_checkpoint_id": "cp-1"})),
            // wait_for_indexing poll: everything indexed.
            Ok(json!({"unknown_memory_names": [], "nonindexed_blob_names": []})),
        ]));
        let c = client(&mock);
        let checkpoint = ensure_index(&c, &set).unwrap();
        assert_eq!(checkpoint, "cp-1");
        let calls = mock.calls();
        assert_eq!(
            calls.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
            vec![
                "find-missing",
                "batch-upload",
                "checkpoint-blobs",
                "find-missing"
            ]
        );
        // batch-upload carries ONLY the missing blob, exact field shapes.
        let uploaded = calls[1].1.get("blobs").unwrap().as_array().unwrap();
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0].get("blob_name").unwrap(), &json!(b.blob_name));
        assert_eq!(uploaded[0].get("path").unwrap(), &json!("src/b.rs"));
        assert_eq!(uploaded[0].get("content").unwrap(), &json!("fn b() {}\n"));
        // checkpoint references the full set with a null prior checkpoint.
        let blobs = calls[2].1.get("blobs").unwrap();
        assert_eq!(blobs.get("checkpoint_id").unwrap(), &serde_json::Value::Null);
        assert_eq!(blobs.get("added_blobs").unwrap(), &json!([a.blob_name, b.blob_name]));
        assert_eq!(blobs.get("deleted_blobs").unwrap(), &json!([]));
    }

    #[test]
    fn retrieval_sends_the_sdk_payload_and_extracts_formatted_retrieval() {
        let mock = Arc::new(MockTransport::new(vec![Ok(json!({
            "formatted_retrieval": "…evidence…"
        }))]));
        let c = client(&mock);
        let text = retrieve(&c, "cp-1", "implementation sites for x", Some(20_000)).unwrap();
        assert_eq!(text, "…evidence…");
        let calls = mock.calls();
        assert_eq!(calls[0].0, "agents/codebase-retrieval");
        let p = &calls[0].1;
        assert_eq!(p.get("information_request").unwrap(), &json!("implementation sites for x"));
        assert_eq!(p.get("blobs").unwrap().get("checkpoint_id").unwrap(), &json!("cp-1"));
        assert_eq!(p.get("blobs").unwrap().get("added_blobs").unwrap(), &json!([]));
        assert_eq!(p.get("dialog").unwrap(), &json!([]));
        assert_eq!(p.get("max_output_length").unwrap(), &json!(20_000));
    }

    #[test]
    fn retries_retriable_statuses_and_fails_fast_otherwise() {
        // 503 twice, then success → 3 attempts total.
        let mock = Arc::new(MockTransport::new(vec![
            Err(DirectError::http(503, "unavailable")),
            Err(DirectError::http(503, "unavailable")),
            Ok(json!({"formatted_retrieval": "ok"})),
        ]));
        let c = client(&mock);
        assert_eq!(retrieve(&c, "cp", "q", None).unwrap(), "ok");
        assert_eq!(mock.calls().len(), 3);

        // 401 is not retriable → single attempt, loud failure.
        let mock = Arc::new(MockTransport::new(vec![Err(DirectError::http(401, "denied"))]));
        let c = client(&mock);
        let err = retrieve(&c, "cp", "q", None).unwrap_err();
        assert_eq!(err.status, Some(401));
        assert!(!err.retryable());
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn wait_for_indexing_polls_until_empty_then_succeeds() {
        let mock = Arc::new(MockTransport::new(vec![
            Ok(json!({"unknown_memory_names": [], "nonindexed_blob_names": ["b1"]})),
            Ok(json!({"unknown_memory_names": [], "nonindexed_blob_names": []})),
        ]));
        let mut c = client(mock.clone());
        // A small nonzero budget: the first poll comes back pending, the
        // second empty — with a zero budget the timeout would fire before
        // the second poll (as the SDK's semantics dictate).
        c.wait_policy.max_wait = std::time::Duration::from_millis(50);
        c.wait_for_indexing(&["b1".into()]).unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn wait_for_indexing_times_out_when_blobs_stay_pending() {
        let mock = Arc::new(MockTransport::new(vec![Ok(
            json!({"unknown_memory_names": [], "nonindexed_blob_names": ["b1"]}),
        )]));
        let c = client(&mock);
        let err = c.wait_for_indexing(&["b1".into()]).unwrap_err();
        assert!(err.message.contains("indexing timeout"));
        assert!(err.message.contains("1 blob(s) still pending"));
        assert_eq!(mock.calls().len(), 1);
    }
}
