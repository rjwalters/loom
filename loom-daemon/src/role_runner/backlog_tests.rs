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

#[test]
#[serial(loom_config_env)]
fn without_a_target_the_prompt_is_byte_identical_and_nothing_is_recorded() {
    let dir = tempdir().unwrap();
    let (prompt, captured) =
        capture(|| no_defaults(|| with_backlog_args(dir.path(), CURATOR_ROLE, "/loom:curator")));
    assert_eq!(prompt, "/loom:curator");
    assert!(captured.records.is_empty());
}

#[test]
fn the_record_names_repo_level_and_dispatch() {
    let at = chrono::Utc::now();
    let r = BacklogDispatch::from_count(140, Ok(412)).record("RJWalters/Loom", at);
    assert_eq!(r.repo, "rjwalters/loom");
    assert_eq!(r.open, Some(412));
    assert_eq!(r.target, 140);
    assert_eq!(r.level, BacklogLevel::Strict);
    assert!(r.dispatch_id.starts_with("role-curator-"), "{}", r.dispatch_id);
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
