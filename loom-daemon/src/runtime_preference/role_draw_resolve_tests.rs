//! The weighted role draw (#11726) through the real resolver: config on disk,
//! shipped role/runtime manifests, real availability.
//!
//! The regression the issue names (2026-10-12): Judge, Curator and Champion drew Codex, every tool
//! call was hook-denied, the tick exited 0 and the Claude entry never fired.
//! Here a launched attempt that fails is recorded exactly as
//! `role_runner::launch::hold_feed` records it, and the next role tick must
//! draw from the attempt-2 table and land on Claude.
use super::role_draw::{self, attempts};
use super::tests::{fixture, provision_claude_pool, write_config, ClearedRuntimeEnv};
use super::{
    resolve_for_dispatch, resolve_runtime_for, Decision, DispatchContext, Intent, PreferenceSource,
};
use serde_json::json;
use std::path::Path;

/// A bare-metal Codex account under the guard's isolated profile root.
fn provision_codex_account(name: &str) {
    let root = std::env::var_os("LOOM_CODEX_PROFILE_ROOT").expect("ClearedRuntimeEnv sets it");
    std::fs::create_dir_all(Path::new(&root).join(name)).unwrap();
}

struct DrawEnv {
    _cleared: ClearedRuntimeEnv,
}

impl DrawEnv {
    fn new() -> Self {
        let cleared = ClearedRuntimeEnv::new();
        attempts::reset_for_tests();
        Self { _cleared: cleared }
    }
}

impl Drop for DrawEnv {
    fn drop(&mut self) {
        attempts::reset_for_tests();
    }
}

fn role_tick() -> DispatchContext {
    DispatchContext {
        intent: Intent::Probe,
        role_tick: true,
    }
}

/// `(runtime, draw record)` of one curator role-tick resolution.
fn curator_tick(root: &Path) -> (Option<String>, role_draw::RoleDrawRecord) {
    let Decision::Preference {
        source,
        resolution,
        draw,
        ..
    } = resolve_runtime_for(root, "curator", None, 1_800_000_000, role_tick()).unwrap()
    else {
        panic!("expected the draw to decide");
    };
    assert_eq!(source, PreferenceSource::RoleDraw);
    let runtime = resolution.chosen.map(|c| c.admitted.runtime);
    (runtime, *draw.expect("a draw record"))
}

fn workspace(runtimes: &serde_json::Value) -> tempfile::TempDir {
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    provision_codex_account("bare-metal");
    write_config(dir.path(), &json!({ "runtimes": runtimes }));
    dir
}

#[test]
#[serial_test::serial]
fn a_failed_codex_attempt_is_followed_by_the_attempt_two_table() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({
        // The ordered list the incident ran on; the draw must replace it.
        "rolePreference": {"curator": ["codex", "claude"]},
        "roleDraw": {"curator": [
            {"attempt": 1, "draw": {"codex": 100}},
            {"attempt": 2, "draw": {"claude/fable": 100}},
        ]},
    }));
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("codex"), "{draw:?}");
    assert_eq!((draw.attempt, draw.table_attempt, draw.pick.as_deref()), (1, 1, Some("codex")));
    // The tick launched, every tool call was denied: a failed attempt.
    attempts::record_failure(dir.path(), "curator");
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("claude"));
    assert_eq!((draw.attempt, draw.table_attempt), (2, 2));
    assert_eq!(
        (draw.pick.as_deref(), draw.model.as_deref()),
        (Some("claude/fable"), Some("fable"))
    );
    // A success puts the next tick back on attempt 1.
    attempts::record_success(dir.path(), "curator");
    assert_eq!(curator_tick(dir.path()).1.attempt, 1);
}

#[test]
#[serial_test::serial]
fn a_sweep_dispatch_always_draws_attempt_one_and_carries_the_drawn_model() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"curator": {
        "1": {"claude/opus": 100}, "2": {"claude/fable": 100}}}}));
    attempts::record_failure(dir.path(), "curator");
    let admitted = resolve_for_dispatch(dir.path(), "curator", None)
        .unwrap()
        .admitted
        .unwrap();
    let stamp = admitted.preference.unwrap();
    assert_eq!(stamp.model.as_deref(), Some("opus"));
    let draw = stamp.draw.unwrap();
    assert_eq!(draw.attempt, 1);
    assert!(stamp.marker.contains("source=role-draw"), "{}", stamp.marker);
    assert!(stamp.marker.contains("pick:claude/opus"), "{}", stamp.marker);
}

#[test]
#[serial_test::serial]
fn an_unavailable_pick_is_passed_over_within_the_same_table() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"curator": [
        {"attempt": 1, "draw": {"codex": 1000, "claude": 1}}]}}));
    // No Claude pool: whatever the roll, only Codex can serve.
    provision_claude_pool(dir.path(), 0);
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("codex"));
    assert_eq!(draw.pick.as_deref(), Some("codex"));
    // And the other way round: Codex excluded on this host, Claude wins.
    let local = dir.path().join(".loom-local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("local.json"),
        json!({"runtimes": {"excludeProviders": ["codex"]}}).to_string(),
    )
    .unwrap();
    provision_claude_pool(dir.path(), 2);
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("claude"));
    assert_eq!(draw.table.len(), 1);
    assert!(draw.off[0].starts_with("codex: excluded"), "{:?}", draw.off);
}

#[test]
#[serial_test::serial]
fn a_table_empty_after_exclusions_fails_closed_with_a_diagnostic() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({
        "excludeProviders": ["codex"],
        "roleDraw": {"curator": [{"attempt": 1, "draw": {"codex": 100}}]},
    }));
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime, None);
    assert!(draw.table.is_empty());
    let error = resolve_for_dispatch(dir.path(), "curator", None).unwrap_err();
    assert!(error.reason.contains("EMPTY after exclusions"), "{}", error.reason);
}

#[test]
#[serial_test::serial]
fn without_a_role_draw_table_role_preference_decides_unchanged() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({
        "rolePreference": {"curator": ["codex", "claude"]},
        "roleDraw": {"guide": [{"attempt": 1, "draw": {"claude": 1}}]},
    }));
    let Decision::Preference {
        source,
        resolution,
        draw,
        ..
    } = resolve_runtime_for(dir.path(), "curator", None, 0, role_tick()).unwrap()
    else {
        panic!("expected the preference list");
    };
    assert_eq!(source, PreferenceSource::RolePreference);
    assert!(draw.is_none());
    assert_eq!(resolution.chosen.unwrap().admitted.runtime, "codex");
    // A role whose only table starts at attempt 2 keeps its list on attempt 1.
    write_config(
        dir.path(),
        &json!({"runtimes": {"rolePreference": {"curator": ["claude"]},
            "roleDraw": {"curator": {"2": {"codex": 1}}}}}),
    );
    let decision = resolve_runtime_for(dir.path(), "curator", None, 0, role_tick()).unwrap();
    assert!(matches!(
        decision,
        Decision::Preference {
            source: PreferenceSource::RolePreference,
            ..
        }
    ));
}

#[test]
#[serial_test::serial]
fn an_operator_pin_still_wins_over_a_draw() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"curator": {"1": {"codex": 1}}}}));
    std::env::set_var("LOOM_RUNTIME_CURATOR", "claude");
    let decision = resolve_runtime_for(dir.path(), "curator", None, 0, role_tick());
    std::env::remove_var("LOOM_RUNTIME_CURATOR");
    assert!(matches!(decision.unwrap(), Decision::Static { .. }));
}
