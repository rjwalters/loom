//! `loom.kind` -- the kind discriminator on every OTLP log record (#9881,
//! #10899).
//!
//! The dashboard and the ETA readers filter logs on `loom.kind`, never on the
//! log `name` column (loom-ui#747). Until #10899 each `log_record_for` arm
//! had to push the attribute itself, and the `eta.*`, `ci.*`, `sweep.phase`,
//! `daemon.event`, `pr.resolved`, `session.output`, `auto_update.tick` and
//! `token_ranking.refresh` arms never did, so `loom.kind LIKE 'eta%'` read as
//! "no ETAs ever". The stamp is now applied once, after the match. The
//! contract below walks the kind registry: every `otlp: Logs` row must have a
//! minimal sample here and must map to exactly one `loom.kind` equal to its
//! wire tag, so a kind added later cannot ship unqueryable.

use super::*;
use crate::eta::emit::Trigger;
use crate::eta::heuristics::LandV1;
use crate::eta::score::{score, EstimateSummary, OutcomeKind};
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, Provenance, Stage, Subject,
};
use crate::telemetry::kinds::eta::{EtaEstimateRecord, EtaOutcomeRecord};
use crate::telemetry::kinds::eta_backtest::{EtaBacktestFoldRecord, EtaBacktestSummaryRecord};
use crate::telemetry::kinds::pick_decision::{
    PickCandidate, PickDecisionRecord, PickTick, PickVerdict,
};
use crate::telemetry::TELEMETRY_KINDS;
use serde_json::{json, Value};

const AT: &str = "2026-07-30T12:00:00Z";
const SWEEP_IDENTITY: &str = include_str!("../../../../../tests/fixtures/sweep-identity.json");

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.900".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn provenance_json() -> Value {
    serde_json::to_value(provenance()).unwrap()
}

/// A record from its wire JSON (`{"kind": …, …fields}`) — the cheapest way
/// to name a minimal instance of every payload shape.
fn wire(value: Value) -> TelemetryRecord {
    serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("minimal sample {value} must decode: {e}"))
}

/// A `ci.*` record: the fields every CI kind shares, plus `extra`.
fn ci(kind: &str, extra: Value) -> TelemetryRecord {
    let mut v = json!({"kind": kind, "repo": "rjwalters/loom", "run_id": 1, "workflow": "CI",
        "completed_at": AT});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    wire(v)
}

fn eta_estimate() -> EtaEstimateRecord {
    let as_of = ts();
    let input = EstimateInput {
        subject: Subject::new("rjwalters/loom", Some(1), 10899),
        as_of,
        current: CurrentState::At(CurrentStage {
            stage: Stage::ReviewWait,
            entered_at: Some(as_of),
            age_sec: 0,
            age_source: AgeSource::Bus,
            rework_rounds: 0,
            episode_entered_at: None,
        }),
        features: Default::default(),
        features_omitted: Vec::new(),
        provenance: provenance(),
        dispatch: None,
        stalls: Vec::new(),
        held: None,
        queue: Vec::new(),
        dependencies: None,
    };
    EtaEstimateRecord {
        trigger: Trigger::First,
        primary: true,
        explanation: Box::new(LandV1.estimate(&input, &Default::default())),
    }
}

fn eta_outcome() -> EtaOutcomeRecord {
    let summary = EstimateSummary::of(&eta_estimate().explanation);
    let at = summary.as_of + chrono::Duration::seconds(600);
    EtaOutcomeRecord {
        score: score(&summary, OutcomeKind::Landed, at, &[]),
        estimate: summary,
        loom: provenance(),
        outcome_source: "pulls_read".to_string(),
        outcome_resolution_sec: Some(120),
        result: None,
    }
}

fn pick_decision() -> PickDecisionRecord {
    PickDecisionRecord::build(
        PickTick {
            role: "judge".into(),
            host: "host-a".into(),
            tick_id: "t".into(),
            started_at: ts(),
            ended_at: ts(),
            outcome: "success".into(),
        },
        vec![(
            PickCandidate {
                rank: 1,
                repo: "rjwalters/loom".into(),
                number: 7,
                stage: "loom:review-requested".into(),
                sort_key: None,
            },
            PickVerdict::Undecided,
        )],
    )
}

// `eta.backtest.*` payloads carry their own `kind` field, which collides with
// the record's serde tag in flat wire JSON, so these two are struct literals.
fn backtest_fold() -> EtaBacktestFoldRecord {
    EtaBacktestFoldRecord {
        fold_id: "f".into(),
        heuristic: "h".into(),
        kind: "land".into(),
        day: "2026-07-30".into(),
        cutoff: ts(),
        compared_to: "c".into(),
        is_current: true,
        n_cases: 0,
        n_answered: 0,
        answer_rate: None,
        pinball4_loss_sec: None,
        cov_25_75: None,
        late_surprise: None,
        paired_pairs: 0,
        delta_pinball4_loss_sec: None,
        delta_answer_rate: None,
        delta_late_surprise: None,
        win: None,
        fit_id: None,
        loom: provenance(),
    }
}

fn backtest_summary() -> EtaBacktestSummaryRecord {
    EtaBacktestSummaryRecord {
        summary_id: "s".into(),
        heuristic: "h".into(),
        kind: "land".into(),
        compared_to: "c".into(),
        as_of_day: "2026-07-30".into(),
        cutoff: ts(),
        cases: 0,
        days: 0,
        wins: 0,
        ties: 0,
        win_rate: None,
        ci_low: None,
        ci_high: None,
        min_folds: 1,
        gate_ready: false,
        gate_detail: "d".into(),
        fitted_from: None,
        cases_before_fit: 0,
        fit_id: None,
        loom: provenance(),
    }
}

/// One minimal record per `otlp: Logs` kind. A new log kind fails
/// [`every_otlp_log_kind_carries_exactly_one_loom_kind_equal_to_its_tag`]
/// until it adds a sample here.
fn samples() -> Vec<TelemetryRecord> {
    let p = provenance_json();
    vec![
        sweep_started_envelope().record,
        wire(serde_json::from_str(SWEEP_IDENTITY).unwrap()),
        sweep_phase_envelope().record,
        sweep_completed_envelope(SweepResult::Success).record,
        sweep_outcome_envelope().record,
        role_tick_outcome_envelope(RoleTickResult::Success, None).record,
        wire(json!({"kind": "session.summary", "session_id": "s", "runtime": "claude",
                    "models": [], "tokens_input": 0, "tokens_output": 0,
                    "tokens_cache_read": 0, "tokens_cache_write": 0, "wall_ms": 0,
                    "turns": 0, "tool_calls": [], "tool_errors": 0})),
        wire(json!({"kind": "session.analysis", "session_id": "s"})),
        wire(json!({"kind": "daemon.event", "topic": "t", "payload": {}})),
        ci(
            "ci.run",
            json!({"run_attempt": 1, "head_sha": "abc", "event": "push",
                   "status": "completed", "started_at": AT, "duration_ms": 1}),
        ),
        ci(
            "ci.job",
            json!({"job_id": 2, "job": "test", "attempts": 1, "status": "completed",
                   "timed_out": false, "started_at": AT, "duration_ms": 1}),
        ),
        ci(
            "ci.job.log",
            json!({"job_id": 2, "job": "test", "chunk_index": 0, "chunk_count": 1,
                   "log_bytes_total": 2, "truncated": false, "text": "ok"}),
        ),
        TelemetryRecord::EtaEstimate(eta_estimate()),
        TelemetryRecord::EtaOutcome(eta_outcome()),
        wire(json!({"kind": "session.output", "schema": 1, "runtime": "claude",
                    "category": "output", "stream": "assistant", "stream_id": "s",
                    "sequence": 0, "event_id": "e", "source_at": AT, "observed_at": AT,
                    "coverage": "live", "state": "running", "redaction": "v1"})),
        wire(json!({"kind": "eta.fleet_refresh", "repo": "rjwalters/loom", "cycle_id": "c",
                    "started_at": AT, "pass": "p", "stop_reason": "done", "promoted": true,
                    "prs_read": 0, "pass_done": 0, "timelines_incomplete": 0,
                    "samples_added": 0, "forge_calls": 0, "not_modified_calls": 0,
                    "duration_ms": 0, "loom": p})),
        wire(json!({"kind": "auto_update.tick", "tick_id": "t", "started_at": AT,
                    "decision": "skip", "reason": "r", "roll_armed": false,
                    "drain": {"armed": false, "pending": false, "refusals": 0},
                    "consecutive_failures": 0, "duration_ms": 0, "loom": p})),
        wire(json!({"kind": "eta.fit", "check_id": "c", "trigger": "t", "started_at": AT,
                    "outcome": "skipped", "snapshots": 0, "duration_ms": 0, "loom": p})),
        TelemetryRecord::PickDecision(pick_decision()),
        wire(json!({"kind": "pr.resolved", "repo": "rjwalters/loom", "pr_number": 1,
                    "state": "merged", "resolved_at": AT, "observed_at": AT,
                    "resolution_sec": 0, "loom": p})),
        TelemetryRecord::EtaBacktestFold(backtest_fold()),
        TelemetryRecord::EtaBacktestSummary(backtest_summary()),
        wire(json!({"kind": "pass.summary", "pass_id": "p", "mechanism": "m",
                    "repo": "rjwalters/loom", "host": "host-a", "mode": "dry_run",
                    "outcome": "completed", "started_at": AT, "ended_at": AT,
                    "duration_ms": 0, "examined": 0, "verdicts": {}, "skipped": {},
                    "write_cap_hit": false,
                    "github": {"calls": 0, "writes": 0, "not_modified": 0},
                    "verdicts_emitted": 0, "verdicts_unchanged": 0, "loom": p})),
        wire(json!({"kind": "pass.verdict", "pass_id": "p", "mechanism": "m",
                    "repo": "rjwalters/loom", "number": 1, "artifact": "issue",
                    "verdict": "skipped", "mode": "dry_run", "applied": false, "at": AT})),
        wire(json!({"kind": "token_ranking.refresh", "round_id": "r", "started_at": AT,
                    "workspace": "loom", "outcome": "success", "source": "probe",
                    "probed_count": 0, "api_key_probe_count": 0, "accounts": [],
                    "duration_ms": 0, "loom": p})),
    ]
}

fn loom_kinds(log: &LogRecord) -> Vec<(usize, String)> {
    log.attributes
        .iter()
        .enumerate()
        .filter(|(_, kv)| kv.key == "loom.kind")
        .map(|(i, kv)| match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(any_value::Value::StringValue(s)) => (i, s.clone()),
            other => panic!("loom.kind must be a string, got {other:?}"),
        })
        .collect()
}

#[test]
fn every_otlp_log_kind_carries_exactly_one_loom_kind_equal_to_its_tag() {
    let samples = samples();
    let log_kinds: Vec<_> = TELEMETRY_KINDS
        .iter()
        .filter(|meta| meta.otlp == TelemetryKindOtlp::Logs)
        .collect();
    assert!(log_kinds.iter().any(|m| m.kind == "eta.estimate"), "registry walk is live");
    for meta in log_kinds {
        let record = samples
            .iter()
            .find(|r| r.kind() == meta.kind)
            .unwrap_or_else(|| {
                panic!(
                    "`{}` declares `otlp: Logs` in telemetry/kinds.rs but has no minimal \
                     sample in mapping/tests/loom_kind.rs — add one so the contract proves \
                     it is queryable by `loom.kind` (#10899)",
                    meta.kind
                )
            });
        let log = log_record_for(&envelope("host-a", record.clone()))
            .unwrap_or_else(|| panic!("`{}` declares `otlp: Logs` but maps to no log", meta.kind));
        let kinds = loom_kinds(&log);
        assert_eq!(
            kinds.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>(),
            vec![meta.kind],
            "`{}` must carry exactly one loom.kind equal to its registry tag (#9881, #10899)",
            meta.kind
        );
        // Stamped right after `loom.record_id`, so the 64-attribute bound in
        // `metadata::bounded` can never be what drops it.
        assert_eq!(kinds[0].0, 1, "`{}`: loom.kind must sit at index 1", meta.kind);
        assert_eq!(log.attributes[0].key, "loom.record_id");
    }
}

#[test]
fn eta_logs_keep_their_loom_eta_attributes_alongside_the_kind() {
    // The `eta.*` panels filter on `loom.eta.*` keys; the kind is additive.
    for record in [
        TelemetryRecord::EtaEstimate(eta_estimate()),
        TelemetryRecord::EtaOutcome(eta_outcome()),
    ] {
        let log = log_record_for(&envelope("host-a", record.clone())).unwrap();
        assert_eq!(loom_kinds(&log).len(), 1);
        assert!(
            log.attributes.iter().any(|kv| kv.key == "loom.eta.kind"),
            "{} lost its loom.eta.* attributes",
            record.kind()
        );
    }
}
