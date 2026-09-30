//! Calibration helper for burst-concurrency assertions, split out of
//! `ipc/tests.rs` (#9194) so that already-over-threshold file does not grow
//! past the `.loom/docs/file-size-policy.md` ratchet.

use super::*;

/// Measures the wall-clock cost of one fully serialized `SweepRegistry::dispatch`
/// call against a live registry, then cancels the dispatched sweep. Used as a
/// per-host, per-run calibration baseline for burst-concurrency assertions, so
/// the "if this were serialized" bound reflects the actual per-dispatch cost on
/// this host right now instead of assuming an idle-host `poll_delay` constant,
/// which drifts under host load (issue #9194). Runs via `spawn_blocking` since
/// `dispatch` blocks synchronously for ~`poll_delay`.
pub(super) async fn measure_serial_cost(
    sr: &Arc<Mutex<SweepRegistry>>,
    calibration_issue: u32,
) -> Duration {
    let sr_calibration = sr.clone();
    let start = std::time::Instant::now();
    let calibration = tokio::task::spawn_blocking(move || {
        sr_calibration.lock().unwrap().dispatch(
            &SweepKind::Issue(calibration_issue),
            None,
            None,
            None,
            None,
        )
    })
    .await
    .expect("calibration task panicked")
    .expect("calibration dispatch should succeed");
    let elapsed = start.elapsed();
    let mut guard = sr.lock().unwrap();
    let _ = guard.cancel(&calibration.sweep_id, Duration::from_millis(50));
    elapsed
}
