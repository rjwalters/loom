//! Tests for run-state enforcement (#9598).
//!
//! Every forge interaction goes through the injected [`FakeForge`] transport
//! and a `tempfile` cache dir — no network, no daemon, no real `DrainState`
//! except where a real one is the thing under test.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::fleet_store::test_support::{location, sample_files, FakeForge};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

/// `sample_files()` with `fleet/state.yml` replaced by `yaml`.
fn files_with_state(yaml: &str) -> BTreeMap<String, String> {
    let mut f = sample_files();
    f.insert(crate::fleet_store::STATE_PATH.to_string(), yaml.to_string());
    f
}

/// `sample_files()` with no `fleet/state.yml` at all.
fn files_without_state() -> BTreeMap<String, String> {
    let mut f = sample_files();
    f.remove(crate::fleet_store::STATE_PATH);
    f
}

/// Read `host`'s state from a live forge serving `files`.
fn pass_for(files: BTreeMap<String, String>, host: &str) -> (StatePass, tempfile::TempDir) {
    let forge = FakeForge::new(files);
    let dir = tempfile::tempdir().unwrap();
    let pass = state_pass(&forge, dir.path(), &location(), host, None, t0());
    (pass, dir)
}

// ============================================================================
// Reading the state: the three states, and everything that is not one
// ============================================================================

#[test]
fn a_fleet_default_of_running_proceeds() {
    let (pass, _d) = pass_for(files_with_state("fleet:\n  state: running\n"), "build-1");
    assert_eq!(pass.desired, Some(RunState::Running));
    assert_eq!(pass.source.as_deref(), Some("fleet"));
    assert_eq!(pass.enforcement(), Enforcement::Proceed);
    assert!(!pass.cached);
    assert!(pass.error.is_none());
}

#[test]
fn a_host_entry_of_paused_holds_and_overrides_the_fleet_default() {
    // The shipped fixture: `fleet: running`, `hosts.build-2: paused`.
    let (running, _d1) = pass_for(sample_files(), "build-1");
    assert_eq!(running.enforcement(), Enforcement::Proceed);

    let (paused, _d2) = pass_for(sample_files(), "build-2");
    assert_eq!(paused.desired, Some(RunState::Paused));
    assert_eq!(paused.source.as_deref(), Some("host"));
    assert_eq!(paused.enforcement(), Enforcement::Hold);
}

#[test]
fn a_host_entry_of_stopped_stops() {
    let (pass, _d) = pass_for(
        files_with_state(
            "fleet:\n  state: running\nhosts:\n  build-1:\n    state: stopped\n    by: robb\n",
        ),
        "build-1",
    );
    assert_eq!(pass.desired, Some(RunState::Stopped));
    assert_eq!(pass.enforcement(), Enforcement::Stop);
    assert_eq!(pass.by.as_deref(), Some("robb"));
}

#[test]
fn metadata_travels_with_the_entry_that_set_the_state() {
    let (pass, _d) = pass_for(
        files_with_state(concat!(
            "fleet:\n  state: running\n",
            "hosts:\n",
            "  build-1:\n",
            "    state: paused\n",
            "    by: operator\n",
            "    since: 2026-01-01T00:00Z\n",
            "    reason: disk replacement\n",
        )),
        "build-1",
    );
    assert_eq!(pass.by.as_deref(), Some("operator"));
    assert_eq!(pass.since.as_deref(), Some("2026-01-01T00:00Z"));
    assert_eq!(pass.reason.as_deref(), Some("disk replacement"));
    let note = hold_note(&pass, "build-1", "acme/fleet");
    assert!(note.contains("host entry"), "{note}");
    assert!(note.contains("by operator"), "{note}");
    assert!(note.contains("reason: disk replacement"), "{note}");
}

#[test]
fn a_store_with_no_state_file_proceeds_and_says_why() {
    let (pass, _d) = pass_for(files_without_state(), "build-1");
    assert_eq!(pass.desired, None);
    assert_eq!(pass.enforcement(), Enforcement::Proceed);
    let err = pass.error.expect("a missing state file is reported");
    assert!(err.contains(crate::fleet_store::STATE_PATH), "{err}");
}

#[test]
fn an_unparseable_state_file_proceeds_rather_than_stranding_the_host() {
    let (pass, _d) = pass_for(files_with_state("fleet:\n  state: retired\n"), "build-1");
    assert_eq!(pass.desired, None);
    assert_eq!(pass.enforcement(), Enforcement::Proceed);
    assert!(pass.error.is_some());
}

#[test]
fn a_state_file_naming_neither_this_host_nor_a_default_proceeds() {
    let (pass, _d) =
        pass_for(files_with_state("hosts:\n  build-9:\n    state: stopped\n"), "build-1");
    assert_eq!(pass.desired, None);
    assert_eq!(pass.enforcement(), Enforcement::Proceed);
}

// ============================================================================
// The unreachable forge — the safety invariant
// ============================================================================

#[test]
fn an_unreachable_forge_honours_the_last_good_cached_snapshot() {
    let forge = FakeForge::new(files_with_state(
        "fleet:\n  state: running\nhosts:\n  build-1:\n    state: stopped\n",
    ));
    let dir = tempfile::tempdir().unwrap();
    // One good read populates the cache...
    let live = state_pass(&forge, dir.path(), &location(), "build-1", None, t0());
    assert_eq!(live.enforcement(), Enforcement::Stop);
    assert!(!live.cached);

    // ...then the forge goes away entirely.
    forge.offline.set(true);
    let later = t0() + Duration::hours(6);
    let cached = state_pass(&forge, dir.path(), &location(), "build-1", None, later);
    assert_eq!(
        cached.enforcement(),
        Enforcement::Stop,
        "a forge outage must never resume a stopped host"
    );
    assert!(cached.cached, "the answer came from the cache");
    assert!(!cached.from_last_recorded);
    let warning = cached
        .staleness
        .clone()
        .expect("a cached read warns about its age");
    assert!(warning.contains("CACHED"), "{warning}");
    // The refusal message carries the staleness warning through to the operator.
    let msg = refusal_message(&cached, "build-1", "acme/fleet");
    assert!(msg.contains("CACHED snapshot"), "{msg}");
    assert!(msg.contains(&warning), "{msg}");
}

#[test]
fn an_unreachable_forge_with_no_cache_falls_back_to_the_last_recorded_state() {
    let forge = FakeForge::new(sample_files());
    forge.offline.set(true);
    let dir = tempfile::tempdir().unwrap();
    let pass =
        state_pass(&forge, dir.path(), &location(), "build-1", Some(RunState::Stopped), t0());
    assert_eq!(
        pass.enforcement(),
        Enforcement::Stop,
        "a wiped cache must not resume a host this process last recorded as stopped"
    );
    assert!(pass.from_last_recorded);
    assert!(pass.error.is_some(), "the fetch failure is still reported");
    let msg = refusal_message(&pass, "build-1", "acme/fleet");
    assert!(msg.contains("last recorded state"), "{msg}");
}

#[test]
fn an_unreachable_forge_on_a_first_boot_proceeds() {
    // Nothing cached, nothing ever recorded: there is no operator intent to
    // honour, and refusing to boot on no evidence would strand a whole fleet on
    // one bad fetch.
    let forge = FakeForge::new(sample_files());
    forge.offline.set(true);
    let dir = tempfile::tempdir().unwrap();
    let pass = state_pass(&forge, dir.path(), &location(), "build-1", None, t0());
    assert_eq!(pass.desired, None);
    assert_eq!(pass.enforcement(), Enforcement::Proceed);
    assert!(pass.error.is_some());
}

#[test]
fn a_forge_that_answers_with_an_error_status_behaves_like_an_outage() {
    let forge = FakeForge::new(files_with_state(
        "fleet:\n  state: running\nhosts:\n  build-1:\n    state: paused\n",
    ));
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        state_pass(&forge, dir.path(), &location(), "build-1", None, t0()).enforcement(),
        Enforcement::Hold
    );
    forge.fail_status.set(Some(401));
    let pass = state_pass(&forge, dir.path(), &location(), "build-1", None, t0());
    assert_eq!(pass.enforcement(), Enforcement::Hold);
    assert!(pass.cached);
}

// ============================================================================
// The timer decision
// ============================================================================

#[test]
fn timer_action_only_acts_on_transitions() {
    use Enforcement::{Hold, Proceed, Stop};
    // running, not held -> nothing; running, held by us -> release.
    assert_eq!(timer_action(Proceed, false), TimerAction::None);
    assert_eq!(timer_action(Proceed, true), TimerAction::Release);
    // paused, not held -> hold; paused, already held -> nothing (silent).
    assert_eq!(timer_action(Hold, false), TimerAction::Hold);
    assert_eq!(timer_action(Hold, true), TimerAction::None);
    // stopped always stops, held or not.
    assert_eq!(timer_action(Stop, false), TimerAction::Stop);
    assert_eq!(timer_action(Stop, true), TimerAction::Stop);
}

// ============================================================================
// Applying the decision, through a recording enforcer
// ============================================================================

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
    held: Mutex<bool>,
    /// What [`Enforcer::stop`] answers — `false` models "no supervisor".
    stop_accepts: bool,
    /// Whether a refused stop falls back to a hold, as `IpcEnforcer` does.
    stop_falls_back_to_hold: bool,
}

impl Recorder {
    fn accepting() -> Self {
        Self {
            stop_accepts: true,
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Enforcer for Recorder {
    fn hold(&self, note: String) -> bool {
        self.calls.lock().unwrap().push(format!("hold: {note}"));
        let mut held = self.held.lock().unwrap();
        if *held {
            return false;
        }
        *held = true;
        true
    }

    fn release(&self) -> bool {
        self.calls.lock().unwrap().push("release".to_string());
        let mut held = self.held.lock().unwrap();
        std::mem::replace(&mut held, false)
    }

    fn is_held(&self) -> bool {
        *self.held.lock().unwrap()
    }

    fn stop(&self, reason: String) -> bool {
        self.calls.lock().unwrap().push(format!("stop: {reason}"));
        if !self.stop_accepts && self.stop_falls_back_to_hold {
            *self.held.lock().unwrap() = true;
        }
        self.stop_accepts
    }
}

fn paused_pass() -> StatePass {
    StatePass {
        desired: Some(RunState::Paused),
        source: Some("host".to_string()),
        ..StatePass::default()
    }
}

fn stopped_pass() -> StatePass {
    StatePass {
        desired: Some(RunState::Stopped),
        source: Some("fleet".to_string()),
        ..StatePass::default()
    }
}

#[test]
fn a_running_store_never_touches_a_host_it_did_not_hold() {
    let rec = Recorder::accepting();
    let applied = apply(
        TimerAction::None,
        Enforcement::Proceed,
        &rec,
        &StatePass::default(),
        "build-1",
        "acme/fleet",
    );
    assert!(applied.log.is_none());
    assert_eq!(applied.effective, Enforcement::Proceed);
    assert!(rec.calls().is_empty(), "no drain primitive was called at all");
}

#[test]
fn paused_holds_once_then_goes_quiet() {
    let rec = Recorder::accepting();
    let pass = paused_pass();

    let first = apply(
        timer_action(Enforcement::Hold, rec.is_held()),
        Enforcement::Hold,
        &rec,
        &pass,
        "build-1",
        "acme/fleet",
    );
    assert!(first.log.is_some(), "the transition is logged");
    assert_eq!(first.effective, Enforcement::Hold);
    assert!(rec.is_held());

    let second = apply(
        timer_action(Enforcement::Hold, rec.is_held()),
        Enforcement::Hold,
        &rec,
        &pass,
        "build-1",
        "acme/fleet",
    );
    assert!(second.log.is_none(), "the steady state is silent");
    assert_eq!(second.effective, Enforcement::Hold);
    assert_eq!(rec.calls().len(), 1, "no repeated drain requests");
}

#[test]
fn running_again_releases_the_hold_this_mechanism_placed() {
    let rec = Recorder::accepting();
    apply(
        TimerAction::Hold,
        Enforcement::Hold,
        &rec,
        &paused_pass(),
        "build-1",
        "acme/fleet",
    );
    assert!(rec.is_held());

    let released = apply(
        timer_action(Enforcement::Proceed, rec.is_held()),
        Enforcement::Proceed,
        &rec,
        &StatePass {
            desired: Some(RunState::Running),
            ..StatePass::default()
        },
        "build-1",
        "acme/fleet",
    );
    assert!(released.log.is_some());
    assert_eq!(released.effective, Enforcement::Proceed);
    assert!(!rec.is_held());
    assert_eq!(rec.calls().len(), 2);
    assert_eq!(rec.calls()[1], "release");
}

#[test]
fn stopped_drains_and_exits_when_a_supervisor_can_perform_it() {
    let rec = Recorder::accepting();
    let applied = apply(
        TimerAction::Stop,
        Enforcement::Stop,
        &rec,
        &stopped_pass(),
        "build-1",
        "acme/fleet",
    );
    assert_eq!(applied.effective, Enforcement::Stop);
    let line = applied.log.expect("a stop is always logged");
    assert!(line.contains("stopped"), "{line}");
    assert!(rec.calls()[0].contains("in-flight work finishes first"), "{:?}", rec.calls());
}

#[test]
fn a_refused_stop_degrades_to_a_hold_and_reports_it_honestly() {
    let rec = Recorder {
        stop_accepts: false,
        stop_falls_back_to_hold: true,
        ..Recorder::default()
    };
    let applied = apply(
        TimerAction::Stop,
        Enforcement::Stop,
        &rec,
        &stopped_pass(),
        "build-1",
        "acme/fleet",
    );
    assert_eq!(
        applied.effective,
        Enforcement::Hold,
        "status must not claim a stop that did not happen"
    );
    let line = applied.log.expect("a refused stop is always logged");
    assert!(line.contains("refused"), "{line}");
    assert!(line.contains("HOLDING"), "{line}");
}

// ============================================================================
// Messages
// ============================================================================

#[test]
fn the_refusal_message_says_deliberate_not_crash_and_names_the_way_out() {
    let msg = refusal_message(&stopped_pass(), "build-1", "acme/fleet");
    assert!(msg.contains("refusing to start"), "{msg}");
    assert!(msg.contains("NOT a crash"), "{msg}");
    assert!(msg.contains(&EXIT_FLEET_STOPPED.to_string()), "{msg}");
    assert!(msg.contains("fleet-config propose state running"), "{msg}");
    assert!(msg.contains(crate::fleet_store::FLEET_REPO_KEY), "{msg}");
}

#[test]
fn the_stopped_exit_code_is_not_the_relaunch_code() {
    assert_ne!(
        EXIT_FLEET_STOPPED,
        crate::ipc::EXIT_RESTART,
        "a fleet stop must never ask a supervisor to relaunch"
    );
    assert_ne!(
        EXIT_FLEET_STOPPED,
        crate::ipc::EXIT_STARTUP_FAILURE,
        "an operator must be able to tell a policy refusal from a crash"
    );
    // A `const` block: the range is the *contract* of the constant (non-zero so
    // no supervisor reads it as a clean restart, below 126 so it cannot collide
    // with a shell's signal-encoding range), so a violation should fail to
    // compile rather than at test time.
    const { assert!(EXIT_FLEET_STOPPED > 0 && EXIT_FLEET_STOPPED < 126) };
}

// ============================================================================
// The boot seam
// ============================================================================

/// The two boot outcomes that *return*. The third — `stopped` — exits the
/// process by design, so it is covered by the message and exit-code tests above
/// rather than by calling it.
#[tokio::test]
async fn the_boot_pass_holds_on_paused_and_is_inert_otherwise() {
    assert_eq!(
        enforce_at_boot(
            &StatePass {
                desired: Some(RunState::Running),
                ..StatePass::default()
            },
            "build-1",
            "acme/fleet",
        )
        .await,
        None,
        "a `running` host boots with no hold at all"
    );
    assert_eq!(
        enforce_at_boot(&StatePass::default(), "build-1", "acme/fleet").await,
        None,
        "an unreadable state (and an unset fleet.repo) boots unchanged"
    );
    let note = enforce_at_boot(&paused_pass(), "build-1", "acme/fleet")
        .await
        .expect("a `paused` host boots held");
    assert!(note.contains("dispatch HELD"), "{note}");
    assert!(note.contains("In-flight work finishes"), "{note}");
}

#[test]
fn enforcement_names_are_stable() {
    assert_eq!(Enforcement::Proceed.as_str(), "proceed");
    assert_eq!(Enforcement::Hold.as_str(), "hold");
    assert_eq!(Enforcement::Stop.as_str(), "stop");
}

#[test]
fn a_state_pass_round_trips_through_the_host_snapshot() {
    // `FleetSyncStatus` persists this verbatim; a pre-#9598 snapshot must also
    // still parse (the `#[serde(default)]` on the field).
    let pass = paused_pass();
    let json = serde_json::to_string(&pass).unwrap();
    assert_eq!(serde_json::from_str::<StatePass>(&json).unwrap(), pass);
    assert!(json.contains("\"paused\""), "{json}");
    assert_eq!(serde_json::from_str::<StatePass>("{}").unwrap(), StatePass::default());
}

// ============================================================================
// The real DrainState: hold scoping
// ============================================================================

#[test]
fn a_fleet_hold_is_released_only_by_the_fleet_mechanism() {
    let drain = crate::ipc::DrainState::new();
    assert!(!drain.is_fleet_held());

    assert!(drain.hold_for_fleet_state("paused by the store".to_string()));
    assert!(drain.is_fleet_held());
    assert!(drain.is_draining(), "dispatch is paused by the same flag a drain uses");
    assert!(!drain.hold_for_fleet_state("again".to_string()), "holding twice is a no-op");

    assert!(drain.release_fleet_hold());
    assert!(!drain.is_fleet_held());
    assert!(!drain.is_draining());
    assert!(!drain.release_fleet_hold(), "releasing a hold that is not there is a no-op");
}

#[test]
fn a_running_store_does_not_release_a_local_operator_stop_hold() {
    // #9588's startup hold: an operator stopped this host locally. The store
    // saying `running` must not undo that.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy");
    crate::operator_stop::record(&marker, "operator stopped it").unwrap();
    let drain = crate::ipc::DrainState::new().with_stop_marker(marker);
    assert!(drain.is_draining(), "the operator-stop record holds dispatch");
    assert!(!drain.is_fleet_held(), "a local operator stop is not a fleet hold");
    assert!(
        !drain.release_fleet_hold(),
        "the store must not release a local operator's stop"
    );
    assert!(drain.is_draining());
}

#[test]
fn with_fleet_hold_is_a_no_op_without_a_note_and_yields_to_a_local_stop() {
    // No note (every `running` host, and every host with no `fleet.repo`).
    let plain = crate::ipc::DrainState::new().with_fleet_hold(None);
    assert!(!plain.is_draining());
    assert!(!plain.is_fleet_held());

    // With a note, on a clean boot.
    let held = crate::ipc::DrainState::new().with_fleet_hold(Some("store says paused".to_string()));
    assert!(held.is_draining());
    assert!(held.is_fleet_held());

    // With a note, but a local operator stop already in place: the local
    // intent wins and keeps its own note and its own release path.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy");
    crate::operator_stop::record(&marker, "operator stopped it").unwrap();
    let both = crate::ipc::DrainState::new()
        .with_stop_marker(marker)
        .with_fleet_hold(Some("store says paused".to_string()));
    assert!(both.is_draining());
    assert!(!both.is_fleet_held(), "a local operator stop is the more specific intent");
}
