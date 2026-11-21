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

use serde::Serialize;
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
/// `loom.role_draw.first_pick`: the candidate rolled first.
pub const FIRST_PICK: &str = "loom.role_draw.first_pick";
/// `loom.role_draw.table`: ordered pairs of candidate key and decimal weight.
pub const TABLE: &str = "loom.role_draw.table";
/// `loom.role_draw.passed_over`: ordered pairs of skipped key and closed reason.
pub const PASSED_OVER: &str = "loom.role_draw.passed_over";
/// The exported role-tick and dispatch-step draw attributes. Each must
/// survive the daemon allowlist and the gateway's span `keep_keys`.
pub const KEYS: [&str; 8] = [
    PICK,
    ATTEMPT,
    TABLE_ATTEMPT,
    LAYER,
    MODEL,
    FIRST_PICK,
    TABLE,
    PASSED_OVER,
];

// Keep projection values within the span exporter's existing byte limit.
const MAX_SPAN_VALUE_BYTES: usize = 256;

fn common_attributes(draw: &RoleDrawRecord) -> TraceAttributes {
    let mut attrs = TraceAttributes::new();
    attrs.insert(PICK.to_owned(), draw.pick.clone().unwrap_or_else(|| "none".into()));
    attrs.insert(ATTEMPT.to_owned(), draw.attempt.to_string());
    attrs.insert(TABLE_ATTEMPT.to_owned(), draw.table_attempt.to_string());
    attrs.insert(LAYER.to_owned(), draw.layer.as_str().to_owned());
    attrs
}

/// The role tick's draw attributes.
#[must_use]
pub fn attributes(draw: &RoleDrawRecord) -> TraceAttributes {
    let mut attrs = common_attributes(draw);
    if let Some(model) = &draw.model {
        attrs.insert(MODEL.to_owned(), model.clone());
    }
    attrs
}

/// The spawned step's draw attributes. Skip details and seed inputs stay
/// in the existing local draw record.
pub(crate) fn step_attributes(draw: &RoleDrawRecord) -> TraceAttributes {
    let mut attrs = common_attributes(draw);
    if let Some(first) = draw.steps.first().and_then(|step| export_key(&step.picked)) {
        attrs.insert(FIRST_PICK.into(), first.to_owned());
    }
    if let Some(table) = encode_pairs(
        draw.table
            .iter()
            .map(|row| (row.candidate.as_str(), row.weight.to_string())),
    ) {
        attrs.insert(TABLE.into(), table);
    }
    if !draw.passed_over_for_span.is_empty() {
        if let Some(skipped) = encode_pairs(
            draw.passed_over_for_span
                .iter()
                .map(|(key, reason)| (key.as_str(), *reason)),
        ) {
            attrs.insert(PASSED_OVER.into(), skipped);
        }
    }
    attrs
}

/// Drawable keys can pin a profile or model. These optional segments are
/// exporter data: arbitrary config stays valid locally, but filesystem
/// spellings must not reach the first-pick attribute or either composite.
fn export_key(key: &str) -> Option<&str> {
    if key.len() > MAX_SPAN_VALUE_BYTES {
        return None;
    }
    let (tap, pin) = key
        .split_once('/')
        .map_or((key, None), |(tap, pin)| (tap, Some(pin)));
    let (runtime, profile) = tap
        .split_once(':')
        .map_or((tap, None), |(runtime, profile)| (runtime, Some(profile)));
    let safe_segment = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['.', '~'])
            && !part.ends_with([':', '='])
            && !part.contains(['/', '\\'])
            && !part.chars().any(|c| c.is_control() || c.is_whitespace())
    };
    let identifier = |part: &str| {
        safe_segment(part)
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let safe_pin = |pin: &str| match pin.split_once('/') {
        // The second slash is a single provider namespace, not an open path.
        Some((namespace, name)) => identifier(namespace) && safe_segment(name),
        None => safe_segment(pin),
    };
    (identifier(runtime) && profile.is_none_or(safe_segment) && pin.is_none_or(safe_pin))
        .then_some(key)
}

fn encode_pairs<'a, V: Serialize>(pairs: impl Iterator<Item = (&'a str, V)>) -> Option<String> {
    let pairs: Vec<_> = pairs
        .map(|(key, value)| Some((export_key(key)?, value)))
        .collect::<Option<_>>()?;
    let value = serde_json::to_string(&pairs).ok()?;
    (value.len() <= MAX_SPAN_VALUE_BYTES).then_some(value)
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
