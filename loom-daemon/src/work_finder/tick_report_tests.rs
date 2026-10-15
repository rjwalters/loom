//! Typed dispatch-refusal reasons and admission spans (Issue #8907).

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;

use super::TickReport;
use crate::observability::ops::dispatch::{admission_spans, tick_span};
use crate::sweep_registry::ClaimLockDispatchError;

#[test]
fn the_claim_lock_error_keeps_its_text() {
    let e = ClaimLockDispatchError {
        issue: 9,
        lock: PathBuf::from("/l/issue-9"),
    };
    assert_eq!(
        e.to_string(),
        "lock collision: issue #9 is already claimed (lock at /l/issue-9)"
    );
}

#[test]
fn a_tick_with_no_dispatch_attempt_has_no_admission_spans() {
    let report = TickReport::default();
    let tick = tick_span(&report, 1, Utc::now(), Utc::now());
    assert!(admission_spans(&report, &tick, &HashMap::new(), &HashMap::new()).is_empty());
}
