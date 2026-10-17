//! Reachable-path watchdog-protection surfacing (#4354, AC4 of #4331).
//!
//! These tests pin the `--json` contract for the `protection` object and the
//! deliberate separation from the unreachable path's `install_state` block
//! (#4069): the reachable payload carries `protection` and never
//! `install_state`, and `protection` is wire-compatible-nullable so a host
//! where no loom dir resolves still emits a well-formed payload.
use super::{build_is_stale, build_status_json_value, deep_clean_lines, render_build_status_line};
use crate::cli::status::sample_report::sample_report;
use loom_daemon::daemon_install_state::{ProtectionReport, ProtectionState, WatchdogJob};
use std::path::PathBuf;

fn protection(
    state: ProtectionState,
    marker_present: bool,
    watchdog_provisioned: Option<bool>,
) -> ProtectionReport {
    ProtectionReport {
        state,
        marker_present,
        marker_path: PathBuf::from("/home/u/.loom/autonomy-desired"),
        job: WatchdogJob::SystemdTimer {
            timer_unit: "loom-daemon-watchdog.timer".to_string(),
        },
        watchdog_provisioned,
        detail: "fixture detail".to_string(),
    }
}

/// The `update` argument's fixture — an "unknown" self-update status is
/// enough for these tests (they assert only on the `protection` object).
fn no_update() -> loom_daemon::self_update::SelfUpdateStatus {
    loom_daemon::self_update::SelfUpdateStatus {
        built_commit: "abc1234".to_string(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    }
}

#[test]
fn protection_object_is_emitted_on_the_reachable_path() {
    let report = sample_report();
    let value = build_status_json_value(
        &report,
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::Protected, true, Some(true))),
        None,
    );
    let p = &value["protection"];
    assert_eq!(p["state"], "protected");
    assert_eq!(p["marker_present"], true);
    assert_eq!(p["marker_path"], "/home/u/.loom/autonomy-desired");
    assert_eq!(p["watchdog_job"], "loom-daemon-watchdog.timer");
    assert_eq!(p["watchdog_job_kind"], "systemd-timer");
    assert_eq!(p["watchdog_provisioned"], true);
    assert_eq!(p["detail"], "fixture detail");
}

#[test]
fn protection_object_carries_both_facts_for_the_no_marker_verdict() {
    // `state` names the dominant fact (no marker), but a consumer must still
    // be able to read the watchdog fact independently (#4354 AC1: "both
    // should be reported").
    let value = build_status_json_value(
        &sample_report(),
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::NoMarker, false, Some(true))),
        None,
    );
    let p = &value["protection"];
    assert_eq!(p["state"], "no-marker");
    assert_eq!(p["marker_present"], false);
    assert_eq!(p["watchdog_provisioned"], true);
}

#[test]
fn protection_watchdog_not_provisioned_and_unknown_serialize_distinctly() {
    let not_provisioned = build_status_json_value(
        &sample_report(),
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::WatchdogNotProvisioned, true, Some(false))),
        None,
    );
    assert_eq!(not_provisioned["protection"]["state"], "watchdog-not-provisioned");
    assert_eq!(not_provisioned["protection"]["watchdog_provisioned"], false);

    // An unanswerable probe is `unknown` with a NULL provisioning fact —
    // never `false` (AC4: degrade, never mis-report).
    let unknown = build_status_json_value(
        &sample_report(),
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::Unknown, true, None)),
        None,
    );
    assert_eq!(unknown["protection"]["state"], "unknown");
    assert!(unknown["protection"]["watchdog_provisioned"].is_null());
}

/// Issue #7609: `status --json` reports the release artifact the
/// artifact-first auto_update tick resolved for this host's platform —
/// `null` when none resolved (no Releases yet, an unreachable API,
/// artifact-fetch disabled), a `{version, published_at}` object when one
/// did.
#[test]
fn auto_update_artifact_available_is_reported_next_to_the_installed_version() {
    let value = build_status_json_value(&sample_report(), None, &no_update(), None, None, None);
    assert!(
        value["auto_update"]["artifact_available"].is_null(),
        "no resolved artifact ⇒ null, not an empty object"
    );

    let mut report = sample_report();
    report.auto_update_enabled = true;
    report.auto_update_artifact_version = Some("0.19.24".to_string());
    report.auto_update_artifact_published_at = Some("2026-09-13T12:00:00Z".to_string());
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    assert_eq!(value["auto_update"]["artifact_available"]["version"], "0.19.24");
    assert_eq!(
        value["auto_update"]["artifact_available"]["published_at"],
        "2026-09-13T12:00:00Z"
    );
}

#[test]
fn observability_host_id_mismatch_is_null_when_the_ids_agree() {
    // #4830: the common case — no exporter, or an exporter whose key is
    // bound to this very host — is a null field, so `status` is unchanged
    // for every daemon that is not actually misconfigured.
    let value = build_status_json_value(&sample_report(), None, &no_update(), None, None, None);
    assert!(value["observability_host_id_mismatch"].is_null());
}

#[test]
fn observability_host_id_mismatch_names_both_identities_in_json() {
    let mut report = sample_report();
    report.observability_host_id_mismatch = Some(loom_daemon::types::ObservabilityHostIdMismatch {
        daemon_host_id: "robb-studio".to_string(),
        ingest_host_id: "robb-pro".to_string(),
        first_seen_at: chrono::Utc::now(),
    });
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let m = &value["observability_host_id_mismatch"];
    assert_eq!(m["daemon_host_id"], "robb-studio");
    assert_eq!(m["ingest_host_id"], "robb-pro");
    assert!(m["first_seen_at"].is_string());
}

// ===================================================================
// Telemetry export liveness (#5083); first-hop scope (#9015)
// ===================================================================

/// A healthy export record pushing to `endpoint`. The human-readable line
/// is covered in `status_render::observability_line`'s own tests; these two
/// pin the `--json` contract a watch loop reads.
fn healthy_export(endpoint: &str) -> loom_daemon::types::ObservabilityExportStatus {
    let mut status = loom_daemon::types::ObservabilityExportStatus {
        state: loom_daemon::types::ObservabilityExportState::Healthy,
        host_id: Some("robb-studio".to_string()),
        endpoint: Some(endpoint.to_string()),
        exporter: Some("https".to_string()),
        started_at: Some(chrono::Utc::now() - chrono::Duration::hours(4)),
        last_success_at: Some(chrono::Utc::now()),
        records_exported: 12,
        flush_interval_secs: Some(30),
        ..Default::default()
    };
    status.refresh_endpoint_scope();
    status
}

#[test]
fn observability_export_is_machine_readable_in_json() {
    // AC5 of #5083: a watch loop asserts on the state string directly.
    let mut report = sample_report();
    report.observability_export = Some(healthy_export("https://dashboard.example/ingest"));
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let e = &value["observability_export"];
    assert_eq!(e["state"], "healthy");
    assert_eq!(e["host_id"], "robb-studio");
    assert_eq!(e["records_exported"], 12);
    assert_eq!(e["endpoint"], "https://dashboard.example/ingest");
    assert!(e["last_success_at"].is_string());
    // #9015: and it says how far that `healthy` reaches.
    assert_eq!(e["scope"], "first_hop");
    assert_eq!(e["endpoint_loopback"], false);
}

#[test]
fn observability_export_json_flags_a_local_collector_endpoint() {
    // #9015 AC1, machine-readable half: a `healthy` first hop into a local
    // edge collector is flagged, so `jq '.state == "healthy"'` alone can no
    // longer be mistaken for "the data is in the backend".
    let mut report = sample_report();
    report.observability_export = Some(healthy_export("http://127.0.0.1:14318/v1/logs"));
    report.observability_exports =
        [("otlp".to_string(), healthy_export("http://127.0.0.1:14318/v1/logs"))]
            .into_iter()
            .collect();
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    assert_eq!(value["observability_export"]["scope"], "first_hop");
    assert_eq!(value["observability_export"]["endpoint_loopback"], true);
    // Each per-exporter cell (#8756) carries the same two facts.
    assert_eq!(value["observability_exports"]["otlp"]["scope"], "first_hop");
    assert_eq!(value["observability_exports"]["otlp"]["endpoint_loopback"], true);
}

#[test]
fn observability_export_is_null_for_a_pre_5083_daemon() {
    let value = build_status_json_value(&sample_report(), None, &no_update(), None, None, None);
    assert!(value["observability_export"].is_null());
}

// ===================================================================
// Artifact reclaim / deep-clean visibility (#5919)
// ===================================================================

fn deep_clean_entry(root: &str) -> loom_daemon::types::DeepCleanRepoStatus {
    loom_daemon::types::DeepCleanRepoStatus {
        root: std::path::PathBuf::from(root),
        last_evaluated_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-10T21:19:10Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
        last_reason: Some("118G free >= 20G floor — no disk pressure".to_string()),
        last_free_gb: Some(118),
        last_fired_at: None,
        last_reclaimed: None,
    }
}

#[test]
fn deep_clean_says_so_when_no_pass_has_been_evaluated_yet() {
    // A pre-#5919 daemon, a disabled reaper, and a daemon that has not
    // completed its first post-startup tick all land here — and must NOT
    // read as "nothing needed reclaiming".
    let lines = deep_clean_lines(&sample_report());
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("not evaluated yet"), "{}", lines[0]);
}

#[test]
fn deep_clean_renders_the_healthy_non_firing_case_with_its_reason() {
    let mut report = sample_report();
    report.deep_clean = vec![deep_clean_entry("/home/u/GitHub/loom")];
    let rendered = deep_clean_lines(&report).join("\n");
    assert!(rendered.contains("/home/u/GitHub/loom"), "{rendered}");
    assert!(rendered.contains("no pass has fired since this daemon started"), "{rendered}");
    assert!(rendered.contains("no disk pressure"), "{rendered}");
}

#[test]
fn deep_clean_names_when_a_pass_last_ran_and_what_it_reclaimed() {
    // AC4 of #5919: "is reclamation happening" answerable without logs.
    let mut entry = deep_clean_entry("/home/u/GitHub/loom");
    entry.last_fired_at = Some(
        chrono::DateTime::parse_from_rfc3339("2026-08-10T21:04:10Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    );
    entry.last_reclaimed = Some("target/ (34.1G), node_modules/ (1.2G)".to_string());
    entry.last_reason = Some("DISK PRESSURE — 2G free < 20G floor".to_string());
    let mut report = sample_report();
    report.deep_clean = vec![entry];

    let rendered = deep_clean_lines(&report).join("\n");
    assert!(rendered.contains("last fired 2026-08-10T21:04:10"), "{rendered}");
    assert!(rendered.contains("target/ (34.1G)"), "{rendered}");
    assert!(rendered.contains("DISK PRESSURE"), "{rendered}");
}

#[test]
fn deep_clean_state_is_carried_on_the_json_surface() {
    let mut report = sample_report();
    report.deep_clean = vec![deep_clean_entry("/home/u/GitHub/loom")];
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    assert_eq!(value["deep_clean"][0]["root"], "/home/u/GitHub/loom");
    assert_eq!(value["deep_clean"][0]["last_free_gb"], 118);
    assert!(value["deep_clean"][0]["last_fired_at"].is_null());
}

#[test]
fn deep_clean_is_empty_for_a_pre_5919_daemon() {
    let value = build_status_json_value(&sample_report(), None, &no_update(), None, None, None);
    assert_eq!(value["deep_clean"].as_array().unwrap().len(), 0);
}

// ===================================================================
// Running-vs-disk build staleness (#5341)
// ===================================================================

#[test]
fn build_is_stale_reports_true_on_a_genuine_mismatch() {
    // The `loom-worker-1` incident this issue exists for: a running
    // daemon process built from an OLDER commit than the on-disk binary.
    assert_eq!(build_is_stale(Some("5111b74a"), "3f5132a5"), Some(true));
}

#[test]
fn build_is_stale_reports_false_on_a_match() {
    assert_eq!(build_is_stale(Some("5111b74a"), "5111b74a"), Some(false));
}

#[test]
fn build_is_stale_is_none_when_the_daemon_never_reported_a_running_commit() {
    // A pre-#5341 daemon binary — never misread as "matches" or "stale".
    assert_eq!(build_is_stale(None, "3f5132a5"), None);
}

#[test]
fn build_is_stale_is_none_for_an_unknown_or_empty_commit_on_either_side() {
    assert_eq!(build_is_stale(Some("unknown"), "3f5132a5"), None);
    assert_eq!(build_is_stale(Some("5111b74a"), "unknown"), None);
    assert_eq!(build_is_stale(Some(""), "3f5132a5"), None);
}

#[test]
fn build_status_line_warns_and_recommends_a_restart_on_a_mismatch() {
    let line = render_build_status_line(
        Some("5111b74a"),
        Some("2026-08-03T02:09:51Z"),
        "3f5132a5",
        "2026-08-04T01:00:12Z",
    );
    assert!(line.starts_with("Build:         STALE"), "{line}");
    assert!(line.contains("running 5111b74a"), "{line}");
    assert!(line.contains("disk 3f5132a5"), "{line}");
    assert!(line.contains("restart to roll"), "{line}");
    assert!(
        line.contains("loom-daemon-stop.sh") && line.contains("loom-daemon-start.sh"),
        "{line}"
    );
}

#[test]
fn build_status_line_is_quiet_on_a_match() {
    let line = render_build_status_line(
        Some("5111b74a"),
        Some("2026-08-03T02:09:51Z"),
        "5111b74a",
        "2026-08-03T02:09:51Z",
    );
    assert!(line.contains("running matches disk"), "{line}");
    assert!(!line.contains("STALE"), "{line}");
}

#[test]
fn build_status_line_from_an_older_daemon_is_unknown_not_a_false_match() {
    // A `None` running commit means the daemon predates #5341 — reporting
    // it as "matches" would invent a fact about a daemon that said nothing.
    let line = render_build_status_line(None, None, "3f5132a5", "2026-08-04T01:00:12Z");
    assert!(line.contains("unknown"), "{line}");
    assert!(line.contains("older daemon binary"), "{line}");
}

#[test]
fn build_status_line_degrades_legibly_for_a_tarball_build() {
    // No git info baked into either binary — never claim staleness from a
    // non-comparison.
    let line = render_build_status_line(Some("unknown"), None, "unknown", "unknown");
    assert!(line.contains("staleness unknown"), "{line}");
    assert!(!line.contains("STALE"), "{line}");
}

#[test]
fn daemon_build_json_block_carries_the_running_and_disk_facts() {
    let mut report = sample_report();
    report.daemon_build_commit = Some("5111b74a".to_string());
    report.daemon_built_at_raw = Some("2026-08-03T02:09:51Z".to_string());
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let b = &value["daemon_build"];
    assert_eq!(b["running_commit"], "5111b74a");
    assert_eq!(b["running_built_at"], "2026-08-03T02:09:51Z");
    // The disk side is this TEST binary's own compile-time build — the
    // exact quantity `render_build_status_line` also compares against.
    assert_eq!(b["disk_commit"], loom_daemon::self_update::BUILT_COMMIT);
    assert_eq!(b["disk_built_at"], loom_daemon::self_update::BUILT_AT_RAW);
    assert_eq!(
        b["stale"],
        serde_json::json!(build_is_stale(Some("5111b74a"), loom_daemon::self_update::BUILT_COMMIT))
    );
}

#[test]
fn daemon_build_json_block_running_commit_is_null_for_a_pre_5341_daemon() {
    let mut report = sample_report();
    report.daemon_build_commit = None;
    report.daemon_built_at_raw = None;
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let b = &value["daemon_build"];
    assert!(b["running_commit"].is_null());
    assert!(b["running_built_at"].is_null());
    assert!(b["stale"].is_null());
}

#[test]
fn protection_is_null_when_no_report_could_be_built() {
    // No loom dir resolvable ⇒ the field is present but null, so the payload
    // stays well-formed for consumers that always read it.
    let value = build_status_json_value(&sample_report(), None, &no_update(), None, None, None);
    assert!(value["protection"].is_null());
}

#[test]
fn reachable_payload_never_carries_the_unreachable_install_state_block() {
    // #4069 regression guard: `install_state` (with its exit-code semantics)
    // belongs to the unreachable `Err` arm ONLY. Protection is a sibling
    // classification, so adding it must not leak `install_state` into the
    // reachable payload. This fixture (`NoMarker`, work_finder unset) is
    // not an `autonomy_mismatch` case, so it also exercises the ordinary
    // reachable-path exit-0 case (#5409's `EXIT_AUTONOMY_MISMATCH` fires
    // ONLY when `protection.autonomy_mismatch` is true — see the
    // `autonomy_mismatch_*` tests below for that decision, made in
    // `handle_status_command`, not in this JSON builder).
    let value = build_status_json_value(
        &sample_report(),
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::NoMarker, false, Some(false))),
        None,
    );
    assert!(
        value.get("install_state").is_none(),
        "install_state must stay on the unreachable path only"
    );
    assert!(value.get("protection").is_some());
}

// ---- marker-vs-non-autonomous-daemon mismatch (#4693, AC3) ----
// The sibling of #4069's ExpectedButDead state, but for the REACHABLE
// path: the daemon IS alive and answering, yet its work-finder loop is
// OFF while the marker records an operator's autonomy-desired intent —
// exactly the 2026-07-30 incident's end state (a "protected", healthy
// daemon that had silently stopped dispatching).

#[test]
fn autonomy_mismatch_true_when_marker_present_and_work_finder_off() {
    let mut report = sample_report();
    report.work_finder_enabled = Some(false);
    let value = build_status_json_value(
        &report,
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::Protected, true, Some(true))),
        None,
    );
    assert_eq!(value["work_finder"]["enabled"], false);
    assert_eq!(value["protection"]["autonomy_mismatch"], true);
}

#[test]
fn autonomy_mismatch_false_when_work_finder_is_on() {
    let mut report = sample_report();
    report.work_finder_enabled = Some(true);
    let value = build_status_json_value(
        &report,
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::Protected, true, Some(true))),
        None,
    );
    assert_eq!(value["work_finder"]["enabled"], true);
    assert_eq!(value["protection"]["autonomy_mismatch"], false);
}

#[test]
fn autonomy_mismatch_false_when_marker_absent_even_if_work_finder_off() {
    // Work-finder off with NO marker is the ordinary, deliberate FLAGS-OFF
    // reliability-daemon case (#3911) -- not a mismatch.
    let mut report = sample_report();
    report.work_finder_enabled = Some(false);
    let value = build_status_json_value(
        &report,
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::NoMarker, false, Some(true))),
        None,
    );
    assert_eq!(value["protection"]["autonomy_mismatch"], false);
}

#[test]
fn autonomy_mismatch_false_when_work_finder_enabled_is_unknown() {
    // A pre-#4693 daemon binary never computed `work_finder_enabled` —
    // `null` must degrade to "no claim", never a false positive.
    let mut report = sample_report();
    report.work_finder_enabled = None;
    let value = build_status_json_value(
        &report,
        None,
        &no_update(),
        None,
        Some(&protection(ProtectionState::Protected, true, Some(true))),
        None,
    );
    assert!(value["work_finder"]["enabled"].is_null());
    assert_eq!(value["protection"]["autonomy_mismatch"], false);
}

// ---- #5409 AC2: the shared `autonomy_mismatch()` predicate, exercised
// directly (not just through the JSON payload it feeds) since it is now
// ALSO the input to `handle_status_command`'s exit-code decision
// (`EXIT_AUTONOMY_MISMATCH`) — the two consumers must never be able to
// disagree, which sharing one function (rather than recomputing the
// condition twice) makes structurally impossible.

#[test]
fn autonomy_mismatch_fn_true_when_marker_present_and_work_finder_off() {
    let mut report = sample_report();
    report.work_finder_enabled = Some(false);
    let p = protection(ProtectionState::Protected, true, Some(true));
    assert!(super::autonomy_mismatch(Some(&p), &report));
}

#[test]
fn autonomy_mismatch_fn_false_when_protection_is_none() {
    // No loom dir resolvable ⇒ no marker fact available at all — never a
    // false positive just because work_finder happens to read off.
    let mut report = sample_report();
    report.work_finder_enabled = Some(false);
    assert!(!super::autonomy_mismatch(None, &report));
}

#[test]
fn exit_autonomy_mismatch_is_distinct_from_every_unreachable_and_fleet_exit_code() {
    // #5409 AC2's own guardrail: this reachable-path exit code must not
    // collide with (a) the unreachable-path `InstallState` codes (1/3/4,
    // the SAME `status` command's OTHER branch) or (b) `fleet status`'s
    // `HealthReport::exit_code()` (0/1/2, a different command). A
    // collision would not break *this* command directly, but would make
    // "was this the mismatch, or was it something else" ambiguous for any
    // script that greps a bare exit code instead of `--json`.
    use loom_daemon::daemon_install_state::{
        EXIT_ALIVE_BUT_UNRESPONSIVE, EXIT_AUTONOMY_MISMATCH, EXIT_EXPECTED_BUT_DEAD,
        EXIT_NOT_EXPECTED,
    };
    assert_ne!(EXIT_AUTONOMY_MISMATCH, EXIT_NOT_EXPECTED);
    assert_ne!(EXIT_AUTONOMY_MISMATCH, EXIT_EXPECTED_BUT_DEAD);
    assert_ne!(EXIT_AUTONOMY_MISMATCH, EXIT_ALIVE_BUT_UNRESPONSIVE);
    assert_ne!(EXIT_AUTONOMY_MISMATCH, 0, "must not silently collapse to the OK exit code");
}
