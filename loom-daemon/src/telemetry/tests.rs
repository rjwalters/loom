//! Unit tests for [`super`] — extracted from the parent module's inline
//! `#[cfg(test)] mod tests` (Issue #8056) so the parent stays inside the
//! file-size ratchet (`scripts/check-file-size-budget.sh`). Content is
//! unchanged apart from dedenting and the new-module additions.

use super::*;

mod admission_brake;
mod daemon_event;
mod role_tick;
mod session_analysis;
mod session_summary;
mod token_snapshot;
use role_tick::role_tick_outcome;
use token_snapshot::tokens_snapshot;

// ------------------------------------------------------------------
// Test fixtures — one freshly-constructed record per kind.
// ------------------------------------------------------------------

pub(super) fn ts() -> DateTime<Utc> {
    // A fixed instant keeps round-trip equality deterministic.
    DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn sweep_started() -> TelemetryRecord {
    TelemetryRecord::SweepStarted(SweepStartedRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 4703,
        sweep_id: "sweep-issue-4703-0".to_string(),
        started_at: ts(),
        model: Some("opus".to_string()),
        effort: Some("high".to_string()),
        runtime: Some("claude".to_string()),
    })
}

fn sweep_phase() -> TelemetryRecord {
    TelemetryRecord::SweepPhase(SweepPhaseRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        issue: 4703,
        sweep_id: "sweep-issue-4703-0".to_string(),
        phase: "builder".to_string(),
        entered_at: ts(),
    })
}

fn sweep_completed() -> TelemetryRecord {
    TelemetryRecord::SweepCompleted(SweepCompletedRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 4703,
        sweep_id: "sweep-issue-4703-0".to_string(),
        completed_at: ts(),
        result: SweepResult::Success,
        tokens_by_model: Some(vec![ModelUsageTotals {
            model: "claude-sonnet-5".to_string(),
            speed: "standard".to_string(),
            service_tier: "standard".to_string(),
            input: 48_000,
            cache_read: 15_000,
            cache_write_5m: 500,
            cache_write_1h: 1_500,
            output: 6_120,
        }]),
    })
}

fn sweep_outcome() -> TelemetryRecord {
    let mut config = std::collections::BTreeMap::new();
    config.insert("runtime".to_string(), "claude".to_string());
    TelemetryRecord::SweepOutcome(SweepOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 4703,
        sweep_id: "sweep-issue-4703-0".to_string(),
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
        pr_number: Some(4710),
        tokens_in: Some(48_213),
        tokens_out: Some(6_120),
        lines_added: Some(214),
        lines_deleted: Some(37),
        tokens_by_model: Some(vec![ModelUsageTotals {
            model: "claude-sonnet-5".to_string(),
            speed: "standard".to_string(),
            service_tier: "standard".to_string(),
            input: 48_000,
            cache_read: 15_000,
            cache_write_5m: 500,
            cache_write_1h: 1_500,
            output: 6_120,
        }]),
        failure_class: None,
        models_used: Some(vec!["claude-sonnet-5".to_string()]),
        doctor_cycles: Some(0),
        judge_verdicts: Some(vec![JudgeVerdict {
            attempt: 1,
            verdict: "pass".to_string(),
        }]),
        runtime: Some("opencode".to_string()),
        provider: Some("friendli".to_string()),
        profile: Some("zai-flash".to_string()),
        complexity: Some("routine".to_string()),
    })
}

fn host_health() -> TelemetryRecord {
    TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.16.0".to_string(),
        build_commit: "8c16fb5b".to_string(),
        built_at: Some(ts()),
        uptime_sec: 86_400,
        logical_cpus: 28,
        cpu_idle_fraction: Some(0.83),
        load_per_core: Some(0.51),
        worktree_root_free_gb: Some(200),
        worktree_root_total_gb: Some(1000),
        active_sweep_ids: vec!["sweep-issue-4703-0".to_string()],
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: vec![
            ManagedRepoEntry {
                slug: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
            },
            ManagedRepoEntry {
                slug: "2AMLogic/gf180-pll".to_string(),
                visibility: RepoVisibility::Private,
            },
        ],
        roles: RoleTickHealth {
            total: 12,
            ok: 10,
            persistent: vec![RoleTickFailureEntry {
                root: PathBuf::from("/repos/loom"),
                role: "judge".to_string(),
                failures: 2,
                last_at: ts(),
                detail: Some("no-token-pool".to_string()),
            }],
        },
        protection: Some(HostProtectionSummary {
            state: "protected".to_string(),
            watchdog_provisioned: Some(true),
        }),
        admission_brake: None,
    })
}

fn every_record() -> Vec<TelemetryRecord> {
    // Per-kind-versioned records (trace.span → 3, sweep.identity → 4,
    // session.summary → 5, session.analysis → 6, daemon.event → 7) are
    // deliberately absent here — they get their own round-trip/version
    // coverage in their modules' tests (`trace/tests.rs`,
    // `tests/role_tick.rs`, `tests/session_summary.rs`,
    // `tests/session_analysis.rs`, `tests/daemon_event.rs`) and would break
    // `fresh_envelope_carries_current_schema_version`, which asserts the
    // default stamp for every kind in this list.
    vec![
        sweep_started(),
        sweep_phase(),
        sweep_completed(),
        sweep_outcome(),
        tokens_snapshot(),
        host_health(),
        role_tick_outcome(),
    ]
}

// ------------------------------------------------------------------
// Serde round-trip — every record kind, wrapped in the envelope.
// ------------------------------------------------------------------

#[test]
fn envelope_round_trips_for_every_record_kind() {
    for record in every_record() {
        let envelope = TelemetryEnvelope::new("host-abc", record.clone());
        let json = serde_json::to_string(&envelope).unwrap();
        let decoded: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, decoded, "round-trip mismatch for {record:?}");
    }
}

#[test]
fn bare_record_round_trips_for_every_record_kind() {
    // The record enum is consumed directly by #4704/#4705 too, not only
    // inside the envelope, so it must round-trip on its own.
    for record in every_record() {
        let json = serde_json::to_string(&record).unwrap();
        let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, decoded);
    }
}

// ------------------------------------------------------------------
// sweep.outcome work-output fields (Issue #5357): tokens_in/tokens_out
// and lines_added/lines_deleted — every field optional, omitted (never
// a fabricated zero) when unavailable, and a pre-#5357 record must
// still decode.
// ------------------------------------------------------------------

#[test]
fn sweep_outcome_omits_work_output_fields_when_unavailable() {
    // A no-PR, pruned-logs sweep: none of the four new fields were ever
    // sampled, so all four must be entirely absent from the wire
    // payload — never `null` or a fabricated `0`.
    let record = SweepOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        issue: 5357,
        sweep_id: "sweep-issue-5357-0".to_string(),
        model: None,
        effort: None,
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 40,
        result: SweepResult::Failure,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
        complexity: None,
    };
    let value = serde_json::to_value(&record).unwrap();
    for field in [
        "tokens_in",
        "tokens_out",
        "lines_added",
        "lines_deleted",
        "pr_number",
        "tokens_by_model",
        "runtime",
        "provider",
        "profile",
        "complexity",
    ] {
        assert!(
            value.get(field).is_none(),
            "unavailable field {field:?} must be omitted, not present: {value}"
        );
    }
    // Still decodes back to the same all-`None` record.
    let decoded: SweepOutcomeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_outcome_carries_work_output_fields_when_sampled() {
    let record = sweep_outcome();
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value.get("tokens_in").and_then(serde_json::Value::as_u64), Some(48_213));
    assert_eq!(value.get("tokens_out").and_then(serde_json::Value::as_u64), Some(6_120));
    assert_eq!(value.get("lines_added").and_then(serde_json::Value::as_i64), Some(214));
    assert_eq!(
        value
            .get("lines_deleted")
            .and_then(serde_json::Value::as_i64),
        Some(37)
    );
    let by_model = value
        .get("tokens_by_model")
        .and_then(serde_json::Value::as_array)
        .expect("tokens_by_model must be present when sampled");
    assert_eq!(by_model.len(), 1);
    assert_eq!(by_model[0]["model"], "claude-sonnet-5");
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_outcome_from_a_pre_5357_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // the work-output fields (Issue #5357) must decode, not poison the
    // batch — the exact shape #4704 shipped with.
    let json = r#"{
        "kind": "sweep.outcome",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 4703,
        "sweep_id": "sweep-issue-4703-0",
        "model": "opus",
        "total_duration_sec": 512,
        "result": "success",
        "pr_number": 4710
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.pr_number, Some(4710));
            assert_eq!(r.tokens_in, None);
            assert_eq!(r.tokens_out, None);
            assert_eq!(r.lines_added, None);
            assert_eq!(r.lines_deleted, None);
            assert_eq!(r.tokens_by_model, None);
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// Outcome-journal completeness (Issue #8056): failure_class,
// models_used, doctor_cycles — all additive and all optional, with the
// same "unknown != zero" contract as the #5357/#6384 fields above.
// ------------------------------------------------------------------

#[test]
fn sweep_outcome_round_trips_the_completeness_fields() {
    let record = SweepOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 8056,
        sweep_id: "sweep-issue-8056-0".to_string(),
        model: Some("sonnet".to_string()),
        effort: Some("high".to_string()),
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 900,
        result: SweepResult::Success,
        pr_number: Some(8100),
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        failure_class: Some("account-exhausted:model-credits-exhausted".to_string()),
        models_used: Some(vec!["claude-opus-5".to_string(), "claude-sonnet-5".to_string()]),
        doctor_cycles: Some(2),
        judge_verdicts: Some(vec![
            JudgeVerdict {
                attempt: 1,
                verdict: "fail".to_string(),
            },
            JudgeVerdict {
                attempt: 2,
                verdict: "pass".to_string(),
            },
        ]),
        runtime: None,
        provider: None,
        profile: None,
        complexity: Some("complex".to_string()),
    };
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["complexity"], "complex");
    assert_eq!(value["failure_class"], "account-exhausted:model-credits-exhausted");
    assert_eq!(value["models_used"][1], "claude-sonnet-5");
    assert_eq!(value["doctor_cycles"], 2);
    // Issue #8222: the first-pass approval rate a consumer computes is
    // `judge_verdicts[0].verdict == "pass"` — this record's first pass was a
    // rejection, so it counts against that rate.
    assert_eq!(value["judge_verdicts"][0]["attempt"], 1);
    assert_eq!(value["judge_verdicts"][0]["verdict"], "fail");
    assert_eq!(value["judge_verdicts"][1]["verdict"], "pass");
    let decoded: SweepOutcomeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_outcome_distinguishes_an_omitted_doctor_cycles_from_zero() {
    // The whole point of `Option<u32>`: `Some(0)` is "observed, no Doctor
    // phase" and must be ON the wire, while `None` is "not observed" and
    // must be absent. A consumer that cannot tell them apart
    // (#8057's doctor-phase rate) would silently count the second as the
    // first.
    let base = SweepOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        issue: 8056,
        sweep_id: "sweep-issue-8056-1".to_string(),
        model: None,
        effort: None,
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 10,
        result: SweepResult::Failure,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
        complexity: None,
    };

    let unobserved = serde_json::to_value(&base).unwrap();
    for field in [
        "failure_class",
        "models_used",
        "doctor_cycles",
        "judge_verdicts",
        "complexity",
    ] {
        assert!(
            unobserved.get(field).is_none(),
            "unobserved {field:?} must be omitted, not null: {unobserved}"
        );
    }

    let observed_zero = serde_json::to_value(SweepOutcomeRecord {
        doctor_cycles: Some(0),
        judge_verdicts: Some(Vec::new()),
        ..base
    })
    .unwrap();
    assert_eq!(
        observed_zero
            .get("doctor_cycles")
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "an observed zero must be present on the wire: {observed_zero}"
    );
    // Issue #8222's twin of the same contract: a timeline that WAS read and
    // carried no verdict is an empty array on the wire, not an absent key.
    assert_eq!(
        observed_zero
            .get("judge_verdicts")
            .and_then(serde_json::Value::as_array),
        Some(&vec![]),
        "an observed-but-unjudged PR must be an empty list on the wire: {observed_zero}"
    );
}

#[test]
fn sweep_outcome_from_a_pre_8056_daemon_still_decodes() {
    // Backward compatibility (no `schema_version` bump accompanies these
    // fields): a record emitted by a daemon that predates them must
    // decode, with each new field reading as "not observed".
    let json = r#"{
        "kind": "sweep.outcome",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 6384,
        "sweep_id": "sweep-issue-6384-0",
        "model": "sonnet",
        "config": { "runtime": "claude", "token_account": "unknown" },
        "total_duration_sec": 512,
        "result": "failure",
        "tokens_in": 100,
        "tokens_out": 20
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.tokens_in, Some(100));
            assert_eq!(r.failure_class, None);
            assert_eq!(r.models_used, None);
            assert_eq!(r.doctor_cycles, None);
            assert_eq!(r.judge_verdicts, None);
            assert_eq!(r.complexity, None);
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// sweep.outcome Curator complexity tier (Issue #8542): additive, omitted
// (never a fabricated "routine") when the issue carried no recognized
// marker or the fetch was not attempted/failed, and a pre-#8542 record
// must still decode.
// ------------------------------------------------------------------

#[test]
fn sweep_outcome_carries_complexity_when_the_marker_was_read() {
    let record = sweep_outcome();
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["complexity"], "routine");
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_outcome_omits_complexity_when_no_marker_was_read() {
    let base = SweepOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        issue: 8542,
        sweep_id: "sweep-issue-8542-0".to_string(),
        model: None,
        effort: None,
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 10,
        result: SweepResult::Failure,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        complexity: None,
    };
    let value = serde_json::to_value(&base).unwrap();
    assert!(
        value.get("complexity").is_none(),
        "an unmarked issue must omit complexity, never a fabricated \"routine\": {value}"
    );
    let decoded: SweepOutcomeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, base);
}

#[test]
fn sweep_outcome_from_a_pre_8542_daemon_still_decodes() {
    // Backward compatibility (no `schema_version` bump accompanies this
    // field): a record emitted by a daemon that predates it must decode,
    // with `complexity` reading as "not observed".
    let json = r#"{
        "kind": "sweep.outcome",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 8542,
        "sweep_id": "sweep-issue-8542-0",
        "model": "sonnet",
        "total_duration_sec": 512,
        "result": "success",
        "pr_number": 8600
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.pr_number, Some(8600));
            assert_eq!(r.complexity, None);
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// sweep.completed per-model token usage (Issue #6384): additive,
// omitted (never a fabricated empty vec) when no attributable
// transcript was found, and a pre-#6384 record must still decode.
// ------------------------------------------------------------------

#[test]
fn sweep_completed_carries_tokens_by_model_when_present() {
    let record = sweep_completed();
    let value = serde_json::to_value(&record).unwrap();
    let by_model = value
        .get("tokens_by_model")
        .and_then(serde_json::Value::as_array)
        .expect("tokens_by_model must be present when sampled");
    assert_eq!(by_model.len(), 1);
    assert_eq!(by_model[0]["model"], "claude-sonnet-5");
    assert_eq!(by_model[0]["output"], 6_120);
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_completed_omits_tokens_by_model_when_absent() {
    let record = SweepCompletedRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        issue: 6384,
        sweep_id: "sweep-issue-6384-0".to_string(),
        completed_at: ts(),
        result: SweepResult::Failure,
        tokens_by_model: None,
    };
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("tokens_by_model").is_none(),
        "absent tokens_by_model must be omitted, not null: {value}"
    );
    let decoded: SweepCompletedRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_completed_from_a_pre_6384_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // the token-usage field (Issue #6384) must decode, not poison the
    // batch — the original six-field shape.
    let json = r#"{
        "kind": "sweep.completed",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 4703,
        "sweep_id": "sweep-issue-4703-0",
        "completed_at": "2026-07-30T12:00:00Z",
        "result": "success"
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepCompleted(r) => {
            assert_eq!(r.tokens_by_model, None);
        }
        other => panic!("expected SweepCompleted, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// schema_version presence.
// ------------------------------------------------------------------

#[test]
fn fresh_envelope_carries_current_schema_version() {
    for record in every_record() {
        let envelope = TelemetryEnvelope::new("host-abc", record);
        assert_eq!(envelope.schema_version, CURRENT_SCHEMA_VERSION);
        let value: serde_json::Value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(CURRENT_SCHEMA_VERSION)),
            "serialized envelope must carry schema_version"
        );
    }
}

// ------------------------------------------------------------------
// The `kind` discriminant is on the wire for pattern-matching.
// ------------------------------------------------------------------

#[test]
fn record_serializes_with_kind_tag() {
    let value = serde_json::to_value(sweep_outcome()).unwrap();
    assert_eq!(value.get("kind").and_then(serde_json::Value::as_str), Some("sweep.outcome"));
    // The tag flattens into the same object as the payload fields.
    assert_eq!(value.get("issue").and_then(serde_json::Value::as_u64), Some(4703));
}

// ------------------------------------------------------------------
// RepoVisibility — private-safe decoding (the load-bearing property).
// ------------------------------------------------------------------

#[test]
fn visibility_default_is_private() {
    assert_eq!(RepoVisibility::default(), RepoVisibility::Private);
}

#[test]
fn visibility_public_string_decodes_public() {
    assert_eq!(
        serde_json::from_str::<RepoVisibility>("\"public\"").unwrap(),
        RepoVisibility::Public
    );
    // Case-insensitive.
    assert_eq!(
        serde_json::from_str::<RepoVisibility>("\"PUBLIC\"").unwrap(),
        RepoVisibility::Public
    );
}

#[test]
fn visibility_unknown_or_malformed_decodes_private_never_public() {
    // An explicit "private".
    assert_eq!(
        serde_json::from_str::<RepoVisibility>("\"private\"").unwrap(),
        RepoVisibility::Private
    );
    // Every one of these MUST decode (not error) and MUST be Private.
    for raw in [
        "\"internal\"",       // unknown label
        "\"Public \"",        // trailing space — not exactly "public"
        "\"\"",               // empty string
        "null",               // null
        "true",               // wrong-typed scalar
        "0",                  // number
        "1",                  // number (must never map to public)
        "[\"public\"]",       // array
        "{\"v\":\"public\"}", // object
    ] {
        let decoded: RepoVisibility = serde_json::from_str(raw).unwrap_or_else(|e| {
            panic!("visibility {raw:?} should decode (defaulting to Private), got error: {e}")
        });
        assert_eq!(
            decoded,
            RepoVisibility::Private,
            "visibility {raw:?} must decode to Private, never Public"
        );
    }
}

#[test]
fn record_with_missing_visibility_defaults_to_private() {
    // A record object with NO `visibility` key at all (an older-schema or
    // partial record) must decode with visibility = Private.
    let json = r#"{
        "kind": "sweep.started",
        "repo": "rjwalters/loom",
        "issue": 4703,
        "sweep_id": "sweep-issue-4703-0",
        "started_at": "2026-07-30T12:00:00Z"
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepStarted(r) => {
            assert_eq!(r.visibility, RepoVisibility::Private);
        }
        other => panic!("expected SweepStarted, got {other:?}"),
    }
}

#[test]
fn record_with_unknown_visibility_defaults_to_private() {
    // A record whose `visibility` is a value this daemon doesn't recognize
    // must still decode, as Private (never leaking into the public view).
    let json = r#"{
        "kind": "sweep.completed",
        "repo": "rjwalters/loom",
        "visibility": "internal-only",
        "issue": 4703,
        "sweep_id": "sweep-issue-4703-0",
        "completed_at": "2026-07-30T12:00:00Z",
        "result": "success"
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepCompleted(r) => {
            assert_eq!(r.visibility, RepoVisibility::Private);
        }
        other => panic!("expected SweepCompleted, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// host.health build identity (#4956).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_build_identity_alongside_version() {
    let value = serde_json::to_value(host_health()).unwrap();
    assert_eq!(
        value
            .get("daemon_version")
            .and_then(serde_json::Value::as_str),
        Some("0.16.0")
    );
    // The whole point of #4956: two builds sharing `daemon_version` are
    // told apart by the commit, so it must be on the wire.
    assert_eq!(
        value
            .get("build_commit")
            .and_then(serde_json::Value::as_str),
        Some("8c16fb5b")
    );
    assert_eq!(
        value.get("built_at").and_then(serde_json::Value::as_str),
        Some("2026-07-30T12:00:00Z")
    );
}

#[test]
fn host_health_round_trips_build_identity() {
    let json = serde_json::to_string(&host_health()).unwrap();
    let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.build_commit, "8c16fb5b");
            assert_eq!(r.built_at, Some(ts()));
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

#[test]
fn host_health_omits_built_at_when_unknown() {
    // `build.rs` stamps the literal "unknown" when the build host had no
    // usable `date`; that must serialize as an ABSENT field, never as a
    // fabricated instant (the struct's "unknown != zero" contract).
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("built_at").is_none(),
        "an unknown build time must be absent, not a fabricated instant"
    );
    // The commit sentinel, unlike the instant, IS sent — "unknown" is a
    // meaningful answer for a tarball build, not a missing measurement.
    assert_eq!(
        value
            .get("build_commit")
            .and_then(serde_json::Value::as_str),
        Some("unknown")
    );
    // Still round-trips.
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_from_a_pre_4956_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // the build-identity fields must decode, not poison the batch.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.daemon_version, "0.16.0");
            assert_eq!(r.build_commit, "");
            assert_eq!(r.built_at, None);
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// host.health managed_repos roster (#4976).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_managed_repos_with_slug_and_visibility() {
    let value = serde_json::to_value(host_health()).unwrap();
    let repos = value
        .get("managed_repos")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].get("slug").and_then(serde_json::Value::as_str), Some("rjwalters/loom"));
    assert_eq!(
        repos[0]
            .get("visibility")
            .and_then(serde_json::Value::as_str),
        Some("public")
    );
    assert_eq!(
        repos[1].get("slug").and_then(serde_json::Value::as_str),
        Some("2AMLogic/gf180-pll")
    );
    assert_eq!(
        repos[1]
            .get("visibility")
            .and_then(serde_json::Value::as_str),
        Some("private")
    );
}

#[test]
fn host_health_omits_managed_repos_when_empty() {
    // `skip_serializing_if = "Vec::is_empty"` — a host with no registered
    // workspaces sends no key at all, mirroring `active_sweep_ids`.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("managed_repos").is_none(),
        "an empty roster must be absent from the wire, not an empty array"
    );
}

#[test]
fn host_health_from_a_pre_4976_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // the managed-repo roster must decode with an empty roster, not fail
    // the whole envelope.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert!(r.managed_repos.is_empty());
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

#[test]
fn managed_repo_entry_with_missing_visibility_defaults_to_private() {
    // A repo entry with no `visibility` tag at all must decode Private —
    // the same private-safe-default every other visibility field holds.
    let json = r#"{"slug": "owner/repo"}"#;
    let decoded: ManagedRepoEntry = serde_json::from_str(json).unwrap();
    assert_eq!(decoded.visibility, RepoVisibility::Private);
    assert_eq!(decoded.slug, "owner/repo");
}

// ------------------------------------------------------------------
// host.health role-tick health (#5022).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_role_tick_health_persistent_failures() {
    let value = serde_json::to_value(host_health()).unwrap();
    let roles = value.get("roles").unwrap();
    assert_eq!(roles.get("total").and_then(serde_json::Value::as_u64), Some(12));
    assert_eq!(roles.get("ok").and_then(serde_json::Value::as_u64), Some(10));
    let persistent = roles
        .get("persistent")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(persistent.len(), 1);
    assert_eq!(
        persistent[0]
            .get("role")
            .and_then(serde_json::Value::as_str),
        Some("judge")
    );
    assert_eq!(
        persistent[0]
            .get("detail")
            .and_then(serde_json::Value::as_str),
        Some("no-token-pool")
    );
}

#[test]
fn host_health_omits_persistent_when_empty_but_still_carries_roles() {
    // A `total: 0` (or all-ok) summary must still be sent — "no role
    // ticks sampled" / "every tick ok" is meaningful information, not
    // nothing to report — but its empty `persistent` list is omitted from
    // the wire, mirroring `managed_repos`/`active_sweep_ids`.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth {
            total: 5,
            ok: 5,
            persistent: Vec::new(),
        },
        protection: None,
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    let roles = value.get("roles").unwrap();
    assert_eq!(roles.get("total").and_then(serde_json::Value::as_u64), Some(5));
    assert!(roles.get("persistent").is_none());
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_from_a_pre_5022_daemon_decodes_with_a_zero_value_roles_summary() {
    // Backward compatibility: a record emitted by a daemon that predates
    // role-tick health must decode with the zero-value ("nothing sampled")
    // summary, never fail the whole envelope.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.roles, RoleTickHealth::default());
            assert_eq!(r.roles.total, 0);
            assert!(r.roles.persistent.is_empty());
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// host.health worktree_root_total_gb (#5356).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_worktree_root_total_gb_when_measurable() {
    let value = serde_json::to_value(host_health()).unwrap();
    assert_eq!(
        value
            .get("worktree_root_free_gb")
            .and_then(serde_json::Value::as_u64),
        Some(200)
    );
    assert_eq!(
        value
            .get("worktree_root_total_gb")
            .and_then(serde_json::Value::as_u64),
        Some(1000)
    );
}

// ------------------------------------------------------------------
// host.health watchdog/crash-protection state (#5352).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_protection_state_and_watchdog_provisioned() {
    let value = serde_json::to_value(host_health()).unwrap();
    let protection = value.get("protection").unwrap();
    assert_eq!(protection.get("state").and_then(serde_json::Value::as_str), Some("protected"));
    assert_eq!(
        protection
            .get("watchdog_provisioned")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn host_health_omits_worktree_root_total_gb_when_unmeasurable() {
    // "unknown != zero" (#4164/#5356): an unmeasurable total must be
    // ABSENT from the wire, never a fabricated 0.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("worktree_root_total_gb").is_none(),
        "an unmeasurable total must be absent, not a fabricated 0"
    );
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_omits_protection_when_absent() {
    // `None` (a pre-#5352 daemon, or a probe that could not construct a
    // report at all) must be absent from the wire, not a fabricated
    // `"unknown"` verdict.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("protection").is_none(),
        "an absent protection report must omit the key, not fabricate a verdict"
    );
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_free_without_total_serializes_with_no_fabricated_denominator() {
    // Acceptance criterion: a record with free-but-no-total (e.g. a total
    // probe that failed independently, or simply a daemon that has not
    // measured total yet) must still send its free reading — and must
    // NEVER synthesize a total to go with it.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "8c16fb5b".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: Some(200),
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(
        value
            .get("worktree_root_free_gb")
            .and_then(serde_json::Value::as_u64),
        Some(200),
        "the free reading must still be sent on its own"
    );
    assert!(
        value.get("worktree_root_total_gb").is_none(),
        "no denominator must be fabricated for a total the probe never measured"
    );
}

#[test]
fn host_health_omits_watchdog_provisioned_when_the_probe_could_not_answer() {
    // `ProtectionState::Unknown`: the probe ran but the watchdog-
    // provisioning check itself could not answer (no launchctl/systemctl).
    // `watchdog_provisioned` must be absent, not a fabricated `false`.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: Some(HostProtectionSummary {
            state: "unknown".to_string(),
            watchdog_provisioned: None,
        }),
        admission_brake: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    let protection = value.get("protection").unwrap();
    assert_eq!(protection.get("state").and_then(serde_json::Value::as_str), Some("unknown"));
    assert!(protection.get("watchdog_provisioned").is_none());
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_from_a_pre_5356_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // worktree_root_total_gb (this includes a record that DOES carry
    // worktree_root_free_gb, since that field existed first) must decode
    // with an absent total, not fail the whole envelope.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28,
        "worktree_root_free_gb": 200
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.worktree_root_free_gb, Some(200));
            assert_eq!(
                r.worktree_root_total_gb, None,
                "a pre-#5356 record has no total key at all, which must decode as None, not 0"
            );
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

#[test]
fn host_health_from_a_pre_5352_daemon_decodes_with_protection_absent() {
    // Backward compatibility: a record emitted by a daemon that predates
    // watchdog/crash-protection telemetry must decode with `protection:
    // None`, never fail the whole envelope — and never a false
    // "unprotected" negative synthesized from its absence.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert!(r.protection.is_none());
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}
