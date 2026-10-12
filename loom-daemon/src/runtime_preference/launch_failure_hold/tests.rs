//! The hold table's rules, on the pure value type with an injected `now`
//! (#11453). The process-global wrappers are covered by the walk tests in
//! `runtime_preference::launch_failure_hold_tests` and the role-runner test.
use super::*;

const SECS: u64 = 600;
const N: u32 = 2;

fn key(root: &str, role: &str, tap: &str) -> Key {
    Key {
        root: PathBuf::from(root),
        role: role.into(),
        tap: tap.into(),
    }
}

fn doctor_zai() -> Key {
    key("/w/a", "doctor", "opencode:zai-flash")
}

fn live(holds: &mut LaunchFailureHolds, key: &Key, fp: u64, now: u64) -> bool {
    matches!(holds.lookup(key, || fp, now), Lookup::Live(_))
}

#[test]
fn the_hold_arms_at_exactly_n() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    assert_eq!(
        holds.record_failure(&k, "toolless-launch", 7, 100, N, SECS),
        Recorded::Counted { streak: 1 }
    );
    let Recorded::Armed(hold) = holds.record_failure(&k, "toolless-launch", 7, 200, N, SECS) else {
        panic!("the second identical failure trips");
    };
    assert_eq!(hold.class, "toolless-launch");
    assert_eq!(hold.streak, 2);
    assert_eq!((hold.armed_at, hold.until), (200, 200 + SECS));
    assert!(live(&mut holds, &k, 7, 201));
    let detail = hold.describe(250);
    assert!(detail.contains("toolless-launch x2"), "{detail}");
    assert!(detail.contains("armed=1970-01-01T00:03:20Z"), "{detail}");
    assert!(detail.contains("remaining=550s"), "{detail}");
}

#[test]
fn the_hold_does_not_arm_at_n_minus_one() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    for _ in 0..2 {
        holds.record_failure(&k, "exit-78", 7, 100, 3, SECS);
    }
    assert!(!live(&mut holds, &k, 7, 101));
    assert!(matches!(
        holds.record_failure(&k, "exit-78", 7, 102, 3, SECS),
        Recorded::Armed(_)
    ));
}

#[test]
fn a_different_signature_resets_the_streak() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    for (class, now) in [
        ("toolless-launch", 1),
        ("exit-78", 2),
        ("toolless-launch", 3),
    ] {
        assert!(
            matches!(
                holds.record_failure(&k, class, 7, now, N, SECS),
                Recorded::Counted { streak: 1 }
            ),
            "{class}"
        );
    }
    assert!(!live(&mut holds, &k, 7, 4));
}

#[test]
fn a_success_or_transient_failure_resets_the_streak() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    holds.record_failure(&k, "toolless-launch", 7, 1, N, SECS);
    assert_eq!(holds.reset(&k, 2), None, "no live hold to report");
    assert_eq!(
        holds.record_failure(&k, "toolless-launch", 7, 3, N, SECS),
        Recorded::Counted { streak: 1 }
    );
    // A reset of a live hold reports it, so the caller logs the clear once.
    holds.record_failure(&k, "toolless-launch", 7, 4, N, SECS);
    assert!(holds.reset(&k, 5).is_some());
    assert!(!live(&mut holds, &k, 7, 6));
    assert!(holds.is_empty());
}

#[test]
fn expiry_drops_the_hold_and_one_more_identical_failure_re_arms() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    holds.record_failure(&k, "toolless-launch", 7, 0, N, SECS);
    holds.record_failure(&k, "toolless-launch", 7, 0, N, SECS);
    assert!(live(&mut holds, &k, 7, SECS - 1));
    assert!(matches!(holds.lookup(&k, || 7, SECS), Lookup::Cleared(ClearReason::Expired, _)));
    assert_eq!(holds.lookup(&k, || 7, SECS), Lookup::None, "cleared exactly once");
    // The probe tick fails the same way: re-armed at once, streak retained.
    let Recorded::Armed(hold) = holds.record_failure(&k, "toolless-launch", 7, SECS, N, SECS)
    else {
        panic!("one more identical failure re-arms immediately");
    };
    assert_eq!(hold.streak, 3);
}

#[test]
fn a_fingerprint_mismatch_drops_the_hold() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    holds.record_failure(&k, "toolless-launch", 7, 0, N, SECS);
    holds.record_failure(&k, "toolless-launch", 7, 0, N, SECS);
    assert!(matches!(
        holds.lookup(&k, || 8, 1),
        Lookup::Cleared(ClearReason::ConfigChanged, _)
    ));
    assert_eq!(holds.lookup(&k, || 7, 2), Lookup::None);
    assert_eq!(
        holds.record_failure(&k, "toolless-launch", 8, 3, N, SECS),
        Recorded::Counted { streak: 1 },
        "a config change also restarts the streak"
    );
}

#[test]
fn zero_secs_never_arms() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    for now in 0..5 {
        assert_eq!(holds.record_failure(&k, "toolless-launch", 7, now, N, 0), Recorded::Disabled);
    }
    assert!(holds.is_empty());
}

#[test]
fn keys_are_independent_per_root_role_and_tap() {
    let mut holds = LaunchFailureHolds::default();
    let k = doctor_zai();
    holds.record_failure(&k, "toolless-launch", 7, 0, N, SECS);
    holds.record_failure(&k, "toolless-launch", 7, 0, N, SECS);
    assert!(live(&mut holds, &k, 7, 1));
    for other in [
        key("/w/b", "doctor", "opencode:zai-flash"),
        key("/w/a", "builder", "opencode:zai-flash"),
        key("/w/a", "doctor", "claude"),
        key("/w/a", "doctor", "opencode"),
    ] {
        assert_eq!(holds.lookup(&other, || 7, 1), Lookup::None, "{other:?}");
    }
    // A failure on another key never extends this streak, and vice versa.
    let b = key("/w/b", "doctor", "opencode:zai-flash");
    assert_eq!(
        holds.record_failure(&b, "toolless-launch", 7, 2, N, SECS),
        Recorded::Counted { streak: 1 }
    );
    assert_eq!(holds.len(), 2);
}

#[test]
fn the_fingerprint_follows_the_runtimes_block_and_the_tap() {
    let tap = Tap {
        runtime: "opencode".into(),
        model_profile: Some("zai-flash".into()),
    };
    let base = serde_json::json!({"runtimes": {"rolePreference": {"doctor": ["claude"]}}});
    let same = serde_json::json!({
        "runtimes": {"rolePreference": {"doctor": ["claude"]}},
        "autonomous": {"anything": true}
    });
    let changed = serde_json::json!({"runtimes": {"rolePreference": {"doctor": ["codex"]}}});
    assert_eq!(fingerprint(&base, &tap), fingerprint(&same, &tap), "only runtimes counts");
    assert_ne!(fingerprint(&base, &tap), fingerprint(&changed, &tap));
    assert_ne!(fingerprint(&base, &tap), fingerprint(&base, &Tap::runtime("opencode")));
}

#[test]
#[serial_test::serial]
fn the_env_knobs_read_their_overrides_and_default_otherwise() {
    let prior = [HOLD_SECS_ENV, THRESHOLD_ENV].map(|k| (k, std::env::var(k).ok()));
    std::env::remove_var(HOLD_SECS_ENV);
    std::env::remove_var(THRESHOLD_ENV);
    assert_eq!(hold_secs(), DEFAULT_HOLD_SECS);
    assert_eq!(threshold(), DEFAULT_THRESHOLD);
    std::env::set_var(HOLD_SECS_ENV, "0");
    std::env::set_var(THRESHOLD_ENV, "0");
    assert_eq!(hold_secs(), 0);
    assert_eq!(threshold(), 1, "a threshold below 1 reads as 1");
    std::env::set_var(HOLD_SECS_ENV, "soon");
    std::env::set_var(THRESHOLD_ENV, "5");
    assert_eq!(hold_secs(), DEFAULT_HOLD_SECS);
    assert_eq!(threshold(), 5);
    for (k, v) in prior {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
}
