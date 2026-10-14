//! OTLP exporter (Epic #4702, Phase 4 — issue #4858): a drop-in second
//! [`super::exporter::Exporter`] implementation translating
//! [`crate::telemetry::TelemetryEnvelope`] batches into the OTLP wire format,
//! for operators with an existing OpenTelemetry stack (a self-hosted
//! collector, Grafana, Honeycomb, …) who want to skip the native Cloudflare
//! backend entirely. Selected via `observability.exporter = "otlp"` or an
//! `observability.exporters` entry (`observability::resolve_exporters`) —
//! [`super::exporter::HttpsExporter`] stays the default (`"https"`).
//!
//! Compiled into every build (#10700): `opentelemetry_proto` is a
//! non-optional dependency, so no build path can produce a daemon that
//! silently cannot honour a configured `otlp` sink. The `otlp` Cargo feature
//! survives only as a no-op alias (see `loom-daemon/Cargo.toml`).
//!
//! # Transport, not the OTel SDK
//!
//! This is deliberately a thin translator, not an embedding of the
//! `opentelemetry-otlp` SDK exporter: [`OtlpExporter::emit_batch`] posts the
//! mapped OTLP request(s) over the *same kind* of `reqwest` client
//! [`super::exporter::HttpsExporter`] already depends on, and
//! [`super::sender`]'s drain/retry/backoff loop — generic over any
//! `E: `[`super::exporter::Exporter`] — governs retries exactly as it does
//! for the HTTPS sink. OTLP response classification lives in `transport`;
//! acknowledged signal prefixes are removed before retrying the remaining suffix.
//! `opentelemetry-proto`'s `gen-tonic-messages` feature buys only the
//! generated message *types* (`prost`-derived structs); `tonic`/gRPC
//! transport is deliberately never enabled, so this feature adds no gRPC
//! stack — just JSON-serializable Rust structs for the OTLP wire shapes.
//!
//! # `TelemetryEnvelope` → OTLP mapping
//!
//! | Envelope field | OTLP destination |
//! |---|---|
//! | `host_id` | `Resource` attributes `service.instance.id`, `host.id` and `host.name` (all the same string) — one `ResourceLogs`/`ResourceMetrics` entry per distinct `host_id` in a batch |
//! | `emitted_at` | `LogRecord.time_unix_nano` / `.observed_time_unix_nano`, or `NumberDataPoint.time_unix_nano` |
//! | a record's repo-visibility tag (when present) | **not** a `Resource` attribute — a `Resource` describes the emitting *host*, and one host's batch can reference many repos, so visibility is a per-`LogRecord` attribute `loom.repo.visibility` (alongside `loom.repo`) instead |
//!
//! [`TelemetryRecord`](crate::telemetry::TelemetryRecord) kinds split into
//! two OTLP signals:
//!
//! - **Logs** (`ExportLogsServiceRequest`, one `LogRecord` per envelope): the
//!   four sweep-lifecycle records — `sweep.started`, `sweep.phase`,
//!   `sweep.completed`, `sweep.outcome` — become `LogRecord`s. Each record's
//!   own `kind` tag is carried as `LogRecord.event_name`, its fields flatten
//!   into `LogRecord.attributes` under a `loom.` prefix (`sweep.outcome`'s
//!   `config` map becomes `loom.config.<key>` attributes and its
//!   `phase_durations` becomes a nested `loom.phase_durations` array
//!   attribute), and `severity_number` reflects `SweepResult` where one is
//!   present (`Failure` → `Error`, `Blocked` → `Warn`, else `Info`).
//! - **Metrics** (`ExportMetricsServiceRequest`, one `Gauge` `Metric` per
//!   distinct name, one `NumberDataPoint` per envelope/account that measured
//!   it): the two host-level records — `tokens.snapshot`, `host.health` —
//!   become `Gauge` metrics (`loom.host.*`, `loom.tokens.*`). An unmeasured
//!   optional field (e.g. `cpu_idle_fraction: None`) produces **no** data
//!   point — the schema's "unknown != zero" contract carries through to the
//!   OTLP mapping. `host.health`'s `daemon_version` becomes the `Resource`
//!   attribute `service.version`, not a metric, since it describes the
//!   emitting entity rather than a measurement. Every signal (traces and logs
//!   included) carries `service.version`; without a `host.health` record it
//!   is the exporting build's `CARGO_PKG_VERSION`.
//!
//! See [`mapping`] for the field-by-field implementation and its unit tests
//! (fixture envelopes for every record kind, verifying the log/metric split
//! and every attribute above).

mod mapping;
pub mod relay;
mod traces;
mod transport;

use transport::{post, Signal};

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::exporter::{BatchOutcome, ExportError, Exporter};
use super::request_headers::RequestHeaders;
use crate::telemetry::TelemetryEnvelope;

/// Per-request timeout — same rationale and value as
/// [`super::exporter::HttpsExporter`]'s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The OTLP/HTTP+JSON [`Exporter`]. POSTs mapped logs/metrics batches to
/// `{base_endpoint}/v1/logs` and `{base_endpoint}/v1/metrics` respectively —
/// the standard OTLP/HTTP path suffixes — each with `Authorization: Bearer
/// <ingest_key>`, the same auth convention [`super::exporter::HttpsExporter`]
/// uses, so `observability.ingestKeyFile` is shared across both sinks. An
/// entry whose `headers_file` carries all its credentials exports with no
/// ingest key and no Bearer header at all (#11555).
///
/// An entry with `headers_file` (Issue #10961) additionally sends that file's
/// headers on all three signals; a file-supplied `Authorization` replaces the
/// Bearer header. See [`super::request_headers`].
///
/// Deliberately not `Debug`: it holds the ingest key.
pub struct OtlpExporter {
    client: reqwest::Client,
    logs_endpoint: String,
    metrics_endpoint: String,
    traces_endpoint: String,
    /// The Bearer ingest key, when this exporter was built with one. `None`
    /// on an entry whose `headers_file` supplies all its credentials
    /// (#11555): no `Authorization: Bearer` is sent unless the file itself
    /// sets one.
    ingest_key: Option<String>,
    /// The entry's `headers_file` as a live credential source. Empty unless
    /// built by [`OtlpExporter::with_headers_file`] with a path.
    extra_headers: HeadersSource,
}

impl OtlpExporter {
    /// Build an exporter posting to `{base_endpoint}/v1/logs` and
    /// `{base_endpoint}/v1/metrics` (a trailing slash on `base_endpoint` is
    /// tolerated and stripped), authenticating with `ingest_key`. Validates
    /// the URL and builds the HTTP client without touching the network.
    pub fn new(base_endpoint: String, ingest_key: String) -> Result<Self, ExportError> {
        Self::with_headers_file(base_endpoint, Some(ingest_key), None)
    }

    /// Build an exporter for an `observability.exporters` entry: exactly
    /// [`Self::new`] plus, when the entry names a `headers_file`
    /// (Issue #10961), that file's extra request headers; and — when no
    /// ingest key was resolved for the entry (#11555) — no Bearer header at
    /// all, the file's headers being the entry's whole authentication.
    ///
    /// The file is read once here, and [`HeadersSource`] re-reads it
    /// whenever it changes, so a rotated credential is picked up without a
    /// restart. The endpoint is validated first: a headers file is never
    /// opened for a destination the exporter would refuse.
    ///
    /// # Errors
    ///
    /// [`Self::new`]'s, or the refused file as an [`ExportError::Transport`]
    /// naming the path and line — never a header value.
    pub fn with_headers_file(
        base_endpoint: String,
        ingest_key: Option<String>,
        headers_file: Option<&str>,
    ) -> Result<Self, ExportError> {
        if !super::endpoint_policy::valid_otlp_endpoint(&base_endpoint) {
            return Err(ExportError::Transport(
                "OTLP base URL must be HTTP(S), without credentials, query or fragment".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ExportError::Transport(error.to_string()))?;
        let base = base_endpoint.trim_end_matches('/');
        let extra_headers = HeadersSource::at(headers_file)?;
        Ok(OtlpExporter {
            client,
            logs_endpoint: format!("{base}/v1/logs"),
            metrics_endpoint: format!("{base}/v1/metrics"),
            traces_endpoint: format!("{base}/v1/traces"),
            ingest_key,
            extra_headers,
        })
    }
}

/// An exporter entry's `headers_file` as a live credential source (#10961,
/// #11555): the parsed headers plus the file stamp they were read at,
/// re-read whenever the stamp changes — a rotated credential reaches the
/// next request without a restart.
///
/// `RequestHeaders` is deliberately not `Clone` (#10961), so the cached
/// value is held in an [`Arc`] and handed out by reference.
struct HeadersSource {
    /// The configured path; `None` for an entry without a `headers_file`.
    path: Option<String>,
    state: Mutex<HeadersState>,
}

/// The cached read of a headers file: the stamp it was read at, the parsed
/// headers, and the last refusal already WARN-logged — so a persistently
/// broken file logs once per distinct problem, not once per request
/// (#11555).
#[derive(Default)]
struct HeadersState {
    stamp: FileStamp,
    headers: Arc<RequestHeaders>,
    warned: Option<String>,
}

/// What makes a headers file "the same file as last time" (#11555): its
/// mtime, length and — on Unix — inode. mtime alone misses a same-second
/// rotation on coarse-grained filesystems, and an atomic replace
/// (`rename(2)` over the live path) keeps the path but changes the inode,
/// so all three are compared.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct FileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileStamp {
    fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        })
    }
}

impl HeadersSource {
    /// No headers file: nothing is ever stat'ed or re-read.
    fn no_file() -> Self {
        Self {
            path: None,
            state: Mutex::new(HeadersState::default()),
        }
    }

    /// The headers file at `path`, read and parsed now. `None` is
    /// [`Self::no_file`].
    ///
    /// # Errors
    ///
    /// The refused file as an [`ExportError::Transport`] naming the path
    /// and line — never a header value.
    fn at(path: Option<&str>) -> Result<Self, ExportError> {
        let Some(path) = path else {
            return Ok(Self::no_file());
        };
        // Stat before load, matching `RequestHeaders::load`'s own
        // metadata-before-read ordering: a change racing the load can only
        // cause one extra re-read on the next post, never a missed one.
        let stamp = FileStamp::of(Path::new(path)).unwrap_or_default();
        let headers = RequestHeaders::load(Path::new(path))
            .map_err(|error| ExportError::Transport(error.to_string()))?;
        log::info!(
            "observability: OTLP exporter sends {} extra request header(s) from {path}{}",
            headers.len(),
            if headers.sets_authorization() {
                " (its Authorization replaces the Bearer ingest key)"
            } else {
                ""
            }
        );
        Ok(Self {
            path: Some(path.to_string()),
            state: Mutex::new(HeadersState {
                stamp,
                headers: Arc::new(headers),
                warned: None,
            }),
        })
    }

    fn locked_state(&self) -> std::sync::MutexGuard<'_, HeadersState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The headers to apply to a request, re-read when the file's stamp
    /// changed since the last look (#11555).
    ///
    /// A reload that fails — bad permissions, a malformed line, a vanished
    /// file — keeps the **last good** headers: a broken file must never
    /// stop or panic the sender, and the receiver keeps authenticating
    /// until the operator fixes it. Each distinct problem WARNs exactly
    /// once, named by path (and line) and never by a value (#10961).
    fn current(&self) -> Arc<RequestHeaders> {
        let Some(path) = self.path.as_deref() else {
            return Arc::clone(&self.locked_state().headers);
        };
        let mut state = self.locked_state();
        match FileStamp::of(Path::new(path)) {
            // Unchanged since the last look — nothing to re-read.
            Ok(stamp) if stamp == state.stamp => {}
            Ok(stamp) => {
                state.stamp = stamp;
                match RequestHeaders::load(Path::new(path)) {
                    Ok(headers) => {
                        if state.warned.take().is_some() {
                            log::info!(
                                "observability: headers file {path} is readable again — \
                                 carrying its {} header(s)",
                                headers.len()
                            );
                        } else {
                            log::info!(
                                "observability: headers file {path} changed — carrying its \
                                 {} header(s)",
                                headers.len()
                            );
                        }
                        state.headers = Arc::new(headers);
                    }
                    Err(error) => {
                        let rendered = error.to_string();
                        if state.warned.as_deref() != Some(rendered.as_str()) {
                            log::warn!(
                                "observability: could not re-read headers file — keeping the \
                                 last good headers: {rendered}"
                            );
                            state.warned = Some(rendered);
                        }
                    }
                }
            }
            Err(error) => {
                // The stat itself failed — the rename window of an atomic
                // rotation, or a deleted file. Same posture as a refused
                // reload: keep the last good headers, warn once per
                // distinct problem.
                let rendered = format!("could not stat headers file {path}: {error}");
                if state.warned.as_deref() != Some(rendered.as_str()) {
                    log::warn!("observability: {rendered} — keeping the last good headers");
                    state.warned = Some(rendered);
                }
            }
        }
        Arc::clone(&state.headers)
    }
}

impl fmt::Debug for HeadersSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Path and header *names* only (#10961): `RequestHeaders`'s own
        // Debug is redacted, and the stamp carries no file content. The
        // last-warned problem is deliberately not rendered either.
        let state = self.locked_state();
        f.debug_struct("HeadersSource")
            .field("path", &self.path)
            .field("stamp", &state.stamp)
            .field("headers", &state.headers)
            .finish()
    }
}

/// The HTTP status a transport error names when the receiver answered with a
/// refusal — `transport.rs`'s failure wording is `OTLP HTTP <status> …` — for
/// callers (like `telemetry_join`, #11528) that must treat a credential
/// refusal as terminal rather than retryable. `None` for pure transport
/// failures and every other error shape.
#[must_use]
pub fn rejected_status(error: &ExportError) -> Option<u16> {
    let ExportError::Transport(message) = error else {
        return None;
    };
    let rest = message.strip_prefix("OTLP HTTP ")?;
    let digits: &str = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();
    if digits.len() != 3 {
        return None;
    }
    digits.parse().ok()
}

impl Exporter for OtlpExporter {
    async fn emit_batch(&self, envelopes: &[TelemetryEnvelope]) -> Result<(), ExportError> {
        match self.emit_batch_outcome(envelopes).await.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn emit_batch_outcome(&self, envelopes: &[TelemetryEnvelope]) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        let mut offset = 0;
        while offset < envelopes.len() {
            let signal = signal_for(&envelopes[offset]);
            let count = envelopes[offset..]
                .iter()
                .take_while(|e| signal_for(e) == signal)
                .count();
            let group = &envelopes[offset..offset + count];
            // The headers (and their file, when there is one) are re-checked
            // before every post (#11555), so a rotated credential reaches
            // the very next request.
            let extra_headers = self.extra_headers.current();
            let mut exported_envelopes = count;
            let response = match signal {
                Signal::Logs => {
                    let Some(request) = mapping::build_logs_request(group) else {
                        outcome.acknowledged += count;
                        offset += count;
                        continue;
                    };
                    let items = request
                        .resource_logs
                        .iter()
                        .flat_map(|r| &r.scope_logs)
                        .map(|s| s.log_records.len() as u64)
                        .sum();
                    post(
                        &self.client,
                        &self.logs_endpoint,
                        self.ingest_key.as_deref(),
                        &extra_headers,
                        signal,
                        items,
                        &request,
                    )
                    .await
                }
                Signal::Metrics => {
                    let Some(request) = mapping::build_metrics_request(group) else {
                        outcome.acknowledged += count;
                        offset += count;
                        continue;
                    };
                    let items = request
                        .resource_metrics
                        .iter()
                        .flat_map(|r| &r.scope_metrics)
                        .flat_map(|s| &s.metrics)
                        .map(|m| match &m.data {
                            Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(
                                g,
                            )) => g.data_points.len() as u64,
                            Some(
                                opentelemetry_proto::tonic::metrics::v1::metric::Data::Histogram(h),
                            ) => h.data_points.len() as u64,
                            Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Sum(s)) => {
                                s.data_points.len() as u64
                            }
                            _ => 0,
                        })
                        .sum();
                    post(
                        &self.client,
                        &self.metrics_endpoint,
                        self.ingest_key.as_deref(),
                        &extra_headers,
                        signal,
                        items,
                        &request,
                    )
                    .await
                }
                Signal::Traces => {
                    let request = traces::build_traces_request(group);
                    let items = request.as_ref().map_or(0, |request| {
                        request
                            .resource_spans
                            .iter()
                            .flat_map(|r| &r.scope_spans)
                            .map(|s| s.spans.len())
                            .sum::<usize>()
                    });
                    exported_envelopes = items;
                    outcome
                        .signals
                        .entry(signal.unit().to_string())
                        .or_default()
                        .dropped += (count - items) as u64;
                    let Some(request) = request else {
                        // Every envelope in the group is unmappable and the
                        // group is acknowledged without ever being posted:
                        // the loss is decided here, not on a later retry.
                        *outcome
                            .lost
                            .entry(super::loss_ledger::REASON_TRACE_UNMAPPABLE.to_string())
                            .or_default() += count as u64;
                        outcome.acknowledged += count;
                        offset += count;
                        continue;
                    };
                    post(
                        &self.client,
                        &self.traces_endpoint,
                        self.ingest_key.as_deref(),
                        &extra_headers,
                        signal,
                        items as u64,
                        &request,
                    )
                    .await
                }
            };
            let fully_accepted =
                response.counts.rejected == 0 && response.counts.dropped == 0 && !response.retry;
            outcome
                .signals
                .entry(signal.unit().to_string())
                .or_default()
                .accumulate(&response.counts);
            // Permanently lost groups get a durable, by-reason count (Issue
            // #11527): envelope units for a refused/unreadable response,
            // signal units for partial rejection (the receiver counts items
            // and does not identify the envelopes that carried them).
            if !response.retry && response.counts.dropped > 0 {
                if let Some(reason) = &response.loss_reason {
                    // A traces group's `count - exported_envelopes`
                    // unmappable envelopes were never submitted — they are
                    // counted under `trace_unmappable`, so the status reason
                    // counts only the envelopes actually sent (#11588).
                    let lost = if signal == Signal::Traces {
                        exported_envelopes
                    } else {
                        count
                    };
                    *outcome.lost.entry(reason.clone()).or_default() += lost as u64;
                }
            }
            if response.counts.rejected > 0 {
                *outcome
                    .lost
                    .entry(super::loss_ledger::REASON_RECEIVER_REJECTED.to_string())
                    .or_default() += response.counts.rejected;
            }
            if response.error.is_some() {
                outcome.error = response.error;
            }
            if response.retry {
                break;
            }
            // Unmappable trace envelopes are a permanent, counted loss (Issue
            // #11527) — the mapping will never send them. Recorded only once
            // the group's fate is decided: a held batch (401/403, transient
            // statuses, timeouts) is still queued at this point, and counting
            // before the retry decision would charge the same envelope on
            // every retry attempt (#11588).
            if signal == Signal::Traces && exported_envelopes < count {
                *outcome
                    .lost
                    .entry(super::loss_ledger::REASON_TRACE_UNMAPPABLE.to_string())
                    .or_default() += (count - exported_envelopes) as u64;
            }
            outcome.acknowledged += count;
            if fully_accepted {
                outcome.exported += exported_envelopes;
            }
            offset += count;
        }
        outcome
    }
}

/// Which OTLP endpoint an envelope's kind is exported to, derived from the
/// `otlp:` routing class the kind declares in `telemetry/kinds.rs` (#8921) —
/// not from a per-kind chain maintained here. A kind OTLP does not export
/// (`NotExported`, e.g. native-HTTPS-only `queue.snapshot`) keeps the historical
/// `Signal::Logs` bucket: nothing ever emits it on this path, and the logs
/// request builder drops it (`log_record_for` returns `None`).
fn signal_for(envelope: &TelemetryEnvelope) -> Signal {
    use crate::telemetry::TelemetryKindOtlp;
    match envelope.record.otlp_class() {
        TelemetryKindOtlp::Gauges
        | TelemetryKindOtlp::Histograms
        | TelemetryKindOtlp::OpsPoints => Signal::Metrics,
        TelemetryKindOtlp::Spans => Signal::Traces,
        TelemetryKindOtlp::Logs | TelemetryKindOtlp::NotExported => Signal::Logs,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
