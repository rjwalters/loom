//! Test-only recorder for the global [`super::emit_metrics`] /
//! [`super::emit_span`] path (Issue #8931).
//!
//! Seams deep inside the daemon (a reaper's account mark, the pool pre-flight)
//! emit through the process-global functions, so a test cannot hand them a
//! sink. [`capture`] records, on the **calling thread only**, everything those
//! functions are given while `f` runs, instead of offering it to the global
//! sink. Per-thread, so tests running in parallel never see each other's
//! signals and never need a process-global registration.

use std::cell::RefCell;

use crate::telemetry::ops::MetricPoint;
use crate::telemetry::trace::SpanRecord;

/// What one [`capture`] call recorded.
#[derive(Debug, Default, Clone)]
pub struct Captured {
    pub metrics: Vec<MetricPoint>,
    pub spans: Vec<SpanRecord>,
}

thread_local! {
    static CAPTURE: RefCell<Option<Captured>> = const { RefCell::new(None) };
}

/// Run `f`, recording every ops signal emitted on this thread meanwhile.
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Captured) {
    let previous = CAPTURE.with(|slot| slot.replace(Some(Captured::default())));
    let value = f();
    let captured = CAPTURE
        .with(|slot| slot.replace(previous))
        .unwrap_or_default();
    (value, captured)
}

/// Record `points` when capturing (returning `None`), else hand them back.
pub(super) fn metrics(points: Vec<MetricPoint>) -> Option<Vec<MetricPoint>> {
    CAPTURE.with(|slot| match slot.borrow_mut().as_mut() {
        Some(captured) => {
            captured.metrics.extend(points);
            None
        }
        None => Some(points),
    })
}

/// Record `span` when capturing (returning `None`), else hand it back.
pub(super) fn span(span: SpanRecord) -> Option<SpanRecord> {
    CAPTURE.with(|slot| match slot.borrow_mut().as_mut() {
        Some(captured) => {
            captured.spans.push(span);
            None
        }
        None => Some(span),
    })
}
