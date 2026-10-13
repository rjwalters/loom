//! The launch-failure hold drives real fall-through in the role-tick walk and
//! nowhere else (#11453): with `rolePreference.doctor = [opencode:zai-flash,
//! claude]`, a selectable `zai` seat and a healthy Claude pool, a role tick
//! chooses `opencode:zai-flash` until two identical deterministic launch
//! failures arm the hold, then `claude` until the hold clears. A sweep
//! dispatch, a probe, another workspace, another role and an operator pin are
//! all unaffected.
//!
//! Hermetic: [`ClearedRuntimeEnv`] clears `LOOM_RUNTIME*`, `ZAI_API_KEY` and
//! friends (#10767); [`HoldEnv`] additionally pins the hold's own knobs and
//! resets the process-global table on both ends.
use super::launch_failure_hold::{self as hold, Key};
use super::tests::{fixture, provision_claude_pool, write_config, ClearedRuntimeEnv};
use super::unprovisioned_tests::provision_zai_pool;
use super::{resolve_runtime, resolve_runtime_for, Decision, DispatchContext, Intent, Tap};
use std::path::Path;

struct HoldEnv {
    prior: Vec<(&'static str, Option<String>)>,
    _cleared: ClearedRuntimeEnv,
}

impl HoldEnv {
    fn new() -> Self {
        let cleared = ClearedRuntimeEnv::new();
        let prior = [hold::HOLD_SECS_ENV, hold::THRESHOLD_ENV]
            .map(|key| {
                let prior = std::env::var(key).ok();
                std::env::remove_var(key);
                (key, prior)
            })
            .to_vec();
        hold::reset_for_tests();
        Self {
            prior,
            _cleared: cleared,
        }
    }
}

impl Drop for HoldEnv {
    fn drop(&mut self) {
        hold::reset_for_tests();
        for (key, value) in self.prior.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn zai() -> Tap {
    Tap {
        runtime: "opencode".into(),
        model_profile: Some("zai-flash".into()),
    }
}

/// A workspace whose Doctor and Builder both prefer `opencode:zai-flash`
/// over `claude`, with both taps selectable.
fn workspace() -> tempfile::TempDir {
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    provision_zai_pool(dir.path(), true);
    let list = serde_json::json!([{"runtime": "opencode", "modelProfile": "zai-flash"}, "claude"]);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"doctor": list, "builder": list}}}),
    );
    dir
}

fn role_tick() -> DispatchContext<'static> {
    DispatchContext {
        complexity: None,
        intent: Intent::Probe,
        role_tick: true,
    }
}

/// The walk's chosen tap, and every skipped tap's rendering.
fn walk(root: &Path, role: &str, now: u64, context: DispatchContext<'_>) -> (String, Vec<String>) {
    let Ok(Decision::Preference { resolution, .. }) =
        resolve_runtime_for(root, role, None, now, context)
    else {
        panic!("expected the preference path");
    };
    let skipped = resolution.skipped.iter().map(ToString::to_string).collect();
    (resolution.chosen.unwrap().tap.to_string(), skipped)
}

/// One failed launch of `tap` for `role` in `root`, fed exactly as the role
/// runner's `hold_feed` feeds it.
fn fail(root: &Path, role: &str, tap: &Tap, class: &str, now: u64) -> hold::Recorded {
    let config = crate::config_resolver::resolve_effective_config(root);
    let key = Key::new(root, role, tap);
    hold::record_failure(&key, class, hold::fingerprint(&config, tap), now)
}

/// Acceptance 1 and the walk half of acceptance 7/8: two `toolless-launch`
/// failures route W's next Doctor tick to `claude`, with a skip reason naming
/// the class; a sweep/probe walk, another workspace and another role still
/// choose `opencode:zai-flash`.
#[test]
#[serial_test::serial]
fn two_toolless_launches_route_the_role_tick_to_the_next_tap_and_nothing_else() {
    let _env = HoldEnv::new();
    let w = workspace();
    let other = workspace();
    assert_eq!(walk(w.path(), "doctor", 1_000, role_tick()).0, "opencode:zai-flash");

    assert!(matches!(
        fail(w.path(), "doctor", &zai(), "toolless-launch", 1_000),
        hold::Recorded::Counted { streak: 1 }
    ));
    assert_eq!(
        walk(w.path(), "doctor", 1_001, role_tick()).0,
        "opencode:zai-flash",
        "one failure is not a trip"
    );
    assert!(matches!(
        fail(w.path(), "doctor", &zai(), "toolless-launch", 1_001),
        hold::Recorded::Armed(_)
    ));

    let (chosen, skipped) = walk(w.path(), "doctor", 1_002, role_tick());
    assert_eq!(chosen, "claude");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(skipped[0].contains("unavailable(launch-failure-hold: "), "{skipped:?}");
    assert!(skipped[0].contains("toolless-launch x2"), "{skipped:?}");
    assert!(skipped[0].contains("remaining=1799s"), "{skipped:?}");

    // Sweeps and probes never consult it.
    let Ok(Decision::Preference { resolution, .. }) =
        resolve_runtime(w.path(), "doctor", None, 1_002)
    else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.unwrap().tap.to_string(), "opencode:zai-flash");
    let sweep = DispatchContext {
        role_tick: false,
        ..role_tick()
    };
    assert_eq!(walk(w.path(), "doctor", 1_002, sweep).0, "opencode:zai-flash");
    // Another workspace, another role: untouched.
    assert_eq!(walk(other.path(), "doctor", 1_002, role_tick()).0, "opencode:zai-flash");
    assert_eq!(walk(w.path(), "builder", 1_002, role_tick()).0, "opencode:zai-flash");
}

/// Acceptance 8: `resolve_for_dispatch` (the sweep entry point) ignores an
/// armed hold.
#[test]
#[serial_test::serial]
fn a_sweep_dispatch_ignores_an_armed_hold() {
    let _env = HoldEnv::new();
    let w = workspace();
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap();
    fail(w.path(), "doctor", &zai(), "exit-78", now);
    fail(w.path(), "doctor", &zai(), "exit-78", now);
    assert_eq!(walk(w.path(), "doctor", now + 1, role_tick()).0, "claude");
    let admission = super::resolve_for_dispatch(w.path(), "doctor", None).unwrap();
    let admitted = admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "opencode");
    assert_eq!(admitted.preference.unwrap().tap, "opencode:zai-flash");
}

/// Acceptance 9: a pinned role never reaches the walk, so it is not re-routed.
#[test]
#[serial_test::serial]
fn a_pinned_role_is_not_re_routed() {
    let _env = HoldEnv::new();
    let w = workspace();
    fail(w.path(), "doctor", &zai(), "toolless-launch", 1_000);
    fail(w.path(), "doctor", &zai(), "toolless-launch", 1_000);
    std::env::set_var("LOOM_RUNTIME_DOCTOR", "opencode");
    let decision = resolve_runtime_for(w.path(), "doctor", None, 1_001, role_tick()).unwrap();
    std::env::remove_var("LOOM_RUNTIME_DOCTOR");
    let Decision::Static { result, .. } = decision else {
        panic!("a pin takes the static path, which never reads the hold");
    };
    assert_eq!(result.unwrap().runtime, "opencode");
}

/// Acceptance 5: expiry re-tests the tap (and one more identical failure
/// re-arms at once); a success clears; a `runtimes` change clears.
#[test]
#[serial_test::serial]
fn the_hold_clears_on_expiry_success_and_config_change() {
    let _env = HoldEnv::new();
    let w = workspace();
    let arm = |now| {
        fail(w.path(), "doctor", &zai(), "toolless-launch", now);
        fail(w.path(), "doctor", &zai(), "toolless-launch", now);
    };
    arm(0);
    assert_eq!(walk(w.path(), "doctor", 1_799, role_tick()).0, "claude");
    assert_eq!(walk(w.path(), "doctor", 1_800, role_tick()).0, "opencode:zai-flash");
    assert!(
        matches!(
            fail(w.path(), "doctor", &zai(), "toolless-launch", 1_800),
            hold::Recorded::Armed(_)
        ),
        "the probe's identical failure re-arms immediately"
    );
    assert_eq!(walk(w.path(), "doctor", 1_801, role_tick()).0, "claude");

    hold::record_reset(&Key::new(w.path(), "doctor", &zai()), hold::ClearReason::Success, 1_802);
    assert_eq!(walk(w.path(), "doctor", 1_803, role_tick()).0, "opencode:zai-flash");

    arm(2_000);
    assert_eq!(walk(w.path(), "doctor", 2_001, role_tick()).0, "claude");
    let list = serde_json::json!([{"runtime": "opencode", "modelProfile": "zai-flash"}, "claude"]);
    write_config(w.path(), &serde_json::json!({"runtimes": {"rolePreference": {"doctor": list}}}));
    assert_eq!(
        walk(w.path(), "doctor", 2_002, role_tick()).0,
        "opencode:zai-flash",
        "a runtimes config change drops the hold"
    );
}

/// Acceptance 2: a tap that fails deterministically forever launches at most
/// `N` times before the first trip and once per cooldown after — with the
/// defaults, at most 3 launches in the first 30 min and about 50 in 24 h,
/// against one per tick (288 at a 5-minute cadence) before this hold.
#[test]
#[serial_test::serial]
fn a_permanently_broken_tap_launches_a_bounded_number_of_times() {
    let _env = HoldEnv::new();
    let w = workspace();
    let interval = 300;
    let mut launches = Vec::new();
    for now in (0..86_400).step_by(interval) {
        if walk(w.path(), "doctor", now, role_tick()).0 == "opencode:zai-flash" {
            launches.push(now);
            fail(w.path(), "doctor", &zai(), "toolless-launch", now);
        }
    }
    let first_half_hour = launches.iter().filter(|t| **t < 1_800).count();
    assert!(first_half_hour <= 3, "{launches:?}");
    assert!(launches.len() <= 50, "{} launches: {launches:?}", launches.len());
    assert!(launches.len() >= 40, "the tap is still re-tested every cooldown: {launches:?}");
}

/// Acceptances 3 and 4 through the walk: transient failures and alternating
/// deterministic classes never trip.
#[test]
#[serial_test::serial]
fn transient_and_alternating_failures_never_trip() {
    let _env = HoldEnv::new();
    let w = workspace();
    let key = Key::new(w.path(), "doctor", &zai());
    for now in 0..6 {
        // A transient failure resets: this is what hold_feed does for one.
        hold::record_reset(&key, hold::ClearReason::TransientFailure, now);
        assert_eq!(walk(w.path(), "doctor", now, role_tick()).0, "opencode:zai-flash");
    }
    for (now, class) in [
        (10, "toolless-launch"),
        (11, "exit-78"),
        (12, "toolless-launch"),
    ] {
        fail(w.path(), "doctor", &zai(), class, now);
        assert_eq!(walk(w.path(), "doctor", now, role_tick()).0, "opencode:zai-flash");
    }
}

/// Acceptance 10: `LOOM_LAUNCH_FAILURE_HOLD_SECS=0` records nothing and the
/// walk is unchanged.
#[test]
#[serial_test::serial]
fn zero_secs_disables_the_feature() {
    let _env = HoldEnv::new();
    let w = workspace();
    std::env::set_var(hold::HOLD_SECS_ENV, "0");
    for now in 0..5 {
        assert_eq!(
            fail(w.path(), "doctor", &zai(), "toolless-launch", now),
            hold::Recorded::Disabled
        );
        let (chosen, skipped) = walk(w.path(), "doctor", now, role_tick());
        assert_eq!(chosen, "opencode:zai-flash");
        assert!(skipped.is_empty(), "{skipped:?}");
    }
}
