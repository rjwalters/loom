//! Pure `TelemetryEnvelope` → OTLP record mapping (no network I/O). See the
//! parent module's doc comment for the mapping table this file implements;
//! this module is the field-by-field implementation plus its unit tests.

mod metadata;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value, AnyValue, ArrayValue, KeyValue, KeyValueList,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs, SeverityNumber};
use opentelemetry_proto::tonic::metrics::v1::{
    metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_proto::tonic::resource::v1::Resource;

use crate::telemetry::{
    RepoVisibility, RoleTickResult, SweepResult, TelemetryEnvelope, TelemetryRecord,
};

// ============================================================================
// Small AnyValue / KeyValue constructors
// ============================================================================

fn any_string(value: impl Into<String>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(value.into())),
    }
}

fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(value),
        ..Default::default()
    }
}

pub(super) fn kv_string(key: &str, value: impl Into<String>) -> KeyValue {
    kv(key, any_string(value))
}

fn kv_int(key: &str, value: i64) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::IntValue(value)),
        },
    )
}

/// UNIX-epoch nanoseconds for `ts`, floored at 0 — `chrono`'s
/// `timestamp_nanos_opt` only returns `None` far outside any timestamp this
/// daemon ever produces (year ~1677 or ~2262), so the floor is unreachable in
/// practice and exists only to avoid a panic/wraparound on the conversion.
pub(super) fn nanos(ts: DateTime<Utc>) -> u64 {
    ts.timestamp_nanos_opt()
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(0)
}

fn visibility_str(visibility: RepoVisibility) -> &'static str {
    match visibility {
        RepoVisibility::Public => "public",
        RepoVisibility::Private => "private",
    }
}

fn result_str(result: SweepResult) -> &'static str {
    match result {
        SweepResult::Success => "success",
        SweepResult::Failure => "failure",
        SweepResult::Cancelled => "cancelled",
        SweepResult::Blocked => "blocked",
    }
}

/// `Failure` → `Error`, `Blocked` → `Warn`, everything else → `Info` — the
/// only two `SweepResult` variants that represent something worth a reader's
/// elevated attention.
fn severity_for_result(result: SweepResult) -> SeverityNumber {
    match result {
        SweepResult::Failure => SeverityNumber::Error,
        SweepResult::Blocked => SeverityNumber::Warn,
        SweepResult::Success | SweepResult::Cancelled => SeverityNumber::Info,
    }
}

/// The wire string for a role-tick result (Issue #8056) — the serde
/// `rename_all = "snake_case"` spelling, restated here so the OTLP attribute
/// value matches the JSON one exactly.
fn role_tick_result_str(result: RoleTickResult) -> &'static str {
    match result {
        RoleTickResult::Success => "success",
        RoleTickResult::Failure => "failure",
        RoleTickResult::RuntimeRejected => "runtime_rejected",
        RoleTickResult::SkippedNoTokenPool => "skipped_no_token_pool",
        RoleTickResult::SkippedPoolExhausted => "skipped_pool_exhausted",
        RoleTickResult::SkippedModelRuntimeMismatch => "skipped_model_runtime_mismatch",
        RoleTickResult::SkippedLoad => "skipped_load",
    }
}

/// Only an outright `failure` is an error; every *skip* is `Info` (Issue
/// #8056/#7607 — a fleet-wide exhausted pool's identical skips must not page
/// as hundreds of broken roles), and a fail-closed runtime rejection is a
/// `Warn` config signal.
fn severity_for_role_tick(result: RoleTickResult) -> SeverityNumber {
    match result {
        RoleTickResult::Failure => SeverityNumber::Error,
        RoleTickResult::RuntimeRejected | RoleTickResult::SkippedModelRuntimeMismatch => {
            SeverityNumber::Warn
        }
        _ => SeverityNumber::Info,
    }
}

fn severity_text(severity: SeverityNumber) -> &'static str {
    match severity {
        SeverityNumber::Error => "ERROR",
        SeverityNumber::Warn => "WARN",
        _ => "INFO",
    }
}

/// A `Resource` describing the emitting daemon host. `daemon_version` is only
/// known from a `host.health` record, so it is threaded in separately rather
/// than read off `envelope.record` — see [`build_metrics_request`].
pub(super) fn resource_for_host(host_id: &str, daemon_version: Option<&str>) -> Resource {
    let mut attributes = vec![
        kv_string("service.name", "loom-daemon"),
        kv_string("service.instance.id", host_id),
        kv_string("host.id", host_id),
    ];
    if let Some(version) = daemon_version {
        attributes.push(kv_string("service.version", version));
    }
    Resource {
        attributes,
        ..Default::default()
    }
}

// ============================================================================
// Logs — the four sweep-lifecycle record kinds
// ============================================================================

/// Maps one lifecycle-kind envelope to a `LogRecord`. Returns `None` for the
/// two host-level record kinds (`tokens.snapshot`, `host.health`) — those
/// become metrics instead (see [`metric_samples_for`]).
fn log_record_for(envelope: &TelemetryEnvelope) -> Option<LogRecord> {
    let time_unix_nano = nanos(envelope.emitted_at);
    let (event_name, severity, _body, attributes) = match &envelope.record {
        TelemetryRecord::SweepStarted(r) => {
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.sweep_id", r.sweep_id.clone()),
            ];
            if let Some(model) = &r.model {
                attributes.push(kv_string("loom.model", model.clone()));
            }
            if let Some(effort) = &r.effort {
                attributes.push(kv_string("loom.effort", effort.clone()));
            }
            if let Some(runtime) = &r.runtime {
                attributes.push(kv_string("loom.runtime", runtime.clone()));
            }
            (
                "sweep.started",
                SeverityNumber::Info,
                format!("sweep started: {} issue #{}", r.repo, r.issue),
                attributes,
            )
        }
        TelemetryRecord::SweepIdentity(r) => {
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.sweep_id", r.sweep_id.clone()),
            ];
            for (key, value) in [
                ("loom.runtime", &r.runtime),
                ("loom.provider", &r.provider),
                ("loom.model", &r.model),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_string(key, value.clone()));
                }
            }
            (
                "sweep.identity",
                SeverityNumber::Info,
                "sweep launch identity".to_string(),
                attributes,
            )
        }
        TelemetryRecord::SweepPhase(r) => (
            "sweep.phase",
            SeverityNumber::Info,
            format!("sweep phase {}: {} issue #{}", r.phase, r.repo, r.issue),
            vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.sweep_id", r.sweep_id.clone()),
                kv_string("loom.phase", r.phase.clone()),
            ],
        ),
        TelemetryRecord::SweepCompleted(r) => {
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.sweep_id", r.sweep_id.clone()),
                kv_string("loom.result", result_str(r.result)),
            ];
            if let Some(usage) = metadata::usage(r.tokens_by_model.as_deref()) {
                attributes.push(usage);
            }
            (
                "sweep.completed",
                severity_for_result(r.result),
                format!(
                    "sweep completed ({}): {} issue #{}",
                    result_str(r.result),
                    r.repo,
                    r.issue
                ),
                attributes,
            )
        }
        TelemetryRecord::SweepOutcome(r) => {
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.sweep_id", r.sweep_id.clone()),
                kv_string("loom.result", result_str(r.result)),
                kv_int("loom.total_duration_sec", r.total_duration_sec),
            ];
            if let Some(model) = &r.model {
                attributes.push(kv_string("loom.model", model.clone()));
            }
            if let Some(effort) = &r.effort {
                attributes.push(kv_string("loom.effort", effort.clone()));
            }
            if let Some(pr_number) = r.pr_number {
                attributes.push(kv_int("loom.pr_number", i64::from(pr_number)));
            }
            attributes.extend(metadata::outcome(r));
            if !r.phase_durations.is_empty() {
                let entries = r
                    .phase_durations
                    .iter()
                    .map(|phase_duration| AnyValue {
                        value: Some(any_value::Value::KvlistValue(KeyValueList {
                            values: vec![
                                kv_string("phase", phase_duration.phase.clone()),
                                kv_int("duration_sec", phase_duration.duration_sec),
                            ],
                        })),
                    })
                    .collect();
                attributes.push(kv(
                    "loom.phase_durations",
                    AnyValue {
                        value: Some(any_value::Value::ArrayValue(ArrayValue { values: entries })),
                    },
                ));
            }
            (
                "sweep.outcome",
                severity_for_result(r.result),
                format!(
                    "sweep outcome ({}): {} issue #{}, {}s total",
                    result_str(r.result),
                    r.repo,
                    r.issue,
                    r.total_duration_sec
                ),
                attributes,
            )
        }
        TelemetryRecord::RoleTickOutcome(r) => {
            // Issue #8056: the seventh record kind. Mapped as a log record
            // alongside the sweep lifecycle kinds (not as a metric) because a
            // tick is an event with a result and a detail string, not a gauge.
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_string("loom.role", r.role.clone()),
                kv_string("loom.result", role_tick_result_str(r.result)),
                kv_int("loom.duration_sec", r.duration_sec),
            ];
            // Every optional field stays optional here too: an unobserved
            // measurement must be an ABSENT attribute, never a zero one.
            if let Some(model) = &r.model {
                attributes.push(kv_string("loom.model", model.clone()));
            }
            if let Some(effort) = &r.effort {
                attributes.push(kv_string("loom.effort", effort.clone()));
            }
            // Arbitrary failure detail can contain credentials or workload text.
            // The typed result and bounded observed metadata are sufficient here.
            // #8408: which credential pool gated a pre-spawn pool skip.
            if let Some(gated_pool) = &r.gated_pool {
                attributes.push(kv_string("loom.gated_pool", gated_pool.clone()));
            }
            if let Some(models) = metadata::models(r.models_used.as_deref()) {
                attributes.push(models);
            }
            if let Some(usage) = metadata::usage(r.tokens_by_model.as_deref()) {
                attributes.push(usage);
            }
            if let Some(actions) = &r.actions {
                attributes
                    .push(kv_int("loom.actions.issues_labeled", i64::from(actions.issues_labeled)));
                attributes.push(kv_int("loom.actions.prs_merged", i64::from(actions.prs_merged)));
                attributes.push(kv_int(
                    "loom.actions.comments_posted",
                    i64::from(actions.comments_posted),
                ));
            }
            (
                "role_tick.outcome",
                severity_for_role_tick(r.result),
                format!(
                    "role tick ({}): {} {} on {}, {}s",
                    role_tick_result_str(r.result),
                    r.role,
                    r.model.as_deref().unwrap_or("model-unresolved"),
                    r.repo,
                    r.duration_sec
                ),
                attributes,
            )
        }
        TelemetryRecord::SessionSummary(r) => {
            // Issue #8757 (G3 of #8714): session shape, mapped as a log
            // record like the lifecycle kinds — an event with counts, not a
            // gauge. Every attribute is a count, id, or allowlisted name;
            // the parse never copies message text or tool output, so
            // nothing here needs a free-text bound beyond `bounded`'s.
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_string("loom.session_id", r.session_id.clone()),
                kv_string("loom.runtime", r.runtime.clone()),
                kv_int("loom.tokens.input", r.tokens_input),
                kv_int("loom.tokens.output", r.tokens_output),
                kv_int("loom.tokens.cache_read", r.tokens_cache_read),
                kv_int("loom.tokens.cache_write", r.tokens_cache_write),
                kv_int("loom.wall_ms", r.wall_ms),
                kv_int("loom.turns", i64::try_from(r.turns).unwrap_or(i64::MAX)),
                kv_int("loom.tool_errors", i64::try_from(r.tool_errors).unwrap_or(i64::MAX)),
            ];
            // Optional fields stay absent when unknown — an unobserved
            // attribution must be an ABSENT attribute, never a zero one.
            if let Some(parent) = &r.parent_session_id {
                attributes.push(kv_string("loom.parent_session_id", parent.clone()));
            }
            if let Some(role) = &r.role {
                attributes.push(kv_string("loom.role", role.clone()));
            }
            if let Some(issue) = r.issue {
                attributes.push(kv_int("loom.issue", i64::from(issue)));
            }
            if let Some(pr_number) = r.pr_number {
                attributes.push(kv_int("loom.pr_number", i64::from(pr_number)));
            }
            if let Some(outcome) = &r.outcome {
                attributes.push(kv_string("loom.outcome", outcome.clone()));
            }
            if let Some(models) = metadata::models(Some(&r.models)) {
                attributes.push(models);
            }
            if !r.tool_calls.is_empty() {
                let entries = r
                    .tool_calls
                    .iter()
                    .map(|call| AnyValue {
                        value: Some(any_value::Value::KvlistValue(KeyValueList {
                            values: vec![
                                kv_string("tool", call.tool.clone()),
                                kv_int("count", i64::try_from(call.count).unwrap_or(i64::MAX)),
                            ],
                        })),
                    })
                    .collect();
                attributes.push(kv(
                    "loom.tool_calls",
                    AnyValue {
                        value: Some(any_value::Value::ArrayValue(ArrayValue { values: entries })),
                    },
                ));
            }
            (
                "session.summary",
                SeverityNumber::Info,
                format!(
                    "session summary: {} {} on {}, {} turn(s), {} tool call(s)",
                    r.runtime,
                    r.role.as_deref().unwrap_or("role-unknown"),
                    r.repo,
                    r.turns,
                    r.tool_calls.iter().map(|c| c.count).sum::<u64>(),
                ),
                attributes,
            )
        }
        TelemetryRecord::TokensSnapshot(_)
        | TelemetryRecord::HostHealth(_)
        | TelemetryRecord::Span(_) => return None,
    };
    Some(LogRecord {
        time_unix_nano,
        observed_time_unix_nano: time_unix_nano,
        severity_number: severity as i32,
        severity_text: severity_text(severity).to_string(),
        body: Some(any_string(event_name)),
        attributes: metadata::bounded(attributes),
        event_name: event_name.to_string(),
        trace_id: envelope
            .trace_context
            .as_ref()
            .map(|c| c.trace_id.bytes())
            .unwrap_or_default(),
        span_id: envelope
            .trace_context
            .as_ref()
            .map(|c| c.span_id.bytes())
            .unwrap_or_default(),
        flags: envelope
            .trace_context
            .as_ref()
            .map(|c| u32::from(c.flags))
            .unwrap_or_default(),
        ..Default::default()
    })
}

/// Groups every lifecycle-kind envelope in `envelopes` into one
/// `ExportLogsServiceRequest`, one `ResourceLogs` entry per distinct
/// `host_id`. Returns `None` when the batch carries no lifecycle-kind
/// envelope at all (e.g. an all-metrics batch), so the caller can skip the
/// `/v1/logs` POST entirely.
pub(super) fn build_logs_request(
    envelopes: &[TelemetryEnvelope],
) -> Option<ExportLogsServiceRequest> {
    let mut by_host: BTreeMap<&str, Vec<LogRecord>> = BTreeMap::new();
    for envelope in envelopes {
        if let Some(log_record) = log_record_for(envelope) {
            by_host
                .entry(envelope.host_id.as_str())
                .or_default()
                .push(log_record);
        }
    }
    if by_host.is_empty() {
        return None;
    }
    let resource_logs = by_host
        .into_iter()
        .map(|(host_id, log_records)| ResourceLogs {
            resource: Some(resource_for_host(host_id, None)),
            scope_logs: vec![ScopeLogs {
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        })
        .collect();
    Some(ExportLogsServiceRequest { resource_logs })
}

// ============================================================================
// Metrics — the two host-level record kinds
// ============================================================================

/// One measured field, ready to become a `Gauge` `NumberDataPoint` once
/// grouped by (`host_id`, `name`) in [`build_metrics_request`].
struct MetricSample {
    name: &'static str,
    description: &'static str,
    unit: &'static str,
    attributes: Vec<KeyValue>,
    value: number_data_point::Value,
    time_unix_nano: u64,
}

/// Maps one host-level envelope to zero or more [`MetricSample`]s. Returns an
/// empty `Vec` for the four lifecycle-kind records (those become log
/// records instead — see [`log_record_for`]) and for any optional field left
/// unmeasured on this sample (the daemon's "unknown != zero" contract: an
/// absent measurement produces no data point, never a fabricated zero).
fn metric_samples_for(envelope: &TelemetryEnvelope) -> Vec<MetricSample> {
    let time_unix_nano = nanos(envelope.emitted_at);
    match &envelope.record {
        TelemetryRecord::HostHealth(r) => {
            let mut samples = vec![
                MetricSample {
                    name: "loom.host.uptime_seconds",
                    description: "Daemon uptime.",
                    unit: "s",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsInt(
                        i64::try_from(r.uptime_sec).unwrap_or(i64::MAX),
                    ),
                    time_unix_nano,
                },
                MetricSample {
                    name: "loom.host.logical_cpus",
                    description: "Logical CPU count.",
                    unit: "{cpu}",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsInt(
                        i64::try_from(r.logical_cpus).unwrap_or(i64::MAX),
                    ),
                    time_unix_nano,
                },
                // Role-tick health (Issue #5022): unlike the CPU/disk probes
                // below, a role-tick count of zero is a real, known value
                // (the role runner sampled nothing this snapshot — idle or
                // disabled), not an unmeasurable probe — so these are
                // unconditional, like `uptime_seconds`/`logical_cpus` above,
                // rather than gated behind an `Option`.
                MetricSample {
                    name: "loom.host.roles_total_ticks",
                    description: "Total role-tick records sampled in this host.health snapshot.",
                    unit: "{tick}",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsInt(
                        i64::try_from(r.roles.total).unwrap_or(i64::MAX),
                    ),
                    time_unix_nano,
                },
                MetricSample {
                    name: "loom.host.roles_ok_ticks",
                    description:
                        "Successful role-tick records sampled in this host.health snapshot.",
                    unit: "{tick}",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsInt(
                        i64::try_from(r.roles.ok).unwrap_or(i64::MAX),
                    ),
                    time_unix_nano,
                },
                MetricSample {
                    name: "loom.host.roles_persistent_failures",
                    description: "Count of (root, role) pairs with a persistent tick failure.",
                    unit: "1",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsInt(
                        i64::try_from(r.roles.persistent.len()).unwrap_or(i64::MAX),
                    ),
                    time_unix_nano,
                },
            ];
            if let Some(cpu_idle_fraction) = r.cpu_idle_fraction {
                samples.push(MetricSample {
                    name: "loom.host.cpu_idle_fraction",
                    description: "Measured CPU idle fraction (0..1).",
                    unit: "1",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsDouble(cpu_idle_fraction),
                    time_unix_nano,
                });
            }
            if let Some(load_per_core) = r.load_per_core {
                samples.push(MetricSample {
                    name: "loom.host.load_per_core",
                    description: "1-minute load average per logical core.",
                    unit: "1",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsDouble(load_per_core),
                    time_unix_nano,
                });
            }
            if let Some(worktree_root_free_gb) = r.worktree_root_free_gb {
                samples.push(MetricSample {
                    name: "loom.host.worktree_root_free_gb",
                    description: "Free space on the worktree-root scratch volume.",
                    unit: "GBy",
                    attributes: Vec::new(),
                    value: number_data_point::Value::AsInt(
                        i64::try_from(worktree_root_free_gb).unwrap_or(i64::MAX),
                    ),
                    time_unix_nano,
                });
            }
            samples
        }
        TelemetryRecord::TokensSnapshot(r) => {
            let mut samples = Vec::new();
            for account in &r.accounts {
                let mut attributes = vec![
                    kv_string("account", account.account.clone()),
                    kv_string("provider", account.provider.clone()),
                ];
                if let Some(rank) = account.rank {
                    attributes.push(kv_int("rank", i64::from(rank)));
                }
                if let Some(usage_fraction) = account.usage_fraction {
                    samples.push(MetricSample {
                        name: "loom.tokens.usage_fraction",
                        description: "Fraction of the current limit window consumed (0..1).",
                        unit: "1",
                        attributes: attributes.clone(),
                        value: number_data_point::Value::AsDouble(usage_fraction),
                        time_unix_nano,
                    });
                }
                samples.push(MetricSample {
                    name: "loom.tokens.exhausted",
                    description: "Whether the account is currently excluded from the usable pool (1 = exhausted).",
                    unit: "1",
                    attributes,
                    value: number_data_point::Value::AsInt(i64::from(account.exhausted)),
                    time_unix_nano,
                });
            }
            samples
        }
        // Every lifecycle-shaped kind — including `role_tick.outcome`
        // (#8056) — becomes a log record instead (see `log_record_for`).
        TelemetryRecord::SweepStarted(_)
        | TelemetryRecord::SweepIdentity(_)
        | TelemetryRecord::SweepPhase(_)
        | TelemetryRecord::SweepCompleted(_)
        | TelemetryRecord::SweepOutcome(_)
        | TelemetryRecord::RoleTickOutcome(_)
        // `session.summary` is a log record (see log_record_for), not a
        // gauge — its counters are per-session events, not host samples.
        | TelemetryRecord::SessionSummary(_)
        | TelemetryRecord::Span(_) => Vec::new(),
    }
}

/// Groups every host-level envelope in `envelopes` into one
/// `ExportMetricsServiceRequest`, one `ResourceMetrics` entry per distinct
/// `host_id` and one `Gauge` `Metric` per distinct metric name within that
/// host. Returns `None` when the batch carries no host-level envelope at all
/// (e.g. an all-lifecycle-events batch), so the caller can skip the
/// `/v1/metrics` POST entirely.
pub(super) fn build_metrics_request(
    envelopes: &[TelemetryEnvelope],
) -> Option<ExportMetricsServiceRequest> {
    let mut daemon_version_by_host: BTreeMap<&str, &str> = BTreeMap::new();
    for envelope in envelopes {
        if let TelemetryRecord::HostHealth(r) = &envelope.record {
            daemon_version_by_host.insert(envelope.host_id.as_str(), r.daemon_version.as_str());
        }
    }

    // (description, unit, data points), keyed by metric name, within each host.
    type MetricsForHost =
        BTreeMap<&'static str, (&'static str, &'static str, Vec<NumberDataPoint>)>;
    let mut by_host: BTreeMap<&str, MetricsForHost> = BTreeMap::new();
    for envelope in envelopes {
        let samples = metric_samples_for(envelope);
        if samples.is_empty() {
            continue;
        }
        let host_metrics = by_host.entry(envelope.host_id.as_str()).or_default();
        for sample in samples {
            let (_, _, data_points) = host_metrics.entry(sample.name).or_insert((
                sample.description,
                sample.unit,
                Vec::new(),
            ));
            data_points.push(NumberDataPoint {
                attributes: sample.attributes,
                time_unix_nano: sample.time_unix_nano,
                value: Some(sample.value),
                ..Default::default()
            });
        }
    }
    if by_host.is_empty() {
        return None;
    }
    let resource_metrics = by_host
        .into_iter()
        .map(|(host_id, metrics)| {
            let metrics = metrics
                .into_iter()
                .map(|(name, (description, unit, data_points))| Metric {
                    name: name.to_string(),
                    description: description.to_string(),
                    unit: unit.to_string(),
                    data: Some(metric::Data::Gauge(Gauge { data_points })),
                    ..Default::default()
                })
                .collect();
            ResourceMetrics {
                resource: Some(resource_for_host(
                    host_id,
                    daemon_version_by_host.get(host_id).copied(),
                )),
                scope_metrics: vec![ScopeMetrics {
                    metrics,
                    ..Default::default()
                }],
                ..Default::default()
            }
        })
        .collect();
    Some(ExportMetricsServiceRequest { resource_metrics })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
