//! Observability **export liveness** health-section coverage (#5083 config
//! errors #5337, first-hop scope #9015).
//!
//! A child module of `health::tests` rather than more lines in `tests.rs`,
//! which is over `.loom/docs/file-size-policy.md`'s threshold and frozen —
//! same split as `section_inventory` and `auto_update_stale_repo`. Pure move
//! apart from the #9015 test at the end; `use super::*` keeps every fixture
//! (`now`, `healthy_inputs`, `mismatched_inputs`) shared with the parent so the
//! two suites cannot drift over what "healthy" means.

use super::*;

/// Inputs whose daemon reports a running exporter with the given export
/// record. `now()` is the report's `at`, so ages are exact.
fn exporting_inputs(
    mutate: impl FnOnce(&mut crate::types::ObservabilityExportStatus),
) -> HealthInputs {
    let mut inputs = healthy_inputs();
    let mut export = crate::types::ObservabilityExportStatus {
        state: crate::types::ObservabilityExportState::Starting,
        host_id: Some("robb-studio".to_string()),
        ingest_host_id: None,
        endpoint: Some("https://dashboard.example/ingest".to_string()),
        exporter: Some("https".to_string()),
        started_at: Some(now() - chrono::Duration::hours(4)),
        last_success_at: None,
        records_exported: 0,
        consecutive_failures: 0,
        flush_interval_secs: Some(30),
        ..Default::default()
    };
    mutate(&mut export);
    inputs.status.as_mut().unwrap().observability_export = Some(export);
    inputs
}

#[test]
fn a_healthy_exporter_still_renders_no_observability_section() {
    // The #4830 guarantee this issue must NOT reverse: exporting normally
    // stays silent on the anomaly-only surface. The positive confirmation
    // lives on `loom-daemon status` instead.
    let inputs = exporting_inputs(|e| {
        e.last_success_at = Some(now() - chrono::Duration::seconds(12));
        e.records_exported = 3481;
    });
    let report = assess(&inputs);
    assert!(report.section("observability").is_none());
    assert_eq!(report.overall, Verdict::Green);
    // 13 always-present sections: + `peer_coordination` (#6157),
    // `role_liveness` (#6201), `stale_sweeps` (#7529), `auto_update`
    // (#7584), `worktree_reaper` (#7590), `pool_hold` (#7990), and
    // `operator_attention` (#8091).
    assert_eq!(report.sections.len(), 13);
}

#[test]
fn a_disabled_exporter_still_renders_no_observability_section() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().observability_export =
        Some(crate::types::ObservabilityExportStatus::disabled());
    assert!(assess_observability(&inputs).is_none());
}

#[test]
fn a_misconfigured_exporter_is_a_degraded_observability_note() {
    // Issue #5337: `enabled: true` with a required piece of config
    // missing/unreadable must NOT stay silent like `disabled` above — it
    // is a config error an operator should fix, so it earns a section,
    // and that section must name the offending detail and reflect
    // whatever endpoint DID resolve.
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().observability_export =
            Some(crate::types::ObservabilityExportStatus::misconfigured(
                Some("https://ingest.example.com/v1/telemetry".to_string()),
                "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
                    .to_string(),
            ));
    let report = assess(&inputs);
    let section = report.section("observability").expect("section present");
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("/etc/loom/ingest.key") && section.summary.contains("os error 2"),
        "the note must name the offending path and errno: {}",
        section.summary
    );
    assert_eq!(section.detail["state"], "misconfigured");
    assert_eq!(section.detail["endpoint"], "https://ingest.example.com/v1/telemetry");
    assert_eq!(report.overall, Verdict::Degraded);
    assert_eq!(report.exit_code(), EXIT_DEGRADED);
}

#[test]
fn a_degraded_observability_detail_names_the_first_hop_scope() {
    // #9015: `health --json`'s machine-readable detail carries how far the
    // export state reaches, so a consumer of a DEGRADED section (or of the
    // positive facts folded into one) can tell a verified backend from a
    // verified local hop. Derived from the endpoint, so a pre-#9015 payload —
    // which sends neither field — still reports it.
    let inputs = exporting_inputs(|e| {
        e.endpoint = Some("http://127.0.0.1:14318/v1/logs".to_string());
        e.consecutive_failures = 3;
        e.last_success_at = Some(now() - chrono::Duration::hours(2));
        e.last_failure_detail = Some("sink rejected batch: HTTP 502".to_string());
        // Explicitly stale/unset, as an older daemon would send it.
        e.endpoint_loopback = false;
    });
    let section = assess_observability(&inputs).expect("failing exporter earns a section");
    assert_eq!(section.detail["state"], "failing");
    assert_eq!(section.detail["scope"], "first_hop");
    assert_eq!(
        section.detail["endpoint_loopback"], true,
        "a loopback endpoint must be flagged even when the wire payload predates #9015"
    );
}

#[test]
fn a_starting_exporter_is_not_yet_called_out() {
    // A daemon restarted 20 seconds ago has not had a fair chance to
    // flush; reporting it as broken would make every roll look like an
    // outage.
    let inputs = exporting_inputs(|e| {
        e.started_at = Some(now() - chrono::Duration::seconds(20));
    });
    assert!(assess_observability(&inputs).is_none());
}

#[test]
fn a_never_exporting_host_is_a_degraded_observability_note() {
    // THE gap this issue exists for: configured, running for four hours,
    // and has never landed a single batch — which before #5083 rendered
    // exactly like a healthy host: no section at all.
    let inputs = exporting_inputs(|_| {});
    let report = assess(&inputs);
    let section = report.section("observability").expect("section present");
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("NEVER"),
        "the note must name the condition unambiguously: {}",
        section.summary
    );
    assert!(
        section.summary.contains("robb-studio") && section.summary.contains("dashboard.example"),
        "the note must name the host identity AND the endpoint: {}",
        section.summary
    );
    // Machine-readable for a watch loop (AC5).
    assert_eq!(section.detail["state"], "never_exported");
    assert_eq!(section.detail["host_id"], "robb-studio");
    assert!(section.detail["last_success_at"].is_null());
    assert_eq!(report.overall, Verdict::Degraded);
    assert_eq!(report.exit_code(), EXIT_DEGRADED);
}

#[test]
fn a_failing_exporter_is_a_degraded_observability_note() {
    let inputs = exporting_inputs(|e| {
        e.last_success_at = Some(now() - chrono::Duration::hours(2));
        e.records_exported = 900;
        e.consecutive_failures = 4;
        e.last_failure_at = Some(now() - chrono::Duration::seconds(30));
        e.last_failure_detail = Some("sink rejected batch: HTTP 401 — denied".to_string());
    });
    let report = assess(&inputs);
    let section = report.section("observability").expect("section present");
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("HTTP 401"),
        "the exporter's own error must reach the operator: {}",
        section.summary
    );
    assert!(
        section.summary.contains("2h"),
        "a regression must be distinguishable from never-worked: {}",
        section.summary
    );
    assert_eq!(section.detail["state"], "failing");
    assert_eq!(section.detail["consecutive_failures"], 4);
}

#[test]
fn a_mismatch_still_wins_and_now_carries_the_export_facts() {
    // #4830 regression guard: the mismatch note is unchanged, and the
    // positive facts ride along in `detail.export` so a machine consumer
    // reading a DEGRADED section still learns whether anything is landing.
    let mut inputs = mismatched_inputs(3600);
    inputs.status.as_mut().unwrap().observability_export =
        Some(crate::types::ObservabilityExportStatus {
            state: crate::types::ObservabilityExportState::HostIdMismatch,
            host_id: Some("robb-studio".to_string()),
            ingest_host_id: Some("robb-pro".to_string()),
            last_success_at: Some(now() - chrono::Duration::seconds(12)),
            records_exported: 77,
            started_at: Some(now() - chrono::Duration::hours(4)),
            ..Default::default()
        });
    let section = assess_observability(&inputs).expect("section present");
    assert_eq!(section.verdict, Verdict::Degraded);
    // Unchanged #4830 keys.
    assert_eq!(section.detail["daemon_host_id"], "robb-studio");
    assert_eq!(section.detail["ingest_host_id"], "robb-pro");
    assert_eq!(section.detail["first_seen_age_secs"], 3600);
    // Additive #5083 payload.
    assert_eq!(section.detail["export"]["records_exported"], 77);
    assert_eq!(section.detail["export"]["state"], "host_id_mismatch");
}

#[test]
fn a_pre_5083_daemon_reporting_no_export_field_renders_nothing() {
    // An older daemon cannot answer, so the section stays absent — the
    // pre-#5083 baseline, never a fabricated "never exported".
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.observability_export = None;
    status.observability_host_id_mismatch = None;
    assert!(assess_observability(&inputs).is_none());
}
