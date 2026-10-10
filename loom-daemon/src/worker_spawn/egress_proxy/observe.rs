//! Transparent observe mode for the egress proxy (issue #11300, slice 1).
//!
//! The proxy already sits on the request path of a proxied launch. Observe
//! mode makes it *read* the provider's responses passively and export
//! per-request telemetry (tokens, model, status, latency, time to first byte,
//! classified error code) through [`crate::observability::ops`]. It never
//! originates, alters or delays traffic:
//!
//! - **Request side is untouched.** The forwarded request is the client's own,
//!   minus the hop-by-hop set and the credential swap (pinned by the
//!   header-fidelity test in `observe_tests.rs`). Nothing Loom-identifying is
//!   added, in observe mode or out of it.
//! - **Response side is a passive tee.** [`Tap::feed`] sees each chunk *after*
//!   the forwarder has decided to relay it, is infallible, never awaits, and
//!   its parser failing (malformed JSON, oversize line, even a panic) only
//!   disables further parsing: the relayed bytes are unaffected.
//! - **Bounded.** An SSE stream keeps at most [`MAX_LINE_BYTES`] of one
//!   partial line; a non-streaming body is copied up to [`MAX_BODY_CAPTURE`].
//!   Nothing else is retained.
//! - **No bodies, no credentials leave.** Only counts, a closed error-code
//!   vocabulary, a sanitised model name and ids reach telemetry. An error body
//!   is classified to a code and dropped.
//!
//! Default off: a record carries an [`ObserveContext`] only when the profile's
//! `credentialProxy.observe` is true **and** [`enabled`] (env
//! `LOOM_EGRESS_PROXY_OBSERVE` over `runtimes.containment.credentialProxyObserve`).
//! Whether this reading of a provider's terms is acceptable is for the
//! operator to confirm with that provider before enabling it on a fleet.

use crate::api_keys_pool::classify;
use crate::observability::ops;
use crate::observability::ops::pool_marks::{provider_label, MarkReason};
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Longest partial SSE line kept while waiting for its newline. A longer line
/// is skipped, not parsed.
pub const MAX_LINE_BYTES: usize = 256 * 1024;
/// Most of a non-streaming response body copied for usage extraction.
pub const MAX_BODY_CAPTURE: usize = 1024 * 1024;
/// SSE `data:` lines parsed for a model name before only `usage` lines are.
const MODEL_PROBE_LINES: usize = 4;

/// Is observe mode switched on for this workspace? Env wins over config; off
/// by default. A profile must additionally opt in (`credentialProxy.observe`).
#[must_use]
pub fn enabled(config: &Value) -> bool {
    let truthy = |v: &str| matches!(v.trim(), "1" | "true" | "yes");
    if let Some(raw) = std::env::var("LOOM_EGRESS_PROXY_OBSERVE")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        return truthy(&raw);
    }
    match config.pointer("/runtimes/containment/credentialProxyObserve") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => truthy(s),
        _ => false,
    }
}

/// The non-secret tags attached to every observation of one launch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObserveContext {
    /// api-keys account name (never key material); `-` when not pool-selected.
    pub seat: String,
    /// `<runtime>[:<profile>]`.
    pub tap: String,
    pub profile: String,
    pub runtime: String,
    pub role: Option<String>,
    pub issue: Option<u32>,
    pub pr: Option<u32>,
    pub sweep_id: Option<String>,
}

impl ObserveContext {
    /// Tags for a launch, with role / issue / PR / sweep read from the
    /// launching process's environment (`LOOM_ROLE`, `LOOM_SWEEP_ID`,
    /// `LOOM_ISSUE_NUMBER` or the `sweep-issue-<N>-…` id, `LOOM_PR_NUMBER`).
    #[must_use]
    pub fn from_env(seat: &str, tap: &str, profile: &str, runtime: &str) -> Self {
        let get = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        let sweep_id = get("LOOM_SWEEP_ID");
        let issue = get("LOOM_ISSUE_NUMBER")
            .and_then(|v| v.trim().parse().ok())
            .or_else(|| {
                sweep_id
                    .as_deref()
                    .and_then(|id| id.strip_prefix("sweep-issue-"))
                    .and_then(|rest| rest.split('-').next())
                    .and_then(|n| n.parse().ok())
            });
        Self {
            seat: seat.to_string(),
            tap: tap.to_string(),
            profile: profile.to_string(),
            runtime: runtime.to_string(),
            role: get("LOOM_ROLE"),
            issue,
            pr: get("LOOM_PR_NUMBER").and_then(|v| v.trim().parse().ok()),
            sweep_id,
        }
    }
}

/// Token counts and model read off one response. `None` = not reported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub model: Option<String>,
    /// Uncached input tokens.
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

impl Usage {
    /// Fold one JSON object (an SSE event or a whole response) in. Later
    /// values overwrite earlier ones, which is right for both Anthropic
    /// (`message_start` carries input, `message_delta` the running output)
    /// and OpenAI-compatible streams (one final usage chunk).
    fn absorb(&mut self, value: &Value) {
        let model = value
            .get("model")
            .or_else(|| value.pointer("/message/model"))
            .and_then(Value::as_str);
        if let Some(model) = model {
            self.model = Some(model.to_string());
        }
        let Some(usage) = value
            .get("usage")
            .or_else(|| value.pointer("/message/usage"))
            .filter(|u| u.is_object())
        else {
            return;
        };
        let num = |ptr: &str| usage.pointer(ptr).and_then(Value::as_u64);
        let cached = num("/cache_read_input_tokens")
            .or_else(|| num("/prompt_tokens_details/cached_tokens"))
            .or_else(|| num("/input_tokens_details/cached_tokens"));
        if let Some(cached) = cached {
            self.cache_read = Some(cached);
        }
        if let Some(write) = num("/cache_creation_input_tokens") {
            self.cache_write = Some(write);
        }
        // Anthropic's `input_tokens` is already uncached; OpenAI-style
        // `prompt_tokens` includes the cached part.
        if let Some(input) = num("/input_tokens") {
            self.input = Some(input);
        } else if let Some(prompt) = num("/prompt_tokens") {
            self.input = Some(prompt.saturating_sub(cached.unwrap_or(0)));
        }
        if let Some(output) = num("/output_tokens").or_else(|| num("/completion_tokens")) {
            self.output = Some(output);
        }
    }
}

/// Passive per-response reader. See the module docs for the guarantees.
pub struct Tap {
    started: Instant,
    first_byte: Option<Duration>,
    sse: bool,
    /// Partial SSE line carried across chunks, or the captured JSON body.
    carry: Vec<u8>,
    /// The current SSE line outgrew [`MAX_LINE_BYTES`]; skip to its newline.
    skipping: bool,
    body_truncated: bool,
    data_lines: usize,
    usage: Usage,
    poisoned: bool,
}

impl Tap {
    /// Start timing a request that has just been sent. `sse` selects the
    /// streaming reader (decided from the response `content-type`).
    #[must_use]
    pub fn new(started: Instant, sse: bool) -> Self {
        Self {
            started,
            first_byte: None,
            sse,
            carry: Vec::new(),
            skipping: false,
            body_truncated: false,
            data_lines: 0,
            usage: Usage::default(),
            poisoned: false,
        }
    }

    /// Is this response a server-sent-event stream?
    #[must_use]
    pub fn is_sse(content_type: Option<&str>) -> bool {
        content_type.is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"))
    }

    /// Observe one relayed chunk. Infallible and non-blocking by design.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.first_byte.is_none() {
            self.first_byte = Some(self.started.elapsed());
        }
        if self.poisoned {
            return;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.sse {
                self.feed_sse(chunk);
            } else {
                self.feed_body(chunk);
            }
        }));
        if outcome.is_err() {
            self.poisoned = true;
        }
    }

    fn feed_body(&mut self, chunk: &[u8]) {
        let room = MAX_BODY_CAPTURE.saturating_sub(self.carry.len());
        if chunk.len() > room {
            self.body_truncated = true;
        }
        self.carry
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            let (line, tail) = rest.split_at(i);
            rest = &tail[1..];
            if self.skipping {
                self.skipping = false;
                self.carry.clear();
                continue;
            }
            // Bound the *completed* line too, whatever the chunk boundaries:
            // a whole oversized line in one chunk, or a carried partial line
            // completed past the limit, is skipped rather than copied/parsed.
            if self.carry.len() + line.len() > MAX_LINE_BYTES {
                self.carry.clear();
                continue;
            }
            if self.carry.is_empty() {
                self.line(line);
            } else {
                self.carry.extend_from_slice(line);
                let whole = std::mem::take(&mut self.carry);
                self.line(&whole);
            }
        }
        if self.skipping {
            return;
        }
        if self.carry.len() + rest.len() > MAX_LINE_BYTES {
            self.skipping = true;
            self.carry.clear();
        } else {
            self.carry.extend_from_slice(rest);
        }
    }

    fn line(&mut self, raw: &[u8]) {
        let Some(data) = raw
            .strip_prefix(b"data:")
            .map(<[u8]>::trim_ascii)
            .filter(|d| !d.is_empty() && *d != b"[DONE]")
        else {
            return;
        };
        self.data_lines += 1;
        let has_usage = data.windows(7).any(|w| w == b"\"usage\"");
        if !has_usage && (self.usage.model.is_some() || self.data_lines > MODEL_PROBE_LINES) {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(data) {
            self.usage.absorb(&value);
        }
    }

    /// Finish a response with `status`. `error_body` is the buffered error
    /// body for a non-2xx response (classified, then dropped).
    #[must_use]
    pub fn finish(mut self, status: u16, error_body: Option<&[u8]>) -> Observation {
        let latency = self.started.elapsed();
        if !self.sse && !self.poisoned && !self.body_truncated && !self.carry.is_empty() {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.carry) {
                self.usage.absorb(&value);
            }
        }
        let error_code = if (200..300).contains(&status) {
            None
        } else {
            Some(error_code(status, error_body.unwrap_or_default()))
        };
        Observation {
            status,
            latency,
            ttft: self.first_byte.unwrap_or(latency),
            stream: self.sse,
            usage: self.usage,
            error_code,
        }
    }
}

/// A closed vocabulary: the body is read only to choose one of these.
#[must_use]
pub fn error_code(status: u16, body: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(body);
    match classify::classify(&text, 1) {
        Some(found) => MarkReason::from_api_key(found).as_str(),
        None if status == 429 => MarkReason::RateLimited.as_str(),
        None if status == 401 || status == 403 => "credential",
        None if status >= 500 => "http_5xx",
        None => "http_4xx",
    }
}

/// Everything known about one finished request. Contains no body text.
#[derive(Clone, Debug)]
pub struct Observation {
    pub status: u16,
    pub latency: Duration,
    pub ttft: Duration,
    pub stream: bool,
    pub usage: Usage,
    pub error_code: Option<&'static str>,
}

impl Observation {
    /// An upstream that could not be reached at all (no response).
    #[must_use]
    pub fn unreachable(started: Instant) -> Self {
        let latency = started.elapsed();
        Self {
            status: 502,
            latency,
            ttft: latency,
            stream: false,
            usage: Usage::default(),
            error_code: Some("upstream_error"),
        }
    }
}

/// Keep `[A-Za-z0-9._:/-]`, at most 64 bytes; anything else becomes `other`
/// (a model name comes from the upstream, so it is not trusted as a label).
fn safe_label(raw: &str) -> String {
    if !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-'))
    {
        raw.to_string()
    } else {
        "other".to_string()
    }
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The metric points one observation yields. Labels are all on the existing
/// allowlist; per-request ids ride the span instead.
#[must_use]
pub fn metric_points(ctx: &ObserveContext, provider: &str, obs: &Observation) -> Vec<MetricPoint> {
    let model = obs
        .usage
        .model
        .as_deref()
        .map_or_else(|| "-".to_string(), safe_label);
    let role = ctx
        .role
        .as_deref()
        .map_or_else(|| "-".to_string(), safe_label);
    let outcome = if obs.error_code.is_none() {
        "ok"
    } else {
        "error"
    };
    let tag = |point: MetricPoint| {
        point
            .label("provider", provider_label(provider))
            .label("account", safe_label(&ctx.seat))
            .label("model", model.clone())
            .label("role", role.clone())
            .label("outcome", outcome)
    };
    let secs = |d: Duration| d.as_secs_f64();
    let mut requests = tag(MetricPoint::int(MetricName::EgressRequests, 1));
    if let Some(code) = obs.error_code {
        requests = requests.label("reason", code);
    }
    let mut points = vec![
        requests,
        tag(MetricPoint::double(MetricName::EgressLatency, secs(obs.latency))),
        tag(MetricPoint::double(MetricName::EgressTtft, secs(obs.ttft))),
    ];
    for (kind, value) in [
        ("input", obs.usage.input),
        ("output", obs.usage.output),
        ("cache_read", obs.usage.cache_read),
        ("cache_write", obs.usage.cache_write),
    ] {
        if let Some(value) = value {
            let value = i64::try_from(value).unwrap_or(i64::MAX);
            points.push(tag(MetricPoint::int(MetricName::EgressTokens, value)).label("kind", kind));
        }
    }
    points
}

/// The `loom.egress.request` span for one observation: its own root trace,
/// carrying the per-request ids that are too high-cardinality for a metric.
#[must_use]
pub fn span(ctx: &ObserveContext, launch_id: &str, obs: &Observation) -> SpanRecord {
    let ended_at = chrono::Utc::now();
    let started_at = ended_at
        - chrono::Duration::from_std(obs.latency).unwrap_or_else(|_| chrono::Duration::zero());
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed).to_string();
    let mut attributes = TraceAttributes::new();
    let mut put = |key: &str, value: String| {
        attributes.insert(key.to_string(), value);
    };
    put("loom.egress.seat", ctx.seat.clone());
    put("loom.egress.tap", ctx.tap.clone());
    put("loom.egress.profile", ctx.profile.clone());
    put("loom.egress.launch_id", launch_id.to_string());
    put("loom.egress.status", obs.status.to_string());
    put("loom.egress.latency_ms", obs.latency.as_millis().to_string());
    put("loom.egress.ttft_ms", obs.ttft.as_millis().to_string());
    put("loom.egress.stream", obs.stream.to_string());
    put("loom.runtime", ctx.runtime.clone());
    if let Some(code) = obs.error_code {
        put("loom.egress.error_code", code.to_string());
    }
    if let Some(role) = &ctx.role {
        put("loom.role", role.clone());
    }
    if let Some(issue) = ctx.issue {
        put("loom.issue", issue.to_string());
    }
    if let Some(pr) = ctx.pr {
        put("loom.pr_number", pr.to_string());
    }
    if let Some(sweep) = &ctx.sweep_id {
        put("loom.sweep_id", sweep.clone());
    }
    if let Some(model) = obs.usage.model.as_deref() {
        put("loom.model", safe_label(model));
    }
    for (key, value) in [
        ("loom.tokens.input", obs.usage.input),
        ("loom.tokens.output", obs.usage.output),
        ("loom.tokens.cache_read", obs.usage.cache_read),
        ("loom.tokens.cache_write", obs.usage.cache_write),
    ] {
        if let Some(value) = value {
            put(key, value.to_string());
        }
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::EgressRequest.as_str(),
            &[
                launch_id,
                &crate::telemetry::trace::instant(started_at),
                &sequence,
            ],
        ),
        parent_span_id: None,
        name: SpanName::EgressRequest,
        started_at,
        ended_at,
        status: if obs.error_code.is_none() {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one observation. A no-op when no ops sink is registered.
pub fn publish(ctx: &ObserveContext, provider: &str, launch_id: &str, obs: &Observation) {
    if !ops::spans_exported() {
        return;
    }
    ops::emit_metrics(metric_points(ctx, provider, obs));
    ops::emit_span(span(ctx, launch_id, obs));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "observe_tests.rs"]
mod tests;
