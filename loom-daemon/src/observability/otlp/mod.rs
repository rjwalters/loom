//! OTLP exporter (Epic #4702, Phase 4 — issue #4858): a drop-in second
//! [`super::exporter::Exporter`] implementation translating
//! [`crate::telemetry::TelemetryEnvelope`] batches into the OTLP wire format,
//! for operators with an existing OpenTelemetry stack (a self-hosted
//! collector, Grafana, Honeycomb, …) who want to skip the native Cloudflare
//! backend entirely. Selected via `observability.exporter = "otlp"` or an
//! `observability.exporters` entry (`observability::resolve_exporters`) —
//! [`super::exporter::HttpsExporter`] stays the default (`"https"`).
//!
//! Gated behind the `otlp` Cargo feature (see `loom-daemon/Cargo.toml`): a
//! default build never compiles `opentelemetry_proto` in, so choosing this
//! sink costs nothing for operators who stick with the HTTPS exporter.
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
//! | `host_id` | `Resource` attribute `service.instance.id` (and `host.id`) — one `ResourceLogs`/`ResourceMetrics` entry per distinct `host_id` in a batch |
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
//!   emitting entity rather than a measurement.
//!
//! See [`mapping`] for the field-by-field implementation and its unit tests
//! (fixture envelopes for every record kind, verifying the log/metric split
//! and every attribute above).

mod mapping;
mod traces;
mod transport;

use transport::{post, Signal};

use std::time::Duration;

use super::exporter::{BatchOutcome, ExportError, Exporter};
use crate::telemetry::TelemetryEnvelope;

/// Per-request timeout — same rationale and value as
/// [`super::exporter::HttpsExporter`]'s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The OTLP/HTTP+JSON [`Exporter`]. POSTs mapped logs/metrics batches to
/// `{base_endpoint}/v1/logs` and `{base_endpoint}/v1/metrics` respectively —
/// the standard OTLP/HTTP path suffixes — each with `Authorization: Bearer
/// <ingest_key>`, the same auth convention [`super::exporter::HttpsExporter`]
/// uses, so `observability.ingestKeyFile` is shared across both sinks.
pub struct OtlpExporter {
    client: reqwest::Client,
    logs_endpoint: String,
    metrics_endpoint: String,
    traces_endpoint: String,
    ingest_key: String,
}

impl OtlpExporter {
    /// Build an exporter posting to `{base_endpoint}/v1/logs` and
    /// `{base_endpoint}/v1/metrics` (a trailing slash on `base_endpoint` is
    /// tolerated and stripped), authenticating with `ingest_key`. Validates the
    /// URL and builds the HTTP client without touching the network.
    pub fn new(base_endpoint: String, ingest_key: String) -> Result<Self, ExportError> {
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
        Ok(OtlpExporter {
            client,
            logs_endpoint: format!("{base}/v1/logs"),
            metrics_endpoint: format!("{base}/v1/metrics"),
            traces_endpoint: format!("{base}/v1/traces"),
            ingest_key,
        })
    }
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
                        &self.ingest_key,
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
                            _ => 0,
                        })
                        .sum();
                    post(
                        &self.client,
                        &self.metrics_endpoint,
                        &self.ingest_key,
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
                        outcome.acknowledged += count;
                        offset += count;
                        continue;
                    };
                    post(
                        &self.client,
                        &self.traces_endpoint,
                        &self.ingest_key,
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
            if response.error.is_some() {
                outcome.error = response.error;
            }
            if response.retry {
                break;
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

fn signal_for(envelope: &TelemetryEnvelope) -> Signal {
    match envelope.record {
        crate::telemetry::TelemetryRecord::HostHealth(_)
        | crate::telemetry::TelemetryRecord::TokensSnapshot(_)
        | crate::telemetry::TelemetryRecord::CiDuration(_) => Signal::Metrics,
        crate::telemetry::TelemetryRecord::Span(_) => Signal::Traces,
        _ => Signal::Logs,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
