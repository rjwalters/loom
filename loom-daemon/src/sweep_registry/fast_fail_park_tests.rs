//! Issue #11854: the widened fast-fail window, the 6 h ladder ceiling, the
//! park after three fast failures, and its reconciliation with quarantine.

use super::*;
use crate::sweep_registry::test_support::{backoff_registry, insert_dead_running_at};
use crate::telemetry::{ensure_failure_class, SweepResult, UNCLASSIFIED_CLASS};
use tempfile::tempdir;

const SIX_HOURS: u64 = 6 * 3600;

fn registry(dir: &std::path::Path) -> SweepRegistry {
    backoff_registry(dir, DEFAULT_DISPATCH_BACKOFF_BASE_SECS, DEFAULT_DISPATCH_BACKOFF_MAX_SECS)
}

/// The defaults the issue names: a 2-minute fast-fail window, a 6 h ceiling.
/// The quarantine's own insta-crash window is unchanged.
#[test]
fn defaults_are_a_two_minute_window_and_a_six_hour_ceiling() {
    assert_eq!(FAST_FAIL_WINDOW_SECS, 120);
    assert_eq!(DEFAULT_QUARANTINE_INSTA_CRASH_SECS, 60);
    assert_eq!(DEFAULT_DISPATCH_BACKOFF_MAX_SECS, SIX_HOURS);
    assert_eq!(DispatchBackoffConfig::default().max, Duration::from_secs(SIX_HOURS));
}

/// The window: under 2 min and not a clean exit; a larger configured
/// insta-crash window still widens it.
#[test]
fn the_fast_fail_window_is_two_minutes() {
    assert!(is_fast_failure(90, Some(1), 60));
    assert!(is_fast_failure(119, None, 60));
    assert!(!is_fast_failure(120, Some(1), 60));
    assert!(!is_fast_failure(90, Some(0), 60), "a clean exit is not a crash");
    assert!(is_fast_failure(200, Some(1), 300), "a wider configured window wins");
}

/// The doubling now climbs past 900 s and stops at 6 h.
#[test]
fn the_ladder_climbs_to_a_six_hour_ceiling() {
    let base = Duration::from_secs(60);
    let max = Duration::from_secs(SIX_HOURS);
    assert_eq!(backoff_delay(5, base, max), Duration::from_secs(960), "past the old 900 s cap");
    assert_eq!(backoff_delay(9, base, max), Duration::from_secs(15_360));
    assert_eq!(backoff_delay(10, base, max), max, "60 * 2^9 = 30720 clamps to 6 h");
    assert_eq!(backoff_delay(u32::MAX, base, max), max);
}

/// A live registry's streak reaches the 6 h ceiling (`record_dispatch_failure`
/// alone never parks), and a refused open-PR guard stays at 900 s.
#[test]
fn a_registry_streak_reaches_six_hours_but_open_pr_refusals_stay_at_900s() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    for _ in 0..12 {
        reg.record_dispatch_failure(7);
    }
    let remaining = reg.dispatch_backoff_remaining(7, Utc::now()).unwrap();
    assert!(remaining > Duration::from_secs(SIX_HOURS - 60), "{remaining:?}");
    assert!(remaining <= Duration::from_secs(SIX_HOURS));

    for _ in 0..12 {
        reg.record_open_pr_guard_backoff(8);
    }
    let remaining = reg.dispatch_backoff_remaining(8, Utc::now()).unwrap();
    assert!(remaining <= Duration::from_secs(900), "open-PR cap: {remaining:?}");
    assert!(remaining > Duration::from_secs(840), "{remaining:?}");
}

/// A sweep that dies 90 s in (outside the old 60 s window, inside 2 min) arms
/// the ladder; one past the window does not.
#[test]
fn a_ninety_second_failure_arms_the_ladder() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    insert_dead_running_at(&mut reg, 41, 0, Utc::now() - chrono::Duration::seconds(90));
    reg.reap_once();
    assert_eq!(reg.dispatch_failure_count(41), 1, "a 90 s death is a fast failure");
    assert!(reg.dispatch_backoff_remaining(41, Utc::now()).is_some());
    assert_eq!(reg.insta_crash_count(41), 0, "the quarantine's 60 s window is unchanged");

    insert_dead_running_at(&mut reg, 42, 0, Utc::now() - chrono::Duration::seconds(150));
    reg.reap_once();
    assert_eq!(reg.dispatch_failure_count(42), 0, "past 2 min is the watchdogs' remit");
}

/// The class a park names: the first non-empty of pre-flight, crash
/// classifier and no-op loop, else `unclassified`.
#[test]
fn the_park_class_falls_back_to_unclassified() {
    assert_eq!(fast_fail_class(None, None, None), UNCLASSIFIED_CLASS);
    assert_eq!(fast_fail_class(Some(" "), Some(""), None), UNCLASSIFIED_CLASS);
    assert_eq!(fast_fail_class(None, Some("exit-1"), Some("noop-loop:blocked")), "exit-1");
    assert_eq!(
        fast_fail_class(Some("preflight-no-cli-start"), Some("exit-1"), None),
        "preflight-no-cli-start"
    );
    assert_eq!(
        fast_fail_class(None, None, Some("noop-loop:human-gate")),
        "noop-loop:human-gate"
    );
}

/// The park rule: the threshold-th failure of an issue-attributable class.
#[test]
fn only_issue_attributable_classes_park_at_the_threshold() {
    assert!(!fast_fail_parks(2, 3, "exit-1"));
    assert!(fast_fail_parks(3, 3, "exit-1"));
    assert!(fast_fail_parks(4, 3, UNCLASSIFIED_CLASS));
    assert!(!fast_fail_parks(3, 0, "exit-1"), "a zero threshold never parks");
    for env in [
        "preflight-no-cli-start",
        "account-exhausted:rate-limited",
        "no-usable-account",
        "self-kill:bg-wait",
        "execution-error",
    ] {
        assert!(!fast_fail_parks(9, 3, env), "{env} is the environment's fault");
    }
}

/// The acceptance criterion: the third consecutive fast failure parks the
/// issue, through the quarantine, and the first two do not.
#[test]
fn the_third_consecutive_fast_failure_parks() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.charge_fast_failure("s1", 51, "exit-1", true);
    reg.charge_fast_failure("s2", 51, "exit-1", true);
    assert!(!reg.is_quarantined(51));
    reg.charge_fast_failure("s3", 51, "exit-1", true);
    assert!(reg.is_quarantined(51), "parked on the third");
    assert_eq!(reg.quarantine_generation(51), 1);
    assert_eq!(reg.dispatch_failure_count(51), 3);
}

/// The park writes `loom:blocked` with the park record and a comment that
/// names the class, under the quarantine's anchored marker.
#[test]
fn the_park_comment_names_the_failure_class() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh.log");
    let fake_gh =
        crate::sweep_registry::test_support::install_fake_gh_env_logger(dir.path(), &gh_log, "", 0);
    let mut reg = registry(dir.path());
    reg.config.skip_label_flip = false;
    reg.config.gh_bin = Some(fake_gh);
    for sweep in ["a", "b", "c"] {
        reg.charge_fast_failure(sweep, 52, "unclassified", true);
    }
    assert!(reg.is_quarantined(52));
    let log = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(log.contains("--add-label loom:blocked"), "{log}");
    assert!(log.contains("failure class `unclassified`"), "{log}");
    assert!(log.contains(QUARANTINE_COMMENT_MARKER), "{log}");
}

/// Environmental failures and open-PR yields climb the ladder but never park.
#[test]
fn environmental_failures_and_open_pr_yields_never_park() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    for sweep in ["a", "b", "c", "d"] {
        reg.charge_fast_failure(sweep, 61, "preflight-no-cli-start", true);
        reg.charge_fast_failure(sweep, 62, "exit-1", false);
    }
    assert_eq!(reg.dispatch_failure_count(61), 4);
    assert_eq!(reg.dispatch_failure_count(62), 4);
    assert!(!reg.is_quarantined(61));
    assert!(!reg.is_quarantined(62));
}

/// With quarantine disabled the ladder only backs off; it never parks.
#[test]
fn a_disabled_quarantine_disables_the_park() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.set_quarantine_config(QuarantineConfig {
        enabled: false,
        ..QuarantineConfig::default()
    });
    for sweep in ["a", "b", "c"] {
        reg.charge_fast_failure(sweep, 63, "exit-1", true);
    }
    assert!(!reg.is_quarantined(63));
    assert!(reg.dispatch_backoff_remaining(63, Utc::now()).is_some());
}

/// Quarantine parks first: the ladder sees the entry and does not park again
/// (the generation, which every park advances, stays at 1).
#[test]
fn the_ladder_does_not_re_park_a_quarantined_issue() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    for _ in 0..3 {
        reg.record_terminal_outcome(71, true);
    }
    assert!(reg.is_quarantined(71));
    assert_eq!(reg.quarantine_generation(71), 1);
    for sweep in ["a", "b", "c", "d"] {
        reg.charge_fast_failure(sweep, 71, "exit-1", true);
    }
    assert_eq!(reg.quarantine_generation(71), 1, "one park, not two");
}

/// The ladder parks first: the quarantine's own tally then crosses its
/// threshold on an issue that is already parked and does not park again.
#[test]
fn the_quarantine_does_not_re_park_a_ladder_parked_issue() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    for sweep in ["a", "b", "c"] {
        reg.charge_fast_failure(sweep, 72, "exit-1", true);
    }
    assert!(reg.is_quarantined(72));
    for _ in 0..3 {
        reg.record_terminal_outcome(72, true);
    }
    assert_eq!(reg.quarantine_generation(72), 1, "one park, not two");
}

/// End to end through `reap_once`: three 90 s deaths are outside the
/// quarantine's window, so only the ladder sees them — and it parks.
#[test]
fn three_reaped_ninety_second_deaths_park() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    for seq in 0..3 {
        insert_dead_running_at(&mut reg, 73, seq, Utc::now() - chrono::Duration::seconds(90));
        reg.reap_once();
    }
    assert_eq!(reg.dispatch_failure_count(73), 3);
    assert_eq!(reg.insta_crash_count(73), 0);
    assert!(reg.is_quarantined(73));
    assert_eq!(reg.quarantine_generation(73), 1, "the park's generation survives the reap");
}

/// End to end: three 30 s deaths feed both the ladder and the quarantine
/// tally, which cross together on the third — one park, not two.
#[test]
fn three_reaped_insta_crashes_park_once() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    for seq in 0..3 {
        insert_dead_running_at(&mut reg, 74, seq, Utc::now() - chrono::Duration::seconds(30));
        reg.reap_once();
    }
    assert_eq!(reg.dispatch_failure_count(74), 3);
    assert_eq!(reg.insta_crash_count(74), 3);
    assert!(reg.is_quarantined(74));
    assert_eq!(reg.quarantine_generation(74), 1, "one park, not two");
}

/// Every reported `failure` carries a class; nothing else is touched.
#[test]
fn a_failure_is_never_unclassified_by_omission() {
    assert_eq!(
        ensure_failure_class(SweepResult::Failure, None).as_deref(),
        Some(UNCLASSIFIED_CLASS)
    );
    assert_eq!(
        ensure_failure_class(SweepResult::Failure, Some("  ".into())).as_deref(),
        Some(UNCLASSIFIED_CLASS)
    );
    assert_eq!(
        ensure_failure_class(SweepResult::Failure, Some("exit-1".into())).as_deref(),
        Some("exit-1")
    );
    assert_eq!(ensure_failure_class(SweepResult::Success, None), None);
    assert_eq!(ensure_failure_class(SweepResult::Cancelled, None), None);
}
