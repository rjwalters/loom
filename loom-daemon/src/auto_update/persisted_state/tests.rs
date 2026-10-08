//! Issue #10713: `auto_update_state.json` round trips, degrades to empty state on
//! every bad file, and keeps the settle ceiling across a restart.

use super::*;
use crate::auto_update::floor_roll::alert::REMINDER;
use crate::auto_update::floor_roll::Release;
use crate::auto_update::roll_window::{RollWindowTuning, WindowGate};
use crate::auto_update::stall_state::StallEpisode;
use crate::auto_update::supersede::ArmedRoll;
use crate::auto_update::{ArtifactInfo, ArtifactResolution, TickDecision, TickInputs, UpdateCheck};
use chrono::TimeZone;

const BIN: &str = "0.19.900+abc";
const DAY: u64 = 86_400;

/// A state with nothing in it and persistence off (not `default()` directly, so
/// the field assignments below do not trip `field_reassign_with_default`).
fn empty() -> AutoUpdateState {
    AutoUpdateState::new_with_record_path(None)
}

fn fixed_utc() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap()
}

/// A daily window whose current occurrence opened 60s before `now`.
fn open_window(now: DateTime<Utc>) -> WindowGate {
    let offset = (now.timestamp() - 60).rem_euclid(DAY as i64) as u64;
    WindowGate::new(RollWindowTuning {
        period: Some(Duration::from_secs(DAY)),
        offset: Duration::from_secs(offset),
        open_for: Duration::from_secs(1800),
        launchd_live_reload: false,
    })
}

fn release(version: &str, sha: &str) -> ArtifactResolution {
    ArtifactResolution::Resolved(ArtifactInfo {
        repo: "test-owner/test-repo".to_string(),
        tag: format!("v{version}"),
        version: version.to_string(),
        asset_sha256: Some(sha.repeat(64)),
        installed_version: Some("0.19.1".to_string()),
        installed_sha256: Some("f".repeat(64)),
        ..ArtifactInfo::default()
    })
}

fn no_source() -> UpdateCheck {
    UpdateCheck {
        update_available: None,
        source_commit: None,
        commits_behind: None,
        hours_behind: None,
    }
}

fn decide(
    state: &mut AutoUpdateState,
    now: Instant,
    artifact: &ArtifactResolution,
) -> TickDecision {
    let check = no_source();
    let inputs = TickInputs {
        artifact,
        check: &check,
        tree_clean: false,
        in_flight: 0,
    };
    state.decide(now, &inputs, SETTLE, Duration::from_secs(3600))
}

/// Settle 10s, so the ceiling (6x) is 60s.
const SETTLE: Duration = Duration::from_secs(10);

fn pending_roll(refusals: u32) -> ArmedRoll {
    ArmedRoll {
        target: Some("v0.19.2@aaaa".to_string()),
        pending: true,
        then_exit: false,
        refusals,
    }
}

/// A state with every persisted field set: settle clocks, a consumed window,
/// and a standing unsatisfiable declaration.
fn populated(now: Instant, now_utc: DateTime<Utc>) -> AutoUpdateState {
    let mut state = empty();
    state.window = open_window(now_utc);
    state.tracked_target = Some("artifact:0.19.2:aaaa".to_string());
    state.stale_since = now.checked_sub(Duration::from_secs(5));
    state.first_stale_since = now.checked_sub(Duration::from_secs(40));
    state.deferred_since = now.checked_sub(Duration::from_secs(20));
    let gated = state.window.gate(
        now_utc,
        TickDecision::Rebuild {
            low_priority: false,
        },
    );
    assert!(matches!(gated, TickDecision::Rebuild { .. }));
    assert!(state.window.mark_armed().is_some());
    state.roll_stall.set_threshold(1);
    let report = state.roll_stall.observe(now_utc, Some(&pending_roll(1)), 3);
    assert!(report.is_some(), "threshold 1: one deadline declares");
    state
}

#[test]
fn a_saved_state_round_trips_through_the_file_and_back_into_a_fresh_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    let (now, now_utc) = (Instant::now(), fixed_utc());
    let original = populated(now, now_utc);
    let saved = original.persisted_state(now, now_utc, BIN);
    assert!(matches!(saved.stall, StallState::Unsatisfiable { .. }));
    assert!(saved.window.as_ref().unwrap().consumed.is_some());

    store(&path, &saved).unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["schema_version"], 1);
    assert_eq!(raw["stall"]["kind"], "unsatisfiable");

    let LoadOutcome::Loaded(loaded) = load(&path) else {
        panic!("a freshly written file loads");
    };
    assert_eq!(*loaded, saved);

    let mut restarted = empty();
    restarted.window = open_window(now_utc);
    restarted.roll_stall.set_threshold(1);
    let note = restarted.apply_persisted_state(*loaded, now, now_utc, BIN);
    assert!(note.starts_with("restored settle clocks"), "{note}");
    assert_eq!(restarted.persisted_state(now, now_utc, BIN), saved);
    assert_eq!(restarted.roll_stall, original.roll_stall);
    assert_eq!(restarted.first_stale_since, original.first_stale_since);
}

#[test]
fn a_missing_file_is_missing_and_attaching_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    assert_eq!(load(&path), LoadOutcome::Missing);
    let mut state = empty();
    state.attach_persistence(Some(path));
    assert!(state.first_stale_since.is_none());
    assert_eq!(state.roll_stall.stall_state(), StallState::default());
}

#[test]
fn a_corrupt_file_is_corrupt_and_attaching_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    for body in [
        "\u{0}\u{1}garbage",
        r#"{"schema_version": 1, "saved_at": "2026-"#, // a torn write
        r#"{"settle": {}}"#,                           // no schema_version
        r#"{"schema_version": "1"}"#,                  // not an integer
        r#"{"schema_version": 1, "saved_at": 7, "binary": "x"}"#,
    ] {
        std::fs::write(&path, body).unwrap();
        assert!(
            matches!(load(&path), LoadOutcome::Corrupt(_)),
            "{body:?} must load as Corrupt, got {:?}",
            load(&path)
        );
        let mut state = empty();
        state.attach_persistence(Some(path.clone()));
        assert!(state.first_stale_since.is_none());
        assert!(state.tracked_target.is_none());
    }
}

#[test]
fn an_unknown_schema_version_is_unknown_version_and_attaching_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    for version in [0_u64, 2, 99] {
        let body = serde_json::json!({
            "schema_version": version,
            "saved_at": "2026-10-07T12:00:00Z",
            "binary": running_binary(),
            "settle": { "first_stale_since": "2026-10-07T00:00:00Z" },
        });
        std::fs::write(&path, body.to_string()).unwrap();
        assert_eq!(load(&path), LoadOutcome::UnknownVersion(version));
        let mut state = empty();
        state.attach_persistence(Some(path.clone()));
        assert!(state.first_stale_since.is_none(), "schema {version} must not be applied");
    }
}

/// The #10418 regression: a restart must not restart the settle ceiling.
#[test]
fn a_restart_keeps_the_settle_ceiling_so_a_steady_release_stream_still_rolls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);

    // Process 1 sees release .2 at t0 and keeps deferring: each later release
    // restarts the quiet period, so only the 60s ceiling can end the wait.
    let t0 = Instant::now();
    let mut before = empty();
    assert!(matches!(
        decide(&mut before, t0, &release("0.19.2", "a")),
        TickDecision::Skip(_)
    ));
    let at_50 = t0 + Duration::from_secs(50);
    assert!(matches!(
        decide(&mut before, at_50, &release("0.19.3", "b")),
        TickDecision::Skip(_)
    ));
    let saved_utc = fixed_utc();
    let saved = before.persisted_state(at_50, saved_utc, BIN);
    store(&path, &saved).unwrap();

    // The daemon restarts: 5s down, then a fresh process loads the file.
    let LoadOutcome::Loaded(loaded) = load(&path) else {
        panic!("saved state loads");
    };
    let (now, now_utc) = (Instant::now(), saved_utc + chrono::Duration::seconds(5));
    let mut after = empty();
    after.apply_persisted_state(*loaded, now, now_utc, BIN);
    assert_eq!(
        after
            .persisted_state(now, now_utc, BIN)
            .settle
            .first_stale_since,
        saved.settle.first_stale_since,
        "the settle ceiling's origin is unchanged by the restart"
    );

    // 50s before + 5s down + 6s = 61s >= the 60s ceiling: a brand-new release
    // (quiet period restarted) still rolls.
    let at_61 = now + Duration::from_secs(6);
    assert!(
        matches!(
            decide(&mut after, at_61, &release("0.19.4", "c")),
            TickDecision::FetchArtifact { .. }
        ),
        "the ceiling carried across the restart forces the roll"
    );

    // Without the file the same tick defers: the ceiling restarted from zero.
    let mut amnesiac = empty();
    decide(&mut amnesiac, now, &release("0.19.3", "b"));
    assert!(matches!(
        decide(&mut amnesiac, at_61, &release("0.19.4", "c")),
        TickDecision::Skip(_)
    ));
}

#[test]
fn a_new_binary_keeps_window_consumption_but_drops_settle_clocks_and_stall() {
    let (now, now_utc) = (Instant::now(), fixed_utc());
    let saved = populated(now, now_utc).persisted_state(now, now_utc, BIN);

    let mut rolled = empty();
    rolled.window = open_window(now_utc);
    let note = rolled.apply_persisted_state(saved.clone(), now, now_utc, "0.19.901+def");
    assert!(note.contains("DROPPED"), "{note}");
    let after = rolled.persisted_state(now, now_utc, "0.19.901+def");
    assert_eq!(after.settle, SettleClocks::default());
    assert_eq!(after.stall, StallState::default());
    assert_eq!(after.window, saved.window);
}

/// #10188 item 2: the restart that completes a roll must not let a second roll
/// arm in the same window.
#[test]
fn a_window_consumed_before_a_restart_stays_consumed_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    let now_utc = Utc::now();
    let arm = || TickDecision::Rebuild {
        low_priority: false,
    };

    let mut before = empty();
    before.window = open_window(now_utc);
    before.persist = Persistence {
        path: Some(path.clone()),
    };
    assert!(matches!(before.window.gate(now_utc, arm()), TickDecision::Rebuild { .. }));
    // `begin_roll_arm` persists before the drain is armed.
    let prior = before.begin_roll_arm(&RebuildOutcome::Success);
    assert_eq!(prior, Some(None));
    before.end_roll_arm(prior, true);

    let LoadOutcome::Loaded(loaded) = load(&path) else {
        panic!("begin_roll_arm wrote the file");
    };
    let mut after = empty();
    after.window = open_window(now_utc);
    after.apply_persisted_state(*loaded, Instant::now(), now_utc, "a different binary");
    let TickDecision::Skip(reason) = after.window.gate(now_utc, arm()) else {
        panic!("the window was already used before the restart");
    };
    assert!(reason.contains("already armed"), "{reason}");

    // A fresh process without the file would have armed again.
    let mut amnesiac = empty();
    amnesiac.window = open_window(now_utc);
    assert!(matches!(amnesiac.window.gate(now_utc, arm()), TickDecision::Rebuild { .. }));
}

#[test]
fn a_roll_that_fails_to_arm_gives_the_window_back() {
    let now_utc = Utc::now();
    let mut state = empty();
    state.window = open_window(now_utc);
    let arm = TickDecision::Rebuild {
        low_priority: false,
    };
    assert!(matches!(state.window.gate(now_utc, arm.clone()), TickDecision::Rebuild { .. }));
    let failed = RebuildOutcome::Retryable("fetch failed".to_string());
    assert_eq!(state.begin_roll_arm(&failed), None);
    let prior = state.begin_roll_arm(&RebuildOutcome::Success);
    assert_eq!(prior, Some(None));
    state.end_roll_arm(prior, false);
    assert_eq!(state.window.consumption().unwrap().consumed, None);
    assert!(matches!(state.window.gate(now_utc, arm), TickDecision::Rebuild { .. }));
}

#[test]
fn window_consumption_from_a_different_schedule_is_ignored() {
    let now_utc = fixed_utc();
    let mut gate = open_window(now_utc);
    let other = WindowConsumption {
        period_secs: DAY / 2,
        offset_secs: 0,
        consumed: Some(5),
        timed_out: None,
    };
    assert!(!gate.restore_consumption(&other));
    assert_eq!(gate.consumption().unwrap().consumed, None);
    assert!(!WindowGate::default().restore_consumption(&other), "windowing off");
}

#[test]
fn every_stall_variant_round_trips_through_the_tracker() {
    let episode = StallEpisode {
        since: fixed_utc(),
        carried_deadlines: 2,
        live_refusals: Some(1),
        floor: Some(3),
        deadlines_at_floor: 1,
        target: Some("v0.19.2@aaaa".to_string()),
        retries: 4,
    };
    for stall in [
        StallState::None { retries: 0 },
        StallState::None { retries: 2 },
        StallState::DrainDeadlines {
            episode: episode.clone(),
        },
        StallState::Unsatisfiable {
            episode,
            declared_at: fixed_utc() + chrono::Duration::seconds(30),
        },
    ] {
        let mut state = empty();
        state.roll_stall.restore_stall_state(stall.clone());
        assert_eq!(state.roll_stall.stall_state(), stall);
        let json = serde_json::to_string(&stall).unwrap();
        assert_eq!(serde_json::from_str::<StallState>(&json).unwrap(), stall);
    }
}

#[test]
fn writes_are_atomic_replacements_that_leave_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    // A torn file from an earlier crash, and a stray temp file beside it.
    std::fs::write(&path, "{\"schema_version\": 1, \"sav").unwrap();
    std::fs::write(dir.path().join(".auto_update_state.XXXX.tmp"), "junk").unwrap();
    assert!(matches!(load(&path), LoadOutcome::Corrupt(_)));

    let (now, now_utc) = (Instant::now(), fixed_utc());
    let mut state = populated(now, now_utc);
    state.persist = Persistence {
        path: Some(path.clone()),
    };
    state.persist_state();
    state.persist_state();
    assert!(matches!(load(&path), LoadOutcome::Loaded(_)));
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ".auto_update_state.XXXX.tmp".to_string(),
            STATE_FILE.to_string()
        ],
        "only the pre-existing stray remains beside the state file"
    );
}

#[test]
fn attach_restores_a_file_written_by_this_binary_and_persist_writes_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    let (now, now_utc) = (Instant::now(), Utc::now());
    let saved = populated(now, now_utc).persisted_state(now, now_utc, &running_binary());
    store(&path, &saved).unwrap();

    let mut state = empty();
    state.window = open_window(now_utc);
    state.attach_persistence(Some(path.clone()));
    assert!(state.first_stale_since.is_some());
    assert!(matches!(state.roll_stall.stall_state(), StallState::Unsatisfiable { .. }));

    std::fs::remove_file(&path).unwrap();
    state.persist_state();
    let LoadOutcome::Loaded(rewritten) = load(&path) else {
        panic!("persist_state rewrites the file");
    };
    assert_eq!(rewritten.stall, saved.stall);
    assert_eq!(rewritten.window, saved.window);
}

#[test]
fn a_default_state_never_writes() {
    // Tests across the crate build `AutoUpdateState::new()`/`default()`; none of
    // them may write under the real state dir.
    let state = AutoUpdateState::default();
    assert!(state.persist.path.is_none());
    state.persist_state();
}

#[test]
fn a_wall_time_older_than_the_monotonic_clock_clamps_rather_than_panics() {
    let now = Instant::now();
    let far = instant_ago(now, Duration::from_secs(u64::MAX / 4));
    assert!(far <= now);
    // A saved time in the future (the wall clock stepped back) reads as now.
    let ahead = to_instant(fixed_utc() + chrono::Duration::hours(1), now, fixed_utc());
    assert_eq!(ahead, now);
}

// ---- #10866: the unsatisfiable-floor alert record ----

const FLOOR: &str = "9.0.0";
const RUNNING: &str = "0.19.900";

fn below(version: &str) -> Release {
    Release {
        tag: format!("v{version}"),
        version: version.to_string(),
    }
}

/// A state whose unsatisfiable floor was alerted at `at`.
fn floor_stalled(at: DateTime<Utc>) -> AutoUpdateState {
    let mut state = empty();
    state.floor.set_basis(Some(FLOOR.to_string()), RUNNING);
    state.floor.observe(Some(&below("0.19.950")));
    assert!(state.floor.alert_due(at, REMINDER), "the stall starts");
    state
}

/// Save `state` to a file, load it, and apply it to a fresh state running
/// `binary`.
fn restart(state: &AutoUpdateState, saved_at: DateTime<Utc>, binary: &str) -> AutoUpdateState {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    let now = Instant::now();
    store(&path, &state.persisted_state(now, saved_at, BIN)).unwrap();
    let LoadOutcome::Loaded(loaded) = load(&path) else {
        panic!("a freshly written file loads");
    };
    let mut restarted = empty();
    let note = restarted.apply_persisted_state(*loaded, now, saved_at, binary);
    assert!(note.contains("unsatisfiable-floor alert for floor 9.0.0"), "{note}");
    restarted
}

fn mins(n: i64) -> chrono::Duration {
    chrono::Duration::minutes(n)
}

#[test]
fn a_floor_stall_round_trips_at_schema_version_1() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    let t0 = fixed_utc();
    let saved = floor_stalled(t0).persisted_state(Instant::now(), t0, BIN);
    store(&path, &saved).unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["schema_version"], 1);
    assert_eq!(raw["floor_stall"]["floor"], FLOOR);
    assert_eq!(raw["floor_stall"]["running"], RUNNING);
    assert_eq!(raw["floor_stall"]["newest"], "0.19.950");
    assert_eq!(raw["floor_stall"]["declared_at"], raw["floor_stall"]["last_alerted_at"]);

    let LoadOutcome::Loaded(loaded) = load(&path) else {
        panic!("a freshly written file loads");
    };
    assert_eq!(*loaded, saved);
    assert!(loaded.floor_stall.is_some());
}

#[test]
fn a_restart_inside_the_reminder_interval_does_not_alert_again() {
    let t0 = fixed_utc();
    let mut b = restart(&floor_stalled(t0), t0 + mins(5), BIN);
    b.floor.set_basis(Some(FLOOR.to_string()), RUNNING);
    assert!(b.floor.stall().is_some(), "the stall stands before anything is observed");
    b.floor.observe(Some(&below("0.19.950")));
    assert!(!b.floor.alert_due(t0 + mins(10), REMINDER));
    assert!(b.floor.stall().is_some());
    assert!(b.floor.note_suffix().contains("FLEET FLOOR UNSATISFIABLE"));
    // The reminder is due an hour after the alert before the restart, not an
    // hour after the restart.
    assert!(!b.floor.alert_due(t0 + mins(59), REMINDER));
    assert!(b.floor.alert_due(t0 + mins(61), REMINDER));
    let record = b.floor.stall_state().unwrap();
    assert_eq!((record.declared_at, record.last_alerted_at), (t0, t0 + mins(61)));
}

#[test]
fn a_restart_whose_first_tick_is_unresolved_still_reports_the_stall() {
    let t0 = fixed_utc();
    let mut b = restart(&floor_stalled(t0), t0 + mins(5), BIN);
    b.floor.set_basis(Some(FLOOR.to_string()), RUNNING);
    b.floor.observe(None);
    assert!(b.floor.stall().is_some());
    assert!(b.floor.note_suffix().contains("FLEET FLOOR UNSATISFIABLE"));
    assert!(!b.floor.alert_due(t0 + mins(10), REMINDER));
    assert!(b.floor.alert_due(t0 + mins(61), REMINDER));
}

#[test]
fn a_new_binary_on_the_same_version_keeps_the_floor_stall() {
    let t0 = fixed_utc();
    let a = floor_stalled(t0);
    let saved = a.persisted_state(Instant::now(), t0, BIN);
    let mut b = empty();
    let note = b.apply_persisted_state(saved, Instant::now(), t0, "0.19.900+def");
    assert!(note.contains("DROPPED"), "{note}");
    assert!(note.contains("unsatisfiable-floor alert"), "{note}");
    b.floor.set_basis(Some(FLOOR.to_string()), RUNNING);
    assert_eq!(b.floor.stall(), a.floor.stall());
    b.floor.observe(Some(&below("0.19.950")));
    assert!(!b.floor.alert_due(t0 + mins(10), REMINDER));
}

#[test]
fn a_floor_stall_saved_under_another_basis_is_dropped_and_alerts_as_new() {
    let t0 = fixed_utc();
    for (floor, running) in [("9.0.1", RUNNING), (FLOOR, "0.19.901")] {
        let mut b = restart(&floor_stalled(t0), t0 + mins(5), BIN);
        b.floor.set_basis(Some(floor.to_string()), running);
        assert!(b.floor.stall().is_none(), "{floor} {running}");
        assert_eq!(b.floor.stall_state(), None, "{floor} {running}");
        b.floor.observe(Some(&below("0.19.950")));
        assert!(b.floor.alert_due(t0 + mins(10), REMINDER), "{floor} {running}: a new start");
        assert_eq!(b.floor.stall_state().unwrap().declared_at, t0 + mins(10));
    }
    // The operator removed the floor while the daemon was down.
    let mut b = restart(&floor_stalled(t0), t0 + mins(5), BIN);
    b.floor.set_basis(None, RUNNING);
    b.floor.observe(Some(&below("0.19.950")));
    assert!(b.floor.stall().is_none());
    assert!(!b.floor.alert_due(t0 + mins(10), REMINDER));
}

#[test]
fn a_file_without_a_floor_stall_key_loads_with_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILE);
    let (now, now_utc) = (Instant::now(), fixed_utc());
    // What a #10877-only binary writes: no stall, so no key at all.
    let saved = populated(now, now_utc).persisted_state(now, now_utc, BIN);
    assert_eq!(saved.floor_stall, None);
    store(&path, &saved).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("floor_stall"), "{raw}");

    let LoadOutcome::Loaded(loaded) = load(&path) else {
        panic!("a file without the key loads");
    };
    assert_eq!(loaded.floor_stall, None);
    let mut restarted = empty();
    restarted.window = open_window(now_utc);
    let note = restarted.apply_persisted_state(*loaded, now, now_utc, BIN);
    assert!(!note.contains("floor"), "{note}");
    restarted.floor.set_basis(Some(FLOOR.to_string()), RUNNING);
    assert!(restarted.floor.stall().is_none());
}
