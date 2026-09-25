//! Unit tests for [`super`] — extracted from the parent module's inline
//! `#[cfg(test)] mod tests` (Issue #8056) so the parent stays inside the
//! file-size ratchet (`scripts/check-file-size-budget.sh`). Content is
//! unchanged apart from dedenting and the new-module additions.

use super::*;
use crate::telemetry::{
    HostHealthRecord, PhaseDuration, SweepCompletedRecord, SweepOutcomeRecord, SweepPhaseRecord,
    SweepStartedRecord, TokenAccountState, TokenSnapshotRecord,
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
            runtime: Some("claude".to_string()),
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
            tokens_by_model: None,
        }),
    )
}

fn sweep_completed_envelope_with_tokens_by_model() -> TelemetryEnvelope {
    envelope(
        "host-a",
        TelemetryRecord::SweepCompleted(SweepCompletedRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Public,
            issue: 4858,
            sweep_id: "sweep-issue-4858-0".to_string(),
            completed_at: ts(),
            result: SweepResult::Success,
            tokens_by_model: Some(vec![
                crate::script_helpers::sweep_experiment::ModelUsageTotals {
                    model: "claude-sonnet-5".to_string(),
                    speed: "standard".to_string(),
                    service_tier: "standard".to_string(),
                    input: 48_000,
                    cache_read: 15_000,
                    cache_write_5m: 500,
                    cache_write_1h: 1_500,
                    output: 6_120,
                },
            ]),
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
            tokens_by_model: None,
            // Issue #8507: this fixture is a Claude sweep, which writes no
            // launch record — so all three stay absent.
            runtime: None,
            provider: None,
            profile: None,
            failure_class: None,
            models_used: None,
            doctor_cycles: None,
            judge_verdicts: None,
            complexity: None,
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
                    provider: "claude".to_string(),
                    rank: Some(0),
                    usage_fraction: Some(0.42),
                    limit_window_reset_at: Some(ts()),
                    exhausted: false,
                },
                TokenAccountState {
                    account: "agent-2".to_string(),
                    provider: "codex".to_string(),
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
            worktree_root_total_gb: Some(1000),
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
            protection: None,
            admission_brake: None,
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
    let request = build_logs_request(&batch).expect("lifecycle batch must produce a logs request");
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
fn sweep_completed_emits_tokens_by_model_as_a_kvlist_array_attribute() {
    // Issue #6384: the OTLP mapping must not silently drop the new
    // per-model token field.
    let batch = vec![sweep_completed_envelope_with_tokens_by_model()];
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
    match get("loom.tokens_by_model") {
        Some(any_value::Value::ArrayValue(array)) => {
            assert_eq!(array.values.len(), 1);
            match &array.values[0].value {
                Some(any_value::Value::KvlistValue(kvlist)) => {
                    let row_get = |key: &str| {
                        kvlist
                            .values
                            .iter()
                            .find(|kv| kv.key == key)
                            .and_then(|kv| kv.value.as_ref())
                            .and_then(|v| v.value.clone())
                    };
                    assert_eq!(
                        row_get("model"),
                        Some(any_value::Value::StringValue("claude-sonnet-5".to_string()))
                    );
                    assert_eq!(row_get("output"), Some(any_value::Value::IntValue(6_120)));
                }
                other => panic!("expected a KvlistValue row, got {other:?}"),
            }
        }
        other => panic!("expected loom.tokens_by_model to be an ArrayValue, got {other:?}"),
    }
}

#[test]
fn sweep_completed_omits_tokens_by_model_attribute_when_absent() {
    let batch = vec![sweep_completed_envelope(SweepResult::Success)];
    let request = build_logs_request(&batch).unwrap();
    let log_record = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert!(
        log_record
            .attributes
            .iter()
            .all(|kv| kv.key != "loom.tokens_by_model"),
        "no attribute should be emitted when tokens_by_model is None"
    );
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
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: crate::telemetry::RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
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

/// A `role_tick.outcome` envelope, parameterized on the two fields #8408's
/// `gated_pool` attribute depends on.
fn role_tick_outcome_envelope(
    result: crate::telemetry::RoleTickResult,
    gated_pool: Option<&str>,
) -> TelemetryEnvelope {
    envelope(
        "host-a",
        TelemetryRecord::RoleTickOutcome(crate::telemetry::RoleTickOutcomeRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Private,
            role: "judge".to_string(),
            started_at: ts(),
            duration_sec: 1,
            result,
            model: None,
            effort: None,
            detail: Some("codex-account-pool-exhausted: no account provisioned".to_string()),
            gated_pool: gated_pool.map(str::to_string),
            // Issue #8507: a Claude tick writes no launch record.
            runtime: None,
            provider: None,
            profile: None,
            tokens_by_model: None,
            models_used: None,
            actions: None,
        }),
    )
}

/// Issue #8408 shipped `loom.gated_pool` with no assertion behind it (#8444):
/// a pre-spawn pool skip must carry the pool it was gated on all the way to
/// the wire, and every other tick must leave the attribute absent — never an
/// empty-string placeholder.
#[test]
fn role_tick_outcome_maps_the_gated_pool_attribute_only_when_a_pool_gated_it() {
    use crate::telemetry::RoleTickResult;
    let batch = vec![
        role_tick_outcome_envelope(RoleTickResult::SkippedPoolExhausted, Some("codex_accounts")),
        role_tick_outcome_envelope(RoleTickResult::SkippedNoTokenPool, Some("claude_tokens")),
        role_tick_outcome_envelope(RoleTickResult::Success, None),
    ];
    let request = build_logs_request(&batch).unwrap();
    let log_records = &request.resource_logs[0].scope_logs[0].log_records;
    assert_eq!(log_records.len(), 3);
    let gated_pool = |record: &LogRecord| {
        record
            .attributes
            .iter()
            .find(|kv| kv.key == "loom.gated_pool")
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    };
    assert_eq!(
        gated_pool(&log_records[0]),
        Some(any_value::Value::StringValue("codex_accounts".to_string()))
    );
    assert_eq!(
        gated_pool(&log_records[1]),
        Some(any_value::Value::StringValue("claude_tokens".to_string()))
    );
    assert_eq!(
        gated_pool(&log_records[2]),
        None,
        "an unobserved field stays an ABSENT attribute"
    );
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

#[test]
fn active_identity_maps_distinct_runtime_provider_and_model_without_profile() {
    let record: TelemetryRecord = serde_json::from_str(include_str!(
        "../../../../../dashboard/test/fixtures/sweep-identity.json"
    ))
    .unwrap();
    let request = build_logs_request(&[envelope("host-a", record)]).unwrap();
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    for (key, expected) in [
        ("loom.runtime", "opencode"),
        ("loom.provider", "zai-coding-plan"),
        ("loom.model", "glm-5.3"),
    ] {
        let value = log
            .attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone());
        assert_eq!(value, Some(any_value::Value::StringValue(expected.to_owned())));
    }
    assert!(!log
        .attributes
        .iter()
        .any(|kv| kv.key.contains("profile") || kv.key.contains("credential")));
}
