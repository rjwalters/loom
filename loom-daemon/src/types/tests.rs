//! Wire-payload unit tests for [`super`] — extracted from `types.rs`'s
//! inline `mod tests` (#8514) so that file, already over
//! `.loom/docs/file-size-policy.md`'s threshold, has room for the
//! live drain-roll status field. A pure move: every test below is
//! byte-identical to its inline original, only dedented.

use super::*;

// ---- Issue #3929: event `repo` payload field is additive; topic unchanged ----

#[test]
fn sweep_scoped_event_topics_are_unchanged_by_repo_field() {
    // The topic string format is frozen; adding `repo` to the payload must
    // not shift any topic segment.
    let phase = Event::SweepPhase {
        issue: 42,
        phase: "builder".to_string(),
        pr_number: None,
        repo: Some("/repos/a".to_string()),
    };
    assert_eq!(phase.topic(), "sweep.issue.42.phase");

    let blocker = Event::SweepBlocker {
        issue: 42,
        reason: "x".to_string(),
        label_added: "loom:blocked".to_string(),
        repo: Some("/repos/b".to_string()),
    };
    assert_eq!(blocker.topic(), "sweep.issue.42.blocker");

    let exited = Event::SweepExited {
        issue: 7,
        exit_code: Some(0),
        duration_sec: 5,
        no_progress: false,
        death_class: None,
        repo: None,
    };
    assert_eq!(exited.topic(), "sweep.issue.7.exited");

    let crashed = Event::SweepCrashed {
        issue: 7,
        checkpoint_phase: Some("doctor".to_string()),
        classification: None,
        death_class: None,
        repo: Some("/repos/c".to_string()),
    };
    assert_eq!(crashed.topic(), "sweep.issue.7.crashed");
}

#[test]
fn event_repo_round_trips_through_serde() {
    let ev = Event::SweepPhase {
        issue: 99,
        phase: "judge".to_string(),
        pr_number: Some(1234),
        repo: Some("/repos/alpha".to_string()),
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains("\"repo\":\"/repos/alpha\""));
    let back: Event = serde_json::from_str(&json).unwrap();
    match back {
        Event::SweepPhase { issue, repo, .. } => {
            assert_eq!(issue, 99);
            assert_eq!(repo.as_deref(), Some("/repos/alpha"));
        }
        other => panic!("expected SweepPhase, got {other:?}"),
    }
}

#[test]
fn event_repo_defaults_to_none_for_pre_3929_wire_data() {
    // A payload emitted before #3929 has no `repo` key; it must still parse,
    // with `repo` defaulting to None (backward-compatible subscribers).
    let json = r#"{"type":"SweepPhase","issue":5,"phase":"builder"}"#;
    let ev: Event = serde_json::from_str(json).unwrap();
    match ev {
        Event::SweepPhase {
            issue,
            repo,
            pr_number,
            ..
        } => {
            assert_eq!(issue, 5);
            assert!(repo.is_none());
            assert!(pr_number.is_none());
        }
        other => panic!("expected SweepPhase, got {other:?}"),
    }
}

// ---- Issue #4466: child-published topics upgrade to typed variants ----

#[test]
fn from_published_upgrades_phase_topic() {
    let ev = Event::from_published(
        "sweep.issue.123.phase".to_string(),
        serde_json::json!({"phase": "builder", "pr_number": 42, "repo": "/work/loom"}),
    );
    match ev {
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            repo,
        } => {
            assert_eq!(issue, 123);
            assert_eq!(phase, "builder");
            assert_eq!(pr_number, Some(42));
            assert_eq!(repo.as_deref(), Some("/work/loom"));
        }
        other => panic!("expected SweepPhase, got {other:?}"),
    }
}

#[test]
fn from_published_upgrades_phase_topic_minimal_payload() {
    // Only the required `phase` field — optionals default to None.
    let ev = Event::from_published(
        "sweep.issue.7.phase".to_string(),
        serde_json::json!({"phase": "curator"}),
    );
    match ev {
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            repo,
        } => {
            assert_eq!(issue, 7);
            assert_eq!(phase, "curator");
            assert!(pr_number.is_none());
            assert!(repo.is_none());
        }
        other => panic!("expected SweepPhase, got {other:?}"),
    }
}

#[test]
fn from_published_upgrades_blocker_topic() {
    let ev = Event::from_published(
        "sweep.issue.456.blocker".to_string(),
        serde_json::json!({"reason": "needs human", "label_added": "loom:blocked"}),
    );
    match ev {
        Event::SweepBlocker {
            issue,
            reason,
            label_added,
            repo,
        } => {
            assert_eq!(issue, 456);
            assert_eq!(reason, "needs human");
            assert_eq!(label_added, "loom:blocked");
            assert!(repo.is_none());
        }
        other => panic!("expected SweepBlocker, got {other:?}"),
    }
}

#[test]
fn from_published_keeps_generic_for_unknown_and_malformed() {
    // Malformed payload (missing required field), unknown sub-topic,
    // non-integer issue, and unrelated topics all stay Generic with the
    // payload passed through unchanged.
    let cases: &[(&str, serde_json::Value)] = &[
        ("sweep.issue.1.phase", serde_json::json!({"pr_number": 5})),
        ("sweep.issue.1.blocker", serde_json::json!({"reason": "x"})),
        ("sweep.issue.1.other", serde_json::json!({"phase": "builder"})),
        ("sweep.issue.abc.phase", serde_json::json!({"phase": "builder"})),
        ("sweep.issuetype.foo", serde_json::json!({"phase": "builder"})),
        ("custom.topic", serde_json::json!({"k": "v"})),
    ];
    for (topic, payload) in cases {
        let ev = Event::from_published((*topic).to_string(), payload.clone());
        match ev {
            Event::Generic {
                topic: got_topic,
                payload: got_payload,
            } => {
                assert_eq!(&got_topic, topic);
                assert_eq!(&got_payload, payload, "payload unchanged for {topic}");
            }
            other => panic!("expected Generic for {topic}, got {other:?}"),
        }
    }
}

#[test]
fn from_published_phase_rejects_wrong_typed_pr_number() {
    // A non-integer `pr_number` is a malformed payload → stays Generic.
    let ev = Event::from_published(
        "sweep.issue.1.phase".to_string(),
        serde_json::json!({"phase": "builder", "pr_number": "not-an-int"}),
    );
    assert!(matches!(ev, Event::Generic { .. }));
}

#[test]
fn set_repo_if_absent_stamps_upgraded_child_events() {
    // Issue #4466: an upgraded child-published event with no `repo` in its
    // payload is stampable via the same `set_repo_if_absent` path the
    // daemon uses for its own sweep events.
    let mut phase = Event::from_published(
        "sweep.issue.9.phase".to_string(),
        serde_json::json!({"phase": "judge"}),
    );
    phase.set_repo_if_absent("/repos/stamped");
    match &phase {
        Event::SweepPhase { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/stamped")),
        other => panic!("expected SweepPhase, got {other:?}"),
    }

    let mut blocker = Event::from_published(
        "sweep.issue.9.blocker".to_string(),
        serde_json::json!({"reason": "x", "label_added": "loom:blocked"}),
    );
    blocker.set_repo_if_absent("/repos/stamped");
    match &blocker {
        Event::SweepBlocker { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/stamped")),
        other => panic!("expected SweepBlocker, got {other:?}"),
    }
}

#[test]
fn set_repo_if_absent_stamps_only_when_empty_and_only_sweep_scoped() {
    // Stamps when absent.
    let mut ev = Event::SweepExited {
        issue: 1,
        exit_code: None,
        duration_sec: 0,
        no_progress: false,
        death_class: None,
        repo: None,
    };
    ev.set_repo_if_absent("/repos/x");
    match &ev {
        Event::SweepExited { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/x")),
        other => panic!("unexpected {other:?}"),
    }
    // Does not overwrite an already-known repo.
    ev.set_repo_if_absent("/repos/y");
    match &ev {
        Event::SweepExited { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/x")),
        other => panic!("unexpected {other:?}"),
    }
    // Leaves non-sweep-scoped variants untouched (no panic, no field).
    let mut global = Event::SweepGlobalCompleted {
        sweep_id: "s1".to_string(),
        outcome: SweepOutcome::Exited,
    };
    global.set_repo_if_absent("/repos/z"); // no-op, must not panic
    assert_eq!(global.topic(), "sweep.global.completed");
}

// ---- Issue #3929: SweepInfo `repo` field is additive/backward-compatible ----

#[test]
fn sweep_info_repo_round_trips_and_defaults_to_none() {
    let json = r#"{
        "sweep_id":"s1",
        "kind":{"type":"Issue","value":42},
        "pid":1234,
        "token_name":"agent-1.token",
        "log_path":".loom/logs/sweep-issue-42.log",
        "started_at":"2026-07-24T00:00:00Z",
        "state":{"state":"Running"}
    }"#;
    // Pre-#3929 wire data (no `repo`) parses with repo == None.
    let info: SweepInfo = serde_json::from_str(json).unwrap();
    assert!(info.repo.is_none());

    // Round-trip with repo populated.
    let mut info = info;
    info.repo = Some("/repos/beta".to_string());
    let round = serde_json::to_string(&info).unwrap();
    assert!(round.contains("\"repo\":\"/repos/beta\""));
    let back: SweepInfo = serde_json::from_str(&round).unwrap();
    assert_eq!(back.repo.as_deref(), Some("/repos/beta"));
}

// ---- Issue #4326: RepoStatus `root_missing` is additive/backward-compatible ----

fn sample_repo_status(root_missing: bool) -> RepoStatus {
    RepoStatus {
        root: PathBuf::from("/repos/gamma"),
        priority: 100,
        in_flight_count: 0,
        health_gate_halted: false,
        quarantined_issues: vec![],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: Some(true),
        health_gate_verdict_at: None,
        root_missing,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: None,
        role_runner_enabled: false,
        role_runner_roles: vec![],
        role_runner_intervals: BTreeMap::new(),
        role_runner_on_idle_roles: vec![],
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: None,
        role_runner_shard: None,
        token_pool_dir: None,
        ranking_present: false,
        ranking_age_secs: None,
        stash_total_count: 0,
        stash_quarantine_count: 0,
        stash_oldest_age_secs: None,
        stash_non_quarantine_unrecoverable_count: 0,
        stash_non_quarantine_unrecoverable_oldest_age_secs: None,
        sweep_command_missing: false,
    }
}

#[test]
fn repo_status_root_missing_round_trips_through_serde() {
    let status = sample_repo_status(true);
    let json = serde_json::to_string(&status).unwrap();
    assert!(json.contains("\"root_missing\":true"));
    let back: RepoStatus = serde_json::from_str(&json).unwrap();
    assert!(back.root_missing);
}

#[test]
fn repo_status_root_missing_defaults_to_false_for_pre_4326_wire_data() {
    // A payload emitted before #4326 has no `root_missing` key; it must
    // still parse — old daemons stay wire-compatible with a newer CLI,
    // and the field defaults to "not known to be missing" rather than
    // failing to deserialize.
    let json = r#"{
        "root":"/repos/delta",
        "priority":100,
        "in_flight_count":0,
        "health_gate_halted":false,
        "quarantined_issues":[],
        "health_gate_not_evaluated":false,
        "health_gate_enabled":true
    }"#;
    let status: RepoStatus = serde_json::from_str(json).unwrap();
    assert!(!status.root_missing);
}

// ==================================================================
// Observability export status (Issue #5083)
// ==================================================================

fn export_now() -> DateTime<Utc> {
    "2026-08-03T12:00:00Z".parse().unwrap()
}

/// A running exporter that started `uptime_secs` ago on the default
/// 30s cadence and has never been touched by a flush attempt.
fn running_exporter(uptime_secs: i64) -> ObservabilityExportStatus {
    ObservabilityExportStatus {
        state: ObservabilityExportState::Starting,
        host_id: Some("robb-studio".to_string()),
        ingest_host_id: None,
        endpoint: Some("https://dashboard.example/ingest".to_string()),
        exporter: Some("https".to_string()),
        started_at: Some(export_now() - chrono::Duration::seconds(uptime_secs)),
        last_success_at: None,
        last_failure_at: None,
        last_failure_detail: None,
        records_exported: 0,
        signal_counts: Default::default(),
        consecutive_failures: 0,
        flush_interval_secs: Some(30),
        // #9015's scope fields are derived, not configured — the tests that
        // care call `refresh_endpoint_scope()` after setting an endpoint.
        ..Default::default()
    }
}

#[test]
fn an_unstarted_exporter_classifies_as_disabled() {
    // The "observability off / keyless / under-configured" reading — a real
    // answer, materially different from a `None` field on the wire.
    let status = ObservabilityExportStatus::disabled();
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Disabled);
    assert!(!ObservabilityExportState::Disabled.is_problem());
    assert!(status.uptime_secs(export_now()).is_none());
}

#[test]
fn a_fresh_exporter_with_no_export_yet_is_starting_not_never_exported() {
    // The false-alarm guard: a daemon restarted 12 seconds ago has not yet
    // had a fair chance to flush, so it must not read as broken.
    let status = running_exporter(12);
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Starting);
    assert!(!ObservabilityExportState::Starting.is_problem());
}

#[test]
fn past_the_grace_window_with_no_export_is_never_exported() {
    // THE state this issue exists for: configured, running for hours, and
    // silently never landed a single batch. Pre-#5083 this was
    // indistinguishable from healthy on every surface.
    let status = running_exporter(4 * 3600);
    assert_eq!(status.classify(export_now()), ObservabilityExportState::NeverExported);
    assert!(ObservabilityExportState::NeverExported.is_problem());
}

#[test]
fn the_grace_window_scales_with_the_flush_interval_but_never_below_the_floor() {
    // A host configured with a one-hour flush cadence must not be called
    // out as never-exported before it has had three chances to flush.
    let mut slow = running_exporter(30 * 60);
    slow.flush_interval_secs = Some(3600);
    assert_eq!(slow.never_exported_grace_secs(), 3 * 3600);
    assert_eq!(slow.classify(export_now()), ObservabilityExportState::Starting);

    // ...and a very fast cadence still gets the floor, so a quiet host with
    // nothing to enqueue yet is not misreported either.
    let mut fast = running_exporter(60);
    fast.flush_interval_secs = Some(1);
    assert_eq!(fast.never_exported_grace_secs(), NEVER_EXPORTED_GRACE_FLOOR_SECS);
    assert_eq!(fast.classify(export_now()), ObservabilityExportState::Starting);
}

#[test]
fn an_acked_batch_with_agreeing_ids_is_healthy() {
    let mut status = running_exporter(4 * 3600);
    status.last_success_at = Some(export_now() - chrono::Duration::seconds(12));
    status.records_exported = 3481;
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Healthy);
    assert_eq!(status.last_success_age_secs(export_now()), Some(12));
    assert!(!ObservabilityExportState::Healthy.is_problem());
}

#[test]
fn a_disagreeing_ingest_id_is_a_mismatch_even_while_exporting() {
    // #4830's condition, expressed on the positive surface: data IS
    // landing, under the wrong identity.
    let mut status = running_exporter(4 * 3600);
    status.last_success_at = Some(export_now() - chrono::Duration::seconds(12));
    status.ingest_host_id = Some("robb-pro".to_string());
    assert_eq!(status.classify(export_now()), ObservabilityExportState::HostIdMismatch);
    assert!(ObservabilityExportState::HostIdMismatch.is_problem());
}

#[test]
fn an_echoed_id_that_agrees_is_not_a_mismatch() {
    // Defensive: only a *disagreement* is a mismatch. An echo equal to the
    // daemon's own id must stay healthy.
    let mut status = running_exporter(4 * 3600);
    status.last_success_at = Some(export_now());
    status.ingest_host_id = Some("robb-studio".to_string());
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Healthy);
}

#[test]
fn consecutive_failures_classify_as_failing() {
    let mut status = running_exporter(4 * 3600);
    status.last_success_at = Some(export_now() - chrono::Duration::seconds(7200));
    status.consecutive_failures = 3;
    status.last_failure_detail = Some("sink rejected batch: HTTP 401 — denied".to_string());
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Failing);
    assert!(ObservabilityExportState::Failing.is_problem());
    // "Never worked" vs "worked, then broke" stays legible.
    assert_eq!(status.last_success_age_secs(export_now()), Some(7200));
}

#[test]
fn a_mismatch_outranks_a_transient_flush_failure() {
    // Precedence documented on `classify`: the config-shaped fault that
    // cannot self-recover wins; the failure facts remain readable in the
    // `last_failure_*` fields either way.
    let mut status = running_exporter(4 * 3600);
    status.last_success_at = Some(export_now() - chrono::Duration::seconds(60));
    status.ingest_host_id = Some("robb-pro".to_string());
    status.consecutive_failures = 2;
    assert_eq!(status.classify(export_now()), ObservabilityExportState::HostIdMismatch);
    assert_eq!(status.consecutive_failures, 2);
}

#[test]
fn never_exported_beats_a_stale_failure_only_after_grace() {
    // A brand-new exporter whose very first flush errored is `Failing`
    // (there is a real, current error) — not `Starting`: the error is
    // evidence, not absence of it.
    let mut status = running_exporter(12);
    status.consecutive_failures = 1;
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Failing);
}

#[test]
fn export_state_serializes_snake_case_for_watch_loops() {
    // The AC's machine-readability requirement: a watch loop asserts on
    // these exact strings via `status --json | jq`.
    for (state, wire) in [
        (ObservabilityExportState::Disabled, "\"disabled\""),
        (ObservabilityExportState::Misconfigured, "\"misconfigured\""),
        (ObservabilityExportState::Starting, "\"starting\""),
        (ObservabilityExportState::NeverExported, "\"never_exported\""),
        (ObservabilityExportState::Healthy, "\"healthy\""),
        (ObservabilityExportState::HostIdMismatch, "\"host_id_mismatch\""),
        (ObservabilityExportState::Failing, "\"failing\""),
    ] {
        assert_eq!(serde_json::to_string(&state).unwrap(), wire);
        let back: ObservabilityExportState = serde_json::from_str(wire).unwrap();
        assert_eq!(back, state);
    }
}

#[test]
fn an_unknown_state_from_a_newer_daemon_does_not_break_the_parse() {
    let back: ObservabilityExportState = serde_json::from_str("\"quantum_entangled\"").unwrap();
    assert_eq!(back, ObservabilityExportState::Unrecognized);
}

#[test]
fn export_status_round_trips_and_tolerates_pre_5083_wire_data() {
    let mut status = running_exporter(600);
    status.last_success_at = Some(export_now());
    status.records_exported = 12;
    status.state = status.classify(export_now());
    let json = serde_json::to_string(&status).unwrap();
    let back: ObservabilityExportStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back, status);
    assert_eq!(back.state, ObservabilityExportState::Healthy);

    // A pre-#5083 daemon omits the whole field; the report must still parse
    // and the absence must never be misread as "disabled".
    let minimal: ObservabilityExportStatus = serde_json::from_str("{}").unwrap();
    assert_eq!(minimal.state, ObservabilityExportState::Disabled);
    assert!(minimal.host_id.is_none());
}

// ==================================================================
// First-hop scope of the export record (Issue #9015)
// ==================================================================

#[test]
fn a_healthy_export_record_names_its_first_hop_scope_on_the_wire() {
    // #9015: `healthy` answers "did the CONFIGURED ENDPOINT ack the batch",
    // never "is the data in the backend". A consumer must be able to read
    // that limit off the payload instead of inferring it from the docs — the
    // 30h of total SigNoz loss behind this issue was a `healthy` daemon whose
    // local edge collector accepted every POST and then dropped it.
    let mut status = running_exporter(4 * 3600);
    status.last_success_at = Some(export_now());
    status.state = status.classify(export_now());
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(value["state"], "healthy");
    assert_eq!(value["scope"], "first_hop", "the measured scope must be on the wire: {value}");
}

#[test]
fn a_loopback_endpoint_is_flagged_as_an_unverified_downstream_hop() {
    // The edge-collector deployment: the endpoint is a local otel collector
    // that forwards onward, so an ack proves strictly less than usual.
    let mut edge = running_exporter(4 * 3600);
    edge.endpoint = Some("http://127.0.0.1:14318/v1/logs".to_string());
    edge.refresh_endpoint_scope();
    assert!(edge.endpoint_is_loopback(), "127.0.0.1 is a local hop");
    assert_eq!(serde_json::to_value(&edge).unwrap()["endpoint_loopback"], true);

    // A remote endpoint is the one hop AND (as far as this daemon can see)
    // the backend, so it is not flagged.
    let mut remote = running_exporter(4 * 3600);
    remote.refresh_endpoint_scope();
    assert!(!remote.endpoint_is_loopback(), "a remote endpoint is not a local hop");
    assert_eq!(serde_json::to_value(&remote).unwrap()["endpoint_loopback"], false);
}

#[test]
fn the_scope_fields_default_for_a_pre_9015_payload() {
    // An older daemon omits both fields. `first_hop` is the truthful default
    // (that is all any daemon has ever measured), and the loopback flag
    // defaults to "not known to be a local hop" rather than inventing one.
    let older: ObservabilityExportStatus =
        serde_json::from_str(r#"{"state":"healthy","endpoint":"http://127.0.0.1:14318"}"#).unwrap();
    assert_eq!(older.scope, ObservabilityExportScope::FirstHop);
    assert!(!older.endpoint_loopback);
    // …and the derivation is still available to the consumer, which is why
    // the renderers derive it live instead of trusting the wire flag.
    assert!(older.endpoint_is_loopback());
}

#[test]
fn an_unknown_scope_from_a_newer_daemon_does_not_break_the_parse() {
    let newer: ObservabilityExportStatus =
        serde_json::from_str(r#"{"state":"healthy","scope":"end_to_end"}"#).unwrap();
    assert_eq!(newer.scope, ObservabilityExportScope::Unrecognized);
}

// ==================================================================
// Misconfigured export status (Issue #5337)
// ==================================================================

#[test]
fn misconfigured_is_distinct_from_disabled() {
    // AC #1 / #4: `enabled: true` with a bad `ingestKeyFile` must report a
    // state distinct from — and never collapsing into — `disabled`, which
    // stays reserved for `enabled: false` / no observability block.
    let disabled = ObservabilityExportStatus::disabled();
    let misconfigured = ObservabilityExportStatus::misconfigured(
        Some("https://ingest.example.com/v1/telemetry".to_string()),
        "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
            .to_string(),
    );
    assert_eq!(disabled.classify(export_now()), ObservabilityExportState::Disabled);
    assert_eq!(misconfigured.classify(export_now()), ObservabilityExportState::Misconfigured);
    assert_ne!(disabled.classify(export_now()), misconfigured.classify(export_now()));
    assert!(ObservabilityExportState::Misconfigured.is_problem());
    assert!(!ObservabilityExportState::Disabled.is_problem());
}

#[test]
fn misconfigured_state_is_sticky_across_classify_despite_no_started_at() {
    // The precedence-chain regression this issue's fix guards against:
    // `classify`'s "not running ⇒ Disabled" fallback (branch 1) triggers
    // on ANY status with no `started_at` — which is true of a
    // `misconfigured()` status too, since the exporter never started.
    // Without the new branch-0 check, this would silently read back as
    // `Disabled`, reproducing the exact bug #5337 reports.
    let misconfigured =
        ObservabilityExportStatus::misconfigured(None, "no endpoint configured".to_string());
    assert!(misconfigured.uptime_secs(export_now()).is_none(), "never started ⇒ no uptime");
    assert_eq!(misconfigured.classify(export_now()), ObservabilityExportState::Misconfigured);
}

#[test]
fn misconfigured_detail_names_the_path_and_endpoint_reflects_what_resolved() {
    // AC #2 (detail names the offending path and errno) and AC #3
    // (`endpoint` reflects what IS configured rather than `null`, when a
    // config block exists but a later field — the ingest key file — is
    // what's broken).
    let status = ObservabilityExportStatus::misconfigured(
        Some("https://ingest.example.com/v1/telemetry".to_string()),
        "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
            .to_string(),
    );
    assert_eq!(status.endpoint.as_deref(), Some("https://ingest.example.com/v1/telemetry"));
    let detail = status.last_failure_detail.as_deref().unwrap();
    assert!(
        detail.contains("/etc/loom/ingest.key"),
        "detail must name the offending path: {detail}"
    );
    assert!(
        detail.contains("os error 2"),
        "detail must carry the underlying errno: {detail}"
    );
}

#[test]
fn misconfigured_endpoint_is_none_when_the_endpoint_itself_is_what_is_missing() {
    // When `observability.endpoint` is the missing piece, there is nothing
    // to report — `None`, not an invented value.
    let status = ObservabilityExportStatus::misconfigured(
        None,
        "observability.endpoint not configured".to_string(),
    );
    assert!(status.endpoint.is_none());
    assert_eq!(status.classify(export_now()), ObservabilityExportState::Misconfigured);
}
