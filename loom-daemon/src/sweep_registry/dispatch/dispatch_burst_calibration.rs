//! Calibration helper for burst-concurrency assertions, split out of
//! `dispatch/tests.rs` (#9194) — mirrors
//! `ipc/tests/dispatch_burst_calibration.rs` — so that already-over-threshold
//! file does not grow past the `.loom/docs/file-size-policy.md` ratchet.
//! Registered from its foot via `#[path]`, the shape `dispatch/tests.rs`
//! already uses for `operator_hold_tests.rs`.

use super::*;

/// Measures the wall-clock cost of one fully serialized `SweepRegistry::dispatch`
/// call against a live registry, then cancels the dispatched sweep. Used as a
/// per-host, per-run calibration baseline for burst-concurrency assertions, so
/// the "if this were serialized" bound reflects the actual per-dispatch cost on
/// this host right now instead of assuming an idle-host `poll_delay` constant,
/// which drifts under host load (issue #9194).
pub(super) fn measure_one_serialized_dispatch(
    registry: &Arc<Mutex<SweepRegistry>>,
    calibration_issue: u32,
) -> Duration {
    let start = Instant::now();
    let calibration = {
        let mut sr = registry.lock().unwrap();
        sr.dispatch(&SweepKind::Issue(calibration_issue), None, None, None, None)
            .expect("calibration dispatch should succeed")
    };
    let elapsed = start.elapsed();
    let mut sr = registry.lock().unwrap();
    let _ = sr.cancel(&calibration.sweep_id, Duration::from_millis(50));
    elapsed
}
