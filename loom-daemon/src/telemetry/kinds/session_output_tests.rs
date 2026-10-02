//! `session.output` payload tests (#9764) — the wire-safety, identity and
//! loss-reporting contract, independent of how any adapter produces records.

#![allow(clippy::unwrap_used)]

use chrono::{TimeZone, Utc};

use super::*;

fn identity() -> RunIdentity {
    RunIdentity {
        repo: Some("rjwalters/loom".to_string()),
        visibility: crate::telemetry::RepoVisibility::Private,
        session_kind: Some(crate::telemetry::SessionKind::Sweep),
        issue: Some(9764),
        sweep_id: Some("sweep-issue-9764-1".to_string()),
        session_id: Some("0f8b".to_string()),
        attempt: Some(1),
        runtime: "claude".to_string(),
        role: Some("builder".to_string()),
    }
}

fn at(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, second).unwrap()
}

#[test]
fn output_text_is_always_scrubbed_and_the_policy_is_recorded() {
    let record = SessionOutputRecord::new_output(
        identity(),
        "0f8b",
        3,
        at(0),
        at(1),
        "pushed with ghp_abcdefghijklmnopqrstuvwxyz0123",
    );
    let text = record.text.as_deref().unwrap();
    assert!(!text.contains("ghp_"), "{text}");
    assert!(text.contains("[REDACTED:github-token]"));
    assert_eq!(record.redaction, redact::POLICY);
    assert_eq!(record.schema, SESSION_OUTPUT_SCHEMA);
}

#[test]
fn a_tool_record_carries_a_name_and_an_outcome_and_nothing_else() {
    let start = SessionOutputRecord::tool(identity(), "0f8b", 4, at(0), at(0), "Bash", None);
    assert_eq!(start.category, OutputCategory::ToolStart);
    assert_eq!(start.tool.as_deref(), Some("Bash"));
    assert_eq!(start.tool_ok, None);
    assert_eq!(start.text, None, "a tool record never carries content");

    let finish =
        SessionOutputRecord::tool(identity(), "0f8b", 5, at(0), at(0), "Bash", Some(false));
    assert_eq!(finish.category, OutputCategory::ToolFinish);
    assert_eq!(finish.tool_ok, Some(false));
    assert_eq!(finish.text, None);
}

#[test]
fn event_ids_are_stable_across_replay_and_unique_within_a_stream() {
    let first = SessionOutputRecord::new_output(identity(), "0f8b", 7, at(0), at(0), "a");
    // Same source event, read again after a producer restart: different
    // observed_at, identical id — which is what makes dedup possible.
    let replay = SessionOutputRecord::new_output(identity(), "0f8b", 7, at(0), at(9), "a");
    assert_eq!(first.event_id, replay.event_id);
    assert_ne!(first.observed_at, replay.observed_at);

    let next = SessionOutputRecord::new_output(identity(), "0f8b", 8, at(0), at(0), "b");
    assert_ne!(first.event_id, next.event_id);
    // Same source timestamp, still distinguishable.
    assert_eq!(first.source_at, next.source_at);
}

#[test]
fn two_attempts_of_one_issue_do_not_share_a_stream_or_an_event_id() {
    let mut second_attempt = identity();
    second_attempt.attempt = Some(2);
    second_attempt.session_id = Some("aa11".to_string());
    second_attempt.sweep_id = Some("sweep-issue-9764-2".to_string());

    let a = SessionOutputRecord::new_output(identity(), "0f8b", 0, at(0), at(0), "x");
    let b = SessionOutputRecord::new_output(second_attempt, "aa11", 0, at(0), at(0), "x");
    assert_ne!(a.event_id, b.event_id);
    assert_ne!(a.identity.attempt, b.identity.attempt);
    assert_ne!(a.identity.sweep_id, b.identity.sweep_id);
}

#[test]
fn an_unscoped_session_stays_unscoped() {
    let identity = RunIdentity {
        runtime: "claude".to_string(),
        ..RunIdentity::default()
    };
    let record = SessionOutputRecord::new_output(identity, "solo", 0, at(0), at(0), "hello");
    assert_eq!(record.identity.repo, None);
    assert_eq!(record.identity.issue, None);
    let json = serde_json::to_value(&record).unwrap();
    assert!(json.get("repo").is_none(), "an absent repo is absent, not guessed");
    assert!(json.get("issue").is_none());
}

#[test]
fn truncation_is_reported_on_the_record_and_visible_in_the_body() {
    let long = "z".repeat(MAX_TEXT_CHARS + 500);
    let record = SessionOutputRecord::new_output(identity(), "0f8b", 0, at(0), at(0), &long);
    assert_eq!(record.truncated_bytes, 500);
    assert!(record.text.as_deref().unwrap().contains("truncated"));
}

#[test]
fn a_gap_record_names_its_reason_and_its_loss() {
    let record = SessionOutputRecord::status(
        identity(),
        OutputCategory::Gap,
        "0f8b",
        12,
        at(3),
        Coverage::Degraded,
        RunState::Running,
    )
    .with_gap("queue_overflow", 42);
    assert_eq!(record.gap_reason.as_deref(), Some("queue_overflow"));
    assert_eq!(record.dropped_events, 42);
    assert!(!record.is_content(), "a gap is status, not transcript content");
}

#[test]
fn an_unsupported_runtime_is_explicit_rather_than_quiet() {
    let mut identity = identity();
    identity.runtime = "codex".to_string();
    let record = SessionOutputRecord::status(
        identity,
        OutputCategory::Coverage,
        "sweep-issue-9764-1",
        0,
        at(0),
        Coverage::Unsupported,
        RunState::Running,
    );
    assert_eq!(record.coverage, Coverage::Unsupported);
    assert_eq!(record.text, None);
    assert!(!record.is_content());
}

#[test]
fn a_heartbeat_separates_a_quiet_run_from_a_stalled_export() {
    let record = SessionOutputRecord::status(
        identity(),
        OutputCategory::Heartbeat,
        "0f8b",
        99,
        at(30),
        Coverage::Live,
        RunState::Idle,
    );
    assert_eq!(record.state, RunState::Idle);
    assert_eq!(record.coverage, Coverage::Live);
    // Source and observation collapse only for producer-authored records,
    // where they are genuinely the same instant.
    assert_eq!(record.source_at, record.observed_at);
    assert_eq!(record.producer_lag_ms(), 0);
}

#[test]
fn producer_lag_is_the_source_to_read_delta_and_never_negative() {
    let record = SessionOutputRecord::new_output(identity(), "0f8b", 0, at(0), at(3), "x");
    assert_eq!(record.producer_lag_ms(), 3_000);
    // A source clock ahead of ours must not report a negative lag.
    let skewed = SessionOutputRecord::new_output(identity(), "0f8b", 1, at(9), at(0), "x");
    assert_eq!(skewed.producer_lag_ms(), 0);
}

#[test]
fn the_wire_vocabulary_matches_the_serde_spelling() {
    for category in [
        OutputCategory::Output,
        OutputCategory::ToolStart,
        OutputCategory::ToolFinish,
        OutputCategory::Heartbeat,
        OutputCategory::Gap,
        OutputCategory::Coverage,
    ] {
        let json = serde_json::to_value(category).unwrap();
        assert_eq!(json.as_str(), Some(category.as_str()));
    }
    for stream in [
        OutputStream::Assistant,
        OutputStream::Tool,
        OutputStream::Status,
    ] {
        assert_eq!(serde_json::to_value(stream).unwrap().as_str(), Some(stream.as_str()));
    }
    for coverage in [
        Coverage::Live,
        Coverage::Unsupported,
        Coverage::Degraded,
        Coverage::Ended,
    ] {
        assert_eq!(serde_json::to_value(coverage).unwrap().as_str(), Some(coverage.as_str()));
    }
    for state in [RunState::Running, RunState::Idle, RunState::Ended] {
        assert_eq!(serde_json::to_value(state).unwrap().as_str(), Some(state.as_str()));
    }
}

#[test]
fn the_kind_is_otlp_only_so_the_managed_https_sink_can_never_receive_it() {
    use crate::telemetry::TELEMETRY_KINDS;
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "session.output")
        .expect("session.output is registered");
    assert!(
        !meta.native_ingest,
        "session.output must never be accepted by the native HTTPS ingest backend"
    );
    assert_eq!(meta.otlp, crate::telemetry::TelemetryKindOtlp::Logs);
}

#[test]
fn collector_keeps_every_session_output_attribute() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.session.output.event_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
    // The identity keys this kind reuses rather than redefining.
    for shared in [
        "loom.repo",
        "loom.issue",
        "loom.sweep_id",
        "loom.session_id",
        "loom.attempt",
        "loom.runtime",
        "loom.role",
    ] {
        assert!(log_keep.contains(&format!("\"{shared}\"")), "collector drops {shared}");
    }
}

#[test]
fn the_gateway_re_scrubs_session_output_bodies_before_transform_privacy() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    assert!(
        CONFIG.contains("transform/session_output_redaction"),
        "the gateway-side defence-in-depth stage is missing"
    );
    let logs_pipeline = CONFIG
        .lines()
        .find(|l| l.contains("processors: [memory_limiter") && l.contains("transform/privacy"))
        .expect("the logs pipeline processor list");
    let redaction_at = logs_pipeline
        .find("transform/session_output_redaction")
        .expect("session_output redaction is in the logs pipeline");
    let privacy_at = logs_pipeline
        .find("transform/privacy")
        .expect("privacy stage");
    assert!(
        redaction_at < privacy_at,
        "redaction must run before transform/privacy: {logs_pipeline}"
    );
}

#[test]
fn visibility_is_private_unless_something_proves_otherwise() {
    // The producer makes no forge call, so it must never assert `public`. A
    // default-constructed identity — the shape an unattributed session gets —
    // is Private, and so is one decoded from a payload that omits the field.
    assert_eq!(RunIdentity::default().visibility, RepoVisibility::Private);
    let decoded: RunIdentity =
        serde_json::from_value(serde_json::json!({ "runtime": "claude" })).unwrap();
    assert_eq!(decoded.visibility, RepoVisibility::Private);
    // And a present-but-garbage value still decodes Private rather than Public
    // (the #8714 fail-closed Deserialize impl, restated here because this kind
    // is the one carrying readable session text).
    let hostile: RunIdentity = serde_json::from_value(
        serde_json::json!({ "runtime": "claude", "visibility": "PUBLIC-ish" }),
    )
    .unwrap();
    assert_eq!(hostile.visibility, RepoVisibility::Private);
}

#[test]
fn session_kind_says_why_an_issue_is_absent() {
    // The distinction the issue asks for: a deliberately unscoped interactive
    // session is not the same as a sweep whose attribution was lost, and a
    // null `loom.issue` alone cannot tell them apart.
    let interactive = RunIdentity {
        runtime: "claude".to_string(),
        session_kind: Some(SessionKind::Interactive),
        ..RunIdentity::default()
    };
    assert_eq!(interactive.issue, None);
    assert_eq!(
        interactive.session_kind.map(SessionKind::as_str),
        Some("interactive"),
        "an unattributed session is explicitly unscoped, not merely missing"
    );

    let lost = RunIdentity {
        runtime: "claude".to_string(),
        session_kind: Some(SessionKind::Sweep),
        ..RunIdentity::default()
    };
    assert_eq!(lost.issue, None);
    assert_eq!(lost.session_kind.map(SessionKind::as_str), Some("sweep"));
    assert_ne!(
        interactive.session_kind, lost.session_kind,
        "both lack an issue, so only session_kind separates intent from loss"
    );
}
