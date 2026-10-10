//! Issue #10880: the attempt record's rules, with injected clocks. The tick
//! end to end is in `tests/floor_roll.rs` (`failed_roll`) and the file round
//! trip in `persisted_state/tests.rs`.

use super::*;
use chrono::TimeZone;

const TARGET: &str = "artifact:0.19.900:aaaa";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}

fn armed(target: &str, version: &str) -> RollAttempt {
    RollAttempt {
        target: target.to_string(),
        version: version.to_string(),
        tag: format!("v{version}"),
        source: "floor".to_string(),
        from_binary: "0.19.800+abc".to_string(),
        attempts: 1,
        first_armed_at: t0(),
        last_armed_at: t0(),
        not_before: None,
        last_failure: None,
    }
}

fn mins(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

#[test]
fn the_delay_is_fifteen_minutes_doubling_to_a_six_hour_ceiling() {
    let table: Vec<u64> = (1..=7).map(|n| delay(n).as_secs() / 60).collect();
    assert_eq!(table, [15, 30, 60, 120, 240, 360, 360]);
    assert_eq!(delay(u32::MAX), CEILING);
}

#[test]
fn the_same_target_counts_up_and_another_starts_over() {
    let mut guard = AttemptGuard::default();
    assert_eq!(guard.arm(t0(), armed(TARGET, "0.19.900")), None);
    let later = t0() + chrono::Duration::minutes(20);
    let previous = guard.arm(later, armed(TARGET, "0.19.900"));
    assert_eq!(previous.map(|r| r.attempts), Some(1));
    let rec = guard.record().unwrap();
    assert_eq!((rec.attempts, rec.first_armed_at, rec.last_armed_at), (2, t0(), later));

    // A newer release, and the same version re-published under a new sha.
    for other in ["artifact:0.19.901:aaaa", "artifact:0.19.900:bbbb"] {
        guard.arm(later, armed(TARGET, "0.19.900"));
        guard.arm(later, armed(other, "0.19.900"));
        assert_eq!(guard.record().unwrap().attempts, 1, "{other}");
        assert_eq!(guard.record().unwrap().first_armed_at, later, "{other}");
    }
}

#[test]
fn a_refused_arm_is_undone() {
    let mut guard = AttemptGuard::default();
    let previous = guard.arm(t0(), armed(TARGET, "0.19.900"));
    guard.undo(previous);
    assert_eq!(guard.record(), None);
}

#[test]
fn a_process_below_the_record_judges_the_attempt_failed() {
    let (now, now_utc) = (Instant::now(), t0());
    let mut rec = armed(TARGET, "0.19.900");
    rec.attempts = 2;
    let mut guard = AttemptGuard::default();
    let note = guard.restore(Some(rec), now, now_utc, "0.19.800");
    let rec = guard.record().unwrap();
    assert_eq!(rec.not_before, Some(now_utc + chrono::Duration::minutes(30)));
    assert!(rec.last_failure.as_deref().unwrap().contains("0.19.800"));
    assert!(note.contains("failed roll") && note.contains(TARGET), "{note}");
    assert_eq!(guard.hold_until(), now.checked_add(mins(30)));
    assert!(guard.holding(TARGET, now).is_some());
    assert!(guard.holding(TARGET, now + mins(30)).is_none(), "released at not_before");
    assert!(guard.holding("artifact:0.19.901:aaaa", now).is_none(), "another target");

    // A second load of the same judged attempt keeps its retry time.
    let saved = guard.record().cloned();
    let later = now_utc + chrono::Duration::minutes(10);
    guard.restore(saved, now + mins(10), later, "0.19.800");
    assert_eq!(
        guard.record().unwrap().not_before,
        Some(now_utc + chrono::Duration::minutes(30))
    );
}

#[test]
fn a_process_at_or_above_the_record_leaves_it_inert() {
    for running in ["0.19.900", "0.19.950"] {
        let mut guard = AttemptGuard::default();
        let note = guard.restore(Some(armed(TARGET, "0.19.900")), Instant::now(), t0(), running);
        assert!(note.contains("kept until"), "{note}");
        assert_eq!(guard.record().unwrap().not_before, None, "{running}");
        assert_eq!(guard.hold_until(), None, "{running}");
    }
}

#[test]
fn future_times_are_clamped_on_load() {
    let (now, now_utc) = (Instant::now(), t0());
    let mut rec = armed(TARGET, "0.19.900");
    rec.first_armed_at = now_utc + chrono::Duration::days(2);
    rec.last_armed_at = now_utc + chrono::Duration::days(1);
    rec.not_before = Some(now_utc + chrono::Duration::days(400));
    let mut guard = AttemptGuard::default();
    guard.restore(Some(rec), now, now_utc, "0.19.800");
    let rec = guard.record().unwrap();
    assert_eq!((rec.first_armed_at, rec.last_armed_at), (now_utc, now_utc));
    assert_eq!(rec.not_before, Some(now_utc + chrono::Duration::hours(6)));
    assert_eq!(guard.hold_until(), now.checked_add(CEILING));
}

#[test]
fn the_record_is_cleared_only_on_the_target_after_the_startup_grace() {
    let started = Instant::now();
    let mut guard = AttemptGuard::default();
    guard.set_started(started);
    guard.restore(Some(armed(TARGET, "0.19.900")), started, t0(), "0.19.900");
    let early = started + STARTUP_GRACE - Duration::from_secs(1);
    assert!(!guard.confirm(early, "0.19.900"), "inside the grace");
    assert!(!guard.confirm(started + STARTUP_GRACE, "0.19.800"), "below the target");
    assert!(!guard.confirm(started + STARTUP_GRACE, "not-a-version"));
    assert!(guard.record().is_some());
    assert!(guard.confirm(started + STARTUP_GRACE, "0.19.900"));
    assert_eq!(guard.record(), None);
    assert!(!guard.confirm(started + STARTUP_GRACE, "0.19.900"), "nothing left");
}

#[test]
fn the_alert_names_floor_running_target_attempts_failure_and_retry() {
    let held = RollHeld {
        floor: Some("0.19.850".to_string()),
        running: "0.19.800".to_string(),
        target: TARGET.to_string(),
        cause: "failed_roll".to_string(),
        attempts: 3,
        last_failure: Some("came back on 0.19.800".to_string()),
        next_retry: Some(t0()),
    };
    let text = held_note(&held);
    for part in [
        "FLOOR ROLL FAILING",
        "0.19.850",
        "0.19.800",
        TARGET,
        "3 attempt(s)",
        "came back on 0.19.800",
        "2026-10-09T12:00:00Z",
        "DISPATCH CONTINUES",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    let unfloored = held_note(&RollHeld {
        floor: None,
        next_retry: None,
        cause: "fetch_terminal".to_string(),
        ..held
    });
    assert!(unfloored.starts_with("ROLL HELD"), "{unfloored}");
    assert!(unfloored.contains("not retried until a new release"), "{unfloored}");
}
