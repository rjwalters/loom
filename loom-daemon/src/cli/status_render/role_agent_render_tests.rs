//! Concurrent role-agent surfacing (#6102, AC3).
//!
//! The reporting half of the issue: on a 28-core host `loom-daemon status`
//! read "In-flight sweeps: 1 / Dynamic concurrency cap: 8" while the box
//! carried a 1m load average of 32.41 and eleven `claude-wrapper.sh`
//! processes. Nine of those were role-runner agents that the status surface
//! simply did not report and that `maxConcurrent` does not bound. These
//! tests pin that total agent load is readable from one place, on both the
//! human and `--json` surfaces, and that the wire field is
//! backward-compatible.
use super::{build_status_json_value, render_role_agent_line};
use crate::cli::status::sample_report::sample_report;
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

fn report_with(active: usize, ceiling: Option<usize>) -> DaemonStatusReport {
    DaemonStatusReport {
        active_role_agents: active,
        role_agent_max_concurrent: ceiling,
        ..sample_report()
    }
}

#[test]
fn human_line_reports_the_count_its_ceiling_and_the_total() {
    let report = report_with(9, Some(7));
    let line = render_role_agent_line(&report);
    assert!(
        line.contains("Role-runner agents in flight: 9 (ceiling 7)"),
        "must report the live count and its ceiling: {line}"
    );
    // The whole point: an operator must not have to add these up mentally
    // (or run `pgrep`) to learn this host's agent load.
    let sweeps = report.in_flight.len();
    assert!(
        line.contains(&format!("= {}", sweeps + 9)),
        "must report total agents (sweeps + role agents): {line}"
    );
}

/// The misreading #6102 is about: `maxConcurrent` looks like the
/// per-machine agent knob. The line must say, in band, that it is not.
#[test]
fn human_line_states_that_max_concurrent_does_not_bound_these() {
    let line = render_role_agent_line(&report_with(3, Some(7)));
    assert!(
        line.contains("autonomous.workFinder.maxConcurrent bounds sweep dispatch only"),
        "must disclaim the sweep knob: {line}"
    );
    assert!(
        line.contains("autonomous.roleRunner.maxConcurrent"),
        "must name the knob that DOES bound role agents: {line}"
    );
}

/// A pre-#6102 daemon sends no ceiling. Render it as `unknown`, never as
/// "unbounded": that older daemon really is unbounded, but this client
/// cannot distinguish that from a field it never received.
#[test]
fn human_line_renders_an_absent_ceiling_as_unknown() {
    let line = render_role_agent_line(&report_with(0, None));
    assert!(line.contains("ceiling unknown"), "{line}");
    assert!(!line.contains("unbounded"), "{line}");
}

#[test]
fn json_reports_the_count_ceiling_and_total() {
    let report = report_with(4, Some(7));
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let ra = &value["role_agents"];
    assert_eq!(ra["active"], 4);
    assert_eq!(ra["max_concurrent"], 7);
    assert_eq!(
        ra["total_with_sweeps"],
        report.in_flight.len() + 4,
        "scripted consumers get the sum without re-deriving it"
    );
    assert_eq!(
        ra["bounded_by_work_finder_max_concurrent"], false,
        "the payload must state the #6102 fact machine-readably too"
    );
}

/// A pre-#6102 daemon's `null` ceiling must stay `null` on the JSON
/// surface — a consumer reads that as "unknown", and fabricating a number
/// here would recreate exactly the false confidence #6102 is about.
#[test]
fn json_absent_ceiling_stays_null() {
    let value =
        build_status_json_value(&report_with(2, None), None, &no_update(), None, None, None);
    assert!(value["role_agents"]["max_concurrent"].is_null());
    assert_eq!(value["role_agents"]["active"], 2);
}

#[test]
fn role_agent_fields_survive_a_wire_round_trip_and_older_payloads() {
    let report = report_with(6, Some(7));
    let json = serde_json::to_string(&report).expect("serialize");
    let back: DaemonStatusReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.active_role_agents, 6);
    assert_eq!(back.role_agent_max_concurrent, Some(7));

    // Forward-compat: a pre-#6102 payload lacking both fields must parse,
    // reporting 0 agents and an UNKNOWN (not fabricated) ceiling.
    let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("value");
    let obj = stripped.as_object_mut().expect("object");
    obj.remove("active_role_agents");
    obj.remove("role_agent_max_concurrent");
    let older: DaemonStatusReport =
        serde_json::from_value(stripped).expect("pre-#6102 payload must still parse");
    assert_eq!(older.active_role_agents, 0);
    assert_eq!(older.role_agent_max_concurrent, None);
}
