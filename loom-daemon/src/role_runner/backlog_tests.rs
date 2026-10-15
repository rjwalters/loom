//! #11819: the Curator's backlog level at dispatch.

use std::cell::Cell;
use std::path::Path;

use log::Level;
use serial_test::serial;
use tempfile::tempdir;

use super::*;
use crate::config_resolver::{LEGACY_CONFIG_REL, PRIVATE_DEFAULTS_ENV};
use crate::observability::ops::capture::capture;
use crate::telemetry::TelemetryRecord;

fn with_target(root: &Path, target: u64) {
    let path = root.join(LEGACY_CONFIG_REL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!(r#"{{"curator": {{"backlogTarget": {target}}}}}"#)).unwrap();
}

/// Run `f` with the machine-defaults tier disabled, so only `root`'s files count.
fn no_defaults<T>(f: impl FnOnce() -> T) -> T {
    std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
    let out = f();
    std::env::remove_var(PRIVATE_DEFAULTS_ENV);
    out
}

#[test]
#[serial(loom_config_env)]
fn the_level_follows_the_ratio_at_each_boundary() {
    let dir = tempdir().unwrap();
    with_target(dir.path(), 100);
    for (open, level) in [
        (49, BacklogLevel::Lenient),
        (50, BacklogLevel::Normal),
        (100, BacklogLevel::Normal),
        (101, BacklogLevel::Strict),
    ] {
        let got = no_defaults(|| resolve_with(dir.path(), CURATOR_ROLE, |_| Ok(open))).unwrap();
        assert_eq!(got.level, level, "open={open}");
        assert_eq!(got.open, Some(open));
        assert_eq!(got.target, 100);
    }
}

#[test]
#[serial(loom_config_env)]
fn no_target_means_no_flags_and_no_count() {
    let dir = tempdir().unwrap();
    let counted = Cell::new(false);
    let got = no_defaults(|| {
        resolve_with(dir.path(), CURATOR_ROLE, |_| {
            counted.set(true);
            Ok(1)
        })
    });
    assert_eq!(got, None);
    assert!(!counted.get(), "no forge call without a target");
}

#[test]
#[serial(loom_config_env)]
fn a_zero_target_is_unset() {
    let dir = tempdir().unwrap();
    with_target(dir.path(), 0);
    assert_eq!(no_defaults(|| resolve_with(dir.path(), CURATOR_ROLE, |_| Ok(5))), None);
}

#[test]
#[serial(loom_config_env)]
fn other_roles_never_resolve_a_level() {
    let dir = tempdir().unwrap();
    with_target(dir.path(), 10);
    for role in ["builder", "judge", "architect", "champion", "doctor"] {
        assert_eq!(no_defaults(|| resolve_with(dir.path(), role, |_| Ok(500))), None, "{role}");
    }
}

#[test]
#[serial(loom_config_env)]
fn a_failed_count_passes_normal_and_warns() {
    let dir = tempdir().unwrap();
    with_target(dir.path(), 140);
    let mut got = None;
    let logs = crate::test_log_capture::capture_logs(|| {
        got = no_defaults(|| resolve_with(dir.path(), CURATOR_ROLE, |_| Err("boom".into())));
    });
    let got = got.unwrap();
    assert_eq!(got.level, BacklogLevel::Normal);
    assert_eq!(got.open, None);
    assert_eq!(got.count_error.as_deref(), Some("boom"));
    assert!(
        logs.iter()
            .any(|(level, msg)| *level == Level::Warn && msg.contains("backlog count")),
        "{logs:?}"
    );
    assert_eq!(got.args(), " --backlog-level normal --backlog-target 140");
}

#[test]
fn args_carry_level_open_and_target() {
    let d = BacklogDispatch::from_count(140, Ok(412));
    assert_eq!(d.args(), " --backlog-level strict --backlog-open 412 --backlog-target 140");
}

/// A runner double: records the prompt it was given and reports `trace` as
/// the launched run's identity (`None` = the tick never launched).
struct FakeRunner {
    prompt: Option<String>,
    trace: Option<crate::observability::lifecycle::RoleTrace>,
}

impl crate::role_runner::RoleInvocationRunner for FakeRunner {
    fn invoke(&mut self, _role: &str, prompt: &str) -> crate::role_runner::RoleTickOutcome {
        self.prompt = Some(prompt.to_string());
        crate::role_runner::RoleTickOutcome::Success
    }
    fn trace_context(&self) -> Option<crate::observability::lifecycle::RoleTrace> {
        self.trace.clone()
    }
}

fn launched(execution: &str) -> crate::observability::lifecycle::RoleTrace {
    crate::observability::lifecycle::RoleTrace {
        context: crate::telemetry::trace::TraceContext {
            trace_id: crate::telemetry::trace::TraceContext::root(true).trace_id,
            span_id: crate::telemetry::trace::TraceContext::root(true).span_id,
            flags: 1,
        },
        execution: execution.to_string(),
        started_at: chrono::Utc::now(),
        failure: None,
        observed: Default::default(),
    }
}

#[test]
#[serial(loom_config_env)]
fn without_a_target_the_prompt_is_byte_identical_and_nothing_is_recorded() {
    let dir = tempdir().unwrap();
    let mut runner = FakeRunner {
        prompt: None,
        trace: Some(launched("role-curator-x")),
    };
    let (_, captured) = capture(|| {
        no_defaults(|| {
            invoke_with_backlog(&mut runner, dir.path(), CURATOR_ROLE, "/loom:curator", |_| {
                panic!("no target means no count")
            })
        })
    });
    assert_eq!(runner.prompt.as_deref(), Some("/loom:curator"));
    assert!(captured.records.is_empty());
}

#[test]
#[serial(loom_config_env)]
fn the_record_carries_the_launched_runs_id() {
    let dir = tempdir().unwrap();
    with_target(dir.path(), 140);
    let mut runner = FakeRunner {
        prompt: None,
        trace: Some(launched("role-curator-launched")),
    };
    let (_, captured) = capture(|| {
        no_defaults(|| {
            invoke_with_backlog(&mut runner, dir.path(), CURATOR_ROLE, "/loom:curator", |_| Ok(412))
        })
    });
    assert_eq!(
        runner.prompt.as_deref(),
        Some("/loom:curator --backlog-level strict --backlog-open 412 --backlog-target 140")
    );
    let ids: Vec<_> = captured
        .records
        .iter()
        .filter_map(|r| match r {
            TelemetryRecord::BacklogLevel(b) => Some(b.dispatch_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["role-curator-launched"]);
}

#[test]
#[serial(loom_config_env)]
fn a_tick_that_never_launches_writes_no_record_but_still_gets_the_flags() {
    let dir = tempdir().unwrap();
    with_target(dir.path(), 140);
    let mut runner = FakeRunner {
        prompt: None,
        trace: None,
    };
    let (_, captured) = capture(|| {
        no_defaults(|| {
            invoke_with_backlog(&mut runner, dir.path(), CURATOR_ROLE, "/loom:curator", |_| Ok(412))
        })
    });
    assert!(runner.prompt.unwrap().contains("--backlog-level strict"));
    assert!(captured.records.is_empty());
}

#[test]
fn the_record_names_repo_level_and_dispatch() {
    let at = chrono::Utc::now();
    let id = "role-curator-2026-10-13T00:00:00.000000001Z".to_string();
    let r = BacklogDispatch::from_count(140, Ok(412)).record("RJWalters/Loom", id.clone(), at);
    assert_eq!(r.repo, "rjwalters/loom");
    assert_eq!(r.open, Some(412));
    assert_eq!(r.target, 140);
    assert_eq!(r.level, BacklogLevel::Strict);
    assert_eq!(r.dispatch_id, id);
    assert_eq!(r.observed_at, at);
    // Round-trips as the registered kind.
    let record = TelemetryRecord::BacklogLevel(r);
    assert_eq!(record.kind(), "backlog.level");
}

#[test]
fn count_path_and_total_count() {
    assert_eq!(
        count_path("rjwalters/loom"),
        "search/issues?q=repo%3Arjwalters%2Floom%20is%3Aissue%20is%3Aopen&per_page=1"
    );
    assert_eq!(parse_total_count(r#"{"total_count": 213, "items": []}"#), Ok(213));
    assert!(parse_total_count(r#"{"items": []}"#).is_err());
    assert!(parse_total_count("not json").is_err());
}
