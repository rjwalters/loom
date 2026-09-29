#![allow(clippy::unwrap_used)]
use super::*;
use crate::telemetry::{JudgeVerdict, TelemetryEnvelope, TelemetryRecord};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
fn outcome_record() -> SweepOutcomeRecord {
    serde_json::from_value(serde_json::json!({
        "repo":"test/fixture", "issue":18, "sweep_id":"fixture-attempt-1",
        "total_duration_sec":3, "result":"failure"
    }))
    .unwrap()
}
fn map(record: TelemetryRecord) -> LogRecord {
    super::super::log_record_for(&TelemetryEnvelope::new("fixture-host", record)).unwrap()
}
fn attribute(log: &LogRecord, key: &str) -> Option<any_value::Value> {
    log.attributes
        .iter()
        .find(|v| v.key == key)
        .and_then(|v| v.value.as_ref())
        .and_then(|v| v.value.clone())
}
#[test]
fn outcomes_preserve_observed_zero_empty_history_and_missing_measurements() {
    let mut record = outcome_record();
    let missing = map(TelemetryRecord::SweepOutcome(record.clone()));
    for key in [
        "loom.doctor_cycles",
        "loom.judge_verdicts",
        "loom.tokens_in",
        "loom.tokens_by_model",
    ] {
        assert!(attribute(&missing, key).is_none(), "{key}");
    }
    record.doctor_cycles = Some(0);
    record.judge_verdicts = Some(vec![]);
    record.tokens_in = Some(0);
    record.tokens_by_model = Some(vec![ModelUsageTotals {
        model: "glm-5.3-flash".into(),
        ..Default::default()
    }]);
    let observed = map(TelemetryRecord::SweepOutcome(record));
    assert_eq!(attribute(&observed, "loom.doctor_cycles"), Some(any_value::Value::IntValue(0)));
    assert_eq!(attribute(&observed, "loom.tokens_in"), Some(any_value::Value::IntValue(0)));
    let Some(any_value::Value::ArrayValue(history)) = attribute(&observed, "loom.judge_verdicts")
    else {
        panic!("missing observed history")
    };
    assert!(history.values.is_empty());
    let Some(any_value::Value::ArrayValue(groups)) = attribute(&observed, "loom.tokens_by_model")
    else {
        panic!("missing observed usage")
    };
    let Some(any_value::Value::KvlistValue(group)) = &groups.values[0].value else {
        panic!("missing grouping dimensions")
    };
    assert_eq!(
        group
            .values
            .iter()
            .find(|v| v.key == "input")
            .unwrap()
            .value
            .as_ref()
            .unwrap()
            .value,
        Some(any_value::Value::IntValue(0))
    );
    assert!(attribute(&observed, "loom.tokens_out").is_none());
}

/// Issue #9440: the status pair that makes an absent token counter readable.
/// Both keys are exported so a SigNoz query can `GROUP BY` either; a
/// `measured` status carries no reason, because a measurement needs no excuse.
#[test]
fn outcomes_export_the_tokens_status_pair_as_groupable_attributes() {
    let mut record = outcome_record();
    // Pre-#9440 records carry no status at all, and must stay that way on the
    // wire — an absent status reads as "unknown", never as one of the three.
    let legacy = map(TelemetryRecord::SweepOutcome(record.clone()));
    for key in ["loom.tokens_status", "loom.tokens_status_reason"] {
        assert!(attribute(&legacy, key).is_none(), "{key}");
    }

    record.tokens_status = Some(crate::telemetry::TokensStatus::NotSpawned);
    record.tokens_status_reason = Some("preflight-no-cli-start".to_string());
    record.tokens_in = Some(0);
    record.tokens_out = Some(0);
    let not_spawned = map(TelemetryRecord::SweepOutcome(record.clone()));
    assert_eq!(
        attribute(&not_spawned, "loom.tokens_status"),
        Some(any_value::Value::StringValue("not_spawned".to_string()))
    );
    assert_eq!(
        attribute(&not_spawned, "loom.tokens_status_reason"),
        Some(any_value::Value::StringValue("preflight-no-cli-start".to_string()))
    );
    // The measured zero rides alongside it — this is the one status for which
    // zero is the observation rather than the absence of one.
    assert_eq!(attribute(&not_spawned, "loom.tokens_in"), Some(any_value::Value::IntValue(0)));

    record.tokens_status = Some(crate::telemetry::TokensStatus::Measured);
    record.tokens_status_reason = None;
    let measured = map(TelemetryRecord::SweepOutcome(record));
    assert_eq!(
        attribute(&measured, "loom.tokens_status"),
        Some(any_value::Value::StringValue("measured".to_string()))
    );
    assert!(attribute(&measured, "loom.tokens_status_reason").is_none());
}
#[test]
fn repair_history_and_observed_model_groups_survive_without_creating_metrics() {
    let mut record = outcome_record();
    record.failure_class = Some("account-exhausted:model-credits-exhausted".into());
    record.doctor_cycles = Some(1);
    record.judge_verdicts = Some(vec![
        JudgeVerdict {
            attempt: 1,
            verdict: "fail".into(),
        },
        JudgeVerdict {
            attempt: 2,
            verdict: "pass".into(),
        },
    ]);
    record.models_used = Some(vec!["glm-5.3-flash".into(), "second-observed-model".into()]);
    let envelope = TelemetryEnvelope::new("fixture-host", TelemetryRecord::SweepOutcome(record));
    let log = super::super::log_record_for(&envelope).unwrap();
    assert_eq!(
        attribute(&log, "loom.failure_class"),
        Some(any_value::Value::StringValue(
            "account-exhausted:model-credits-exhausted".into()
        ))
    );
    let Some(any_value::Value::ArrayValue(history)) = attribute(&log, "loom.judge_verdicts") else {
        panic!("missing repair history")
    };
    assert_eq!(history.values.len(), 2);
    let Some(any_value::Value::KvlistValue(first)) = &history.values[0].value else {
        panic!("missing verdict")
    };
    assert_eq!(
        first.values[1].value.as_ref().unwrap().value,
        Some(any_value::Value::StringValue("fail".into()))
    );
    assert!(super::super::build_metrics_request(&[envelope]).is_none());
}
#[test]
fn freeform_details_and_config_credentials_never_enter_otlp() {
    let mut record = outcome_record();
    record.config.insert("runtime".into(), "pi".into());
    record
        .config
        .insert("token_account".into(), "SECRET_ACCOUNT_CONTENTS".into());
    record
        .config
        .insert("api_key".into(), "SECRET_API_KEY".into());
    record
        .config
        .insert("prompt".into(), "SECRET_PROMPT".into());
    let log = map(TelemetryRecord::SweepOutcome(record));
    let json = serde_json::to_string(&log).unwrap();
    assert!(!json.contains("SECRET"));
    assert_eq!(
        attribute(&log, "loom.runtime"),
        Some(any_value::Value::StringValue("pi".into()))
    );
    let role = serde_json::from_value(serde_json::json!({
        "repo":"test/fixture", "role":"judge", "started_at":"2026-09-21T00:00:00Z",
        "duration_sec":1,"result":"failure","detail":"SECRET_EXCEPTION_WITH_TOOL_OUTPUT"
    }))
    .unwrap();
    let log = map(TelemetryRecord::RoleTickOutcome(role));
    assert!(!serde_json::to_string(&log).unwrap().contains("SECRET"));
    assert!(attribute(&log, "loom.detail").is_none());
}

/// Issue #8757: a `session.summary` built from a transcript that contained a
/// prompt, raw tool output, a key and an email carries none of the four onto
/// the OTLP wire — the record's fields are counts, ids and allowlisted
/// names by construction, and this pins that neither the JSON record nor
/// its OTLP mapping opens a free-text channel.
#[test]
fn session_summary_never_carries_prompt_tool_output_key_or_email() {
    let record: crate::telemetry::SessionSummaryRecord =
        serde_json::from_value(serde_json::json!({
            "repo": "fixture-repo", "session_id": "uuid-a",
            "parent_session_id": "uuid-parent", "runtime": "claude",
            "role": "builder", "issue": 8757,
            "models": ["claude-sonnet-5"],
            "tokens_input": 12, "tokens_output": 24,
            "tokens_cache_read": 100, "tokens_cache_write": 10,
            "wall_ms": 181000, "turns": 1,
            "tool_calls": [{"tool": "Bash", "count": 2}],
            "tool_errors": 1,
        }))
        .unwrap();
    let log = map(TelemetryRecord::SessionSummary(record));
    let json = serde_json::to_string(&log).unwrap();
    for leaked in ["SECRET", "sk-ant", "@example.com", "prompt", "tool_output"] {
        assert!(!json.contains(leaked), "{leaked} leaked: {json}");
    }
    assert_eq!(
        attribute(&log, "loom.session_id"),
        Some(any_value::Value::StringValue("uuid-a".into()))
    );
    assert_eq!(
        attribute(&log, "loom.parent_session_id"),
        Some(any_value::Value::StringValue("uuid-parent".into()))
    );
    assert_eq!(attribute(&log, "loom.turns"), Some(any_value::Value::IntValue(1)));
    assert!(attribute(&log, "loom.outcome").is_none(), "unknown outcome stays absent");
}

/// Issue #9445: the join keys map onto the wire as attributes — and an
/// unresolved repo is an **absent** `loom.repo`, never a directory name.
#[test]
fn session_summary_maps_its_join_keys_and_omits_an_unresolved_repo() {
    let base = serde_json::json!({
        "session_id": "uuid-a", "runtime": "claude", "models": [],
        "tokens_input": 0, "tokens_output": 0,
        "tokens_cache_read": 0, "tokens_cache_write": 0,
        "wall_ms": 0, "turns": 0, "tool_calls": [], "tool_errors": 0,
    });
    let with = |extra: serde_json::Value| {
        let mut value = base.clone();
        let (object, extra) = (value.as_object_mut().unwrap(), extra);
        for (key, item) in extra.as_object().unwrap() {
            object.insert(key.clone(), item.clone());
        }
        let record: crate::telemetry::SessionSummaryRecord = serde_json::from_value(value).unwrap();
        map(TelemetryRecord::SessionSummary(record))
    };

    let joined = with(serde_json::json!({
        "repo": "apache/superset", "issue": 9445, "pr_number": 9460,
        "session_kind": "sweep",
    }));
    assert_eq!(
        attribute(&joined, "loom.repo"),
        Some(any_value::Value::StringValue("apache/superset".into()))
    );
    assert_eq!(attribute(&joined, "loom.issue"), Some(any_value::Value::IntValue(9445)));
    assert_eq!(attribute(&joined, "loom.pr_number"), Some(any_value::Value::IntValue(9460)));
    assert_eq!(
        attribute(&joined, "loom.session_kind"),
        Some(any_value::Value::StringValue("sweep".into()))
    );

    let unresolved = with(serde_json::json!({ "session_kind": "interactive" }));
    assert!(
        attribute(&unresolved, "loom.repo").is_none(),
        "an unresolvable repo is absent, not a guessed name"
    );
    assert!(attribute(&unresolved, "loom.issue").is_none());
    assert_eq!(
        attribute(&unresolved, "loom.session_kind"),
        Some(any_value::Value::StringValue("interactive".into())),
        "so the absent issue reads as deliberate"
    );
}
/// Issue #8760: a `session.analysis` record — every field a count, id,
/// allowlisted tool name, duration, or derived dollar figure — carries none
/// of a prompt/tool-output/key/email marker onto the OTLP wire either,
/// mirroring `session_summary_never_carries_prompt_tool_output_key_or_email`
/// above (the redaction test suite extended for the new kind, per #8760's
/// Test Plan).
#[test]
fn session_analysis_never_carries_prompt_tool_output_key_or_email() {
    let record: crate::telemetry::SessionAnalysisRecord =
        serde_json::from_value(serde_json::json!({
            "repo": "fixture-repo", "session_id": "uuid-a",
            "parent_session_id": "uuid-parent",
            "retry_loops": [{"tool": "Bash", "length": 3}],
            "longest_tool_call": {"tool": "Bash", "duration_ms": 5000},
            "cost_usd": 0.042,
            "anomalies": ["high_token_usage"],
        }))
        .unwrap();
    let log = map(TelemetryRecord::SessionAnalysis(record));
    let json = serde_json::to_string(&log).unwrap();
    for leaked in ["SECRET", "sk-ant", "@example.com", "prompt", "tool_output"] {
        assert!(!json.contains(leaked), "{leaked} leaked: {json}");
    }
    assert_eq!(
        attribute(&log, "loom.session_id"),
        Some(any_value::Value::StringValue("uuid-a".into()))
    );
    assert_eq!(
        attribute(&log, "loom.longest_tool_call.tool"),
        Some(any_value::Value::StringValue("Bash".into()))
    );
    assert_eq!(
        attribute(&log, "loom.longest_tool_call.duration_ms"),
        Some(any_value::Value::IntValue(5000))
    );
    assert!(matches!(
        attribute(&log, "loom.cost_usd"),
        Some(any_value::Value::DoubleValue(_))
    ));
}

/// Issue #8760 (G4): a `daemon.event` record's `payload` is carried whole,
/// but it is sourced only from the already-reviewed, small, operator-facing
/// event-bus payloads (`crate::event_bus`'s frozen taxonomy) — never from
/// transcript content — so the same marker set never appears.
#[test]
fn daemon_event_payload_carries_no_secret_markers() {
    let record = crate::telemetry::DaemonEventRecord {
        topic: "daemon.drain.started".to_string(),
        payload: serde_json::json!({"in_flight": 2, "timeout_secs": 300}),
    };
    let log = map(TelemetryRecord::DaemonEvent(record));
    let json = serde_json::to_string(&log).unwrap();
    for leaked in ["SECRET", "sk-ant", "@example.com"] {
        assert!(!json.contains(leaked), "{leaked} leaked: {json}");
    }
    assert_eq!(
        attribute(&log, "loom.topic"),
        Some(any_value::Value::StringValue("daemon.drain.started".into()))
    );
}

#[test]
fn oversized_or_invalid_usage_is_omitted_without_truncating_identity_or_fabricating_zero() {
    let mut record = outcome_record();
    record.model = Some("x".repeat(257));
    record.tokens_out = Some(u64::MAX);
    record.tokens_by_model = Some(vec![ModelUsageTotals {
        model: "glm-5.3-flash".into(),
        input: -1,
        ..Default::default()
    }]);
    let log = map(TelemetryRecord::SweepOutcome(record.clone()));
    for key in ["loom.model", "loom.tokens_out", "loom.tokens_by_model"] {
        assert!(attribute(&log, key).is_none());
    }
    assert!(!serde_json::to_string(&log)
        .unwrap()
        .contains(&"x".repeat(257)));
    record.tokens_by_model = Some(vec![ModelUsageTotals::default(); 65]);
    assert!(
        attribute(&map(TelemetryRecord::SweepOutcome(record)), "loom.tokens_by_model").is_none()
    );
}

/// Issue #9443: the per-phase-attempt breakdown reaches OTLP with the same
/// absent-vs-zero discipline the JSONL record uses, and the remainder is
/// exported beside the totals so an OTLP consumer can check
/// `Σ phases + remainder == total` without the JSONL journal.
#[test]
fn per_phase_usage_and_the_unattributed_remainder_reach_otlp_absent_not_zero() {
    let mut record = outcome_record();
    // Four measured phases total (330, 33); the fifth (the re-judge) is
    // unmeasured, so the sweep's own totals exceed them and the remainder — 40
    // in / 9 out — carries both the re-judge's real spend and the unattributable
    // trailing segment.
    record.tokens_in = Some(370);
    record.tokens_out = Some(42);
    record.tokens_unattributed = Some(crate::telemetry::TokenTotals {
        tokens_in: 40,
        tokens_out: 9,
    });
    record.phase_durations = vec![
        PhaseDuration {
            attempt: Some(1),
            tokens_in: Some(10),
            tokens_out: Some(1),
            ..PhaseDuration::new("curator", 12)
        },
        PhaseDuration {
            attempt: Some(1),
            tokens_in: Some(100),
            tokens_out: Some(10),
            ..PhaseDuration::new("builder", 340)
        },
        PhaseDuration {
            attempt: Some(1),
            tokens_in: Some(20),
            tokens_out: Some(2),
            ..PhaseDuration::new("judge", 30)
        },
        PhaseDuration {
            attempt: Some(1),
            tokens_in: Some(200),
            tokens_out: Some(20),
            ..PhaseDuration::new("doctor", 90)
        },
        // The re-judge: a separate entry with attempt 2, and deliberately
        // unmeasured — its share is part of the remainder, not a `0`.
        PhaseDuration {
            attempt: Some(2),
            ..PhaseDuration::new("judge", 25)
        },
    ];
    let log = map(TelemetryRecord::SweepOutcome(record));
    assert_eq!(
        attribute(&log, "loom.tokens_unattributed_in"),
        Some(any_value::Value::IntValue(40))
    );
    assert_eq!(
        attribute(&log, "loom.tokens_unattributed_out"),
        Some(any_value::Value::IntValue(9))
    );
    let Some(any_value::Value::ArrayValue(phases)) = attribute(&log, "loom.phase_durations") else {
        panic!("missing per-phase breakdown")
    };
    assert_eq!(phases.values.len(), 5, "judge #1 and judge #2 stay separate entries");
    let entry = |index: usize| -> Vec<(String, Option<any_value::Value>)> {
        let Some(any_value::Value::KvlistValue(list)) = &phases.values[index].value else {
            panic!("phase entry {index} is not a kvlist")
        };
        list.values
            .iter()
            .map(|kv| (kv.key.clone(), kv.value.as_ref().and_then(|v| v.value.clone())))
            .collect()
    };
    let field = |index: usize, key: &str| -> Option<any_value::Value> {
        entry(index)
            .into_iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v)
    };
    assert_eq!(field(0, "phase"), Some(any_value::Value::StringValue("curator".into())));
    assert_eq!(field(0, "duration_sec"), Some(any_value::Value::IntValue(12)));
    assert_eq!(field(0, "attempt"), Some(any_value::Value::IntValue(1)));
    assert_eq!(field(0, "tokens_in"), Some(any_value::Value::IntValue(10)));
    assert_eq!(field(0, "tokens_out"), Some(any_value::Value::IntValue(1)));
    // The second judge: addressable by `attempt`, and unmeasured — so its token
    // keys are OMITTED, never published as 0.
    assert_eq!(field(4, "phase"), Some(any_value::Value::StringValue("judge".into())));
    assert_eq!(field(4, "attempt"), Some(any_value::Value::IntValue(2)));
    for key in ["tokens_in", "tokens_out"] {
        assert!(field(4, key).is_none(), "an unmeasured phase must omit {key}");
    }
    // Σ exported phase tokens + the exported remainder == the exported totals.
    let summed = |key: &str| -> i64 {
        (0..5)
            .filter_map(|i| match field(i, key) {
                Some(any_value::Value::IntValue(v)) => Some(v),
                _ => None,
            })
            .sum()
    };
    assert_eq!(
        (summed("tokens_in") + 40, summed("tokens_out") + 9),
        (370, 42),
        "the OTLP side reconciles exactly as the JSONL record does"
    );
    assert_eq!(attribute(&log, "loom.tokens_in"), Some(any_value::Value::IntValue(370)));

    // Per-phase `tokens_by_model` is deliberately NOT nested here — it reaches
    // OTLP as `loom.runtime.usage` spans with `loom.usage.scope=attempt`.
    let json = serde_json::to_string(&log).unwrap();
    assert!(!json.contains("tokens_by_model"), "{json}");
}

/// A control-character or oversized phase name drops the whole attribute rather
/// than exporting a partial breakdown — same posture as `usage` above.
#[test]
fn an_invalid_or_oversized_phase_breakdown_is_omitted_whole() {
    let mut record = outcome_record();
    record.phase_durations = vec![PhaseDuration::new("bui\u{0}lder", 1)];
    assert!(
        attribute(&map(TelemetryRecord::SweepOutcome(record.clone())), "loom.phase_durations")
            .is_none()
    );
    record.phase_durations = vec![PhaseDuration::new("builder", 1); MAX_GROUPS + 1];
    assert!(
        attribute(&map(TelemetryRecord::SweepOutcome(record)), "loom.phase_durations").is_none()
    );
}
