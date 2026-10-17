//! #6374: `loom-daemon status` must be able to answer "which host carries
//! this workspace's role slice?" without the operator remembering which
//! host got which `LOOM_ROLE_RUNNER_SHARD_INDEX`. The pre-#6374
//! `LOOM_ROLE_RUNNER=0` mitigation was an out-of-band env override that
//! nothing rendered; these tests pin that its first-class replacement is
//! never silent about a configuration the operator asked for.
use super::{render_role_runner_shard_header_line, render_role_runner_shard_repo_line};
use crate::cli::status::sample_report::sample_report;
use loom_daemon::types::{
    DaemonStatusReport, RepoStatus, RoleRunnerShardPosture, RoleRunnerShardStatus,
};

fn posture(configured: bool, summary: &str) -> RoleRunnerShardPosture {
    RoleRunnerShardPosture {
        index: configured.then_some(1),
        count: configured.then_some(4),
        summary: summary.to_string(),
        configured,
        roster: None,
    }
}

fn repo_with_shard(shard: Option<RoleRunnerShardStatus>) -> RepoStatus {
    RepoStatus {
        role_runner_shard: shard,
        ..super::role_runner_diagnostic_source_render_tests::repo(true, &[], None)
    }
}

fn shard_status(owned_here: bool) -> RoleRunnerShardStatus {
    RoleRunnerShardStatus {
        owned_here,
        key: "rjwalters/loom".to_string(),
        key_source: "git-remote".to_string(),
        owning_shard: Some(2),
        host_shard: Some(if owned_here { 2 } else { 0 }),
        shard_count: Some(4),
    }
}

// ---- host header line ----

/// A pre-#6374 daemon never sends the field. Printing "unsharded" would
/// be a claim this client cannot substantiate.
#[test]
fn header_absent_when_the_daemon_reported_no_posture() {
    let report = DaemonStatusReport {
        role_runner_shard: None,
        ..sample_report()
    };
    assert!(render_role_runner_shard_header_line(&report).is_none());
}

/// The overwhelmingly common single-host install must not grow a line
/// about a feature it does not use.
#[test]
fn header_absent_when_nothing_is_configured() {
    let report = DaemonStatusReport {
        role_runner_shard: Some(posture(false, "off (no ... configured)")),
        ..sample_report()
    };
    assert!(render_role_runner_shard_header_line(&report).is_none());
}

#[test]
fn header_renders_a_configured_shard_with_its_summary() {
    let report = DaemonStatusReport {
        role_runner_shard: Some(posture(true, "shard 1 of 4 (index from env, count from config)")),
        ..sample_report()
    };
    let line = render_role_runner_shard_header_line(&report).expect("configured => rendered");
    assert!(line.starts_with("Role runner (sharding): "), "{line}");
    assert!(line.contains("shard 1 of 4"), "{line}");
}

/// The load-bearing case: a *misconfigured* shard is still configured, so
/// it must render. Silence here is precisely how the pre-#6374 mitigation
/// became invisible.
#[test]
fn header_renders_a_misconfiguration_rather_than_swallowing_it() {
    let report = DaemonStatusReport {
        role_runner_shard: Some(posture(
            true,
            "off (REFUSED: shardIndex=1 is declared in the workspace's TRACKED \
             .loom/config.json) — this host runs role ticks for EVERY registered workspace",
        )),
        ..sample_report()
    };
    let line = render_role_runner_shard_header_line(&report).expect("configured => rendered");
    assert!(line.contains("REFUSED"), "{line}");
    assert!(
        line.contains("EVERY registered workspace"),
        "the fallback direction must be readable off the status line: {line}"
    );
}

// ---- per-root line ----

#[test]
fn repo_line_absent_when_the_daemon_reported_no_verdict() {
    assert!(render_role_runner_shard_repo_line(&repo_with_shard(None)).is_none());
}

/// An unsharded host owns everything, so the line would be pure noise on
/// every row — `owning_shard`/`shard_count` are `None` there.
#[test]
fn repo_line_absent_on_an_unsharded_host() {
    let unsharded = RoleRunnerShardStatus {
        owned_here: true,
        key: "rjwalters/loom".to_string(),
        key_source: "git-remote".to_string(),
        owning_shard: None,
        host_shard: None,
        shard_count: None,
    };
    assert!(render_role_runner_shard_repo_line(&repo_with_shard(Some(unsharded))).is_none());
}

#[test]
fn repo_line_names_this_host_as_the_owner_when_it_owns_the_slice() {
    let line = render_role_runner_shard_repo_line(&repo_with_shard(Some(shard_status(true))))
        .expect("sharded => rendered");
    assert!(line.contains("role slice OWNED by this host"), "{line}");
    assert!(line.contains("shard 2/4"), "{line}");
}

/// Without this line, a workspace whose slice belongs to a peer is
/// indistinguishable in `status` from one silently getting zero ticks
/// because of a bug — so it must name the owning shard, the env var that
/// selects it, and the key (plus its tier) an operator would diff between
/// two hosts.
#[test]
fn repo_line_names_the_peer_shard_and_the_key_when_it_does_not() {
    let line = render_role_runner_shard_repo_line(&repo_with_shard(Some(shard_status(false))))
        .expect("sharded => rendered");
    assert!(line.contains("NOT this host"), "{line}");
    assert!(line.contains("shard 2 of 4"), "{line}");
    assert!(line.contains("LOOM_ROLE_RUNNER_SHARD_INDEX"), "{line}");
    assert!(line.contains("rjwalters/loom"), "must name the key: {line}");
    assert!(line.contains("git-remote"), "must name the key's tier: {line}");
}
