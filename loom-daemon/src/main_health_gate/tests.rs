use super::*;
use serial_test::serial;
use std::collections::VecDeque;

/// Every [`UnevaluatedClass`], so the "must not halt" and label-uniqueness
/// invariants are checked exhaustively as classes are added.
const ALL_UNEVALUATED_CLASSES: [UnevaluatedClass; 10] = [
    UnevaluatedClass::DirtyTree,
    UnevaluatedClass::NotOnMain,
    UnevaluatedClass::LocalAhead,
    UnevaluatedClass::GitFailure,
    UnevaluatedClass::Timeout,
    UnevaluatedClass::NotExecutable,
    UnevaluatedClass::KilledBySignal,
    UnevaluatedClass::SpawnFailure,
    UnevaluatedClass::ContradictedByForgeCi,
    UnevaluatedClass::ForgeCredentialStale,
];

// ===================================================================
// Credential-freshness isolation (#6663)
//
// `CommandGateRunner::new` and `run_gate_tick`/`run_gate_tick_with_load_fn`
// resolve credential freshness from the PROCESS-GLOBAL streak tracker in
// `credential_preflight`. That tracker outlives every individual test in
// this binary, so a single sibling test that exercises a production path
// which records a refresh failure marks the whole process "stale" for the
// 1800s grace window — and every gate test that reaches the global then
// short-circuits to `ForgeCredentialStale` instead of producing the verdict
// it asserts. That RED-ed 11 tests here, nondeterministically, purely as a
// function of test scheduling order.
//
// The rule for this module: **no test reads the global tracker**, with
// exactly one deliberate, `#[serial]`, self-resetting exception
// (`test_global_credential_tracker_holds_a_real_tick_and_releases_it`)
// which owns the production wiring's coverage. Everything else pins its
// own answer:
//
// - runner-level  -> `gate_runner(cfg, root)` (or an explicit
//   `.with_credential_freshness(...)` for the stale-path tests)
// - tick-level    -> `run_gate_tick_with_fns(.., || false)`, never
//   `run_gate_tick_with_load_fn`
// ===================================================================

/// A [`CredentialFreshness`] with a fixed answer.
struct FixedCredential(bool);
impl CredentialFreshness for FixedCredential {
    fn is_stale(&self) -> bool {
        self.0
    }
}

/// Build a [`CommandGateRunner`] whose credential-freshness source is a
/// pinned "fresh" answer rather than the process global (#6663). Every
/// gate-runner test constructs through this; the handful that need a
/// *stale* credential chain their own `.with_credential_freshness(...)`,
/// which overrides this default.
fn gate_runner(config: BuildGateConfig, repo_root: PathBuf) -> CommandGateRunner {
    CommandGateRunner::new(config, repo_root)
        .with_credential_freshness(Box::new(FixedCredential(false)))
}

fn write_config(dir: &Path, body: &str) {
    let loom_dir = dir.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    std::fs::write(loom_dir.join("config.json"), body).unwrap();
}

fn write_project_config(dir: &Path, body: &str) {
    let full = dir.join(crate::config_resolver::PROJECT_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

// ===================================================================
// Capturing logger (#4083) — a tiny `log::Log` impl so the green-path
// severity can be asserted and cannot silently regress to `debug!`.
//
// The implementation moved to `crate::test_log_capture` (#4641) because
// `log::set_boxed_logger` succeeds only ONCE per process and every
// `#[cfg(test)]` module compiles into the same test binary: a second
// per-module logger would silently lose every record for whichever module
// lost the race. This alias keeps the `capture::capture_logs(...)` call
// sites below unchanged.
// ===================================================================
use crate::test_log_capture as capture;

// ===================================================================
// Green-path log severity (#4083)
// ===================================================================

/// AC5 — the operator-visible fix. A green run from a non-halted state must
/// emit exactly one `info!` line via the production (root-aware) renderer,
/// naming the workspace and the elapsed seconds. If the level regresses to
/// `debug!`, the `Level::Info` assertion fails.
#[test]
fn test_remained_healthy_logs_at_info_via_root_renderer() {
    let root = Path::new("/repo/alpha");
    let state = MainHealthState::new();
    let outcome = GateOutcome::Green {
        elapsed: Duration::from_secs(726),
    };

    let records = capture::capture_logs(|| {
        apply_and_log(&state, &outcome, |t, o, h| log_transition_for_root(root, t, o, h));
    });

    let green: Vec<_> = records
        .iter()
        .filter(|(_, msg)| msg.contains("GREEN in"))
        .collect();
    assert_eq!(green.len(), 1, "exactly one green line expected, got {records:?}");
    let (level, msg) = green[0];
    assert_eq!(*level, log::Level::Info, "green path must log at INFO, not debug");
    assert!(msg.contains("/repo/alpha"), "line must name the workspace: {msg}");
    assert!(msg.contains("726s"), "line must carry the elapsed seconds: {msg}");
    assert!(msg.contains("dispatch unaffected"), "unexpected wording: {msg}");
}

/// AC4 — the single-workspace renderer (no production caller, but kept for
/// API/test parity) must also carry the upgraded severity. It omits the
/// workspace name (there is no root in scope) but still logs at INFO with
/// the elapsed time.
#[test]
fn test_remained_healthy_logs_at_info_via_plain_renderer() {
    let state = MainHealthState::new();
    let outcome = GateOutcome::Green {
        elapsed: Duration::from_secs(42),
    };

    let records = capture::capture_logs(|| {
        apply_and_log(&state, &outcome, log_transition);
    });

    let green: Vec<_> = records
        .iter()
        .filter(|(_, msg)| msg.contains("GREEN in"))
        .collect();
    assert_eq!(green.len(), 1, "exactly one green line expected, got {records:?}");
    let (level, msg) = green[0];
    assert_eq!(*level, log::Level::Info, "green path must log at INFO, not debug");
    assert!(msg.contains("42s"), "line must carry the elapsed seconds: {msg}");
}

/// A green run that *exits a halt* must stay `Recovered` (its own distinct,
/// greppable wording) and must NOT also emit the `RemainedHealthy` "GREEN
/// in Ns" line — the two remain distinguishable.
#[test]
fn test_recovered_is_distinct_from_remained_healthy() {
    let root = Path::new("/repo/beta");
    let state = MainHealthState::new();
    // Prime a halt so the next green is a recovery.
    let _ = apply_gate_outcome(&state, &GateOutcome::red("boom"));

    let records = capture::capture_logs(|| {
        apply_and_log(
            &state,
            &GateOutcome::Green {
                elapsed: Duration::from_secs(5),
            },
            |t, o, h| log_transition_for_root(root, t, o, h),
        );
    });

    assert!(
        records
            .iter()
            .any(|(l, m)| *l == log::Level::Info && m.contains("GREEN again")),
        "recovery must log the distinct 'GREEN again' line: {records:?}"
    );
    assert!(
        !records.iter().any(|(_, m)| m.contains("GREEN in")),
        "a recovery must not also emit the RemainedHealthy 'GREEN in Ns' line: {records:?}"
    );
}

/// Elapsed rendering: a sub-second run must not render as a misleading
/// `0s` (which reads as "the gate never ran") — it renders in ms instead.
#[test]
fn test_format_elapsed_rendering() {
    assert_eq!(format_elapsed(Duration::from_secs(726)), "726s");
    assert_eq!(format_elapsed(Duration::from_secs(1)), "1s");
    // Sub-second → milliseconds, never "0s".
    assert_eq!(format_elapsed(Duration::from_millis(320)), "320ms");
    assert_eq!(format_elapsed(Duration::from_millis(999)), "999ms");
    assert_eq!(format_elapsed(Duration::ZERO), "0ms");
    assert_ne!(format_elapsed(Duration::from_millis(500)), "0s");
}

// ===================================================================
// Config soft-fail
// ===================================================================

#[test]
fn test_config_missing_file_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(read_build_gate_config(tmp.path()), None);
}

#[test]
fn test_config_malformed_json_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "{not valid json");
    assert_eq!(read_build_gate_config(tmp.path()), None);
}

#[test]
fn test_config_missing_build_gate_key_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"terminals": []}"#);
    assert_eq!(read_build_gate_config(tmp.path()), None);
}

#[test]
fn test_config_enabled_false_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"buildGate": {"enabled": false, "command": "true"}}"#);
    assert_eq!(read_build_gate_config(tmp.path()), None);
}

#[test]
fn test_config_enabled_missing_command_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"buildGate": {"enabled": true}}"#);
    assert_eq!(read_build_gate_config(tmp.path()), None);
}

#[test]
fn test_config_enabled_empty_command_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"buildGate": {"enabled": true, "command": "   "}}"#);
    assert_eq!(read_build_gate_config(tmp.path()), None);
}

#[test]
fn test_config_valid_uses_default_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "bash .loom/scripts/build-gate.sh"}}"#,
    );
    let cfg = read_build_gate_config(tmp.path()).unwrap();
    assert_eq!(cfg.command, "bash .loom/scripts/build-gate.sh");
    assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_BUILD_GATE_TIMEOUT_SECS));
}

#[test]
fn test_config_valid_honors_timeout_seconds() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "true", "timeoutSeconds": 42}}"#,
    );
    let cfg = read_build_gate_config(tmp.path()).unwrap();
    assert_eq!(cfg.timeout, Duration::from_secs(42));
}

#[test]
fn test_config_zero_timeout_seconds_falls_back_to_default() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "true", "timeoutSeconds": 0}}"#,
    );
    let cfg = read_build_gate_config(tmp.path()).unwrap();
    assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_BUILD_GATE_TIMEOUT_SECS));
}

// ===================================================================
// config_resolver migration (#4058) — buildGate tier precedence
// ===================================================================

#[test]
#[serial(loom_config_env)]
fn test_build_gate_project_tier_only_is_honored_like_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(tmp.path(), r#"{"buildGate": {"enabled": true, "command": "true"}}"#);
    let cfg = read_build_gate_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.unwrap().command, "true");
}

#[test]
#[serial(loom_config_env)]
fn test_build_gate_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "legacy-cmd", "timeoutSeconds": 99}}"#,
    );
    write_project_config(tmp.path(), r#"{"buildGate": {"command": "project-cmd"}}"#);
    let cfg = read_build_gate_config(tmp.path()).unwrap();
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    // Overlapping `command` -> project tier wins.
    assert_eq!(cfg.command, "project-cmd");
    // Non-overlapping `timeoutSeconds` -> legacy tier still supplies it.
    assert_eq!(cfg.timeout, Duration::from_secs(99));
}

// ===================================================================
// config_resolver migration (#4058) — autonomous.mainHealthGate tier
// precedence
// ===================================================================

#[test]
#[serial(loom_config_env)]
fn test_autonomous_gate_project_tier_only_is_honored_like_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
    let cfg = read_autonomous_gate_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        cfg,
        AutonomousGateConfig {
            enabled: Some(true),
            ci_workflow: None,
            suppress_dispatch_during_gate: None,
        }
    );
}

#[test]
#[serial(loom_config_env)]
fn test_autonomous_gate_local_tier_overrides_legacy_and_project() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": false}}}"#);
    write_project_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": false}}}"#);
    let local_full = tmp.path().join(crate::config_resolver::LOCAL_CONFIG_REL);
    std::fs::create_dir_all(local_full.parent().unwrap()).unwrap();
    std::fs::write(&local_full, r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#)
        .unwrap();

    let cfg = read_autonomous_gate_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.enabled, Some(true));
}

// ===================================================================
// Halt-state transitions (the reactive core)
// ===================================================================

#[test]
fn test_default_state_not_halted() {
    assert!(!MainHealthState::new().is_halted());
    assert!(!MainHealthState::default().is_halted());
}

// ===================================================================
// Gate-in-flight suppressor (#4084)
// ===================================================================

#[test]
fn test_gate_in_flight_false_at_construction() {
    // A fresh state must not suppress dispatch: no gate run has started.
    assert!(!MainHealthState::new().is_gate_in_flight());
    assert!(!MainHealthState::default().is_gate_in_flight());
}

#[test]
fn test_gate_in_flight_guard_sets_true_during_and_clears_after() {
    let state = Arc::new(MainHealthState::new());
    assert!(!state.is_gate_in_flight());
    {
        let _guard = GateInFlightGuard::new(state.clone());
        // True for the guard's whole lifetime (the blocking gate run).
        assert!(state.is_gate_in_flight(), "gate run in flight ⇒ flag set");
    }
    // Cleared the instant the guard drops (the run returned).
    assert!(!state.is_gate_in_flight(), "flag cleared once the run completes");
}

#[test]
fn test_gate_in_flight_guard_clears_on_panic() {
    // The highest-risk regression (#4084): a panicking gate run must still
    // clear the flag, or the latch permanently starves dispatch. The guard
    // lives inside the `spawn_blocking` closure, so its `Drop` runs during
    // the panic unwind — simulate that with `catch_unwind`.
    let state = Arc::new(MainHealthState::new());
    let state_for_closure = state.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = GateInFlightGuard::new(state_for_closure);
        assert!(state.is_gate_in_flight());
        panic!("simulated gate-run panic");
    }));
    assert!(result.is_err(), "the closure must have panicked");
    assert!(
        !state.is_gate_in_flight(),
        "a panicking gate run must not latch gate_in_flight on"
    );
}

#[test]
fn test_workspace_health_states_gate_in_flight_passthrough() {
    let states = WorkspaceHealthStates::new();
    let root = Path::new("/tmp/repo-a");
    // A never-seen root reports no gate in flight (nothing has run).
    assert!(!states.is_gate_in_flight(root));
    // Setting the underlying state's flag is observable through the map.
    states.get_or_create(root).set_gate_in_flight(true);
    assert!(states.is_gate_in_flight(root));
    // A sibling root is unaffected — per-root isolation (#3930).
    let sibling = Path::new("/tmp/repo-b");
    assert!(!states.is_gate_in_flight(sibling));
    states.get_or_create(root).set_gate_in_flight(false);
    assert!(!states.is_gate_in_flight(root));
}

// ===================================================================
// Tiered gate + load-aware deferral (#4259)
// ===================================================================

#[test]
fn decide_gate_tier_runs_full_below_threshold() {
    // Load below the saturation threshold ⇒ full tier, regardless of any
    // (nonexistent) defer streak.
    assert_eq!(
        decide_gate_tier(Some(false), None, Duration::from_secs(1800)),
        GateTierDecision::Full
    );
}

#[test]
fn decide_gate_tier_missing_load_data_runs_full_fail_safe() {
    // No load reading ⇒ run the full tier (fail safe); NEVER defer on absent
    // evidence. Even a long-running (stale) defer streak cannot force a defer
    // here — missing data always runs.
    assert_eq!(decide_gate_tier(None, None, Duration::from_secs(1800)), GateTierDecision::Full);
    assert_eq!(
        decide_gate_tier(None, Some(Duration::from_secs(9999)), Duration::from_secs(1800)),
        GateTierDecision::Full
    );
}

#[test]
fn decide_gate_tier_saturated_defers_within_the_bound() {
    // Saturated, not yet deferring (None) ⇒ defer (start the streak).
    assert_eq!(
        decide_gate_tier(Some(true), None, Duration::from_secs(1800)),
        GateTierDecision::Defer
    );
    // Saturated, deferring but still under the bound ⇒ keep deferring.
    assert_eq!(
        decide_gate_tier(Some(true), Some(Duration::from_secs(600)), Duration::from_secs(1800)),
        GateTierDecision::Defer
    );
}

#[test]
fn decide_gate_tier_saturated_past_the_bound_runs_fast() {
    // Saturated and the defer streak has reached the bound ⇒ FAST tier runs
    // regardless of load (the AC2 guarantee under permanent load).
    assert_eq!(
        decide_gate_tier(Some(true), Some(Duration::from_secs(1800)), Duration::from_secs(1800)),
        GateTierDecision::Fast
    );
    assert_eq!(
        decide_gate_tier(Some(true), Some(Duration::from_secs(3600)), Duration::from_secs(1800)),
        GateTierDecision::Fast
    );
}

#[test]
fn record_gate_deferred_starts_streak_and_is_not_an_evaluation() {
    let state = MainHealthState::new();
    assert!(!state.is_deferred());
    assert_eq!(state.defer_streak_elapsed(), None);

    // First defer starts the streak (returns true → caller logs once).
    assert!(state.record_gate_deferred(1.05, Duration::from_secs(1800)));
    assert!(state.is_deferred());
    assert!(state.defer_streak_elapsed().is_some());
    assert!(state.deferred_since().is_some());
    assert!(state.deferred_summary().is_some());

    // A deferral must NOT be recorded as an evaluation: no SHA memo advance,
    // no indeterminate backoff armed (#4259).
    assert_eq!(state.gate_last_evaluated_sha(), None);
    assert!(!state.gate_backoff_active(Instant::now()));

    // A second defer does NOT restart the streak (returns false → no
    // duplicate log line).
    assert!(!state.record_gate_deferred(1.10, Duration::from_secs(1800)));
    assert!(state.is_deferred());
}

#[test]
fn clear_gate_deferred_ends_the_streak() {
    let state = MainHealthState::new();
    state.record_gate_deferred(1.05, Duration::from_secs(1800));
    assert!(state.is_deferred());
    state.clear_gate_deferred();
    assert!(!state.is_deferred());
    assert_eq!(state.defer_streak_elapsed(), None);
    assert_eq!(state.deferred_summary(), None);
}

#[test]
fn deferred_summary_is_distinct_from_unevaluated_summary() {
    let state = MainHealthState::new();
    // A timeout UNEVALUATED tick populates the not-evaluated summary...
    assert!(state.note_gate_tick(
        Some((UnevaluatedClass::Timeout, "gate command timed out after 1200s")),
        SKIP_WARN_THROTTLE,
    ));
    let uneval = state.unevaluated_summary().unwrap();
    assert!(uneval.contains("timeout"), "got: {uneval}");

    // ...a deferral populates a separate summary that reads about load, not
    // a timeout — the two are never confused on the status surface.
    state.record_gate_deferred(1.05, Duration::from_secs(1800));
    let deferred = state.deferred_summary().unwrap();
    assert!(deferred.contains("load"), "got: {deferred}");
    assert!(!deferred.contains("timeout"), "got: {deferred}");
    assert_ne!(uneval, deferred);
}

#[test]
fn record_gate_tier_tracks_last_verdict_tier() {
    let state = MainHealthState::new();
    assert_eq!(state.gate_last_tier(), None);
    state.record_gate_tier(GateTier::Fast);
    assert_eq!(state.gate_last_tier(), Some(GateTier::Fast));
    state.record_gate_tier(GateTier::Full);
    assert_eq!(state.gate_last_tier(), Some(GateTier::Full));
}

#[test]
fn gate_tier_labels_and_suffixes() {
    assert_eq!(GateTier::Full.label(), "full");
    assert_eq!(GateTier::Fast.label(), "fast");
    // The full tier's suffix is empty so full-tier rendering is unchanged;
    // the fast tier is explicitly marked so it is never mistaken for a
    // full-suite verdict.
    assert_eq!(GateTier::Full.verdict_suffix(), "");
    assert_eq!(GateTier::Fast.verdict_suffix(), " (fast tier)");
}

#[test]
fn workspace_health_states_defer_and_tier_passthrough() {
    let states = WorkspaceHealthStates::new();
    let root = Path::new("/tmp/repo-defer");
    // Never-seen root: not deferring, no summary, no tier.
    assert!(!states.is_deferred(root));
    assert_eq!(states.deferred_summary(root), None);
    assert_eq!(states.gate_last_tier(root), None);

    states
        .get_or_create(root)
        .record_gate_deferred(1.2, Duration::from_secs(1800));
    states.get_or_create(root).record_gate_tier(GateTier::Fast);
    assert!(states.is_deferred(root));
    assert!(states.deferred_summary(root).is_some());
    assert_eq!(states.gate_last_tier(root), Some(GateTier::Fast));

    // Sibling isolation (#3930).
    let sibling = Path::new("/tmp/repo-defer-b");
    assert!(!states.is_deferred(sibling));
    assert_eq!(states.gate_last_tier(sibling), None);
}

#[test]
#[serial]
fn resolve_gate_load_threshold_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var(BUILD_GATE_LOAD_THRESHOLD_ENV);
    // No env, no config key ⇒ default.
    write_config(tmp.path(), r#"{"buildGate": {"enabled": true, "command": "true"}}"#);
    assert!(
        (resolve_gate_load_threshold(tmp.path())
            - crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD)
            .abs()
            < f64::EPSILON
    );
    // Config value honored.
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "true", "loadThreshold": 1.5}}"#,
    );
    assert!((resolve_gate_load_threshold(tmp.path()) - 1.5).abs() < f64::EPSILON);
    // Env wins over config.
    std::env::set_var(BUILD_GATE_LOAD_THRESHOLD_ENV, "0.6");
    assert!((resolve_gate_load_threshold(tmp.path()) - 0.6).abs() < f64::EPSILON);
    std::env::remove_var(BUILD_GATE_LOAD_THRESHOLD_ENV);
}

#[test]
#[serial]
fn resolve_gate_max_defer_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var(BUILD_GATE_MAX_DEFER_ENV);
    write_config(tmp.path(), r#"{"buildGate": {"enabled": true, "command": "true"}}"#);
    assert_eq!(
        resolve_gate_max_defer(tmp.path()),
        Duration::from_secs(DEFAULT_GATE_MAX_DEFER_SECS)
    );
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "true", "maxDeferSeconds": 900}}"#,
    );
    assert_eq!(resolve_gate_max_defer(tmp.path()), Duration::from_secs(900));
    std::env::set_var(BUILD_GATE_MAX_DEFER_ENV, "120");
    assert_eq!(resolve_gate_max_defer(tmp.path()), Duration::from_secs(120));
    std::env::remove_var(BUILD_GATE_MAX_DEFER_ENV);
}

#[test]
fn resolve_gate_fast_command_defaults_to_env_prefixed_base() {
    let tmp = tempfile::tempdir().unwrap();
    // No `fastCommand` ⇒ prefix the base command with the tier env var so
    // the shipped build-gate.sh runs its fast tier.
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "bash .loom/scripts/build-gate.sh"}}"#,
    );
    assert_eq!(
        resolve_gate_fast_command(tmp.path(), "bash .loom/scripts/build-gate.sh"),
        "LOOM_BUILD_GATE_TIER=fast bash .loom/scripts/build-gate.sh"
    );
    // An explicit `fastCommand` overrides the derived form.
    write_config(
        tmp.path(),
        r#"{"buildGate": {"enabled": true, "command": "x", "fastCommand": "cargo build --workspace"}}"#,
    );
    assert_eq!(resolve_gate_fast_command(tmp.path(), "x"), "cargo build --workspace");
}

// ===================================================================
// Verdict timestamp (#4012) — the pending-vs-clear disambiguator
// ===================================================================

#[test]
fn test_fresh_state_has_no_verdict_yet() {
    // The core #4012 regression: a fresh, never-evaluated state must NOT
    // be indistinguishable from a verified-green one. `last_verdict_at`
    // is the new signal a renderer uses to tell them apart.
    let state = MainHealthState::new();
    assert_eq!(state.last_verdict_at(), None);
    assert!(!state.is_halted(), "still not halted -- dispatch allowed either way");
}

#[test]
fn test_green_verdict_stamps_last_verdict_at() {
    let state = MainHealthState::new();
    let before = Utc::now();
    let _ = apply_gate_outcome(
        &state,
        &GateOutcome::Green {
            elapsed: Duration::ZERO,
        },
    );
    let stamped = state
        .last_verdict_at()
        .expect("a completed Green run must stamp a verdict time");
    assert!(stamped >= before, "stamped time must not be in the past relative to the call");
}

#[test]
fn test_red_verdict_also_stamps_last_verdict_at() {
    // Both determinate outcomes count as "a verdict happened" -- a red
    // main is still a real answer, just an unwelcome one.
    let state = MainHealthState::new();
    let _ = apply_gate_outcome(&state, &GateOutcome::red("boom"));
    assert!(state.last_verdict_at().is_some());
}

#[test]
fn test_unevaluated_outcome_does_not_stamp_last_verdict_at() {
    // An UNEVALUATED tick produced no verdict at all -- it must not be
    // mistaken for one by stamping the timestamp.
    let state = MainHealthState::new();
    let _ = apply_gate_outcome(
        &state,
        &GateOutcome::unevaluated(UnevaluatedClass::Timeout, "timed out"),
    );
    assert_eq!(state.last_verdict_at(), None, "an unevaluated tick must not stamp a verdict");
}

#[test]
#[serial]
fn test_run_gate_tick_skip_path_does_not_stamp_last_verdict_at() {
    // The #3984 SHA-memo skip path (unchanged `origin/main`) proves
    // nothing new -- the Curator's explicit design decision is that only
    // the real Green/Red run stamps the verdict time, never the skip.
    //
    // #4615: isolate the machine-wide build slot to a per-test tempdir so
    // this test never contends with a live daemon's real build slot.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (_origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {}", marker_file.display()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let state = MainHealthState::new();

    let first = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    assert!(matches!(first, Some(GateOutcome::Green { .. })));
    // `run_gate_tick` alone does not apply the outcome (the caller does,
    // via `apply_gate_outcome`) -- so no verdict is stamped by the tick
    // itself yet.
    assert_eq!(state.last_verdict_at(), None);
    let _ = apply_gate_outcome(&state, &first.unwrap());
    let after_first = state.last_verdict_at();
    assert!(after_first.is_some(), "the real run's outcome, once applied, stamps a verdict");

    // Second tick: unchanged SHA -> skip. No outcome to apply, so the
    // verdict time must be untouched.
    let second = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    assert_eq!(second, None, "unchanged origin/main must skip the second tick");
    assert_eq!(
        state.last_verdict_at(),
        after_first,
        "a skipped tick must never refresh the verdict timestamp"
    );

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

#[test]
fn test_workspace_health_states_last_verdict_at_unknown_root_is_none() {
    let states = WorkspaceHealthStates::new();
    assert_eq!(states.last_verdict_at(Path::new("/repo/never-seen")), None);
}

#[test]
fn test_workspace_health_states_last_verdict_at_delegates_to_root() {
    let states = WorkspaceHealthStates::new();
    let root = Path::new("/repo/a");
    let state = states.get_or_create(root);
    assert_eq!(states.last_verdict_at(root), None);
    let _ = apply_gate_outcome(
        &state,
        &GateOutcome::Green {
            elapsed: Duration::ZERO,
        },
    );
    assert!(states.last_verdict_at(root).is_some());
}

// ===================================================================
// Per-workspace halt state (#3930)
// ===================================================================

#[test]
fn test_workspace_health_states_unknown_root_not_halted() {
    let states = WorkspaceHealthStates::new();
    assert!(!states.is_halted(Path::new("/repo/never-seen")));
    assert!(states.snapshot().is_empty());
}

#[test]
fn test_workspace_health_states_are_per_root_independent() {
    // Red repo A must not mark repo B halted (the core AC2 property).
    let states = WorkspaceHealthStates::new();
    let a = Path::new("/repo/a");
    let b = Path::new("/repo/b");
    states.set_halted(a, true);
    assert!(states.is_halted(a), "repo A is halted");
    assert!(!states.is_halted(b), "repo B is unaffected by A's halt");

    // Clearing A does not touch B, and setting B does not touch A.
    states.set_halted(b, true);
    states.set_halted(a, false);
    assert!(!states.is_halted(a));
    assert!(states.is_halted(b));
}

#[test]
fn test_workspace_health_states_get_or_create_shares_arc() {
    let states = WorkspaceHealthStates::new();
    let root = Path::new("/repo/a");
    let s1 = states.get_or_create(root);
    s1.set_halted(true);
    // A second get_or_create returns the same shared state (the flag persists).
    let s2 = states.get_or_create(root);
    assert!(s2.is_halted());
    assert!(states.is_halted(root));
}

#[test]
fn test_workspace_health_states_snapshot_lists_seen_roots() {
    let states = WorkspaceHealthStates::new();
    states.set_halted(Path::new("/repo/a"), true);
    states.set_halted(Path::new("/repo/b"), false);
    let snap = states.snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap.get(Path::new("/repo/a")), Some(&true));
    assert_eq!(snap.get(Path::new("/repo/b")), Some(&false));
}

#[test]
fn test_green_then_red_enters_halt() {
    let state = MainHealthState::new();
    assert_eq!(
        apply_gate_outcome(
            &state,
            &GateOutcome::Green {
                elapsed: Duration::ZERO
            }
        ),
        HealthTransition::RemainedHealthy
    );
    assert!(!state.is_halted());

    assert_eq!(
        apply_gate_outcome(&state, &GateOutcome::red("boom")),
        HealthTransition::EnteredHalt
    );
    assert!(state.is_halted(), "a red run must halt dispatch");
}

#[test]
fn test_red_then_red_remains_halted() {
    let state = MainHealthState::new();
    assert_eq!(
        apply_gate_outcome(&state, &GateOutcome::red("boom")),
        HealthTransition::EnteredHalt
    );
    assert_eq!(
        apply_gate_outcome(&state, &GateOutcome::red("still broken")),
        HealthTransition::RemainedHalted
    );
    assert!(state.is_halted());
}

#[test]
fn test_red_then_green_recovers() {
    let state = MainHealthState::new();
    let _ = apply_gate_outcome(&state, &GateOutcome::red("boom"));
    assert!(state.is_halted());

    assert_eq!(
        apply_gate_outcome(
            &state,
            &GateOutcome::Green {
                elapsed: Duration::ZERO
            }
        ),
        HealthTransition::Recovered
    );
    assert!(!state.is_halted(), "a green run must clear the halt");
}

#[test]
fn test_full_red_then_green_sequence_via_fake_runner() {
    // A scripted runner: red, red, green — asserting the halt flag tracks
    // the sequence exactly (halt on first red, stay halted, clear on green).
    struct FakeGateRunner {
        outcomes: VecDeque<GateOutcome>,
    }
    impl GateRunner for FakeGateRunner {
        fn run_gate(&mut self) -> GateOutcome {
            self.outcomes.pop_front().unwrap_or(GateOutcome::Green {
                elapsed: Duration::ZERO,
            })
        }
    }

    let mut runner = FakeGateRunner {
        outcomes: VecDeque::from([
            GateOutcome::red("first failure"),
            GateOutcome::red("second failure"),
            GateOutcome::Green {
                elapsed: Duration::ZERO,
            },
        ]),
    };
    let state = MainHealthState::new();

    // Tick 1: red ⇒ halted.
    let t1 = apply_gate_outcome(&state, &runner.run_gate());
    assert_eq!(t1, HealthTransition::EnteredHalt);
    assert!(state.is_halted());

    // Tick 2: red ⇒ still halted.
    let t2 = apply_gate_outcome(&state, &runner.run_gate());
    assert_eq!(t2, HealthTransition::RemainedHalted);
    assert!(state.is_halted());

    // Tick 3: green ⇒ recovered, dispatch resumes.
    let t3 = apply_gate_outcome(&state, &runner.run_gate());
    assert_eq!(t3, HealthTransition::Recovered);
    assert!(!state.is_halted());
}

// ===================================================================
// Command runner (real subprocess — green + red + timeout)
// ===================================================================

#[test]
fn test_command_runner_green_on_zero_exit() {
    let cfg = BuildGateConfig {
        command: "exit 0".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    assert!(runner.run_gate().is_green());
}

#[test]
fn test_command_runner_red_on_nonzero_exit_captures_output() {
    let cfg = BuildGateConfig {
        command: "echo build-failed-marker >&2; exit 1".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    let outcome = runner.run_gate();
    assert!(!outcome.is_green());
    assert!(
        outcome.is_verified_red(),
        "a command that ran and exited 1 is VERIFIED_RED, got {outcome:?}"
    );
    assert!(
        outcome.detail().contains("build-failed-marker"),
        "red detail should include captured output, got: {}",
        outcome.detail()
    );
}

// ===================================================================
// VERIFIED_RED vs UNEVALUATED classification (#3974)
//
// The incident: with `origin/main` green on GitHub CI the whole time, a
// 600s timeout, a `cargo`-not-on-PATH exit 127, and a broken-process-tree
// `git fetch` failure were each recorded as "main still RED" and halted
// tier-0 dispatch. None of those is a statement about main.
// ===================================================================

#[test]
fn test_command_runner_timeout_is_unevaluated_not_red() {
    let cfg = BuildGateConfig {
        command: "sleep 10".to_string(),
        timeout: Duration::from_secs(1),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    let outcome = runner.run_gate();
    assert!(!outcome.is_green());
    assert!(
        !outcome.is_verified_red(),
        "a timeout must NOT be verified-red — the gate never finished, so it \
             learned nothing about main; got {outcome:?}"
    );
    assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::Timeout));
    assert!(
        outcome.detail().contains("timed out"),
        "timeout detail expected, got: {}",
        outcome.detail()
    );
}

#[test]
fn test_command_runner_exit_127_is_unevaluated_not_red() {
    // Exit 127 = `sh` could not find the command (the incident's
    // "cargo not on PATH after a launchd migration").
    let cfg = BuildGateConfig {
        command: "loom-no-such-command-3974 --version".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    let outcome = runner.run_gate();
    assert!(
        !outcome.is_verified_red(),
        "a command that could not be executed must NOT halt dispatch, got {outcome:?}"
    );
    assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::NotExecutable));
}

#[test]
fn test_command_runner_exit_126_is_unevaluated_not_red() {
    // Exit 126 = found but not executable.
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("not-executable.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    let cfg = BuildGateConfig {
        command: script.display().to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    let outcome = runner.run_gate();
    assert!(!outcome.is_verified_red(), "got {outcome:?}");
    assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::NotExecutable));
}

#[test]
fn test_command_runner_signal_death_is_unevaluated_not_red() {
    // An OOM/`kill -9` of the build (exit 137 as `sh` reports it) is an
    // environmental failure, not a failing build.
    let cfg = BuildGateConfig {
        command: "kill -9 $$".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    let outcome = runner.run_gate();
    assert!(!outcome.is_verified_red(), "got {outcome:?}");
    assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::KilledBySignal));
}

#[test]
fn test_command_runner_cargo_style_failure_is_still_verified_red() {
    // Guard against overcorrection: `cargo test` exits 101 on a genuinely
    // failing test. That command ran to completion and reported failure, so
    // it must still halt dispatch.
    let cfg = BuildGateConfig {
        command: "echo 'test result: FAILED'; exit 101".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, std::env::temp_dir()).without_sync();
    let outcome = runner.run_gate();
    assert!(
        outcome.is_verified_red(),
        "a completed non-zero exit must remain verified-red, got {outcome:?}"
    );

    // …and it must actually halt.
    let state = MainHealthState::new();
    assert_eq!(apply_gate_outcome(&state, &outcome), HealthTransition::EnteredHalt);
    assert!(state.is_halted());
}

#[test]
fn test_unevaluated_outcomes_never_halt_dispatch() {
    // AC1: each environmental failure class must leave a green verdict
    // green (no spurious halt) — the bootstrap-deadlock fix.
    for class in ALL_UNEVALUATED_CLASSES {
        let state = MainHealthState::new();
        let outcome = GateOutcome::unevaluated(class, "environmental failure");
        assert_eq!(
            apply_gate_outcome(&state, &outcome),
            HealthTransition::Unevaluated,
            "{class} must be unevaluated"
        );
        assert!(
            !state.is_halted(),
            "{class} must not halt dispatch — the gate did not run, so it is not \
                 evidence about main"
        );
    }
}

#[test]
fn test_unevaluated_class_labels_are_distinct() {
    let mut labels: Vec<&str> = ALL_UNEVALUATED_CLASSES.iter().map(|c| c.label()).collect();
    labels.sort_unstable();
    let count = labels.len();
    labels.dedup();
    assert_eq!(labels.len(), count, "every class needs a distinct label");
    // Display renders the label (used verbatim in logs and status).
    assert_eq!(UnevaluatedClass::Timeout.to_string(), "timeout");
}

// ===================================================================
// Forge-CI corroboration of a local red (#3974 AC4)
//
// The local gate measures THIS HOST; forge CI measures the COMMIT. On the
// incident host six `integration_basic` tests assert `tmux_session_exists`
// and fail because the tmux server is dead, while CI runs the identical
// `cargo test --workspace` and passes.
// ===================================================================

/// A scripted [`ForgeCiStatus`] returning a fixed verdict, recording the
/// SHA it was asked about.
struct FakeCi {
    verdict: CiVerdict,
    asked: Arc<Mutex<Vec<String>>>,
}
impl ForgeCiStatus for FakeCi {
    fn conclusion_for(&self, _repo_root: &Path, sha: &str) -> CiVerdict {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sha.to_string());
        self.verdict
    }
}

fn run_gate_with_ci(command: &str, verdict: CiVerdict) -> (GateOutcome, Vec<String>) {
    let (_origin, clone) = make_origin_and_clone();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let cfg = BuildGateConfig {
        command: command.to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner =
        gate_runner(cfg, clone.path().to_path_buf()).with_ci_status(Box::new(FakeCi {
            verdict,
            asked: Arc::clone(&asked),
        }));
    let outcome = runner.run_gate();
    let asked = asked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (outcome, asked)
}

#[test]
#[serial]
fn test_local_red_contradicted_by_green_forge_ci_does_not_halt() {
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let (outcome, asked) =
        run_gate_with_ci("echo 'tmux_session_exists failed'; exit 101", CiVerdict::Success);
    assert!(
        !outcome.is_verified_red(),
        "a local failure that CI contradicts on the same commit must not halt, got {outcome:?}"
    );
    assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::ContradictedByForgeCi));
    assert_eq!(asked.len(), 1, "CI is consulted exactly once, for the evaluated SHA");
    assert_eq!(asked[0].len(), 40, "asked about a full commit SHA: {:?}", asked[0]);
    assert!(
        outcome.detail().contains(&asked[0]),
        "the divergence must name the commit, got: {}",
        outcome.detail()
    );

    // And it must leave the halt flag untouched.
    let state = MainHealthState::new();
    assert_eq!(apply_gate_outcome(&state, &outcome), HealthTransition::Unevaluated);
    assert!(!state.is_halted());
}

#[test]
#[serial]
fn test_local_red_corroborated_by_red_forge_ci_still_halts() {
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let (outcome, _) = run_gate_with_ci("exit 1", CiVerdict::Failure);
    assert!(
        outcome.is_verified_red(),
        "CI agreeing keeps the red — a genuinely broken main must still halt"
    );
    assert!(outcome.detail().contains("corroborated"), "got: {}", outcome.detail());
}

#[test]
#[serial]
fn test_local_red_with_unknown_forge_ci_still_halts() {
    // Fail safe: only *positive* contrary evidence relaxes a halt.
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let (outcome, _) = run_gate_with_ci("exit 1", CiVerdict::Unknown);
    assert!(outcome.is_verified_red(), "got {outcome:?}");
    assert!(outcome.detail().contains("unavailable"), "got: {}", outcome.detail());
}

#[test]
#[serial]
fn test_green_local_run_never_consults_forge_ci() {
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let (outcome, asked) = run_gate_with_ci("exit 0", CiVerdict::Failure);
    assert!(outcome.is_green(), "a green local run is authoritative");
    assert!(asked.is_empty(), "CI must not be probed on a green run");
}

#[test]
#[serial]
fn test_ci_corroboration_kill_switch_keeps_local_red() {
    std::env::set_var(GATE_CI_CORROBORATION_ENV, "0");
    let (outcome, asked) = run_gate_with_ci("exit 1", CiVerdict::Success);
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    assert!(
        outcome.is_verified_red(),
        "with corroboration disabled the local red stands, got {outcome:?}"
    );
    assert!(asked.is_empty(), "disabled corroboration must not probe the forge");
}

#[test]
#[serial]
fn test_ci_corroboration_enabled_by_default_and_env_parsing() {
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    assert!(ci_corroboration_enabled(), "unset ⇒ on");
    for v in ["0", "false", "no", "off", "OFF", " No "] {
        std::env::set_var(GATE_CI_CORROBORATION_ENV, v);
        assert!(!ci_corroboration_enabled(), "{v:?} should disable");
    }
    for v in ["1", "true", "yes", "on", "anything-else"] {
        std::env::set_var(GATE_CI_CORROBORATION_ENV, v);
        assert!(ci_corroboration_enabled(), "{v:?} should keep it enabled");
    }
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
}

#[test]
fn test_parse_gh_run_list_verdicts() {
    let sha = "a".repeat(40);
    let other = "b".repeat(40);

    // All completed runs for the SHA succeeded ⇒ green.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"LOC"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Success);

    // Any completed failure for the SHA ⇒ red.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"failure","workflowName":"Sec"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Failure);

    // Only in-progress runs for the SHA ⇒ unknown (never a silent green).
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"in_progress","conclusion":null,"workflowName":"CI"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // A green run for a DIFFERENT commit must never vouch for this one.
    let json = format!(
        r#"[{{"headSha":"{other}","status":"completed","conclusion":"success","workflowName":"CI"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // Empty / unparseable output ⇒ unknown.
    assert_eq!(parse_gh_run_list("[]", &sha, None), CiVerdict::Unknown);
    assert_eq!(parse_gh_run_list("not json", &sha, None), CiVerdict::Unknown);
}

/// Only *positive* contrary evidence may relax a halt (#3974 AC4). These are
/// the shapes that a "saw any completed run ⇒ green" reducer read as green
/// even though no workflow ever concluded the commit was good.
#[test]
fn test_parse_gh_run_list_non_evidence_is_never_success() {
    let sha = "c".repeat(40);

    // 1. `cancel-in-progress: true` supersedes the previous commit's CI run,
    //    which then sits at completed/cancelled FOREVER. Not a statement
    //    about the code — and crucially not a permanent green.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"cancelled","workflowName":"CI"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // Cancelled CI alongside a completed-success bookkeeping workflow: still
    // unknown. The success does not paper over the missing CI verdict.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"cancelled","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // 2. The ~100s window after every push where the fast bookkeeping
    //    workflow has finished but CI is still running.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"in_progress","conclusion":null,"workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // Queued counts the same as in-progress: not yet a verdict.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"queued","conclusion":null,"workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // `action_required` (awaiting a human) and `stale` are likewise not
    // statements about the code.
    for conclusion in ["action_required", "stale"] {
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"{conclusion}","workflowName":"CI"}},
                    {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
        );
        assert_eq!(
            parse_gh_run_list(&json, &sha, None),
            CiVerdict::Unknown,
            "conclusion {conclusion:?} must not vouch for the commit"
        );
    }

    // An unrecognized future conclusion degrades to unknown, not to green.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"some_new_thing","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // Absence of failure is not success: every run skipped ⇒ nothing
    // positively vouches for the commit.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"CI"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Unknown);

    // A real failure still wins over any indeterminate sibling — a halt may
    // always be *established*, it just may not be relaxed on non-evidence.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"in_progress","conclusion":null,"workflowName":"Lint"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"cancelled","workflowName":"Lines of Code"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"failure","workflowName":"CI"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Failure);

    // And the genuine all-clear still reads green: every workflow for the
    // commit reached a verdict, at least one of them `success`.
    let json = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"Release"}}]"#
    );
    assert_eq!(parse_gh_run_list(&json, &sha, None), CiVerdict::Success);
}

/// #3987: with a named verification workflow configured, that workflow must
/// itself have concluded `success` for the SHA — a bookkeeping workflow
/// succeeding on its own may not vouch for a commit whose real build never
/// ran (`paths`-filtered away, so it produced no run at all).
#[test]
fn test_parse_gh_run_list_named_workflow() {
    let sha = "d".repeat(40);

    // Regression for this issue: only a bookkeeping `success`, no `CI` run at
    // all for the SHA. Unnamed ⇒ Success (today's behavior); named "CI" ⇒
    // Unknown, because the workflow that verifies the code never judged it.
    let bookkeeping_only = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&bookkeeping_only, &sha, None), CiVerdict::Success);
    assert_eq!(
        parse_gh_run_list(&bookkeeping_only, &sha, Some("CI")),
        CiVerdict::Unknown,
        "no CI run for the SHA must not be vouched for by a bookkeeping success"
    );

    // The named workflow completed/success (unanimity otherwise satisfied) ⇒
    // Success: the corroboration path stays reachable (preserves #3986/#3974).
    let ci_success = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&ci_success, &sha, Some("CI")), CiVerdict::Success);

    // Named workflow `skipped` alongside a bookkeeping success ⇒ Unknown (a
    // required workflow that declined to run did not verify the commit). Note
    // this differs from the unnamed case, where the same fixture is Success.
    let ci_skipped = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&ci_skipped, &sha, None), CiVerdict::Success);
    assert_eq!(parse_gh_run_list(&ci_skipped, &sha, Some("CI")), CiVerdict::Unknown);

    // A `failure` on any run still yields Failure, configured or not — a halt
    // may always be established, only never relaxed on non-evidence.
    let ci_failure = format!(
        r#"[{{"headSha":"{sha}","status":"completed","conclusion":"failure","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
    );
    assert_eq!(parse_gh_run_list(&ci_failure, &sha, None), CiVerdict::Failure);
    assert_eq!(parse_gh_run_list(&ci_failure, &sha, Some("CI")), CiVerdict::Failure);

    // A configured name matching no workflow anywhere in the window ⇒ Unknown
    // (fail safe) and exercises the misconfiguration `warn!` path.
    assert_eq!(
        parse_gh_run_list(&bookkeeping_only, &sha, Some("Typo Workflow")),
        CiVerdict::Unknown
    );

    // Matching is exact on `workflowName` (case-sensitive): a near-miss name
    // is treated as "no run for the named workflow".
    assert_eq!(parse_gh_run_list(&ci_success, &sha, Some("ci")), CiVerdict::Unknown);
}

/// #3987 Finding-1 guard: real captured `gh run list` output for this repo —
/// where `Shell Script Linting` is `paths`-filtered and appears for only one
/// of several SHAs — must yield `Success` for a green SHA **both** unnamed and
/// with `ciWorkflow: "CI"`, i.e. the low-frequency workflow must not perturb
/// any verdict.
#[test]
fn test_parse_gh_run_list_real_data_fixture() {
    let green = "1111111111111111111111111111111111111111";
    let older = "2222222222222222222222222222222222222222";
    // The green SHA has CI + two other no-filter workflows all green; the
    // paths-filtered `Shell Script Linting` only ever ran on an older SHA.
    let json = format!(
        r#"[
                {{"headSha":"{green}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{green}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}},
                {{"headSha":"{green}","status":"completed","conclusion":"success","workflowName":"Security Scan"}},
                {{"headSha":"{older}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{older}","status":"completed","conclusion":"success","workflowName":"Shell Script Linting"}}
            ]"#
    );
    assert_eq!(parse_gh_run_list(&json, green, None), CiVerdict::Success);
    assert_eq!(parse_gh_run_list(&json, green, Some("CI")), CiVerdict::Success);
}

// ===================================================================
// Workspace preparation — sync to origin/main before a gate run (#3885)
// ===================================================================

/// Run `git <args>` in `dir`, asserting success. Test-only helper for
/// building throwaway repos.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Create an `origin` bare repo with an initial `main` commit and a working
/// clone checked out on `main`. Returns `(origin_dir, clone_dir)` — both
/// `TempDir` guards so they live for the test's duration.
fn make_origin_and_clone() -> (tempfile::TempDir, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    // A bare origin we can fetch from and push to.
    git(origin.path(), &["init", "--bare", "--initial-branch=main"]);

    // Seed it via a scratch clone so origin has a real `main` commit.
    let seed = tempfile::tempdir().unwrap();
    git(seed.path(), &["init", "--initial-branch=main"]);
    git(seed.path(), &["config", "user.email", "t@t.t"]);
    git(seed.path(), &["config", "user.name", "t"]);
    std::fs::write(seed.path().join("file.txt"), "v1\n").unwrap();
    git(seed.path(), &["add", "."]);
    git(seed.path(), &["commit", "-m", "initial"]);
    git(seed.path(), &["remote", "add", "origin", origin.path().to_str().unwrap()]);
    git(seed.path(), &["push", "origin", "main"]);

    // The workspace under test: a fresh clone on `main`.
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &[
            "clone",
            origin.path().to_str().unwrap(),
            clone.path().to_str().unwrap(),
        ],
    );
    git(clone.path(), &["config", "user.email", "t@t.t"]);
    git(clone.path(), &["config", "user.name", "t"]);
    (origin, clone)
}

/// Push a new commit to `origin/main` from a scratch clone, so a workspace
/// that has not fetched is now behind.
fn advance_origin_main(origin: &Path) {
    let scratch = tempfile::tempdir().unwrap();
    git(
        scratch.path(),
        &[
            "clone",
            origin.to_str().unwrap(),
            scratch.path().to_str().unwrap(),
        ],
    );
    git(scratch.path(), &["config", "user.email", "t@t.t"]);
    git(scratch.path(), &["config", "user.name", "t"]);
    std::fs::write(scratch.path().join("file.txt"), "v2\n").unwrap();
    git(scratch.path(), &["add", "."]);
    git(scratch.path(), &["commit", "-m", "advance main"]);
    git(scratch.path(), &["push", "origin", "main"]);
}

fn head_commit(dir: &Path) -> String {
    String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string()
}

#[test]
fn test_prepare_fast_forwards_stale_main_to_origin() {
    let (origin, clone) = make_origin_and_clone();
    let before = head_commit(clone.path());
    // Remote main advances; the local clone is now behind (never fetched).
    advance_origin_main(origin.path());
    assert_eq!(head_commit(clone.path()), before, "clone still stale pre-prep");

    let outcome = prepare_workspace_to_origin_main(clone.path());
    assert_eq!(outcome, PrepOutcome::Ready);
    assert_ne!(
        head_commit(clone.path()),
        before,
        "prepare must fast-forward the workspace to the advanced origin/main"
    );
}

#[test]
fn test_prepare_skips_when_local_main_ahead_of_origin() {
    let (_origin, clone) = make_origin_and_clone();
    // A clean local `main` that carries a commit origin/main lacks.
    std::fs::write(clone.path().join("local.txt"), "local-only\n").unwrap();
    git(clone.path(), &["add", "."]);
    git(clone.path(), &["commit", "-m", "local-only commit"]);
    let ahead = head_commit(clone.path());

    let outcome = prepare_workspace_to_origin_main(clone.path());
    match outcome {
        PrepOutcome::Skip { class, reason } => {
            assert_eq!(class, UnevaluatedClass::LocalAhead);
            assert!(reason.contains("ahead"), "expected ahead-of-origin reason, got: {reason}");
        }
        other => panic!("expected Skip when local main is ahead, got {other:?}"),
    }
    // The local-only commit must NOT have been reset away.
    assert_eq!(
        head_commit(clone.path()),
        ahead,
        "a local main ahead of origin must never be hard-reset away"
    );
}

#[test]
fn test_prepare_skips_dirty_workspace() {
    let (_origin, clone) = make_origin_and_clone();
    // A tracked-file edit makes the tree dirty.
    std::fs::write(clone.path().join("file.txt"), "operator edit\n").unwrap();
    let outcome = prepare_workspace_to_origin_main(clone.path());
    match outcome {
        PrepOutcome::Skip { class, reason } => {
            assert_eq!(class, UnevaluatedClass::DirtyTree);
            // #3974 AC2: the reason must name the root it inspected and the
            // exact porcelain line(s), so the claim is checkable by hand.
            assert!(
                reason.contains("status --porcelain"),
                "dirty reason should cite the command it ran, got: {reason}"
            );
            assert!(
                reason.contains(&clone.path().display().to_string()),
                "dirty reason should name the root it inspected, got: {reason}"
            );
            assert!(
                reason.contains("file.txt"),
                "dirty reason should name the offending path, got: {reason}"
            );
        }
        other => panic!("expected Skip on dirty tree, got {other:?}"),
    }
    // The operator edit must NOT have been reset away.
    assert_eq!(
        std::fs::read_to_string(clone.path().join("file.txt")).unwrap(),
        "operator edit\n",
        "a dirty workspace must never be hard-reset"
    );
}

#[test]
fn test_prepare_skips_untracked_file() {
    let (_origin, clone) = make_origin_and_clone();
    std::fs::write(clone.path().join("scratch.tmp"), "junk\n").unwrap();
    let outcome = prepare_workspace_to_origin_main(clone.path());
    assert!(
        matches!(outcome, PrepOutcome::Skip { .. }),
        "an untracked file must skip (porcelain reports it), got {outcome:?}"
    );
}

// ===================================================================
// Ignore-list: Loom-owned transient paths + build-artifact lockfiles
// (#3950 AC1) — the dirty-tree check must not block on these.
// ===================================================================

#[test]
fn test_is_ignorable_dirt_loom_owned_prefixes() {
    let repo_root = tempfile::tempdir().unwrap();
    let repo_root = repo_root.path();
    assert!(is_ignorable_dirt(".loom/logs/sweep-issue-1.log", repo_root));
    assert!(is_ignorable_dirt(".loom/worktrees/issue-42/foo.rs", repo_root));
    assert!(is_ignorable_dirt(".loom/tokens/agent-1.token", repo_root));
    assert!(is_ignorable_dirt(".loom/sweep-checkpoint/issue-1.json", repo_root));
    assert!(is_ignorable_dirt(".loom/accounts.env", repo_root));
    assert!(is_ignorable_dirt(".loom-managed", repo_root));
}

#[test]
fn test_is_ignorable_dirt_lockfile_basenames() {
    let repo_root = tempfile::tempdir().unwrap();
    let repo_root = repo_root.path();
    assert!(is_ignorable_dirt("mcp-loom/package-lock.json", repo_root));
    assert!(is_ignorable_dirt("package-lock.json", repo_root));
    assert!(is_ignorable_dirt("some/nested/dir/Cargo.lock", repo_root));
    assert!(is_ignorable_dirt("pnpm-lock.yaml", repo_root));
}

#[test]
fn test_is_ignorable_dirt_rejects_unknown_paths() {
    let repo_root = tempfile::tempdir().unwrap();
    let repo_root = repo_root.path();
    // A genuine operator edit outside both lists must never be ignored.
    assert!(!is_ignorable_dirt("src/main.rs", repo_root));
    assert!(!is_ignorable_dirt("scratch.tmp", repo_root));
    // A path that merely starts with ".loom" but isn't one of the listed
    // transient subtrees (e.g. a hypothetical ".loom/config.json" edit)
    // must NOT be ignored — only the explicitly listed prefixes count.
    assert!(!is_ignorable_dirt(".loom/config.json", repo_root));
}

#[test]
fn test_non_ignorable_dirt_filters_porcelain_lines() {
    let repo_root = tempfile::tempdir().unwrap();
    let status = "?? .loom/logs/foo.log\n M mcp-loom/package-lock.json\n M src/main.rs\n";
    let remaining = non_ignorable_dirt(status, repo_root.path());
    assert_eq!(remaining, vec![" M src/main.rs"]);
}

#[test]
fn test_non_ignorable_dirt_empty_when_all_ignorable() {
    let repo_root = tempfile::tempdir().unwrap();
    let status = "?? .loom/logs/foo.log\n M package-lock.json\n";
    assert!(non_ignorable_dirt(status, repo_root.path()).is_empty());
}

// ===================================================================
// Installed-surface byte-match class (#4332) — a dirty installed-surface
// path (`.loom/hooks/`, `.loom/scripts/`, `.loom/roles/`, `.loom/docs/`,
// `.loom/bin/`, `.claude/commands/loom/`) is ignorable IFF its
// `defaults/` counterpart exists and byte-matches: provably
// `resync-installed.sh` output, never an operator hand-edit.
// ===================================================================

/// Write `content` to `repo_root`-relative `rel`, creating parent dirs.
fn write_rel(repo_root: &Path, rel: &str, content: &str) {
    let path = repo_root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn test_is_ignorable_dirt_installed_surface_byte_match() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = repo.path();
    write_rel(repo_root, "defaults/docs/foo.md", "resynced content\n");
    write_rel(repo_root, ".loom/docs/foo.md", "resynced content\n");
    assert!(
        is_ignorable_dirt(".loom/docs/foo.md", repo_root),
        "byte-identical installed copy of a tracked defaults/ source is provably resync output"
    );
}

/// AC4 pin: an installed-surface path whose content DIFFERS from its
/// `defaults/` counterpart is a genuine operator edit (or an
/// out-of-sync tree) and must never be ignored.
#[test]
fn test_is_ignorable_dirt_installed_surface_content_mismatch_stays_non_ignorable() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = repo.path();
    write_rel(repo_root, "defaults/docs/foo.md", "source content\n");
    write_rel(repo_root, ".loom/docs/foo.md", "operator edit\n");
    assert!(
        !is_ignorable_dirt(".loom/docs/foo.md", repo_root),
        "content that diverges from defaults/ must not be assumed to be resync output"
    );
}

#[test]
fn test_is_ignorable_dirt_install_metadata_exact_path() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = repo.path();
    // No defaults/ counterpart needed — install-metadata.json is
    // generated + re-stamped, not copied.
    assert!(is_ignorable_dirt(".loom/install-metadata.json", repo_root));
}

/// Consumer-repo no-op: a repo with no local `defaults/` dir at all
/// cannot resolve the installed-surface mapping, so classification of an
/// installed-surface path is unchanged (non-ignorable) — this fix is
/// loom-repo-scoped by construction.
#[test]
fn test_is_ignorable_dirt_no_defaults_dir_leaves_classification_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = repo.path();
    write_rel(repo_root, ".loom/docs/foo.md", "some content\n");
    // Deliberately no defaults/docs/foo.md.
    assert!(!is_ignorable_dirt(".loom/docs/foo.md", repo_root));
}

/// Safety property: a `defaults/` SOURCE-side edit must remain
/// non-ignorable even when the installed copy it produced still
/// byte-matches — the installed-surface mapping only ever maps installed
/// paths to their source, never the reverse, so an operator editing
/// `defaults/docs/x.md` directly and then running resync still shows the
/// gate a real, unignorable change (the `defaults/` file itself).
#[test]
fn test_is_ignorable_dirt_defaults_source_edit_stays_non_ignorable() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = repo.path();
    write_rel(repo_root, "defaults/docs/x.md", "operator's new content\n");
    write_rel(repo_root, ".loom/docs/x.md", "operator's new content\n");
    assert!(
        !is_ignorable_dirt("defaults/docs/x.md", repo_root),
        "the defaults/ source file itself is never in the installed-surface mapping's domain"
    );
}

/// Edge case: an UNTRACKED installed file (e.g. a brand-new doc synced by
/// resync but not yet `git add`-ed) that byte-matches a tracked
/// `defaults/` counterpart is ignorable the same as a modified one.
#[test]
fn test_non_ignorable_dirt_untracked_installed_surface_byte_match() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = repo.path();
    write_rel(repo_root, "defaults/docs/new.md", "new doc\n");
    write_rel(repo_root, ".loom/docs/new.md", "new doc\n");
    let status = "?? .loom/docs/new.md\n";
    assert!(non_ignorable_dirt(status, repo_root).is_empty());
}

/// AC1(a): a workspace with ONLY Loom-owned transient paths dirty must NOT
/// block the gate (prep proceeds to sync/reset, not Skip).
#[test]
fn test_prepare_ignores_loom_owned_transient_dirt() {
    let (_origin, clone) = make_origin_and_clone();
    // Realistic repo shape: `.loom/config.json` is a tracked, committed
    // file (every installed Loom repo has one) — so `.loom/` itself is
    // never a wholly-untracked directory that `git status --porcelain`
    // could collapse into one opaque `?? .loom/` line. Without this,
    // `.loom/logs/...` below would report as `?? .loom/` (the whole
    // subtree), which no single ignore-list prefix matches.
    std::fs::create_dir_all(clone.path().join(".loom")).unwrap();
    std::fs::write(clone.path().join(".loom/config.json"), "{}\n").unwrap();
    Command::new("git")
        .args(["add", ".loom/config.json"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "add .loom/config.json"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    // Push so this commit is on `origin/main` too — otherwise the clone
    // would be (correctly) skipped as "ahead of origin" by step 4,
    // unrelated to the dirty-tree behavior this test targets.
    Command::new("git")
        .args(["push", "origin", "main"])
        .current_dir(clone.path())
        .status()
        .unwrap();

    std::fs::create_dir_all(clone.path().join(".loom/logs")).unwrap();
    std::fs::write(clone.path().join(".loom/logs/sweep-issue-1.log"), "log\n").unwrap();
    let outcome = prepare_workspace_to_origin_main(clone.path());
    assert_eq!(
        outcome,
        PrepOutcome::Ready,
        "only Loom-owned transient dirt must not block the gate, got {outcome:?}"
    );
}

/// AC1(a) variant: a modified build-artifact lockfile alone must not block
/// the gate either (the reported symptom — a lone modified
/// `mcp-loom/package-lock.json`).
#[test]
fn test_prepare_ignores_lockfile_only_dirt() {
    let (_origin, clone) = make_origin_and_clone();
    std::fs::write(clone.path().join("package-lock.json"), "{}\n").unwrap();
    Command::new("git")
        .args(["add", "package-lock.json"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "add lockfile"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    // Push so the lockfile-add commit is on `origin/main` too — otherwise
    // the clone would be (correctly) skipped as "ahead of origin" by step
    // 4, unrelated to the dirty-tree behavior this test targets.
    Command::new("git")
        .args(["push", "origin", "main"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    // Now mutate it — a benign build-side-effect regen, no real change.
    std::fs::write(clone.path().join("package-lock.json"), "{ \"regen\": true }\n").unwrap();
    let outcome = prepare_workspace_to_origin_main(clone.path());
    assert_eq!(
        outcome,
        PrepOutcome::Ready,
        "a lone modified lockfile must not block the gate, got {outcome:?}"
    );
}

/// AC (#4332): after a `resync-installed.sh`-shaped change — a tracked
/// installed-surface file rewritten to byte-match its `defaults/`
/// source, with no other edits — the gate must proceed to `Ready`
/// instead of `Skip { class: DirtyTree, .. }`.
#[test]
fn test_prepare_ignores_resync_shaped_installed_surface_dirt() {
    let (_origin, clone) = make_origin_and_clone();
    std::fs::create_dir_all(clone.path().join("defaults/docs")).unwrap();
    std::fs::create_dir_all(clone.path().join(".loom/docs")).unwrap();
    std::fs::write(clone.path().join("defaults/docs/foo.md"), "old content\n").unwrap();
    std::fs::write(clone.path().join(".loom/docs/foo.md"), "old content\n").unwrap();
    Command::new("git")
        .args(["add", "defaults/docs/foo.md", ".loom/docs/foo.md"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "seed installed surface"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["push", "origin", "main"])
        .current_dir(clone.path())
        .status()
        .unwrap();

    // Simulate a resync: the source under `defaults/` changed upstream
    // (already committed) and the installed copy is refreshed to match —
    // exactly what `resync-installed.sh` does, and exactly the dirt this
    // issue is about.
    std::fs::write(clone.path().join("defaults/docs/foo.md"), "new content\n").unwrap();
    Command::new("git")
        .args(["add", "defaults/docs/foo.md"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "update defaults/docs/foo.md"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["push", "origin", "main"])
        .current_dir(clone.path())
        .status()
        .unwrap();
    // The installed copy is now stale relative to the freshly-committed
    // source — resync rewrites it to match, leaving it dirty (tracked,
    // modified) but byte-identical to `defaults/docs/foo.md`.
    std::fs::write(clone.path().join(".loom/docs/foo.md"), "new content\n").unwrap();

    let outcome = prepare_workspace_to_origin_main(clone.path());
    assert_eq!(
            outcome,
            PrepOutcome::Ready,
            "resync-shaped installed-surface dirt (byte-identical to its defaults/ source) must not block the gate, got {outcome:?}"
        );
}

/// AC1(b): a genuine unexpected dirty file — alongside otherwise-ignorable
/// dirt — must still cause a skip.
#[test]
fn test_prepare_still_skips_on_unexpected_dirt_alongside_ignorable() {
    let (_origin, clone) = make_origin_and_clone();
    std::fs::create_dir_all(clone.path().join(".loom/logs")).unwrap();
    std::fs::write(clone.path().join(".loom/logs/sweep-issue-1.log"), "log\n").unwrap();
    // A genuine operator edit — not on either ignore list.
    std::fs::write(clone.path().join("file.txt"), "operator edit\n").unwrap();
    let outcome = prepare_workspace_to_origin_main(clone.path());
    match outcome {
        PrepOutcome::Skip { class, reason } => {
            assert_eq!(class, UnevaluatedClass::DirtyTree);
            assert!(
                reason.contains("file.txt"),
                "skip reason should name the unexpected file, got: {reason}"
            );
            assert!(
                !reason.contains(".loom/logs"),
                "skip reason should not blame the ignorable Loom-owned path, got: {reason}"
            );
        }
        other => panic!("expected Skip on genuine unexpected dirt, got {other:?}"),
    }
}

#[test]
fn test_prepare_skips_when_not_on_main() {
    let (_origin, clone) = make_origin_and_clone();
    git(clone.path(), &["checkout", "-b", "feature/x"]);
    let outcome = prepare_workspace_to_origin_main(clone.path());
    match outcome {
        PrepOutcome::Skip { class, reason } => {
            assert_eq!(class, UnevaluatedClass::NotOnMain);
            assert!(
                reason.contains("feature/x") && reason.contains("not 'main'"),
                "expected not-on-main reason, got: {reason}"
            );
        }
        other => panic!("expected Skip off main, got {other:?}"),
    }
}

#[test]
fn test_prepare_skips_non_git_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let outcome = prepare_workspace_to_origin_main(tmp.path());
    assert!(
        matches!(outcome, PrepOutcome::Skip { .. }),
        "a non-git dir cannot determine a branch and must skip, got {outcome:?}"
    );
}

#[test]
fn test_prepare_skips_when_fetch_fails_offline() {
    // A repo whose `origin` points nowhere: on main + clean, but fetch fails.
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "--initial-branch=main"]);
    git(repo.path(), &["config", "user.email", "t@t.t"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("f.txt"), "x\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "c"]);
    git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            "/nonexistent/loom-gate-no-such-remote.git",
        ],
    );
    let outcome = prepare_workspace_to_origin_main(repo.path());
    match outcome {
        PrepOutcome::Skip { class, reason } => {
            assert_eq!(
                class,
                UnevaluatedClass::GitFailure,
                "a failed `git fetch` is a gate-infrastructure failure, not a red main (#3974)"
            );
            assert!(reason.contains("fetch"), "expected fetch-failure reason, got: {reason}");
        }
        other => panic!("expected Skip on fetch failure, got {other:?}"),
    }
}

#[test]
fn test_command_runner_returns_unevaluated_when_prep_skips() {
    // Sync ON (production default) against a non-repo dir ⇒ prep skips ⇒ the
    // gate command is NOT run and the outcome is Unevaluated.
    let cfg = BuildGateConfig {
        command: "exit 1".to_string(), // would be red if it ran
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut runner = gate_runner(cfg, tmp.path().to_path_buf());
    let outcome = runner.run_gate();
    assert!(
        outcome.is_unevaluated(),
        "prep skip must short-circuit before running the command, got {outcome:?}"
    );
}

#[test]
fn test_command_runner_runs_gate_after_successful_prep() {
    // Sync ON against a real on-main clean clone ⇒ prep Ready ⇒ command runs.
    let (_origin, clone) = make_origin_and_clone();
    let cfg = BuildGateConfig {
        command: "exit 0".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = gate_runner(cfg, clone.path().to_path_buf());
    assert!(runner.run_gate().is_green());
}

// ===================================================================
// SHA memoization + `realChangeGlobs` + indeterminate-run backoff
// (#3984) — the doom loop was: the gate re-ran the full (potentially
// minutes-long) command every cadence tick regardless of whether
// `origin/main` had actually moved.
// ===================================================================

/// Push a commit that writes `filename` with `contents` to `origin/main`
/// from a scratch clone (mirrors [`advance_origin_main`] but lets tests
/// control the changed path, for `realChangeGlobs` matching).
fn push_file_change(origin: &Path, filename: &str, contents: &str) {
    let scratch = tempfile::tempdir().unwrap();
    git(
        scratch.path(),
        &[
            "clone",
            origin.to_str().unwrap(),
            scratch.path().to_str().unwrap(),
        ],
    );
    git(scratch.path(), &["config", "user.email", "t@t.t"]);
    git(scratch.path(), &["config", "user.name", "t"]);
    let path = scratch.path().join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
    git(scratch.path(), &["add", "."]);
    git(scratch.path(), &["commit", "-m", &format!("touch {filename}")]);
    git(scratch.path(), &["push", "origin", "main"]);
}

#[test]
fn test_glob_matches_basename_and_full_path() {
    assert!(glob_matches("*.rs", "loom-daemon/src/main.rs"));
    assert!(glob_matches("*.rs", "main.rs"));
    assert!(!glob_matches("*.rs", "main.py"));
    assert!(glob_matches("Cargo.lock", "Cargo.lock"));
    assert!(glob_matches("Cargo.lock", "loom-daemon/Cargo.lock"));
    assert!(!glob_matches("Cargo.lock", "Cargo.toml"));
    // A pattern containing '/' matches the full path, not just basename.
    assert!(glob_matches("src/*.rs", "src/main.rs"));
    assert!(!glob_matches("src/*.rs", "other/main.rs"));
}

#[test]
fn test_decide_gate_run_no_baseline_must_run() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        decide_gate_run(None, "deadbeef", &[], tmp.path()),
        GateRunDecision::Run,
        "no prior determinate evaluation ⇒ must run"
    );
}

#[test]
fn test_decide_gate_run_unchanged_sha_skips_even_with_globs() {
    let tmp = tempfile::tempdir().unwrap();
    let globs = vec!["*.rs".to_string()];
    assert_eq!(
        decide_gate_run(Some("abc123"), "abc123", &globs, tmp.path()),
        GateRunDecision::Skip,
        "identical SHA means no diff at all — must skip regardless of globs"
    );
}

#[test]
fn test_decide_gate_run_changed_sha_no_globs_must_run() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        decide_gate_run(Some("abc123"), "def456", &[], tmp.path()),
        GateRunDecision::Run,
        "no realChangeGlobs configured ⇒ any movement counts as real"
    );
}

#[test]
fn test_decide_gate_run_changed_sha_glob_diff_matches() {
    let (origin, clone) = make_origin_and_clone();
    let before = head_commit(clone.path());
    push_file_change(origin.path(), "src/lib.rs", "fn x() {}\n");
    // Fetch so the clone's local git has the new commit object available
    // for the diff — mirrors what `resolve_remote_main_sha` + the
    // subsequent fetch inside `diff_touches_globs` do in production.
    git(clone.path(), &["fetch", "origin", "main"]);
    let after = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "origin/main"])
            .current_dir(clone.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let globs = vec!["*.rs".to_string()];
    assert_eq!(
        decide_gate_run(Some(&before), &after, &globs, clone.path()),
        GateRunDecision::Run,
        "the diff touches a *.rs path — must run"
    );
}

#[test]
fn test_decide_gate_run_changed_sha_glob_diff_does_not_match() {
    let (origin, clone) = make_origin_and_clone();
    let before = head_commit(clone.path());
    push_file_change(origin.path(), "README.md", "docs only\n");
    git(clone.path(), &["fetch", "origin", "main"]);
    let after = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "origin/main"])
            .current_dir(clone.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let globs = vec!["*.rs".to_string(), "*.toml".to_string()];
    assert_eq!(
        decide_gate_run(Some(&before), &after, &globs, clone.path()),
        GateRunDecision::Skip,
        "the diff touches only README.md — no configured glob matches, must skip"
    );
}

#[test]
fn test_main_health_state_gate_backoff_grows_and_clears() {
    let state = MainHealthState::new();
    let now = Instant::now();
    assert!(!state.gate_backoff_active(now), "fresh state is never backing off");

    let min = Duration::from_secs(60);
    let max = Duration::from_secs(3600);
    state.record_gate_indeterminate_backoff(min, max);
    assert!(
        state.gate_backoff_active(Instant::now()),
        "one indeterminate run must start a backoff window"
    );

    // A determinate evaluation clears the backoff outright.
    state.record_gate_evaluated_sha("abc123");
    assert!(
        !state.gate_backoff_active(Instant::now()),
        "a determinate evaluation must clear any standing backoff"
    );
    assert_eq!(state.gate_last_evaluated_sha(), Some("abc123".to_string()));
}

#[test]
#[serial]
fn test_run_gate_tick_skips_second_run_for_unchanged_sha() {
    // The core #3984 regression: with `origin/main` unchanged between two
    // ticks, the second tick must NOT spawn the gate command again.
    //
    // #4615: isolate the machine-wide build slot to a per-test tempdir so
    // this test never contends with a live daemon's real build slot.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (_origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {}", marker_file.display()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let state = MainHealthState::new();

    let first = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    assert!(
        matches!(first, Some(GateOutcome::Green { .. })),
        "first tick must run and be green"
    );
    let invocations_after_first = std::fs::read_to_string(&marker_file)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(invocations_after_first, 1, "the command must have run exactly once");

    let second = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    assert_eq!(
        second, None,
        "unchanged origin/main must skip the second tick entirely (no outcome to apply)"
    );
    let invocations_after_second = std::fs::read_to_string(&marker_file)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(
        invocations_after_second, 1,
        "no second gate command must be spawned for an unchanged SHA"
    );

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

#[test]
#[serial]
fn test_run_gate_tick_runs_again_after_main_advances() {
    // #4615: isolate the machine-wide build slot to a per-test tempdir so
    // this test never contends with a live daemon's real build slot.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {}", marker_file.display()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let state = MainHealthState::new();

    assert!(matches!(
        run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false),
        Some(GateOutcome::Green { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count(),
        1
    );

    // main moves — the next tick must run again.
    advance_origin_main(origin.path());
    assert!(matches!(
        run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false),
        Some(GateOutcome::Green { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count(),
        2,
        "a real change to origin/main must trigger another run"
    );

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

#[test]
#[serial]
fn test_run_gate_tick_skips_while_backing_off_after_timeout() {
    // #4615: isolate the machine-wide build slot to a per-test tempdir so
    // this test never contends with a live daemon's real build slot.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (_origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {} && sleep 5", marker_file.display()),
        timeout: Duration::from_millis(200),
        ..Default::default()
    };
    let state = MainHealthState::new();

    let first = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    assert!(
        matches!(first, Some(GateOutcome::Unevaluated { .. })),
        "a timeout must be UNEVALUATED, got {first:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count(),
        1
    );

    // Immediately retrying (well within the backoff window derived from
    // the 200ms timeout) must be skipped — no second spawn.
    let second = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    assert_eq!(
        second, None,
        "an indeterminate run must trigger backoff, not an immediate retry"
    );
    assert_eq!(
        std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "no second gate command must be spawned while backing off"
    );

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

#[test]
#[serial]
fn test_run_gate_tick_defers_when_host_is_saturated() {
    // The #4259 defer path (#4441 regression coverage): a saturated host
    // must defer the first tick entirely -- no command spawn, no SHA
    // memo, no indeterminate backoff, and no verdict. Pinning an
    // absurdly high load average makes `is_host_saturated` report
    // `Some(true)` regardless of the real host's CPU count or ambient
    // `LOOM_BUILD_GATE_LOAD_THRESHOLD`.
    //
    // #4615: isolate the machine-wide build slot to a per-test tempdir so
    // this test never contends with a live daemon's real build slot.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (_origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {}", marker_file.display()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let state = MainHealthState::new();

    let first = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(1e9), || false);
    assert_eq!(
        first, None,
        "a saturated host must defer the first tick rather than running the gate command"
    );
    assert_eq!(
        std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count(),
        0,
        "no gate command must spawn while deferring"
    );
    assert_eq!(state.last_verdict_at(), None, "a deferral is not a verdict");

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

// ===================================================================
// Unevaluated outcome + transition (#3885, reclassified in #3974)
// ===================================================================

/// A dirty-tree helper matching the pre-#3974 `skipped(reason)` shape.
fn dirty(reason: &str) -> GateOutcome {
    GateOutcome::unevaluated(UnevaluatedClass::DirtyTree, reason)
}

#[test]
fn test_unevaluated_outcome_leaves_halt_flag_unchanged() {
    // From halted: an unevaluated tick must NOT clear the halt.
    let state = MainHealthState::new();
    state.set_halted(true);
    assert_eq!(apply_gate_outcome(&state, &dirty("dirty")), HealthTransition::Unevaluated);
    assert!(state.is_halted(), "unevaluated must not clear an existing halt");

    // From green: an unevaluated tick must NOT halt.
    let state = MainHealthState::new();
    assert_eq!(
        apply_gate_outcome(
            &state,
            &GateOutcome::unevaluated(UnevaluatedClass::GitFailure, "offline")
        ),
        HealthTransition::Unevaluated
    );
    assert!(!state.is_halted(), "unevaluated must not spuriously halt");
}

// ===================================================================
// Unevaluated-warn throttling (#3950 AC2 / AC3, extended in #3974):
// once per evaluated->unevaluated transition, once more on any change of
// failure class, then throttled; `is_unevaluated()` + the stored class
// back the "not evaluated" status surfaced to `loom-daemon status`.
// ===================================================================

#[test]
fn test_note_gate_tick_warns_once_on_transition_then_throttles() {
    let state = MainHealthState::new();
    let throttle = Duration::from_secs(3600);
    let d = Some((UnevaluatedClass::DirtyTree, "dirty"));

    // First dirty tick: clean -> dirty transition, must warn.
    assert!(state.note_gate_tick(d, throttle));
    assert!(state.is_unevaluated(), "status must reflect the skip");

    // Still dirty, well within the throttle window: must NOT warn again.
    assert!(!state.note_gate_tick(d, throttle));
    assert!(!state.note_gate_tick(d, throttle));
    assert!(state.is_unevaluated(), "still not-evaluated while throttled");
}

#[test]
fn test_note_gate_tick_warns_again_after_throttle_elapses() {
    let state = MainHealthState::new();
    // A throttle of ~0 means "always past the window" on the very next tick.
    let tiny_throttle = Duration::from_millis(1);
    let d = Some((UnevaluatedClass::DirtyTree, "dirty"));

    assert!(state.note_gate_tick(d, tiny_throttle), "first tick always warns");
    std::thread::sleep(Duration::from_millis(5));
    assert!(
        state.note_gate_tick(d, tiny_throttle),
        "a second dirty tick past the throttle window must warn again"
    );
}

#[test]
fn test_note_gate_tick_rewarns_immediately_on_class_change() {
    // #3974: the incident rotated through timeout / exit-101 / exit-127 /
    // git-fetch failure. A new failure class must never be swallowed by the
    // previous class's throttle window.
    let state = MainHealthState::new();
    let throttle = Duration::from_secs(3600);

    assert!(state.note_gate_tick(Some((UnevaluatedClass::DirtyTree, "dirty")), throttle));
    assert!(!state.note_gate_tick(Some((UnevaluatedClass::DirtyTree, "dirty")), throttle));
    assert!(
        state.note_gate_tick(Some((UnevaluatedClass::Timeout, "timed out")), throttle),
        "a different failure class must warn immediately, not stay throttled"
    );
    assert_eq!(state.unevaluated_class(), Some(UnevaluatedClass::Timeout));
}

#[test]
fn test_note_gate_tick_clears_on_recovery_and_rewarns_on_next_dirty_streak() {
    let state = MainHealthState::new();
    let throttle = Duration::from_secs(3600);
    let d = Some((UnevaluatedClass::DirtyTree, "dirty"));

    assert!(state.note_gate_tick(d, throttle));
    assert!(!state.note_gate_tick(d, throttle), "throttled mid-streak");

    // Tree becomes clean again (a completed Green/Red tick) — clears skip
    // status, the stored detail, and the throttle timer.
    assert!(!state.note_gate_tick(None, throttle));
    assert!(!state.is_unevaluated(), "no longer skipped after a completed tick");
    assert_eq!(state.unevaluated_class(), None);
    assert_eq!(state.unevaluated_summary(), None);

    // A NEW dirty streak must warn immediately again, not stay throttled
    // from the previous streak.
    assert!(
        state.note_gate_tick(d, throttle),
        "a fresh dirty streak must warn on its first tick"
    );
}

#[test]
fn test_note_gate_tick_never_warns_when_evaluated() {
    let state = MainHealthState::new();
    assert!(!state.note_gate_tick(None, Duration::from_secs(3600)));
    assert!(!state.is_unevaluated());
}

#[test]
fn test_unevaluated_summary_names_class_and_reason() {
    // #3974 AC2: status must be able to name the *actual* failure rather
    // than always claiming the workspace tree is dirty.
    let state = MainHealthState::new();
    state.note_gate_tick(
        Some((UnevaluatedClass::GitFailure, "`git fetch origin main` failed")),
        Duration::from_secs(3600),
    );
    let summary = state.unevaluated_summary().unwrap();
    assert!(summary.starts_with("git-failure: "), "got: {summary}");
    assert!(summary.contains("git fetch origin main"), "got: {summary}");
}

#[test]
fn test_unevaluated_summary_truncates_long_reasons() {
    let state = MainHealthState::new();
    let long = "x".repeat(MAX_STATUS_REASON_CHARS * 3);
    state.note_gate_tick(Some((UnevaluatedClass::Timeout, &long)), Duration::from_secs(3600));
    let summary = state.unevaluated_summary().unwrap();
    assert!(
        summary.chars().count() <= MAX_STATUS_REASON_CHARS + 32,
        "status reason must stay short, got {} chars",
        summary.chars().count()
    );
    assert!(summary.ends_with('…'), "truncation marker expected, got: {summary}");
}

#[test]
fn test_workspace_health_states_is_unevaluated_tracks_per_root() {
    let states = WorkspaceHealthStates::new();
    let root = Path::new("/repo/a");
    // Never-seen root: not skipped.
    assert!(!states.is_unevaluated(root));
    assert_eq!(states.unevaluated_summary(root), None);

    states
        .get_or_create(root)
        .note_gate_tick(Some((UnevaluatedClass::Timeout, "slow")), Duration::from_secs(3600));
    assert!(states.is_unevaluated(root));
    assert_eq!(states.unevaluated_summary(root), Some("timeout: slow".to_string()));
    assert!(
        !states.is_unevaluated(Path::new("/repo/b")),
        "a sibling root's skip state is independent"
    );
}

#[test]
fn test_unevaluated_outcome_helpers() {
    let s = GateOutcome::unevaluated(UnevaluatedClass::DirtyTree, "because reasons");
    assert!(s.is_unevaluated());
    assert!(!s.is_green());
    assert!(!s.is_verified_red());
    assert_eq!(s.unevaluated_class(), Some(UnevaluatedClass::DirtyTree));
    assert_eq!(s.detail(), "because reasons");
}

// ===================================================================
// Env-var configuration
// ===================================================================

#[test]
#[serial]
fn test_enabled_off_by_default() {
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    assert!(!enabled(), "unset ⇒ disabled (zero behavior change)");
}

#[test]
#[serial]
fn test_enabled_truthy_and_falsy() {
    for v in ["1", "true", "yes", "on", "TRUE", "On", " Yes "] {
        std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, v);
        assert!(enabled(), "{v:?} should enable");
    }
    for v in ["0", "false", "no", "off", "", "maybe"] {
        std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, v);
        assert!(!enabled(), "{v:?} should not enable");
    }
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_interval_default_and_override() {
    std::env::remove_var(MAIN_HEALTH_GATE_INTERVAL_ENV);
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS));

    std::env::set_var(MAIN_HEALTH_GATE_INTERVAL_ENV, "15");
    assert_eq!(resolve_interval(), Duration::from_secs(15));

    // Zero and unparseable fall back to the default.
    std::env::set_var(MAIN_HEALTH_GATE_INTERVAL_ENV, "0");
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS));
    std::env::set_var(MAIN_HEALTH_GATE_INTERVAL_ENV, "garbage");
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS));
    std::env::remove_var(MAIN_HEALTH_GATE_INTERVAL_ENV);
}

// ===================================================================
// Autonomous config surface — autonomous.mainHealthGate (#3813)
// ===================================================================

#[test]
fn test_autonomous_config_missing_file_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(read_autonomous_gate_config(tmp.path()), AutonomousGateConfig::default());
}

#[test]
fn test_autonomous_config_malformed_json_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "{not valid json");
    assert_eq!(read_autonomous_gate_config(tmp.path()), AutonomousGateConfig::default());
}

#[test]
fn test_autonomous_config_missing_block_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
    assert_eq!(read_autonomous_gate_config(tmp.path()), AutonomousGateConfig::default());
}

#[test]
fn test_autonomous_config_enabled_true_and_false() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
    assert_eq!(
        read_autonomous_gate_config(tmp.path()),
        AutonomousGateConfig {
            enabled: Some(true),
            ci_workflow: None,
            suppress_dispatch_during_gate: None,
        }
    );
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": false}}}"#);
    assert_eq!(
        read_autonomous_gate_config(tmp.path()),
        AutonomousGateConfig {
            enabled: Some(false),
            ci_workflow: None,
            suppress_dispatch_during_gate: None,
        }
    );
}

// ===================================================================
// #3987 — optional named forge verification workflow
// ===================================================================

#[test]
fn test_autonomous_config_ci_workflow_parsed() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"mainHealthGate": {"enabled": true, "ciWorkflow": "CI"}}}"#,
    );
    assert_eq!(
        read_autonomous_gate_config(tmp.path()),
        AutonomousGateConfig {
            enabled: Some(true),
            ci_workflow: Some("CI".to_string()),
            suppress_dispatch_during_gate: None,
        }
    );
}

#[test]
fn test_autonomous_config_ci_workflow_empty_and_whitespace_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    // Empty string ⇒ treated as unset.
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"ciWorkflow": ""}}}"#);
    assert_eq!(read_autonomous_gate_config(tmp.path()).ci_workflow, None);
    // Whitespace-only ⇒ trimmed away ⇒ unset.
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"ciWorkflow": "   "}}}"#);
    assert_eq!(read_autonomous_gate_config(tmp.path()).ci_workflow, None);
    // Leading/trailing whitespace on a real value is trimmed.
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"ciWorkflow": "  CI  "}}}"#);
    assert_eq!(read_autonomous_gate_config(tmp.path()).ci_workflow, Some("CI".to_string()));
}

#[test]
#[serial]
fn test_resolve_ci_workflow_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var(GATE_CI_WORKFLOW_ENV);

    // No env, no config ⇒ None (unanimity-only behavior preserved).
    assert_eq!(resolve_ci_workflow(tmp.path()), None);

    // Config alone is used when env is unset.
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"ciWorkflow": "Build"}}}"#);
    assert_eq!(resolve_ci_workflow(tmp.path()), Some("Build".to_string()));

    // Env overrides config.
    std::env::set_var(GATE_CI_WORKFLOW_ENV, "CI");
    assert_eq!(resolve_ci_workflow(tmp.path()), Some("CI".to_string()));

    // Empty/whitespace env falls through to config.
    std::env::set_var(GATE_CI_WORKFLOW_ENV, "   ");
    assert_eq!(resolve_ci_workflow(tmp.path()), Some("Build".to_string()));

    std::env::remove_var(GATE_CI_WORKFLOW_ENV);
}

#[test]
#[serial]
fn test_resolve_enabled_precedence() {
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);

    // Absent config + unset env ⇒ default off (Phase C opt-in preserved).
    assert!(!resolve_enabled(&AutonomousGateConfig::default()));

    // Config alone enables/disables when env is unset.
    assert!(resolve_enabled(&AutonomousGateConfig {
        enabled: Some(true),
        ..Default::default()
    }));
    assert!(!resolve_enabled(&AutonomousGateConfig {
        enabled: Some(false),
        ..Default::default()
    }));

    // Env overrides config in both directions (env is the master switch).
    std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, "1");
    assert!(resolve_enabled(&AutonomousGateConfig {
        enabled: Some(false),
        ..Default::default()
    }));
    std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, "0");
    assert!(!resolve_enabled(&AutonomousGateConfig {
        enabled: Some(true),
        ..Default::default()
    }));
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
}

#[test]
fn test_autonomous_config_suppress_dispatch_parsed() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"mainHealthGate": {"suppressDispatchDuringGate": false}}}"#,
    );
    assert_eq!(
        read_autonomous_gate_config(tmp.path()).suppress_dispatch_during_gate,
        Some(false)
    );
    write_config(
        tmp.path(),
        r#"{"autonomous": {"mainHealthGate": {"suppressDispatchDuringGate": true}}}"#,
    );
    assert_eq!(
        read_autonomous_gate_config(tmp.path()).suppress_dispatch_during_gate,
        Some(true)
    );
}

#[test]
#[serial]
fn test_resolve_suppress_dispatch_during_gate_precedence() {
    std::env::remove_var(MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV);

    // Absent config + unset env ⇒ default ON (#4084 default-true contract,
    // `DEFAULT_SUPPRESS_DISPATCH_DURING_GATE`).
    assert!(resolve_suppress_dispatch_during_gate(&AutonomousGateConfig::default()));

    // Config alone decides when env is unset (both directions).
    assert!(resolve_suppress_dispatch_during_gate(&AutonomousGateConfig {
        suppress_dispatch_during_gate: Some(true),
        ..Default::default()
    }));
    assert!(!resolve_suppress_dispatch_during_gate(&AutonomousGateConfig {
        suppress_dispatch_during_gate: Some(false),
        ..Default::default()
    }));

    // Env overrides config in both directions (env is the master switch).
    std::env::set_var(MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV, "1");
    assert!(resolve_suppress_dispatch_during_gate(&AutonomousGateConfig {
        suppress_dispatch_during_gate: Some(false),
        ..Default::default()
    }));
    std::env::set_var(MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV, "0");
    assert!(!resolve_suppress_dispatch_during_gate(&AutonomousGateConfig {
        suppress_dispatch_during_gate: Some(true),
        ..Default::default()
    }));
    std::env::remove_var(MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV);
}

// ===================================================================
// Effective enablement (#4012) — the "disabled" render signal
// ===================================================================

#[test]
#[serial]
fn test_effective_enabled_false_with_no_config_at_all() {
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    assert!(!effective_enabled(tmp.path()), "no config at all ⇒ disabled");
}

#[test]
#[serial]
fn test_effective_enabled_false_when_enabled_but_no_usable_build_gate() {
    // #4012's "edge case" test-plan item: a root that is nominally
    // enabled but has no usable `buildGate` block (or an empty command)
    // is treated by the gate loop as always-green and must ALSO report
    // as effectively disabled here -- not "pending" -- since nothing
    // will ever evaluate it until `buildGate` is actually configured.
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
    assert!(
        !effective_enabled(tmp.path()),
        "enabled=true with no buildGate block must still report disabled"
    );

    write_config(
        tmp.path(),
        r#"{"autonomous": {"mainHealthGate": {"enabled": true}}, "buildGate": {"enabled": true, "command": "   "}}"#,
    );
    assert!(
        !effective_enabled(tmp.path()),
        "enabled=true with an empty buildGate.command must still report disabled"
    );
}

#[test]
#[serial]
fn test_effective_enabled_true_with_both_signals_configured() {
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"mainHealthGate": {"enabled": true}}, "buildGate": {"enabled": true, "command": "true"}}"#,
    );
    assert!(effective_enabled(tmp.path()));
}

#[test]
#[serial]
fn test_effective_enabled_false_when_build_gate_present_but_autonomous_disabled() {
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"buildGate": {"enabled": true, "command": "true"}}"#);
    assert!(
            !effective_enabled(tmp.path()),
            "a usable buildGate block alone does not enable the gate -- autonomous.mainHealthGate must opt in too"
        );
}

#[test]
#[serial]
fn test_effective_enabled_env_master_switch_overrides_config() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"mainHealthGate": {"enabled": false}}, "buildGate": {"enabled": true, "command": "true"}}"#,
    );
    std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, "1");
    assert!(
        effective_enabled(tmp.path()),
        "the env master switch overrides a config that disables the loop"
    );
    std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
}

// ===================================================================
// Stale-forge-credential vs. genuinely-red main (#5630)
//
// The incident: the daemon's credential-refresh tick timed out under host
// saturation, every managed repo's forge/git calls started failing at
// once, and the gate read that fan-out as 22-of-22 red mains — halting
// dispatch host-wide, then un-halting on the next successful refresh.
// ===================================================================

fn run_gate_with_ci_and_credential(
    command: &str,
    verdict: CiVerdict,
    credential_stale: bool,
) -> GateOutcome {
    let (_origin, clone) = make_origin_and_clone();
    let cfg = BuildGateConfig {
        command: command.to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let mut runner = CommandGateRunner::new(cfg, clone.path().to_path_buf())
        .with_ci_status(Box::new(FakeCi {
            verdict,
            asked: Arc::new(Mutex::new(Vec::new())),
        }))
        .with_credential_freshness(Box::new(FixedCredential(credential_stale)));
    runner.run_gate()
}

#[test]
#[serial]
fn test_local_red_with_stale_credential_is_unevaluated_not_red() {
    // The heart of AC2: "CI unknown because our credentials are stale" is
    // NOT "main is red". A local failure produced while the daemon cannot
    // authenticate is not evidence about main.
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let outcome = run_gate_with_ci_and_credential("exit 1", CiVerdict::Unknown, true);
    assert!(
        !outcome.is_verified_red(),
        "a local red under a stale credential must not halt, got {outcome:?}"
    );
    assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::ForgeCredentialStale));
    assert!(
        outcome.detail().contains("STALE"),
        "the reason must name the credential, got: {}",
        outcome.detail()
    );
}

#[test]
#[serial]
fn test_local_red_with_fresh_credential_and_unknown_ci_still_halts() {
    // The guardrail on AC2: the #3974 fail-safe is untouched when the
    // credential is healthy. An unknown CI answer for a genuinely-failing
    // local run still halts — we did not trade a false halt for a missed
    // real one.
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let outcome = run_gate_with_ci_and_credential("exit 1", CiVerdict::Unknown, false);
    assert!(outcome.is_verified_red(), "got {outcome:?}");
    assert!(outcome.detail().contains("unavailable"), "got: {}", outcome.detail());
}

#[test]
#[serial]
fn test_red_forge_ci_with_stale_credential_does_not_halt() {
    // A stale credential short-circuits BEFORE the corroboration probe:
    // that probe is itself a `gh` call, so its answer under a dead token
    // is not trustworthy in either direction.
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let outcome = run_gate_with_ci_and_credential("exit 1", CiVerdict::Failure, true);
    assert_eq!(
        outcome.unevaluated_class(),
        Some(UnevaluatedClass::ForgeCredentialStale),
        "got {outcome:?}"
    );
}

#[test]
#[serial]
fn test_green_local_run_under_stale_credential_is_still_green() {
    // Only the red path consults the credential — a run that PASSED is
    // self-evidently not a credential artifact, so nothing changes.
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    let outcome = run_gate_with_ci_and_credential("exit 0", CiVerdict::Unknown, true);
    assert!(outcome.is_green(), "got {outcome:?}");
}

#[test]
fn test_stale_credential_outcome_holds_a_previous_green_and_a_previous_halt() {
    // "Holds the previous verdict" in both directions, which is the
    // property that stops `dispatch_halted` oscillating (AC3).
    let outcome =
        GateOutcome::unevaluated(UnevaluatedClass::ForgeCredentialStale, "credential stale");

    let was_green = MainHealthState::new();
    assert_eq!(apply_gate_outcome(&was_green, &outcome), HealthTransition::Unevaluated);
    assert!(!was_green.is_halted(), "a green repo must not be flipped to halted");

    let was_red = MainHealthState::new();
    was_red.set_halted(true);
    assert_eq!(apply_gate_outcome(&was_red, &outcome), HealthTransition::Unevaluated);
    assert!(
        was_red.is_halted(),
        "an already-halted repo must stay halted — a stale credential is not evidence \
             that main recovered either"
    );
}

#[test]
#[serial]
fn test_run_gate_tick_held_by_stale_credential_never_runs_the_command() {
    // The tick-level hold (AC2/AC3): with a stale credential the expensive
    // command does not run at all, the tick yields `None` (nothing to
    // apply), and the halt flag is untouched.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (_origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {}; exit 1", marker_file.display()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let state = MainHealthState::new();

    let held = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || true);
    assert_eq!(held, None, "a credential-held tick produces no verdict to apply");
    assert!(
        !marker_file.exists(),
        "the gate command must not run while the credential is stale"
    );
    assert!(!state.is_halted(), "a held tick must not flip the halt flag");
    assert!(
        state.is_unevaluated(),
        "status should read UNEVALUATED so an operator sees why the gate is quiet"
    );
    assert_eq!(state.unevaluated_class(), Some(UnevaluatedClass::ForgeCredentialStale));
    assert!(
        !state.gate_backoff_active(Instant::now()),
        "a credential hold must NOT arm the indeterminate backoff — evaluation has to \
             resume on the very next tick once the credential recovers"
    );

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

#[test]
#[serial]
fn test_run_gate_tick_evaluates_normally_once_the_credential_recovers() {
    // AC3's other half: the hold is not sticky. The very next tick after
    // recovery evaluates for real — so a flapping refresh tick produces no
    // dispatch oscillation, and a genuinely red main is still caught.
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());

    let (_origin, clone) = make_origin_and_clone();
    let cfg = BuildGateConfig {
        command: "exit 1".to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let state = MainHealthState::new();

    assert_eq!(run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || true), None);
    assert!(!state.is_halted());

    // Credential recovered; corroboration off so the local red stands.
    std::env::set_var(GATE_CI_CORROBORATION_ENV, "0");
    let after = run_gate_tick_with_fns(&state, &cfg, clone.path(), || Some(0.0), || false);
    std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    assert!(
        matches!(after, Some(GateOutcome::Red { .. })),
        "a genuinely red main must still be caught right after recovery, got {after:?}"
    );

    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}

#[test]
fn test_credential_hold_reason_is_none_when_credential_is_healthy() {
    assert!(credential_hold_reason(false).is_none());
    let reason = credential_hold_reason(true).expect("a stale credential yields a reason");
    assert!(reason.contains("STALE"), "{reason}");
    assert!(
        reason.contains("EVERY managed repo"),
        "the reason must explain the N-of-N signature: {reason}"
    );
}

/// #6663 AC4 — the one test in this module that deliberately drives the
/// **process-global** streak tracker, and therefore the only coverage of
/// the production wiring every other test now injects around:
/// `run_gate_tick_with_load_fn` -> `forge_credential_stale()` ->
/// `FORGE_CREDENTIAL_STREAKS`.
///
/// Pinning this here (rather than letting a sibling test's incidental
/// write supply it by accident) is what makes the injection elsewhere a
/// *reduction* in coupling rather than a loss of coverage: if a refactor
/// ever severed the global from the gate, this test — not eleven
/// unrelated ones — is what goes red.
///
/// `#[serial]` on the default key, and it resets the tracker on both
/// sides so it neither inherits poison from
/// `credential_preflight`'s `force_refresh_owner_credential_with_mint_error_is_a_no_op`
/// nor exports any of its own.
#[test]
#[serial]
fn test_global_credential_tracker_holds_a_real_tick_and_releases_it() {
    let slot_dir = tempfile::tempdir().unwrap();
    std::env::set_var(crate::build_slot::BUILD_SLOT_DIR_ENV, slot_dir.path());
    crate::credential_preflight::reset_forge_credential_streaks();

    let (_origin, clone) = make_origin_and_clone();
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("invocations.txt");
    let cfg = BuildGateConfig {
        command: format!("echo run >> {}", marker_file.display()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let runs = || {
        std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count()
    };

    // A healthy global tracker ⇒ the production default path evaluates
    // for real.
    let healthy = MainHealthState::new();
    let outcome = run_gate_tick_with_load_fn(&healthy, &cfg, clone.path(), || Some(0.0));
    assert!(
        matches!(outcome, Some(GateOutcome::Green { .. })),
        "an unpoisoned tracker must let the tick produce a real verdict, got {outcome:?}"
    );
    assert_eq!(runs(), 1, "the gate command must have run once");

    // Now record a failure on the tracker DELIBERATELY (what the daemon's
    // refresh tick does when a mint fails) and re-tick: the gate must hold.
    crate::credential_preflight::record_forge_credential_failure(
        "issue-6663 deliberate test source",
        "deliberately recorded failure",
    );
    let held_state = MainHealthState::new();
    let held = run_gate_tick_with_load_fn(&held_state, &cfg, clone.path(), || Some(0.0));
    assert_eq!(held, None, "a credential-held tick produces no verdict to apply");
    assert_eq!(runs(), 1, "the gate command must not run while the credential is stale");
    assert_eq!(
        held_state.unevaluated_class(),
        Some(UnevaluatedClass::ForgeCredentialStale),
        "the hold must be attributed to the credential, not to some other class"
    );
    assert!(
        !held_state.is_halted(),
        "a credential hold is not a verdict — it must never halt dispatch"
    );

    // Clearing the streak releases the hold on the very next tick.
    crate::credential_preflight::reset_forge_credential_streaks();
    let recovered = MainHealthState::new();
    let after = run_gate_tick_with_load_fn(&recovered, &cfg, clone.path(), || Some(0.0));
    assert!(
        matches!(after, Some(GateOutcome::Green { .. })),
        "evaluation must resume as soon as the tracker is healthy again, got {after:?}"
    );
    assert_eq!(runs(), 2);

    crate::credential_preflight::reset_forge_credential_streaks();
    std::env::remove_var(crate::build_slot::BUILD_SLOT_DIR_ENV);
}
