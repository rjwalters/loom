//! Issue #8252: the build-stampede gate (#4929) defers REBUILDS only.
//!
//! An artifact fetch — download a signed asset, verify its checksum, relaunch
//! under the supervisor — is not a `cargo build --release` and has no stampede
//! to avoid, so it proceeds on the tick it is decided no matter how many sweeps
//! are in flight (niced when the host is busy, never postponed). The source
//! path's deferral is unchanged, and the two are distinguishable in
//! `loom-daemon status`. Both halves are asserted here, at the `decide()` level
//! and end to end through `run_tick`.
//!
//! A sibling module rather than more lines in `tests.rs`, per
//! `.loom/docs/file-size-policy.md` (that file is over the ratchet threshold).

use super::*;

/// Issue #8252: a resolved artifact is fetched on the tick it is decided, no
/// matter how many sweeps are in flight. The build-stampede gate (#4929) exists
/// to keep a `cargo build --release` off a saturated host; a checksum-verified
/// download plus a supervised relaunch is not a build, and deferring it for up
/// to `deferDeadlineSecs` stranded a fleet host on a stale binary for hours.
/// The host being busy still nices the fetch — it no longer postpones it.
///
/// This test asserts the OPPOSITE of what it did before #8252 (it was
/// `test_decide_artifact_defers_for_in_flight_sweeps_then_forces_low_priority`,
/// encoding the deferral this issue removes).
#[test]
fn test_decide_artifact_never_defers_for_in_flight_sweeps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let settle = Duration::from_secs(0);
    // A deliberately huge deadline: under the old behavior the FIRST tick
    // deferred and nothing rolled until this elapsed.
    let deadline = Duration::from_secs(100_000);
    let base = Instant::now();
    let info = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));

    // The very first tick with 3 sweeps in flight fetches — no deferral at all.
    let first = st.decide(base, &inputs(&info, &stale("c1"), true, 3), settle, deadline);
    match &first {
        TickDecision::FetchArtifact {
            version,
            why,
            low_priority,
            ..
        } => {
            assert_eq!(version, "0.19.24");
            assert!(why.contains("fetching"), "why: {why}");
            assert!(*low_priority, "a busy host still nices the fetch");
        }
        other => panic!("expected an immediate FetchArtifact, got {other:?}"),
    }

    // AC #3: nothing in the artifact decision reads as a deferred rebuild.
    let rendered = format!("{first:?}");
    assert!(!rendered.contains("deferring"), "artifact decision must not defer: {rendered}");
    assert!(
        !rendered.contains("rebuild"),
        "artifact decision must not mention a rebuild: {rendered}"
    );

    // Still immediate at an absurd in-flight count, and on a later tick.
    let later = base + Duration::from_secs(60);
    assert!(
        matches!(
            st.decide(later, &inputs(&info, &stale("c1"), true, 97), settle, deadline),
            TickDecision::FetchArtifact {
                low_priority: true,
                ..
            }
        ),
        "a saturated host must still fetch immediately"
    );

    // An idle host fetches too, at normal priority.
    let idle_tick = later + Duration::from_secs(60);
    assert!(
        matches!(
            st.decide(idle_tick, &inputs(&info, &stale("c1"), true, 0), settle, deadline),
            TickDecision::FetchArtifact {
                low_priority: false,
                ..
            }
        ),
        "an idle host fetches at normal priority"
    );
}

/// AC #2's regression guard: the SOURCE path's stampede deferral is untouched
/// by #8252 — it still defers while the host is busy and still forces a
/// low-priority rebuild once `defer_deadline` of continuous deferral elapses.
/// Asserted here alongside the artifact case because both paths share
/// `in_flight_gate`, so a change to one must be visibly absent from the other.
#[test]
fn test_decide_source_still_defers_for_in_flight_sweeps_then_forces_low_priority() {
    let mut st = AutoUpdateState::new();
    let settle = Duration::from_secs(0);
    let deadline = Duration::from_secs(100);
    let base = Instant::now();

    // No artifact resolves ⇒ the source path decides the tick.
    let deferred = st.decide(base, &inputs(&unresolved(), &stale("c1"), true, 3), settle, deadline);
    match &deferred {
        TickDecision::Skip(reason) => {
            assert!(reason.contains("in-flight sweep(s)"), "reason: {reason}");
            // AC #3: the rebuild path's skip reason names a REBUILD, so
            // `loom-daemon status` can tell the two apart.
            assert!(reason.contains("deferring the source rebuild"), "reason: {reason}");
        }
        other => panic!("expected a deferral Skip, got {other:?}"),
    }

    let past = base + deadline + Duration::from_secs(1);
    assert_eq!(
        st.decide(past, &inputs(&unresolved(), &stale("c1"), true, 3), settle, deadline),
        TickDecision::Rebuild { low_priority: true },
        "the rebuild path must still force a low-priority build past the deadline (#4929)"
    );
}

/// AC #1 + #3 end to end (Issue #8252): with sweeps in flight and an artifact
/// available, the tick fetches and installs it — through the fake release
/// resolver, the real `run_tick` — and the status note an operator reads says
/// "fetched release artifact", never "deferring … rebuild".
#[test]
fn test_run_tick_fetches_the_artifact_while_sweeps_are_in_flight() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B))),
        check: stale("c1"),
        tree_clean: Some(true),
        // The 2026-09-18 incident's shape: a busy host sitting on a resolved
        // artifact it refused to fetch.
        in_flight: 6,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    // A deliberately long deadline: pre-#8252 this tick deferred for hours.
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(
        fetch_calls.load(Ordering::SeqCst),
        1,
        "in-flight sweeps must not defer an artifact fetch"
    );
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "no cargo build on the artifact path");
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 1, "a successful fetch triggers the drain");

    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("fetched release artifact"), "note: {note}");
    assert!(
        note.contains("reduced priority"),
        "a busy host still nices the fetch — note: {note}"
    );
    assert!(!note.contains("deferring"), "note must not read as a deferral: {note}");
    assert!(
        !note.contains("forced past the in-flight gate"),
        "the fetch was never gated, so nothing was forced past it — note: {note}"
    );
}

/// The same busy host with NO artifact: the source path still defers, and the
/// status text says so in rebuild terms (AC #2 + #3's other half).
#[test]
fn test_run_tick_without_an_artifact_still_defers_the_rebuild_while_busy() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = ArtifactFakeProbe {
        artifact: unresolved(),
        check: stale("c1"),
        tree_clean: Some(true),
        in_flight: 6,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "the rebuild stays deferred (#4929)");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("deferring the source rebuild"), "note: {note}");
    assert!(!note.contains("fetched release artifact"), "note: {note}");
}
