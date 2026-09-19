//! Issue #5029: the pre-flight-death advisory (#4386) has no freshness
//! signal and is scoped to a single workspace with no way to tell which
//! one from the rendered text alone. These tests pin the display-only fix
//! — an "as of" freshness suffix on the human line, the timestamp on the
//! `--json` surface, and forward/backward wire compatibility — without
//! touching the trip/clear decision logic itself.
use super::{build_status_json_value, render_preflight_advisory_line};
use crate::cli::status::sample_report::sample_report;
use chrono::Utc;
use loom_daemon::self_update::SelfUpdateStatus;
use loom_daemon::types::DaemonStatusReport;

fn no_update() -> SelfUpdateStatus {
    SelfUpdateStatus {
        built_commit: "abc".to_string(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    }
}

fn report_with(
    active: bool,
    message: Option<&str>,
    changed_at: Option<chrono::DateTime<Utc>>,
) -> DaemonStatusReport {
    let mut r = sample_report();
    r.preflight_advisory_active = active;
    r.preflight_advisory_message = message.map(ToString::to_string);
    r.preflight_advisory_changed_at = changed_at;
    r
}

// ---- human surface -----------------------------------------------------

#[test]
fn active_advisory_renders_with_a_freshness_suffix() {
    let ts = Utc::now() - chrono::Duration::seconds(42);
    let report = report_with(
        true,
        Some("WARNING: last 3 dispatches died at claude-wrapper pre-flight (x) — check .mcp.json [workspace: /repos/loom]"),
        Some(ts),
    );
    let line = render_preflight_advisory_line(&report).expect("active advisory renders a line");
    assert!(line.contains("WARNING: last 3 dispatches"), "got: {line}");
    assert!(
        line.contains("workspace: /repos/loom"),
        "the rendered line must name the scoped workspace: {line}"
    );
    assert!(
        line.contains("as of") && line.contains("ago"),
        "the rendered line must carry a freshness indicator: {line}"
    );
}

#[test]
fn active_advisory_without_a_timestamp_renders_the_message_unchanged() {
    // Forward-compat: an older daemon binary that never populated the new
    // field must still render the bare message, not a "None"/error text.
    let report = report_with(true, Some("WARNING: still dying"), None);
    let line = render_preflight_advisory_line(&report).expect("line");
    assert_eq!(line, "WARNING: still dying");
}

#[test]
fn inactive_advisory_renders_nothing() {
    assert!(render_preflight_advisory_line(&report_with(false, None, None)).is_none());
    // Defensive: `active` true with no message must not panic or fabricate
    // a line — should not happen in practice, but stay silent rather than
    // render a garbage string.
    assert!(render_preflight_advisory_line(&report_with(true, None, Some(Utc::now()))).is_none());
}

// ---- --json surface ----------------------------------------------------

#[test]
fn json_carries_the_advisory_timestamp() {
    let ts = Utc::now() - chrono::Duration::seconds(7);
    let value = build_status_json_value(
        &report_with(true, Some("WARNING: x"), Some(ts)),
        None,
        &no_update(),
        None,
        None,
        None,
    );
    assert_eq!(value["preflight_advisory_active"], true);
    assert!(value["preflight_advisory_changed_at"].is_string());
}

#[test]
fn json_advisory_timestamp_is_null_when_absent() {
    let value = build_status_json_value(
        &report_with(false, None, None),
        None,
        &no_update(),
        None,
        None,
        None,
    );
    assert!(value["preflight_advisory_changed_at"].is_null());
}

#[test]
fn advisory_timestamp_survives_a_wire_round_trip_and_older_payloads() {
    let ts = Utc::now() - chrono::Duration::seconds(5);
    let report = report_with(true, Some("WARNING: x"), Some(ts));
    let json = serde_json::to_string(&report).expect("serialize");
    let back: DaemonStatusReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.preflight_advisory_changed_at, report.preflight_advisory_changed_at);

    // Forward-compat: an absent field (pre-#5029 daemon) parses as `None`,
    // never a fabricated/default timestamp.
    let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("value");
    stripped
        .as_object_mut()
        .expect("object")
        .remove("preflight_advisory_changed_at");
    let older: DaemonStatusReport =
        serde_json::from_value(stripped).expect("pre-#5029 payload must still parse");
    assert!(older.preflight_advisory_changed_at.is_none());
}
