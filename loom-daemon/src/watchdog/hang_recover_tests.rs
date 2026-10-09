//! #7855 regression tests: decision, durable state, and stubbed supervisor
//! commands. Nothing here restarts a daemon or touches real host state — every
//! path is a tempdir and every supervisor command is a recording closure.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use super::*;

const SVC: &str = "gui/501/com.rjwalters.loom-daemon";
const PID: u32 = 4242;

fn on() -> Settings {
    Settings::resolve(|k| (k == ENV_ENABLE).then(|| "1".to_string()), |_| None)
}

fn stale() -> HeartbeatSignal {
    classify_heartbeat(Some(HeartbeatFreshness::Stale), Some(1500), Some(300), Some(5000))
}

fn launchd_argv() -> Result<Vec<String>, String> {
    restart_argv(Source::Launchd, Some(SVC), &format!("launchd job {SVC} alive (pid {PID})"), PID)
}

struct Fixture {
    _d: tempfile::TempDir,
    streak: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let d = tempfile::tempdir().expect("tempdir");
    let streak = d.path().join("loom").join(".watchdog-hang-streak");
    let state = d.path().join("loom").join(".watchdog-hang-recover-state");
    Fixture {
        _d: d,
        streak,
        state,
    }
}

fn ev() -> Evidence<'static> {
    Evidence {
        pid: PID,
        ipc_detail: "'loom-daemon quarantine list' did NOT return within the 15s probe budget",
        ipc_streak: 5,
        ipc_threshold: 3,
        load: "16.80, 15.20, 14.00",
    }
}

/// One CONFIRMED tick; returns the outcome and every argv the "supervisor" saw.
fn tick(
    f: &Fixture,
    s: &Settings,
    signal: &HeartbeatSignal,
    argv: Result<Vec<String>, String>,
    now: u64,
) -> (Outcome, Vec<Vec<String>>) {
    let calls = RefCell::new(Vec::new());
    let out = on_confirmed(
        s,
        &f.streak,
        &f.state,
        signal,
        argv,
        &ev(),
        now,
        || Ok(()),
        |a| {
            calls.borrow_mut().push(a.to_vec());
            Some(0)
        },
    );
    (out, calls.into_inner())
}

// ---------------------------------------------------------------- settings

#[test]
fn hang_recovery_is_off_by_default_and_on_only_by_explicit_opt_in() {
    let s = Settings::resolve(|_| None, |_| None);
    assert!(!s.enabled);
    assert_eq!(s.source, "default");
    assert_eq!(s.confirmations, 3);
    assert_eq!(s.cooldown_secs, 1800);

    // Env wins over the marker, in both directions.
    let m_on = |k: &str| (k == MARKER_ENABLE).then(|| "true".to_string());
    assert!(Settings::resolve(|_| None, m_on).enabled);
    assert_eq!(Settings::resolve(|_| None, m_on).source, "marker");
    let env_off = |k: &str| (k == ENV_ENABLE).then(|| "0".to_string());
    assert!(!Settings::resolve(env_off, m_on).enabled);
    // An unrecognised env value is not a decision; the marker stands.
    let typo = |k: &str| (k == ENV_ENABLE).then(|| "ture".to_string());
    assert!(Settings::resolve(typo, m_on).enabled);
    assert!(!Settings::resolve(typo, |_| None).enabled);
}

#[test]
fn the_rulings_bounds_are_floors_not_just_defaults() {
    let low = |k: &str| match k {
        ENV_CONFIRMATIONS => Some("1".to_string()),
        ENV_COOLDOWN => Some("60".to_string()),
        ENV_MAX_UNHEALED => Some("0".to_string()),
        _ => None,
    };
    let s = Settings::resolve(low, |_| None);
    assert_eq!(s.confirmations, 3, "at least three CONFIRMED ticks");
    assert_eq!(s.cooldown_secs, 1800, "at most one restart per >=30 min");
    assert_eq!(s.max_unhealed, 1);
    let high = |k: &str| match k {
        MARKER_CONFIRMATIONS => Some("5".to_string()),
        MARKER_COOLDOWN => Some("3600".to_string()),
        _ => None,
    };
    let s = Settings::resolve(|_| None, high);
    assert_eq!((s.confirmations, s.cooldown_secs), (5, 3600));
}

// --------------------------------------------------------------- decisions

#[test]
fn default_off_is_report_only_and_writes_no_state() {
    let f = fixture();
    let off = Settings::resolve(|_| None, |_| None);
    for now in [10_000, 20_000, 30_000, 40_000] {
        let (out, calls) = tick(&f, &off, &stale(), launchd_argv(), now);
        assert!(calls.is_empty(), "default OFF must never call the supervisor");
        assert!(out.restart_line.is_none());
        assert!(out.note.contains("No automatic kill/restart"), "{}", out.note);
        assert!(out.note.contains("REPORT-ONLY"), "{}", out.note);
        assert!(out.note.contains("heavy legitimate load"), "{}", out.note);
    }
    assert!(!f.streak.exists() && !f.state.exists(), "an unchanged host keeps no new state");
}

#[test]
fn an_enabled_dual_signal_sequence_restarts_once_on_the_third_tick() {
    let f = fixture();
    let s = on();
    let (o1, c1) = tick(&f, &s, &stale(), launchd_argv(), 1_000);
    let (o2, c2) = tick(&f, &s, &stale(), launchd_argv(), 1_300);
    assert!(c1.is_empty() && c2.is_empty());
    assert!(o1.note.contains("tick 1 of 3"), "{}", o1.note);
    assert!(o2.note.contains("tick 2 of 3"), "{}", o2.note);

    let (o3, c3) = tick(&f, &s, &stale(), launchd_argv(), 1_600);
    assert_eq!(c3, vec![vec!["launchctl", "kickstart", "-k", SVC]]);
    let line = o3.restart_line.expect("an evidence line for the restart");
    for want in [
        "HANG RECOVERY",
        "launchctl kickstart -k",
        "wedged pid 4242",
        "exited 0",
        "did NOT return within the 15s probe budget",
        "5 consecutive failed round-trips, threshold 3",
        "1500s old > 300s threshold",
        "for 3 consecutive CONFIRMED ticks",
        "16.80, 15.20, 14.00",
        "1800s",
        "opt-in via env",
    ] {
        assert!(line.contains(want), "missing {want:?} in {line}");
    }
    assert!(!f.streak.exists(), "a restart ends the streak");
    assert_eq!(read_state(&f.state).last_attempt_at, 1_600);
}

#[test]
fn failed_ipc_with_a_fresh_heartbeat_never_counts_and_resets_the_streak() {
    let f = fixture();
    let s = on();
    tick(&f, &s, &stale(), launchd_argv(), 1_000);
    tick(&f, &s, &stale(), launchd_argv(), 1_300);
    let fresh =
        classify_heartbeat(Some(HeartbeatFreshness::Fresh), Some(20), Some(300), Some(5000));
    let (out, calls) = tick(&f, &s, &fresh, launchd_argv(), 1_600);
    assert!(calls.is_empty());
    assert!(out.note.contains("FRESH"), "{}", out.note);
    assert!(!f.streak.exists(), "the streak is reset, not merely paused");
    // The next stale tick starts again from 1.
    let (out, _) = tick(&f, &s, &stale(), launchd_argv(), 1_900);
    assert!(out.note.contains("tick 1 of 3"), "{}", out.note);
}

#[test]
fn missing_unreadable_prior_boot_and_unprovable_heartbeats_are_not_evidence() {
    let cases = [
        (Some(HeartbeatFreshness::Unknown), None, Some(5000), "no heartbeat file"),
        (Some(HeartbeatFreshness::Unknown), Some(9), Some(5000), "unreadable"),
        (None, None, Some(5000), "no heartbeat file"),
        (Some(HeartbeatFreshness::PriorBoot), Some(9000), Some(30), "PREVIOUS boot"),
        // `check_heartbeat` says Stale when it cannot read the process age;
        // that is not proof of a CURRENT-boot heartbeat.
        (Some(HeartbeatFreshness::Stale), Some(1500), None, "process age is unreadable"),
        // Older than the process: not this boot's heartbeat.
        (Some(HeartbeatFreshness::Stale), Some(1500), Some(1000), "current boot"),
    ];
    for (fr, age, proc_age, why) in cases {
        match classify_heartbeat(fr, age, Some(300), proc_age) {
            HeartbeatSignal::NotEvidence(r) => assert!(r.contains(why), "{fr:?}: {r}"),
            HeartbeatSignal::StaleCurrentBoot { .. } => panic!("{fr:?}/{age:?} must not count"),
        }
    }
    let f = fixture();
    let s = on();
    for _ in 0..5 {
        let prior = classify_heartbeat(
            Some(HeartbeatFreshness::PriorBoot),
            Some(9000),
            Some(300),
            Some(30),
        );
        assert!(tick(&f, &s, &prior, launchd_argv(), 1_000).1.is_empty());
    }
}

#[test]
fn a_healthy_round_trip_between_stale_ticks_interrupts_the_streak() {
    // "Stale heartbeat with healthy IPC": the Healthy branch of `ipc_probe`
    // calls exactly these two, so a stale heartbeat alone never accumulates.
    let f = fixture();
    let s = on();
    tick(&f, &s, &stale(), launchd_argv(), 1_000);
    tick(&f, &s, &stale(), launchd_argv(), 1_300);
    reset_streak(&f.streak);
    on_healthy(&f.state);
    let (out, calls) = tick(&f, &s, &stale(), launchd_argv(), 1_600);
    assert!(calls.is_empty());
    assert!(out.note.contains("tick 1 of 3"), "{}", out.note);
}

#[test]
fn startup_grace_skips_the_probe_so_it_cannot_feed_the_streak() {
    // A process younger than the grace window is Skipped — never Unresponsive
    // — and the Skipped branch resets the dual-signal streak.
    let attempt = super::super::probe::Attempt::Ran {
        rc: 124,
        output: String::new(),
        bin: "loom-daemon".to_string(),
    };
    let (verdict, _) =
        super::super::probe::classify_ipc(&attempt, Some(10), 90, Path::new("/tmp/x.sock"), 15);
    assert_eq!(verdict, super::super::probe::IpcVerdict::Skipped);
}

#[test]
fn the_streak_is_keyed_to_the_pid() {
    let f = fixture();
    assert_eq!(advance_streak(&f.streak, 1, &stale()), 1);
    assert_eq!(advance_streak(&f.streak, 1, &stale()), 2);
    assert_eq!(advance_streak(&f.streak, 2, &stale()), 1, "a new process starts fresh");
}

#[test]
fn an_absent_marker_or_an_active_drain_trips_the_guard_and_spends_nothing() {
    let d = tempfile::tempdir().expect("tempdir");
    let marker = d.path().join("autonomy-desired");
    assert!(intent_guard(&marker)
        .unwrap_err()
        .contains("marker is gone"));
    std::fs::write(&marker, "x").expect("marker");
    assert!(intent_guard(&marker).is_ok());
    std::fs::write(crate::operator_stop::record_path(&marker), "at=now\n").expect("stop");
    assert!(intent_guard(&marker).unwrap_err().contains("operator stop"));

    let f = fixture();
    let ran = AtomicUsize::new(0);
    let got = attempt(
        &f.state,
        &on(),
        &launchd_argv().unwrap(),
        PID,
        1_000,
        || intent_guard(&marker),
        |_| {
            ran.fetch_add(1, Ordering::SeqCst);
            Some(0)
        },
    );
    assert!(matches!(got, Attempted::GuardTripped(_)), "{got:?}");
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert!(!f.state.exists(), "a refused attempt does not consume the cooldown");
}

// ------------------------------------------------------------------- state

#[test]
fn the_cooldown_holds_until_exactly_its_expiry() {
    let s = State {
        last_attempt_at: 1_000,
        ..State::default()
    };
    assert_eq!(cooldown_remaining(&s, 1800, 1_000), Some(1800));
    assert_eq!(cooldown_remaining(&s, 1800, 2_799), Some(1));
    assert_eq!(cooldown_remaining(&s, 1800, 2_800), None, "at expiry a restart is allowed");
    assert_eq!(cooldown_remaining(&s, 1800, 500), Some(1800), "clock skew counts as just now");
    assert_eq!(cooldown_remaining(&State::default(), 1800, 5), None);
}

#[test]
fn the_cooldown_survives_a_pid_change_and_a_watchdog_restart() {
    let f = fixture();
    let s = on();
    let got = attempt(&f.state, &s, &launchd_argv().unwrap(), PID, 1_000, || Ok(()), |_| Some(0));
    assert!(matches!(got, Attempted::Ran { rc: Some(0), .. }), "{got:?}");

    // A brand-new watchdog process (nothing in memory) and a brand-new daemon
    // pid, wedged again ten minutes later: the durable record still refuses.
    let new_pid = 9999;
    let argv = restart_argv(
        Source::Launchd,
        Some(SVC),
        &format!("launchd job {SVC} alive (pid {new_pid})"),
        new_pid,
    );
    let g = gate(&s, &stale(), 3, argv.clone(), &read_state(&f.state), 1_600);
    assert_eq!(
        g,
        Gate::CoolingDown {
            remaining: 1200,
            last_at: 1_000
        }
    );
    let note = note_for(&g, &s, &f.state, 1_600);
    assert!(note.contains("No automatic kill/restart"), "{note}");
    assert!(note.contains("600s ago"), "{note}");
    assert!(matches!(
        gate(&s, &stale(), 3, argv, &read_state(&f.state), 2_800),
        Gate::Restart { .. }
    ));
}

#[test]
fn a_failed_command_is_recorded_and_failed_attempts_are_bounded() {
    let f = fixture();
    let s = on();
    let argv = launchd_argv().unwrap();
    let mut now = 1_000;
    for i in 1..=3 {
        let got = attempt(&f.state, &s, &argv, PID, now, || Ok(()), |_| Some(113));
        assert_eq!(
            got,
            Attempted::Ran {
                rc: Some(113),
                unhealed: i
            }
        );
        assert!(read_state(&f.state).last_result.contains("exited 113"));
        now += 1_800;
    }
    let ran = AtomicUsize::new(0);
    let got = attempt(
        &f.state,
        &s,
        &argv,
        PID,
        now,
        || Ok(()),
        |_| {
            ran.fetch_add(1, Ordering::SeqCst);
            Some(0)
        },
    );
    assert_eq!(
        got,
        Attempted::Refused(Gate::BreakerOpen {
            unhealed: 3,
            max: 3
        })
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // A healthy tick re-arms the breaker but keeps the cooldown timestamp.
    let before = read_state(&f.state).last_attempt_at;
    on_healthy(&f.state);
    let after = read_state(&f.state);
    assert_eq!((after.unhealed, after.last_attempt_at), (0, before));
}

#[test]
fn a_command_that_cannot_run_counts_as_a_failure() {
    let f = fixture();
    let got = attempt(&f.state, &on(), &launchd_argv().unwrap(), PID, 1_000, || Ok(()), |_| None);
    assert_eq!(
        got,
        Attempted::Ran {
            rc: None,
            unhealed: 1
        }
    );
    assert!(read_state(&f.state).last_result.contains("could not run"));
}

#[test]
fn a_corrupt_record_never_opens_the_cooldown() {
    let f = fixture();
    std::fs::create_dir_all(f.state.parent().unwrap()).unwrap();
    std::fs::write(&f.state, "garbage\n").unwrap();
    let now = mtime_secs(&f.state).unwrap();
    assert!(cooldown_remaining(&read_state(&f.state), 1800, now + 5).is_some());
}

#[test]
fn an_unwritable_record_refuses_rather_than_restarting_unbounded() {
    let f = fixture();
    std::fs::create_dir_all(&f.state).unwrap(); // a directory: rename onto it fails
    let ran = AtomicUsize::new(0);
    let got = attempt(
        &f.state,
        &on(),
        &launchd_argv().unwrap(),
        PID,
        1_000,
        || Ok(()),
        |_| {
            ran.fetch_add(1, Ordering::SeqCst);
            Some(0)
        },
    );
    assert!(matches!(got, Attempted::StateUnwritable(_)), "{got:?}");
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[test]
fn simultaneous_invocations_issue_exactly_one_supervisor_call() {
    let d = tempfile::tempdir().expect("tempdir");
    let state = Arc::new(d.path().join(".watchdog-hang-recover-state"));
    let calls = Arc::new(AtomicUsize::new(0));
    let n = 8;
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let (state, calls, barrier) = (state.clone(), calls.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                attempt(
                    &state,
                    &on(),
                    &launchd_argv().unwrap(),
                    PID,
                    1_000,
                    || Ok(()),
                    |_| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        Some(0)
                    },
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "{results:?}");
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Attempted::Ran { .. }))
            .count(),
        1
    );
    for r in &results {
        assert!(
            matches!(
                r,
                Attempted::Ran { .. }
                    | Attempted::Busy
                    | Attempted::Refused(Gate::CoolingDown { .. })
            ),
            "{r:?}"
        );
    }
}

// --------------------------------------------------- supervisor commands

#[test]
fn loaded_services_select_the_bounded_supervised_restart() {
    assert_eq!(launchd_argv().unwrap(), ["launchctl", "kickstart", "-k", SVC]);
    let unit = "loom-daemon.service";
    let got = restart_argv(
        Source::Systemd,
        Some(unit),
        &format!("systemd unit {unit} alive (pid {PID})"),
        PID,
    );
    assert_eq!(got.unwrap(), ["systemctl", "--user", "restart", unit]);
}

#[test]
fn an_absent_or_unproven_supervisor_stays_report_only() {
    // Pid-file tier: no supervisor at all.
    let e = restart_argv(Source::PidFile, None, "pid file /x alive (pid 4242)", PID).unwrap_err();
    assert!(e.contains("bare kill"), "{e}");
    // launchd reported the job under a DIFFERENT domain / pid than we would target.
    assert!(restart_argv(
        Source::Launchd,
        Some(SVC),
        "launchd job user/501/x alive (pid 4242)",
        PID
    )
    .is_err());
    assert!(restart_argv(
        Source::Launchd,
        Some(SVC),
        &format!("launchd job {SVC} alive (pid 1)"),
        PID
    )
    .is_err());
    assert!(restart_argv(Source::Systemd, None, "", PID).is_err());

    // Through the whole decision: the streak completes, nothing runs.
    let f = fixture();
    let s = on();
    let none = || restart_argv(Source::PidFile, None, "", PID);
    tick(&f, &s, &stale(), none(), 1_000);
    tick(&f, &s, &stale(), none(), 1_300);
    let (out, calls) = tick(&f, &s, &stale(), none(), 1_600);
    assert!(calls.is_empty());
    assert!(out.note.contains("ONLY a supervised restart"), "{}", out.note);
    assert!(!f.state.exists());
}

#[test]
fn no_path_ever_selects_a_bare_kill_or_the_wedged_ipc_socket() {
    let unit = "loom-daemon.service";
    for argv in [
        launchd_argv().unwrap(),
        restart_argv(
            Source::Systemd,
            Some(unit),
            &format!("systemd unit {unit} alive (pid {PID})"),
            PID,
        )
        .unwrap(),
    ] {
        assert!(argv[0] == "launchctl" || argv[0] == "systemctl", "{argv:?}");
        for a in &argv {
            assert!(
                !["kill", "pkill", "-9", "SIGKILL", "SIGTERM"].contains(&a.as_str()),
                "{argv:?}"
            );
            assert!(!a.contains("loom-daemon restart") && !a.ends_with(".sock"), "{argv:?}");
        }
    }
}

#[test]
fn the_evidence_line_lands_in_the_watchdog_log() {
    let f = fixture();
    let s = on();
    let mut out = None;
    for now in [1_000, 1_300, 1_600] {
        out = Some(tick(&f, &s, &stale(), launchd_argv(), now).0);
    }
    let log = f
        .state
        .parent()
        .unwrap()
        .join("logs")
        .join("daemon-watchdog.log");
    let mut r = super::super::report::Reporter::new(log.clone(), false);
    r.set_colours(false);
    let line = out.unwrap().restart_line.unwrap();
    r.report(super::super::report::Level::Divergence, &line);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("[DIVERGENCE] HANG RECOVERY (#7855"), "{text}");
    assert!(text.contains("host load average 16.80, 15.20, 14.00"), "{text}");
}
