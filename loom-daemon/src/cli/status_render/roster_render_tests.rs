//! Issue #7690, Phase A of #6704: the roster section must be silent on a
//! default (disabled) install, and must never hide an EXPIRED member —
//! the same "misconfiguration must render" principle
//! `role_runner_shard_render_tests` pins for the sharding header above.
use super::render_roster_lines;
use crate::cli::status::sample_report::sample_report;
use loom_daemon::types::{
    DaemonStatusReport, RoleRunnerShardPosture, RosterMemberStatus, RosterStatus,
};

fn member(host: &str, fresh: bool, is_this_host: bool) -> RosterMemberStatus {
    RosterMemberStatus {
        host: host.to_string(),
        fresh,
        last_beat_secs_ago: 41,
        serves_count: 27,
        is_this_host,
    }
}

fn posture_with_roster(roster: Option<RosterStatus>) -> RoleRunnerShardPosture {
    RoleRunnerShardPosture {
        index: Some(1),
        count: Some(4),
        summary: "shard 1 of 4 (index from env, count from config)".to_string(),
        configured: true,
        roster,
    }
}

#[test]
fn no_lines_when_the_daemon_reported_no_shard_posture_at_all() {
    let report = DaemonStatusReport {
        role_runner_shard: None,
        ..sample_report()
    };
    assert!(render_roster_lines(&report).is_empty());
}

#[test]
fn no_lines_when_the_roster_is_disabled_or_never_published() {
    let report = DaemonStatusReport {
        role_runner_shard: Some(posture_with_roster(None)),
        ..sample_report()
    };
    assert!(render_roster_lines(&report).is_empty());
}

#[test]
fn renders_the_header_and_one_line_per_member_including_expired_ones() {
    let roster = RosterStatus {
        issue: "rjwalters/loom#1234".to_string(),
        live_count: 2,
        seen_count: 3,
        generation: None,
        settled_secs: Some(22 * 60),
        fence: None,
        members: vec![
            member("host-a3f9c1d2", true, false),
            member("host-d9142cf3", true, true),
            member("host-e1d4c843", false, false),
        ],
    };
    let report = DaemonStatusReport {
        role_runner_shard: Some(posture_with_roster(Some(roster))),
        ..sample_report()
    };
    let lines = render_roster_lines(&report);
    assert_eq!(lines.len(), 4, "1 header + 3 members: {lines:?}");
    assert!(lines[0].contains("rjwalters/loom#1234"), "{}", lines[0]);
    assert!(lines[0].contains("2 live / 3 seen"), "{}", lines[0]);
    assert!(lines[0].contains("settled 22m"), "{}", lines[0]);
    // An EXPIRED member stays visible, never dropped.
    assert!(
        lines
            .iter()
            .any(|l| l.contains("host-e1d4c843") && l.contains("EXPIRED")),
        "expired member must render: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("host-d9142cf3") && l.contains("this host")),
        "this host must be marked: {lines:?}"
    );
}

/// Issue #7691: a host that the roster fence is holding back runs **no**
/// role ticks at all. That state must be visible — otherwise it is
/// indistinguishable in `status` from a healthy host that simply owns no
/// slice, which is precisely the invisibility #6374 was filed about.
#[test]
fn the_fence_verdict_renders_under_the_roster_header_when_present() {
    let roster = RosterStatus {
        issue: "rjwalters/loom#1234".to_string(),
        live_count: 3,
        seen_count: 3,
        generation: None,
        settled_secs: Some(60),
        fence: Some("YIELDING role ticks — this host's own roster record is stale".to_string()),
        members: vec![member("host-a3f9c1d2", true, true)],
    };
    let report = DaemonStatusReport {
        role_runner_shard: Some(posture_with_roster(Some(roster))),
        ..sample_report()
    };
    let lines = render_roster_lines(&report);
    assert_eq!(lines.len(), 3, "header + fence + 1 member: {lines:?}");
    assert!(lines[1].starts_with("  Fence: "), "{}", lines[1]);
    assert!(lines[1].contains("YIELDING"), "{}", lines[1]);
}
