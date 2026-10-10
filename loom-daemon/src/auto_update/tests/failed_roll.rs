//! Issue #10880: the failed-roll guard end to end through `run_tick` and
//! `decide`, with the state file loaded under a chosen running version (no
//! rollback machinery is needed to create the situation it guards against).
//!
//! A child of `tests/floor_roll.rs`, reusing its fixtures, so neither that
//! file nor `tests.rs` grows (`.loom/docs/file-size-policy.md`).

use super::*;
use crate::auto_update::persisted_state::{load, LoadOutcome, STATE_FILE};
use crate::auto_update::roll_attempt::{self, RollAttempt};
use chrono::{DateTime, Utc};

/// The binary that armed the roll and came back: it runs [`RUNNING`].
const OLD_BIN: &str = "0.19.800+old";

/// The tick's identity for [`newest`].
fn target_id() -> String {
    format!("artifact:{NEWEST}:{SHA_B}")
}

/// A record of `attempts` arms of [`newest`], not yet judged.
fn armed(attempts: u32, at: DateTime<Utc>) -> RollAttempt {
    RollAttempt {
        target: target_id(),
        version: NEWEST.to_string(),
        tag: format!("v{NEWEST}"),
        source: "floor".to_string(),
        from_binary: OLD_BIN.to_string(),
        attempts,
        first_armed_at: at,
        last_armed_at: at,
        not_before: None,
        last_failure: None,
    }
}

/// A host restarted with `binary` on a state file whose settle clocks for
/// [`newest`] elapsed two hours ago and which carries `record`. Returns the
/// state and the load line.
fn restarted(
    dir: &Path,
    floor: Floor,
    record: Option<RollAttempt>,
    binary: &str,
) -> (AutoUpdateState, String) {
    let now_utc = Utc::now();
    let two_hours_ago = now_utc - chrono::Duration::hours(2);
    let path = dir.join(STATE_FILE);
    let body = serde_json::json!({
        "schema_version": 1,
        "saved_at": now_utc - chrono::Duration::minutes(1),
        "binary": OLD_BIN,
        "settle": {
            "tracked_target": target_id(),
            "stale_since": two_hours_ago,
            "first_stale_since": two_hours_ago,
        },
        "roll_attempt": record,
    });
    std::fs::write(&path, body.to_string()).unwrap();
    let LoadOutcome::Loaded(saved) = load(&path) else {
        panic!("the file loads");
    };
    let mut state = floored(dir, floor);
    let note = state.apply_persisted_state(*saved, Instant::now(), now_utc, binary);
    (state, note)
}

/// One `run_tick` with a 600s settle and `outcome` for any fetch. Returns the
/// summary, the published note and the fetch count.
fn tick_once(
    state: &mut AutoUpdateState,
    trigger: &impl RollTrigger,
    artifact: ArtifactResolution,
    outcome: RebuildOutcome,
) -> (TickSummary, String, usize) {
    let fetches = Arc::new(AtomicUsize::new(0));
    let mut probe = probe(artifact, &fetches);
    probe.fetch_outcome = outcome;
    let status = AutoUpdateStatus::new(true);
    let settle = Duration::from_secs(600);
    let summary = run_tick(state, &status, &mut probe, trigger, settle, DEFER);
    let note = status.snapshot().note.unwrap_or_default();
    (summary, note, fetches.load(Ordering::SeqCst))
}

/// Records what `auto_update_state.json` held at the moment of each arm.
struct Peeking {
    path: PathBuf,
    accept: bool,
    seen: Mutex<Vec<Option<RollAttempt>>>,
}

impl RollTrigger for Peeking {
    fn trigger_pause_roll(&self, _target: &RollTarget) -> bool {
        let on_disk = match load(&self.path) {
            LoadOutcome::Loaded(saved) => saved.roll_attempt,
            _ => None,
        };
        self.seen.lock().unwrap().push(on_disk);
        self.accept
    }
    fn roll_in_progress(&self) -> bool {
        false
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        None
    }
}

#[test]
fn the_attempt_is_on_disk_before_the_trigger_and_a_refused_arm_leaves_none() {
    for accept in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(STATE_FILE);
        let mut state = floored(tmp.path(), Floor::Below);
        state.attach_persistence(Some(path.clone()));
        let trigger = Peeking {
            path: path.clone(),
            accept,
            seen: Mutex::new(Vec::new()),
        };
        let (summary, _, fetches) =
            tick_once(&mut state, &trigger, newest(RUNNING), RebuildOutcome::Success);
        assert_eq!((fetches, summary.roll_armed), (1, accept));
        let seen = trigger.seen.lock().unwrap();
        let on_disk = seen[0]
            .as_ref()
            .expect("written before the trigger was called");
        assert_eq!(on_disk.attempts, 1);
        assert_eq!(on_disk.target, target_id());
        assert_eq!((on_disk.version.as_str(), on_disk.source.as_str()), (NEWEST, "floor"));
        if accept {
            assert_eq!(state.attempt.record(), Some(on_disk));
        } else {
            assert_eq!(state.attempt.record(), None, "the refused arm is undone");
            state.persist_state();
            let LoadOutcome::Loaded(saved) = load(&path) else {
                panic!("loads");
            };
            assert_eq!(saved.roll_attempt, None);
        }
    }
}

/// Without the record, the same restored state fetches on its first tick (the
/// settle clocks elapsed); with it, neither an autoUpdate nor a floor target is
/// fetched. The floor case alerts at ERROR and pauses nothing.
#[test]
fn a_host_below_its_last_target_does_not_refetch_it_on_its_first_tick() {
    for floor in [Floor::NoStore, Floor::Below] {
        let tmp = tempfile::tempdir().unwrap();
        let (mut control, _) = restarted(tmp.path(), floor, None, OLD_BIN);
        let trigger = Trigger::new(false);
        let (_, _, fetches) =
            tick_once(&mut control, &trigger, newest(RUNNING), RebuildOutcome::Success);
        assert_eq!(fetches, 1, "{floor:?}: the control rolls at once");

        let tmp = tempfile::tempdir().unwrap();
        let (mut state, load_line) =
            restarted(tmp.path(), floor, Some(armed(1, Utc::now())), OLD_BIN);
        assert!(load_line.contains("restored settle clocks"), "{load_line}");
        let rec = state.attempt.record().unwrap();
        let wait = rec.not_before.unwrap() - Utc::now();
        assert!(wait <= chrono::Duration::minutes(15), "{wait}");
        assert!(wait > chrono::Duration::minutes(14), "{wait}");

        let trigger = Trigger::new(false);
        let (summary, note, fetches) =
            tick_once(&mut state, &trigger, newest(RUNNING), RebuildOutcome::Success);
        assert_eq!(fetches, 0, "{floor:?}");
        assert!(trigger.targets.lock().unwrap().is_empty(), "{floor:?}: nothing paused");
        assert_eq!(summary.decision, TickDecisionKind::Defer, "{floor:?}");
        assert!(note.contains("did not take"), "{floor:?}: {note}");
        let held = summary.roll_held.expect("the held roll alerts");
        assert_eq!((held.cause.as_str(), held.attempts), ("failed_roll", 1));
        assert_eq!(held.target, target_id());
        assert!(held.next_retry.is_some());
        if floor == Floor::Below {
            assert_eq!(held.floor.as_deref(), Some("0.19.850"));
            assert!(note.contains("FLOOR ROLL FAILING"), "{note}");
            assert!(note.contains("DISPATCH CONTINUES"), "{note}");
        } else {
            assert_eq!(held.floor, None);
            assert!(note.contains("ROLL HELD"), "{note}");
        }
    }
}

/// The record survives the branch that drops the settle clocks.
#[test]
fn the_record_is_restored_when_the_binary_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut state, load_line) =
        restarted(tmp.path(), Floor::Below, Some(armed(1, Utc::now())), "0.19.800+other");
    assert!(load_line.contains("DROPPED"), "{load_line}");
    assert!(load_line.contains("failed roll"), "{load_line}");
    let (_, _, fetches) =
        tick_once(&mut state, &Trigger::new(false), newest(RUNNING), RebuildOutcome::Success);
    assert_eq!(fetches, 0);
}

/// Once the retry time passes the target is fetched and armed again, the
/// record counts up, and the next failed load waits twice as long. After many
/// failures it still releases an attempt every 6 h and alerts on every held
/// tick: never terminal.
#[test]
fn retries_double_to_the_ceiling_and_never_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut state, _) = restarted(tmp.path(), Floor::Below, Some(armed(1, Utc::now())), OLD_BIN);
    let trigger = Trigger::new(false);
    let mut waits = Vec::new();
    for attempt in 1..=8_u32 {
        for held_tick in 0..2 {
            let (summary, _, fetches) =
                tick_once(&mut state, &trigger, newest(RUNNING), RebuildOutcome::Success);
            assert_eq!(fetches, 0, "attempt {attempt} tick {held_tick}");
            assert!(summary.roll_held.is_some(), "attempt {attempt} tick {held_tick}");
        }
        // The retry time passes.
        state.attempt.set_hold_until(Some(Instant::now()));
        let (summary, _, fetches) =
            tick_once(&mut state, &trigger, newest(RUNNING), RebuildOutcome::Success);
        assert_eq!((fetches, summary.roll_armed), (1, true), "attempt {attempt}");
        let rec = state.attempt.record().cloned().unwrap();
        assert_eq!(rec.attempts, attempt + 1);
        assert_eq!(rec.not_before, None);
        // ... and it did not take: the old binary comes back.
        let now_utc = Utc::now();
        let saved = state.persisted_state(Instant::now(), now_utc, OLD_BIN);
        let mut back = floored(tmp.path(), Floor::Below);
        back.apply_persisted_state(saved, Instant::now(), now_utc, OLD_BIN);
        let not_before = back.attempt.record().unwrap().not_before.unwrap();
        waits.push((not_before - now_utc).num_minutes());
        state = back;
    }
    assert_eq!(waits, [30, 60, 120, 240, 360, 360, 360, 360]);
    assert_eq!(roll_attempt::delay(u32::MAX), roll_attempt::CEILING);
}

/// A newer release, or the same version re-published under a new checksum,
/// is a new target: tried at once, and the count starts over.
#[test]
fn a_different_target_is_tried_at_once() {
    let newer = resolved(artifact("0.19.901", Some(RUNNING), Some(SHA_B), Some(SHA_A)));
    let republished = resolved(artifact(NEWEST, Some(RUNNING), Some(SHA_A), Some(SHA_B)));
    for (other, id) in [
        (newer, format!("artifact:0.19.901:{SHA_B}")),
        (republished, format!("artifact:{NEWEST}:{SHA_A}")),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (mut state, _) =
            restarted(tmp.path(), Floor::Below, Some(armed(4, Utc::now())), OLD_BIN);
        let (summary, _, fetches) =
            tick_once(&mut state, &Trigger::new(false), other, RebuildOutcome::Success);
        assert_eq!((fetches, summary.roll_armed), (1, true), "{id}");
        let rec = state.attempt.record().unwrap();
        assert_eq!((rec.target.as_str(), rec.attempts), (id.as_str(), 1));
    }
}

/// A host that reached the target keeps the record (no gate), so a candidate
/// that exits inside its startup grace leaves a file from which the older
/// binary still judges the attempt failed.
#[test]
fn a_candidate_that_dies_early_leaves_the_record_for_the_old_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let (candidate, load_line) =
        restarted(tmp.path(), Floor::Below, Some(armed(1, Utc::now())), "0.19.900+new");
    assert!(load_line.contains("kept until"), "{load_line}");
    assert_eq!(candidate.attempt.hold_until(), None, "no gate on the target binary");
    let now_utc = Utc::now();
    let saved = candidate.persisted_state(Instant::now(), now_utc, "0.19.900+new");
    assert_eq!(saved.roll_attempt.as_ref().map(|r| r.attempts), Some(1));

    let mut old = floored(tmp.path(), Floor::Below);
    old.apply_persisted_state(saved, Instant::now(), now_utc, OLD_BIN);
    let rec = old.attempt.record().unwrap();
    assert_eq!(rec.not_before, Some(now_utc + chrono::Duration::minutes(15)));
}

/// A terminal fetch failure on a floor target alerts and is retried after the
/// 6 h ceiling. With no floor the terminal rule is unchanged.
#[test]
fn a_terminal_failure_is_retried_at_the_ceiling_only_for_a_floor_target() {
    let terminal = RebuildOutcome::Terminal("checksum mismatch".to_string());
    let inputs_at = |state: &mut AutoUpdateState, at: Instant| {
        let art = newest(RUNNING);
        let check = no_source();
        let inputs = TickInputs {
            artifact: &art,
            check: &check,
            tree_clean: true,
            in_flight: 0,
        };
        state.decide(at, &inputs, Duration::ZERO, DEFER)
    };

    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Below);
    let start = Instant::now();
    let (summary, note, fetches) =
        tick_once(&mut state, &Trigger::new(false), newest(RUNNING), terminal.clone());
    assert_eq!(fetches, 1);
    assert!(note.contains("never abandoned"), "{note}");
    let held = summary.roll_held.expect("a terminal floor fetch alerts");
    assert_eq!(
        (held.cause.as_str(), held.floor.as_deref()),
        ("fetch_terminal", Some("0.19.850"))
    );
    assert_eq!(held.last_failure.as_deref(), Some("checksum mismatch"));
    assert!(held.next_retry.is_some());
    match inputs_at(&mut state, start + Duration::from_secs(3600)) {
        TickDecision::Skip(reason) => assert!(reason.contains("retrying in"), "{reason}"),
        other => panic!("held for the ceiling, got {other:?}"),
    }
    let after = start + roll_attempt::CEILING + Duration::from_secs(60);
    assert!(matches!(inputs_at(&mut state, after), TickDecision::FetchArtifact { .. }));

    // No fleet store: settle applies, so start from clocks that elapsed.
    let tmp = tempfile::tempdir().unwrap();
    let (mut state, _) = restarted(tmp.path(), Floor::NoStore, None, OLD_BIN);
    let (summary, _, fetches) =
        tick_once(&mut state, &Trigger::new(false), newest(RUNNING), terminal);
    assert_eq!(fetches, 1);
    assert_eq!(summary.roll_held.as_ref().and_then(|h| h.next_retry), None);
    let later = start + roll_attempt::CEILING + Duration::from_secs(3600);
    match inputs_at(&mut state, later) {
        TickDecision::Skip(reason) => {
            assert_eq!(reason, "terminal — not retrying until a new commit: checksum mismatch")
        }
        other => panic!("no retry without a floor, got {other:?}"),
    }
}

/// Fetch backoff below the floor alerts from the third consecutive failure.
#[test]
fn three_retryable_failures_below_the_floor_alert_and_two_do_not() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Below);
    let mut alerted = Vec::new();
    for _ in 0..3 {
        state.backoff_until = None;
        let failed = RebuildOutcome::Retryable("HTTP 502".to_string());
        let (summary, _, fetches) =
            tick_once(&mut state, &Trigger::new(false), newest(RUNNING), failed);
        assert_eq!(fetches, 1);
        alerted.push(
            summary
                .roll_held
                .map(|h| (h.cause, h.attempts, h.last_failure)),
        );
    }
    let third = Some(("fetch_backoff".to_string(), 3, Some("HTTP 502".to_string())));
    assert_eq!(alerted, [None, None, third]);
}
