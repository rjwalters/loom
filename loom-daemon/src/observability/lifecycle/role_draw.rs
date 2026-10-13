//! The weighted role draw's pick on the role tick's `loom.role_attempt` span
//! (#11726; telemetry gap 2AMLogic/2am#4393).
//!
//! The full draw record lands on `role_tick.outcome`, which stays in the
//! host's `role-tick-telemetry.jsonl` and is not shipped. The tick's root
//! span is exported, so the grouping keys a reader needs — which candidate
//! ran (`pick`), on which attempt and which table, set by which config tier,
//! pinning which model — ride it too, under the same attribute names the
//! `role_tick.outcome` mapping uses. A reader then groups `loom.role` ×
//! `loom.role_draw.pick` × `loom.result` from the span alone.
//!
//! The launch site in `role_runner` notes the draw here, on the same
//! thread-local channel [`super::role_failure`] uses; the close in
//! [`super::role_invocation`] reads it back. A tick that `rolePreference`
//! decided (no `roleDraw` table) notes nothing, and its span carries none of
//! these keys.

use std::cell::RefCell;

use crate::runtime_preference::role_draw::RoleDrawRecord;
use crate::telemetry::trace::TraceAttributes;

/// `loom.role_draw.pick`: the candidate key that launched (`none` when
/// nothing could).
pub const PICK: &str = "loom.role_draw.pick";
/// `loom.role_draw.attempt`: this tick's attempt number.
pub const ATTEMPT: &str = "loom.role_draw.attempt";
/// `loom.role_draw.table_attempt`: the configured table that applied.
pub const TABLE_ATTEMPT: &str = "loom.role_draw.table_attempt";
/// `loom.role_draw.layer`: the config tier that set that table.
pub const LAYER: &str = "loom.role_draw.layer";
/// `loom.role_draw.model`: the model the pick pins, when it pins one.
pub const MODEL: &str = "loom.role_draw.model";
/// The exported span attributes this module adds. Each must survive the
/// daemon allowlist and the gateway's span `keep_keys` (contract-tested).
pub const KEYS: [&str; 5] = [PICK, ATTEMPT, TABLE_ATTEMPT, LAYER, MODEL];

/// The span attributes for one draw.
#[must_use]
pub fn attributes(draw: &RoleDrawRecord) -> TraceAttributes {
    let mut attrs = TraceAttributes::new();
    attrs.insert(PICK.to_owned(), draw.pick.clone().unwrap_or_else(|| "none".into()));
    attrs.insert(ATTEMPT.to_owned(), draw.attempt.to_string());
    attrs.insert(TABLE_ATTEMPT.to_owned(), draw.table_attempt.to_string());
    attrs.insert(LAYER.to_owned(), draw.layer.as_str().to_owned());
    if let Some(model) = &draw.model {
        attrs.insert(MODEL.to_owned(), model.clone());
    }
    attrs
}

thread_local! {
    static NOTED: RefCell<Option<TraceAttributes>> = const { RefCell::new(None) };
}

/// Record the draw that decided this thread's in-flight role tick. `None`
/// (no `roleDraw` table decided it) clears any earlier note.
pub fn note_role_draw(draw: Option<&RoleDrawRecord>) {
    NOTED.with(|slot| *slot.borrow_mut() = draw.map(attributes));
}

/// Drop any note left behind, so a tick starts clean.
pub(super) fn clear() {
    NOTED.with(|slot| slot.borrow_mut().take());
}

/// Take this tick's note, if any.
pub(super) fn take() -> Option<TraceAttributes> {
    NOTED.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
#[path = "role_draw/tests.rs"]
mod tests;
