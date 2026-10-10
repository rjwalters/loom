//! `loom.kind` -- the kind discriminator on every OTLP log record (#9881,
//! #10899).
//!
//! The dashboard and its readers filter logs on `loom.kind`, never on the
//! log `name` column (loom-ui#747). Until #10899 each `log_record_for` arm
//! had to push the attribute itself, and the `eta.*`, `ci.*`, `sweep.phase`,
//! `daemon.event`, `pr.resolved`, `session.output`, `auto_update.tick` and
//! `token_ranking.refresh` arms never did, so `loom.kind LIKE 'eta%'` read as
//! "no ETAs ever". The stamp is now applied once, after the match. The
//! contract below walks the kind registry: every `otlp: Logs` row must have a
//! minimal sample here and must map to exactly one `loom.kind` equal to its
//! wire tag, so a kind added later cannot ship unqueryable.

use super::*;
use crate::telemetry::kinds::pick_decision::{
    PickCandidate, PickDecisionRecord, PickTick, PickVerdict,
};
use crate::telemetry::provenance::Provenance;
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
        wire(json!({"kind": "eta.stage_outcome", "repo": "rjwalters/loom", "issue": 1,
                    "stage": "review_wait", "left_at": AT, "exit": "pass",
                    "event": "label.transition", "observed_at": AT, "open_estimates": 0,
                    "loom": p})),
        wire(json!({"kind": "session.output", "schema": 1, "runtime": "claude",
                    "category": "output", "stream": "assistant", "stream_id": "s",
                    "sequence": 0, "event_id": "e", "source_at": AT, "observed_at": AT,
                    "coverage": "live", "state": "running", "redaction": "v1"})),
        wire(json!({"kind": "auto_update.tick", "tick_id": "t", "started_at": AT,
                    "decision": "skip", "reason": "r", "roll_armed": false,
                    "drain": {"armed": false, "pending": false, "refusals": 0},
                    "consecutive_failures": 0, "duration_ms": 0, "loom": p})),
        wire(json!({"kind": "fleet.state", "schema": "fleet-state/v1", "as_of": AT,
                    "anchor": true,
                    "anchor_as_of": AT, "repos": []})),
        wire(json!({"kind": "host.export", "captured_at": AT, "host": "host-a",
                    "exporters": []})),
        TelemetryRecord::PickDecision(pick_decision()),
        wire(json!({"kind": "pr.resolved", "repo": "rjwalters/loom", "pr_number": 1,
                    "state": "merged", "resolved_at": AT, "observed_at": AT,
                    "resolution_sec": 0, "loom": p})),
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
