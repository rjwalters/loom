//! #6470: the idle-edge diagnostics used to blame each root's own
//! `autonomous.roleRunner.enabled` (the #4377 message) even when the
//! host-wide `LOOM_ROLE_RUNNER` env override was the true cause. These
//! tests pin the message-selection logic for both the host-level header
//! line and the per-root diagnostic line.
use super::{render_role_runner_disabled_line, render_role_runner_host_header_line};
use crate::cli::status::sample_report::sample_report;
use loom_daemon::types::{DaemonStatusReport, RepoStatus};

/// `pub(super)` so the sibling #6374 shard-render tests can build a
/// baseline [`RepoStatus`] from the same place rather than duplicating
/// the (large) struct literal.
pub(super) fn repo(enabled: bool, on_idle: &[&str], env_override: Option<bool>) -> RepoStatus {
    RepoStatus {
        root: std::path::PathBuf::from("/repos/loom"),
        maintain_only: None,
        weight: 100,
        in_flight_count: 0,
        health_gate_halted: false,
        quarantined_issues: vec![],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: None,
        health_gate_verdict_at: None,
        root_missing: false,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: None,
        role_runner_enabled: enabled,
        role_runner_roles: vec![],
        role_runner_intervals: std::collections::BTreeMap::new(),
        role_runner_on_idle_roles: on_idle.iter().map(|s| (*s).to_string()).collect(),
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: env_override,
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

// ---- host-level header line ----

#[test]
fn host_header_names_env_off() {
    let report = DaemonStatusReport {
        role_runner_host_env_override: Some(false),
        ..sample_report()
    };
    let line = render_role_runner_host_header_line(&report);
    assert!(line.contains("Role runner (host): OFF"), "{line}");
    assert!(line.contains("LOOM_ROLE_RUNNER"), "must name the env var: {line}");
}

#[test]
fn host_header_names_env_on() {
    let report = DaemonStatusReport {
        role_runner_host_env_override: Some(true),
        ..sample_report()
    };
    let line = render_role_runner_host_header_line(&report);
    assert!(line.contains("Role runner (host): ON"), "{line}");
    assert!(line.contains("LOOM_ROLE_RUNNER"), "must name the env var: {line}");
}

#[test]
fn host_header_summarizes_per_root_when_no_env_override() {
    let report = DaemonStatusReport {
        role_runner_host_env_override: None,
        per_repo: vec![repo(true, &[], None), repo(false, &[], None)],
        ..sample_report()
    };
    let line = render_role_runner_host_header_line(&report);
    assert!(line.contains("no env override"), "{line}");
    assert!(line.contains("1 of 2"), "must count enabled roots: {line}");
}

// ---- per-root diagnostic line ----

#[test]
fn per_root_line_absent_when_enabled() {
    assert!(render_role_runner_disabled_line(&repo(true, &["champion"], None)).is_none());
}

#[test]
fn per_root_line_absent_when_no_on_idle_roles() {
    assert!(render_role_runner_disabled_line(&repo(false, &[], None)).is_none());
}

/// Master env off / root config on (this issue's motivating case, #4377
/// text applied to the wrong tier): must name the env override, not
/// point the reader at editing this root's own config.
#[test]
fn per_root_line_names_env_override_when_env_is_the_cause() {
    let r = repo(false, &["auditor"], Some(false));
    let line = render_role_runner_disabled_line(&r).expect("must warn");
    assert!(line.contains("host-wide env override"), "{line}");
    assert!(line.contains("LOOM_ROLE_RUNNER"), "{line}");
    assert!(
        !line.contains("autonomous.roleRunner.enabled=true is set in this root's own"),
        "must NOT tell the reader to edit this root's config when env overrode it: {line}"
    );
}

/// Master on (or unset) / root config off: the existing #4377 message,
/// unchanged.
#[test]
fn per_root_line_keeps_the_original_4377_message_when_config_is_the_cause() {
    let r = repo(false, &["auditor"], None);
    let line = render_role_runner_disabled_line(&r).expect("must warn");
    assert!(
        line.contains("autonomous.roleRunner.enabled=true is set in this root's own"),
        "{line}"
    );
    assert!(line.contains("#4377"), "{line}");
    assert!(!line.contains("LOOM_ROLE_RUNNER"), "must not blame the env var here: {line}");
}

/// Both on: no message from either surface.
#[test]
fn no_message_when_both_env_and_config_are_on() {
    // Enabled overall (whichever tier decided), so the diagnostic must
    // not fire regardless of `role_runner_env_override`'s value.
    assert!(render_role_runner_disabled_line(&repo(true, &["auditor"], Some(true))).is_none());
    let report = DaemonStatusReport {
        role_runner_host_env_override: Some(true),
        per_repo: vec![repo(true, &["auditor"], Some(true))],
        ..sample_report()
    };
    let line = render_role_runner_host_header_line(&report);
    assert!(line.contains("Role runner (host): ON"), "{line}");
}
