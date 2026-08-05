//! Pure `TelemetryEnvelope` → OTLP record mapping (no network I/O). See the
//! parent module's doc comment for the mapping table this file implements;
//! this module is the field-by-field implementation plus its unit tests.

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

use crate::telemetry::{RepoVisibility, SweepResult, TelemetryEnvelope, TelemetryRecord};

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

fn kv_string(key: &str, value: impl Into<String>) -> KeyValue {
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
fn nanos(ts: DateTime<Utc>) -> u64 {
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
fn resource_for_host(host_id: &str, daemon_version: Option<&str>) -> Resource {
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
    let (event_name, severity, body, attributes) = match &envelope.record {
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
            (
                "sweep.started",
                SeverityNumber::Info,
                format!("sweep started: {} issue #{}", r.repo, r.issue),
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
        TelemetryRecord::SweepCompleted(r) => (
            "sweep.completed",
            severity_for_result(r.result),
            format!("sweep completed ({}): {} issue #{}", result_str(r.result), r.repo, r.issue),
            vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.repo.visibility", visibility_str(r.visibility)),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.sweep_id", r.sweep_id.clone()),
                kv_string("loom.result", result_str(r.result)),
            ],
        ),
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
            for (key, value) in &r.config {
                attributes.push(kv_string(&format!("loom.config.{key}"), value.clone()));
            }
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
        TelemetryRecord::TokensSnapshot(_) | TelemetryRecord::HostHealth(_) => return None,
    };
    Some(LogRecord {
        time_unix_nano,
        observed_time_unix_nano: time_unix_nano,
        severity_number: severity as i32,
        severity_text: severity_text(severity).to_string(),
        body: Some(any_string(body)),
        attributes,
        event_name: event_name.to_string(),
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
                let mut attributes = vec![kv_string("account", account.account.clone())];
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
        TelemetryRecord::SweepStarted(_)
        | TelemetryRecord::SweepPhase(_)
        | TelemetryRecord::SweepCompleted(_)
        | TelemetryRecord::SweepOutcome(_) => Vec::new(),
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
mod tests {
    use super::*;
    use crate::telemetry::{
        HostHealthRecord, PhaseDuration, SweepCompletedRecord, SweepOutcomeRecord,
        SweepPhaseRecord, SweepStartedRecord, TokenAccountState, TokenSnapshotRecord,
    };

    fn ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn envelope(host_id: &str, record: TelemetryRecord) -> TelemetryEnvelope {
        let mut envelope = TelemetryEnvelope::new(host_id, record);
        envelope.emitted_at = ts();
        envelope
    }

    fn sweep_started_envelope() -> TelemetryEnvelope {
        envelope(
            "host-a",
            TelemetryRecord::SweepStarted(SweepStartedRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
                issue: 4858,
                sweep_id: "sweep-issue-4858-0".to_string(),
                started_at: ts(),
                model: Some("opus".to_string()),
                effort: Some("high".to_string()),
            }),
        )
    }

    fn sweep_phase_envelope() -> TelemetryEnvelope {
        envelope(
            "host-a",
            TelemetryRecord::SweepPhase(SweepPhaseRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Private,
                issue: 4858,
                sweep_id: "sweep-issue-4858-0".to_string(),
                phase: "builder".to_string(),
                entered_at: ts(),
            }),
        )
    }

    fn sweep_completed_envelope(result: SweepResult) -> TelemetryEnvelope {
        envelope(
            "host-a",
            TelemetryRecord::SweepCompleted(SweepCompletedRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
                issue: 4858,
                sweep_id: "sweep-issue-4858-0".to_string(),
                completed_at: ts(),
                result,
            }),
        )
    }

    fn sweep_outcome_envelope() -> TelemetryEnvelope {
        let mut config = std::collections::BTreeMap::new();
        config.insert("runtime".to_string(), "claude".to_string());
        envelope(
            "host-a",
            TelemetryRecord::SweepOutcome(SweepOutcomeRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
                issue: 4858,
                sweep_id: "sweep-issue-4858-0".to_string(),
                model: Some("opus".to_string()),
                effort: Some("high".to_string()),
                config,
                phase_durations: vec![
                    PhaseDuration {
                        phase: "curator".to_string(),
                        duration_sec: 12,
                    },
                    PhaseDuration {
                        phase: "builder".to_string(),
                        duration_sec: 340,
                    },
                ],
                total_duration_sec: 512,
                result: SweepResult::Success,
                pr_number: Some(4861),
                tokens_in: None,
                tokens_out: None,
                lines_added: None,
                lines_deleted: None,
            }),
        )
    }

    fn tokens_snapshot_envelope() -> TelemetryEnvelope {
        envelope(
            "host-b",
            TelemetryRecord::TokensSnapshot(TokenSnapshotRecord {
                captured_at: ts(),
                accounts: vec![
                    TokenAccountState {
                        account: "agent-1".to_string(),
                        rank: Some(0),
                        usage_fraction: Some(0.42),
                        limit_window_reset_at: Some(ts()),
                        exhausted: false,
                    },
                    TokenAccountState {
                        account: "agent-2".to_string(),
                        rank: None,
                        usage_fraction: None,
                        limit_window_reset_at: None,
                        exhausted: true,
                    },
                ],
            }),
        )
    }

    fn host_health_envelope() -> TelemetryEnvelope {
        envelope(
            "host-b",
            TelemetryRecord::HostHealth(HostHealthRecord {
                captured_at: ts(),
                daemon_version: "0.17.0".to_string(),
                build_commit: "8c16fb5b".to_string(),
                built_at: Some(ts()),
                uptime_sec: 86_400,
                logical_cpus: 28,
                cpu_idle_fraction: Some(0.83),
                load_per_core: Some(0.51),
                worktree_root_free_gb: Some(200),
                active_sweep_ids: Vec::new(),
                dispatch_halted: false,
                halt_reason: None,
                managed_repos: Vec::new(),
                roles: crate::telemetry::RoleTickHealth {
                    total: 12,
                    ok: 10,
                    persistent: vec![crate::telemetry::RoleTickFailureEntry {
                        root: std::path::PathBuf::from("/repos/loom"),
                        role: "judge".to_string(),
                        failures: 2,
                        last_at: ts(),
                        detail: Some("no-token-pool".to_string()),
                    }],
                },
            }),
        )
    }

    // ------------------------------------------------------------------
    // Logs
    // ------------------------------------------------------------------

    #[test]
    fn lifecycle_records_become_log_records_with_the_kind_tag_as_event_name() {
        let batch = vec![
            sweep_started_envelope(),
            sweep_phase_envelope(),
            sweep_completed_envelope(SweepResult::Success),
            sweep_outcome_envelope(),
        ];
        let request =
            build_logs_request(&batch).expect("lifecycle batch must produce a logs request");
        assert_eq!(request.resource_logs.len(), 1, "single host_id ⇒ one ResourceLogs");
        let resource_logs = &request.resource_logs[0];
        let log_records = &resource_logs.scope_logs[0].log_records;
        assert_eq!(log_records.len(), 4);
        let event_names: Vec<&str> = log_records.iter().map(|r| r.event_name.as_str()).collect();
        assert_eq!(
            event_names,
            vec![
                "sweep.started",
                "sweep.phase",
                "sweep.completed",
                "sweep.outcome"
            ]
        );
    }

    #[test]
    fn host_id_becomes_resource_attributes() {
        let batch = vec![sweep_started_envelope()];
        let request = build_logs_request(&batch).unwrap();
        let resource = request.resource_logs[0].resource.as_ref().unwrap();
        let get = |key: &str| {
            resource
                .attributes
                .iter()
                .find(|kv| kv.key == key)
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.as_ref())
        };
        assert_eq!(
            get("service.instance.id"),
            Some(&any_value::Value::StringValue("host-a".to_string()))
        );
        assert_eq!(get("host.id"), Some(&any_value::Value::StringValue("host-a".to_string())));
    }

    #[test]
    fn emitted_at_becomes_the_log_record_timestamp() {
        let batch = vec![sweep_started_envelope()];
        let request = build_logs_request(&batch).unwrap();
        let log_record = &request.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(log_record.time_unix_nano, nanos(ts()));
        assert_eq!(log_record.observed_time_unix_nano, nanos(ts()));
    }

    #[test]
    fn repo_visibility_tag_becomes_a_log_record_attribute_not_a_resource_attribute() {
        let batch = vec![sweep_started_envelope(), sweep_phase_envelope()];
        let request = build_logs_request(&batch).unwrap();
        // Not on the shared Resource — a batch can mix repos/visibilities
        // under one host_id, so visibility cannot be a Resource-level tag.
        let resource = request.resource_logs[0].resource.as_ref().unwrap();
        assert!(resource
            .attributes
            .iter()
            .all(|kv| kv.key != "loom.repo.visibility"));

        let log_records = &request.resource_logs[0].scope_logs[0].log_records;
        let get_visibility = |record: &LogRecord| {
            record
                .attributes
                .iter()
                .find(|kv| kv.key == "loom.repo.visibility")
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.clone())
        };
        assert_eq!(
            get_visibility(&log_records[0]),
            Some(any_value::Value::StringValue("public".to_string()))
        );
        assert_eq!(
            get_visibility(&log_records[1]),
            Some(any_value::Value::StringValue("private".to_string()))
        );
    }

    #[test]
    fn sweep_outcome_flattens_config_and_nests_phase_durations() {
        let batch = vec![sweep_outcome_envelope()];
        let request = build_logs_request(&batch).unwrap();
        let log_record = &request.resource_logs[0].scope_logs[0].log_records[0];
        let get = |key: &str| {
            log_record
                .attributes
                .iter()
                .find(|kv| kv.key == key)
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.clone())
        };
        assert_eq!(
            get("loom.config.runtime"),
            Some(any_value::Value::StringValue("claude".to_string()))
        );
        assert_eq!(get("loom.pr_number"), Some(any_value::Value::IntValue(4861)));
        match get("loom.phase_durations") {
            Some(any_value::Value::ArrayValue(array)) => {
                assert_eq!(array.values.len(), 2);
            }
            other => panic!("expected loom.phase_durations to be an ArrayValue, got {other:?}"),
        }
    }

    #[test]
    fn failure_and_blocked_results_raise_severity() {
        let failure = sweep_completed_envelope(SweepResult::Failure);
        let request = build_logs_request(std::slice::from_ref(&failure)).unwrap();
        let log_record = &request.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(log_record.severity_number, SeverityNumber::Error as i32);

        let blocked = sweep_completed_envelope(SweepResult::Blocked);
        let request = build_logs_request(std::slice::from_ref(&blocked)).unwrap();
        let log_record = &request.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(log_record.severity_number, SeverityNumber::Warn as i32);

        let success = sweep_completed_envelope(SweepResult::Success);
        let request = build_logs_request(std::slice::from_ref(&success)).unwrap();
        let log_record = &request.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(log_record.severity_number, SeverityNumber::Info as i32);
    }

    #[test]
    fn host_level_records_produce_no_log_records() {
        let batch = vec![tokens_snapshot_envelope(), host_health_envelope()];
        assert!(
            build_logs_request(&batch).is_none(),
            "an all-metrics batch must not produce a (empty) logs request"
        );
    }

    #[test]
    fn empty_batch_produces_no_logs_request() {
        assert!(build_logs_request(&[]).is_none());
    }

    // ------------------------------------------------------------------
    // Metrics
    // ------------------------------------------------------------------

    #[test]
    fn host_health_becomes_gauge_metrics() {
        let batch = vec![host_health_envelope()];
        let request =
            build_metrics_request(&batch).expect("host.health must produce a metrics request");
        assert_eq!(request.resource_metrics.len(), 1);
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        for expected in [
            "loom.host.uptime_seconds",
            "loom.host.logical_cpus",
            "loom.host.cpu_idle_fraction",
            "loom.host.load_per_core",
            "loom.host.worktree_root_free_gb",
            "loom.host.roles_total_ticks",
            "loom.host.roles_ok_ticks",
            "loom.host.roles_persistent_failures",
        ] {
            assert!(names.contains(&expected), "missing metric {expected} in {names:?}");
        }
    }

    #[test]
    fn role_tick_health_becomes_gauge_metrics_with_the_persistent_failure_count() {
        // #5022: `host_health_envelope`'s fixture carries one persistent
        // failure (judge @ /repos/loom, 2 failed ticks of 12 total, 10 ok).
        let batch = vec![host_health_envelope()];
        let request = build_metrics_request(&batch).unwrap();
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        let value_of = |name: &str| -> i64 {
            let metric = metrics
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("missing metric {name}"));
            let points = match &metric.data {
                Some(metric::Data::Gauge(gauge)) => &gauge.data_points,
                other => panic!("expected a Gauge for {name}, got {other:?}"),
            };
            match points[0].value {
                Some(number_data_point::Value::AsInt(v)) => v,
                ref other => panic!("expected AsInt for {name}, got {other:?}"),
            }
        };
        assert_eq!(value_of("loom.host.roles_total_ticks"), 12);
        assert_eq!(value_of("loom.host.roles_ok_ticks"), 10);
        assert_eq!(value_of("loom.host.roles_persistent_failures"), 1);
    }

    #[test]
    fn role_tick_health_with_no_ticks_sampled_reports_zero_not_no_metric() {
        // `total: 0` (role runner idle/disabled) is a known value, not an
        // unmeasurable probe — it must still produce a data point (0), unlike
        // `cpu_idle_fraction: None` which produces none at all.
        let mut record = host_health_envelope();
        if let TelemetryRecord::HostHealth(r) = &mut record.record {
            r.roles = crate::telemetry::RoleTickHealth::default();
        }
        let batch = vec![record];
        let request = build_metrics_request(&batch).unwrap();
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"loom.host.roles_total_ticks"));
        assert!(names.contains(&"loom.host.roles_persistent_failures"));
    }

    #[test]
    fn daemon_version_becomes_a_resource_attribute_not_a_metric() {
        let batch = vec![host_health_envelope()];
        let request = build_metrics_request(&batch).unwrap();
        let resource = request.resource_metrics[0].resource.as_ref().unwrap();
        let version = resource
            .attributes
            .iter()
            .find(|kv| kv.key == "service.version")
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone());
        assert_eq!(version, Some(any_value::Value::StringValue("0.17.0".to_string())));
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        assert!(
            metrics
                .iter()
                .all(|m| m.name != "service.version" && !m.name.contains("daemon_version")),
            "daemon_version must not also appear as a metric"
        );
    }

    #[test]
    fn unmeasured_optional_fields_produce_no_data_point() {
        let mut record = HostHealthRecord {
            captured_at: ts(),
            daemon_version: "0.17.0".to_string(),
            build_commit: "8c16fb5b".to_string(),
            built_at: None,
            uptime_sec: 10,
            logical_cpus: 4,
            cpu_idle_fraction: None,
            load_per_core: None,
            worktree_root_free_gb: None,
            active_sweep_ids: Vec::new(),
            dispatch_halted: false,
            halt_reason: None,
            managed_repos: Vec::new(),
            roles: crate::telemetry::RoleTickHealth::default(),
        };
        let batch = vec![envelope(
            "host-c",
            TelemetryRecord::HostHealth(record.clone()),
        )];
        let request = build_metrics_request(&batch).unwrap();
        let names: Vec<&str> = request.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(!names.contains(&"loom.host.cpu_idle_fraction"));
        assert!(!names.contains(&"loom.host.load_per_core"));
        assert!(!names.contains(&"loom.host.worktree_root_free_gb"));

        // Sanity: setting the field produces the data point.
        record.cpu_idle_fraction = Some(0.5);
        let batch = vec![envelope("host-c", TelemetryRecord::HostHealth(record))];
        let request = build_metrics_request(&batch).unwrap();
        let names: Vec<&str> = request.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(names.contains(&"loom.host.cpu_idle_fraction"));
    }

    #[test]
    fn tokens_snapshot_becomes_per_account_gauge_metrics() {
        let batch = vec![tokens_snapshot_envelope()];
        let request = build_metrics_request(&batch).unwrap();
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        let usage_metric = metrics
            .iter()
            .find(|m| m.name == "loom.tokens.usage_fraction")
            .expect("usage_fraction metric must exist");
        let exhausted_metric = metrics
            .iter()
            .find(|m| m.name == "loom.tokens.exhausted")
            .expect("exhausted metric must exist");

        // Only agent-1 has a known usage_fraction (agent-2's is None ⇒ no
        // data point for that account, per the "unknown != zero" contract).
        let usage_points = match &usage_metric.data {
            Some(metric::Data::Gauge(gauge)) => &gauge.data_points,
            other => panic!("expected Gauge, got {other:?}"),
        };
        assert_eq!(usage_points.len(), 1);
        let account_attr = |point: &NumberDataPoint| {
            point
                .attributes
                .iter()
                .find(|kv| kv.key == "account")
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.clone())
        };
        assert_eq!(
            account_attr(&usage_points[0]),
            Some(any_value::Value::StringValue("agent-1".to_string()))
        );
        assert_eq!(usage_points[0].value, Some(number_data_point::Value::AsDouble(0.42)));

        // Every account (both) has an exhausted data point.
        let exhausted_points = match &exhausted_metric.data {
            Some(metric::Data::Gauge(gauge)) => &gauge.data_points,
            other => panic!("expected Gauge, got {other:?}"),
        };
        assert_eq!(exhausted_points.len(), 2);
    }

    #[test]
    fn metrics_are_grouped_by_host_id() {
        let batch = vec![host_health_envelope(), sweep_started_envelope()];
        // host_health_envelope is host-b, sweep_started_envelope is host-a —
        // but sweep_started is a lifecycle record and contributes no metric
        // sample, so only host-b should appear in the metrics request.
        let request = build_metrics_request(&batch).unwrap();
        assert_eq!(request.resource_metrics.len(), 1);
        let resource = request.resource_metrics[0].resource.as_ref().unwrap();
        let host_id = resource
            .attributes
            .iter()
            .find(|kv| kv.key == "host.id")
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone());
        assert_eq!(host_id, Some(any_value::Value::StringValue("host-b".to_string())));
    }

    #[test]
    fn lifecycle_records_produce_no_metrics() {
        let batch = vec![
            sweep_started_envelope(),
            sweep_phase_envelope(),
            sweep_completed_envelope(SweepResult::Success),
            sweep_outcome_envelope(),
        ];
        assert!(
            build_metrics_request(&batch).is_none(),
            "an all-lifecycle batch must not produce a (empty) metrics request"
        );
    }

    #[test]
    fn empty_batch_produces_no_metrics_request() {
        assert!(build_metrics_request(&[]).is_none());
    }

    #[test]
    fn mixed_batch_produces_both_a_logs_and_a_metrics_request() {
        let batch = vec![
            sweep_started_envelope(),
            host_health_envelope(),
            tokens_snapshot_envelope(),
        ];
        assert!(build_logs_request(&batch).is_some());
        assert!(build_metrics_request(&batch).is_some());
    }
}
