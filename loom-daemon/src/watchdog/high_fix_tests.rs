//! Regression tests for the four HIGH findings on the watchdog port (#8086).
//!
//! In a sibling file because `mod.rs` is at the file-size threshold and these
//! are the newest addition (`.loom/docs/file-size-policy.md`).
//!
//! Each of these covers a fix the RETAINED SUITE could not reach:
//! sibling-script resolution (the suite always pins
//! `LOOM_WATCHDOG_RECOVER_CMD`), escalation when recovery is impossible (the
//! harness pins `AUTO_RECOVER=0` together with `ESCALATE=0`), and heartbeat
//! freshness on the socket-only liveness path. All three shipped without a
//! regression test the first time, which review flagged as the gap most likely
//! to regress silently.
//!
//! Each fix here was mutation-tested: deleting it fails these cases and only
//! these cases.

use super::*;

/// HIGH-2. `cli_dir` must prefer the ENTRY POINT's directory: sibling
/// scripts (loom-daemon-start.sh for bounded recovery, create-issue.sh for
/// the tier-3 escalation fallback) live beside the SCRIPT and never beside
/// `~/.local/bin/loom-daemon`.
///
/// Deriving it from `current_exe()` made bounded recovery REPORT-ONLY on
/// every default install — "no readable loom-daemon-start.sh beside this
/// watchdog" — while the real sibling sat next to the stub that had just
/// invoked it. The retained suite cannot see this: it always pins
/// LOOM_WATCHDOG_RECOVER_CMD.
#[test]
fn an_exported_entry_point_directory_wins() {
    let d = tempfile::tempdir().expect("tempdir");
    let got = cli_dir_from(d.path().to_str());
    assert_eq!(got, d.path(), "an exported, existing dir must win");
}

/// Set a file's mtime `secs` into the past. `libc::utimes` because
/// `std::fs` has no stable mtime setter and `filetime` is not a dependency.
fn backdate(path: &std::path::Path, secs: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let tv = libc::timeval {
        tv_sec: now - secs,
        tv_usec: 0,
    };
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
    // Safety: `c` is a valid NUL-terminated path and `times` is a valid
    // 2-element array for the duration of the call.
    let rc = unsafe { libc::utimes(c.as_ptr(), [tv, tv].as_ptr()) };
    assert_eq!(rc, 0, "utimes must succeed for this test to mean anything");
}

/// HIGH-4. A snapshot flipped to alive by socket corroboration must have
/// its heartbeat freshness RECOMPUTED. Without it a stale heartbeat was
/// reported as an absent one — "no heartbeat file … liveness-only OK",
/// exit 0 — while the file existed and was 1000s past a 300s threshold.
#[test]
fn a_flipped_snapshot_learns_its_heartbeat_is_stale() {
    use crate::daemon_install_state::HeartbeatFreshness;

    let d = tempfile::tempdir().expect("tempdir");
    let hb = d.path().join("daemon.heartbeat");
    std::fs::write(&hb, "x").expect("write");
    backdate(&hb, 1000);

    let mut snap = liveness::Snapshot {
        heartbeat_file: Some(hb.display().to_string()),
        heartbeat_stale_threshold_secs: Some(300),
        // The state socket_corroboration leaves behind: freshness was
        // never computed, because liveness was not established yet.
        heartbeat: None,
        heartbeat_age_secs: None,
        ..liveness::Snapshot::not_expected()
    };

    refresh_heartbeat(&mut snap);

    assert_eq!(
        snap.heartbeat,
        Some(HeartbeatFreshness::Stale),
        "a 1000s-old heartbeat against a 300s threshold is STALE, not absent"
    );
    assert!(snap.heartbeat_age_secs.is_some_and(|a| a >= 900), "and its age is reported");
}

#[test]
fn a_flipped_snapshot_with_a_fresh_heartbeat_is_not_reported_stale() {
    // The counterpart, so the fix cannot degrade to "always stale".
    use crate::daemon_install_state::HeartbeatFreshness;
    let d = tempfile::tempdir().expect("tempdir");
    let hb = d.path().join("daemon.heartbeat");
    std::fs::write(&hb, "x").expect("write");

    let mut snap = liveness::Snapshot {
        heartbeat_file: Some(hb.display().to_string()),
        heartbeat_stale_threshold_secs: Some(300),
        heartbeat: None,
        heartbeat_age_secs: None,
        ..liveness::Snapshot::not_expected()
    };
    refresh_heartbeat(&mut snap);
    assert_eq!(snap.heartbeat, Some(HeartbeatFreshness::Fresh));
}

#[test]
fn a_snapshot_with_no_heartbeat_file_is_left_alone() {
    // Absent really is absent: nothing to recompute, and inventing a
    // verdict would report a divergence that does not exist.
    let mut snap = liveness::Snapshot {
        heartbeat_file: None,
        heartbeat_stale_threshold_secs: Some(300),
        heartbeat: None,
        ..liveness::Snapshot::not_expected()
    };
    refresh_heartbeat(&mut snap);
    assert!(snap.heartbeat.is_none());
}

/// HIGH-1. Both of the shell's escalation conditions, and the boundary
/// between them.
#[test]
fn a_spent_budget_escalates() {
    assert_eq!(
        escalation_trigger(&recovery::Decision::BreakerOpen { attempts: 5 }, 9, 5),
        Some(EscalationTrigger::BreakerOpen)
    );
}

#[test]
fn recovery_being_impossible_escalates_once_the_outage_has_persisted() {
    // The condition the port originally dropped. With auto-recovery off,
    // or no runnable command, a confirmed outage filed NO forge issue —
    // only a log line every tick, forever.
    assert_eq!(
        escalation_trigger(&recovery::Decision::Disabled, 2, 2),
        Some(EscalationTrigger::RecoveryImpossible),
        "at the threshold, the shell escalates"
    );
    assert_eq!(
        escalation_trigger(&recovery::Decision::Disabled, 7, 2),
        Some(EscalationTrigger::RecoveryImpossible)
    );
}

#[test]
fn recovery_being_impossible_does_not_escalate_on_the_first_ticks() {
    // The counterpart: escalating immediately would file an issue for
    // every transient blip. The shell waits the same number of ticks the
    // attempt budget would have allowed.
    assert_eq!(escalation_trigger(&recovery::Decision::Disabled, 1, 2), None);
    assert_eq!(escalation_trigger(&recovery::Decision::Disabled, 0, 5), None);
}

#[test]
fn a_tick_that_still_has_options_never_escalates() {
    // Escalation is the "nothing automatic is left to try" signal. A
    // pending attempt or an armed backoff still has something to try, so
    // filing now would page an operator for an outage the watchdog is
    // about to fix itself.
    assert_eq!(escalation_trigger(&recovery::Decision::Attempt { attempt: 1 }, 9, 2), None);
    assert_eq!(
        escalation_trigger(
            &recovery::Decision::BackingOff {
                next_attempt: 2,
                remaining: 30
            },
            9,
            2
        ),
        None
    );
}

#[test]
fn a_value_that_cannot_hold_siblings_is_ignored() {
    // A stale or mistyped export must not send sibling resolution somewhere
    // that cannot contain siblings. Falling back is safer than trusting it,
    // and matches a direct `loom-daemon daemon-watchdog` invocation, where
    // there is no stub and no siblings to find.
    let d = tempfile::tempdir().expect("tempdir");
    let f = d.path().join("not-a-dir");
    std::fs::write(&f, "x").expect("write");

    let fallback = cli_dir_from(None);
    assert_eq!(cli_dir_from(f.to_str()), fallback, "a file is not a cli dir");
    assert_eq!(cli_dir_from(Some("")), fallback, "empty is not a cli dir");
    assert_eq!(
        cli_dir_from(Some("/nonexistent/loom/cli")),
        fallback,
        "a missing dir is not a cli dir"
    );
}
