//! Emit heartbeat tests (#10898): aging on read with an injected clock.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap()
}

#[test]
fn a_pass_that_emits_stamps_the_emit_and_one_that_does_not_keeps_it() {
    let first = next(None, "h", t0(), true, 2, 5, true);
    assert_eq!(first.last_emit_at, Some(t0()));
    let later = t0() + Duration::minutes(5);
    let idle = next(Some(&first), "h", later, false, 2, 5, true);
    assert_eq!(idle.pass_at, later);
    assert_eq!(idle.last_emit_at, Some(t0()), "a silent pass must not refresh the emit");
    assert_eq!((idle.repos_covered, idle.open_prs), (2, 5));
}

#[test]
fn authority_silent_is_detected_after_the_threshold() {
    let hb = next(None, "h", t0(), true, 2, 5, true);
    let started = t0() - Duration::hours(10);
    let fresh = assess(Some(&hb), t0() + Duration::minutes(119), started, SILENT_AFTER);
    assert!(!fresh.silent);
    assert_eq!(fresh.age_secs, Some(119 * 60));
    let stale = assess(Some(&hb), t0() + Duration::minutes(121), started, SILENT_AFTER);
    assert!(stale.silent && stale.ever_emitted);
}

#[test]
fn a_restarted_authority_gets_a_fresh_window() {
    let hb = next(None, "h", t0(), true, 2, 5, true);
    let now = t0() + Duration::hours(31);
    let just_restarted = assess(Some(&hb), now, now - Duration::minutes(3), SILENT_AFTER);
    assert!(!just_restarted.silent);
    let long_running = assess(Some(&hb), now, t0() - Duration::hours(1), SILENT_AFTER);
    assert!(long_running.silent);
}

#[test]
fn never_emitted_is_silent_once_the_process_has_run_past_the_threshold() {
    let started = t0();
    let early = assess(None, started + Duration::hours(1), started, SILENT_AFTER);
    assert!(!early.silent && !early.ever_emitted);
    let late = assess(None, started + Duration::hours(3), started, SILENT_AFTER);
    assert!(late.silent && !late.ever_emitted);
    assert_eq!(last_emit_age_secs(None, t0()), None);
}

#[test]
fn the_heartbeat_round_trips_through_its_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read(dir.path()), None);
    let hb = next(None, "robb-studio", t0(), true, 2, 7, true);
    write(dir.path(), &hb);
    assert_eq!(read(dir.path()), Some(hb));
}
