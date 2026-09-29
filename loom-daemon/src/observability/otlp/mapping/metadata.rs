//! Allowlisted observed outcome metadata. No prompts, errors, account contents,
//! token-price estimates, or provider billing guesses enter this mapping.
use super::{
    any_string, any_value, kv, kv_int, kv_string, AnyValue, ArrayValue, KeyValue, KeyValueList,
};
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::{PhaseDuration, SweepOutcomeRecord};
const MAX_GROUPS: usize = 64;
const MAX_STRING_BYTES: usize = 256;

fn text(value: &str) -> bool {
    value.len() <= MAX_STRING_BYTES && !value.chars().any(char::is_control)
}
fn array(key: &str, values: Vec<AnyValue>) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::ArrayValue(ArrayValue { values })),
        },
    )
}
fn row(values: Vec<KeyValue>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::KvlistValue(KeyValueList { values })),
    }
}

pub(super) fn usage(rows: Option<&[ModelUsageTotals]>) -> Option<KeyValue> {
    let rows = rows.filter(|rows| !rows.is_empty())?;
    if rows.len() > MAX_GROUPS
        || rows.iter().any(|r| {
            !text(&r.model)
                || !text(&r.speed)
                || !text(&r.service_tier)
                || [
                    r.input,
                    r.cache_read,
                    r.cache_write_5m,
                    r.cache_write_1h,
                    r.output,
                ]
                .iter()
                .any(|v| *v < 0)
        })
    {
        log::warn!("observability: invalid or oversized usage groups omitted");
        return None;
    }
    Some(array(
        "loom.tokens_by_model",
        rows.iter()
            .map(|r| {
                row(vec![
                    kv_string("model", r.model.clone()),
                    kv_string("speed", r.speed.clone()),
                    kv_string("service_tier", r.service_tier.clone()),
                    kv_int("input", r.input),
                    kv_int("cache_read", r.cache_read),
                    kv_int("cache_write_5m", r.cache_write_5m),
                    kv_int("cache_write_1h", r.cache_write_1h),
                    kv_int("output", r.output),
                ])
            })
            .collect(),
    ))
}

/// `loom.phase_durations` (Issue #9443): the per-phase-attempt breakdown, each
/// entry carrying its `attempt` index and token split beside its duration.
///
/// Absent-not-zero is preserved per key: a phase whose usage was not measured
/// emits `phase`/`duration_sec` alone, so a consumer summing `tokens_in` over
/// these entries and comparing against `loom.tokens_in` sees the shortfall
/// (reported explicitly as `loom.tokens_unattributed_in`) rather than a phase
/// claiming it was free.
///
/// Per-phase `tokens_by_model` is deliberately **not** exported here: it would
/// nest an array of kvlists inside an array of kvlists, and the per-phase model
/// breakdown already reaches OTLP the better way — as this sweep's
/// `loom.runtime.usage` spans with `loom.usage.scope=attempt`, one per model
/// under each `loom.role_attempt`.
pub(super) fn phase_durations(entries: &[PhaseDuration]) -> Option<KeyValue> {
    if entries.is_empty() {
        return None;
    }
    if entries.len() > MAX_GROUPS || entries.iter().any(|e| !text(&e.phase)) {
        log::warn!("observability: invalid or oversized phase durations omitted");
        return None;
    }
    Some(array(
        "loom.phase_durations",
        entries
            .iter()
            .map(|entry| {
                let mut values = vec![
                    kv_string("phase", entry.phase.clone()),
                    kv_int("duration_sec", entry.duration_sec),
                ];
                if let Some(attempt) = entry.attempt {
                    values.push(kv_int("attempt", i64::from(attempt)));
                }
                for (key, value) in [
                    ("tokens_in", entry.tokens_in),
                    ("tokens_out", entry.tokens_out),
                ] {
                    if let Some(value) = value.and_then(|v| i64::try_from(v).ok()) {
                        values.push(kv_int(key, value));
                    }
                }
                row(values)
            })
            .collect(),
    ))
}

pub(super) fn models(values: Option<&[String]>) -> Option<KeyValue> {
    let values = values.filter(|rows| !rows.is_empty() && rows.len() <= MAX_GROUPS)?;
    values
        .iter()
        .all(|s| text(s))
        .then(|| array("loom.models_used", values.iter().map(|v| any_string(v.clone())).collect()))
}

pub(super) fn outcome(record: &SweepOutcomeRecord) -> Vec<KeyValue> {
    let mut attrs = Vec::new();
    // Config is otherwise a free-form map (including token_account). Export
    // only known execution settings, retaining legacy config.runtime queries.
    for key in ["runtime", "provider", "configured_model", "arm"] {
        if let Some(value) = record.config.get(key).filter(|value| text(value)) {
            attrs.push(kv_string(&format!("loom.config.{key}"), value.clone()));
            if key != "arm" {
                attrs.push(kv_string(&format!("loom.{key}"), value.clone()));
            }
        }
    }
    if let Some(value) = record.failure_class.as_ref().filter(|v| text(v)) {
        attrs.push(kv_string("loom.failure_class", value.clone()));
    }
    if let Some(value) = record.doctor_cycles {
        attrs.push(kv_int("loom.doctor_cycles", i64::from(value)));
    }
    // Issue #9432: the issue's dispatch-time story-point estimate (its single
    // `points:<N>` label). Absent (never `0`) for every guard-declined shape —
    // the same "unknown != zero" rule `doctor_cycles` above states.
    if let Some(value) = record.story_points {
        attrs.push(kv_int("loom.story_points", i64::from(value)));
    }
    if let Some(verdicts) = &record.judge_verdicts {
        if verdicts.len() <= MAX_GROUPS
            && verdicts
                .iter()
                .all(|v| matches!(v.verdict.as_str(), "pass" | "fail"))
        {
            // Some([]) is an observed empty history; None remains absent.
            attrs.push(array(
                "loom.judge_verdicts",
                verdicts
                    .iter()
                    .map(|v| {
                        row(vec![
                            kv_int("attempt", i64::from(v.attempt)),
                            kv_string("verdict", v.verdict.clone()),
                        ])
                    })
                    .collect(),
            ));
        } else {
            log::warn!("observability: invalid or oversized verdict history omitted");
        }
    }
    if let Some(value) = models(record.models_used.as_deref()) {
        attrs.push(value);
    }
    if let Some(value) = usage(record.tokens_by_model.as_deref()) {
        attrs.push(value);
    }
    // Issue #9440: the discriminator that makes an absent token pair readable.
    // A closed vocabulary (the enum's own serde tags), so this is an exported
    // attribute a query can group by, never free text.
    if let Some(status) = record.tokens_status {
        attrs.push(kv_string(
            "loom.tokens_status",
            match status {
                crate::telemetry::TokensStatus::Measured => "measured",
                crate::telemetry::TokensStatus::NotSpawned => "not_spawned",
                crate::telemetry::TokensStatus::Unattributable => "unattributable",
                crate::telemetry::TokensStatus::Suspect => "suspect",
            }
            .to_string(),
        ));
    }
    if let Some(value) = record.tokens_status_reason.as_ref().filter(|v| text(v)) {
        attrs.push(kv_string("loom.tokens_status_reason", value.clone()));
    }
    for (key, value) in [
        ("loom.tokens_in", record.tokens_in),
        ("loom.tokens_out", record.tokens_out),
        // Issue #9443: the remainder no phase entry accounts for, exported
        // alongside the totals so the OTLP side can check the same
        // Σphases + remainder == total invariant the JSONL record states.
        ("loom.tokens_unattributed_in", record.tokens_unattributed.map(|t| t.tokens_in)),
        ("loom.tokens_unattributed_out", record.tokens_unattributed.map(|t| t.tokens_out)),
    ] {
        if let Some(value) = value.and_then(|v| i64::try_from(v).ok()) {
            attrs.push(kv_int(key, value));
        }
    }
    for (key, value) in [
        ("loom.lines_added", record.lines_added),
        ("loom.lines_deleted", record.lines_deleted),
    ] {
        if let Some(value) = value.filter(|v| *v >= 0) {
            attrs.push(kv_int(key, value));
        }
    }
    attrs
}

// Typed constructors above define permitted keys. Apply bounds again to restored
// lifecycle records, including older phase-duration/model arrays. Omit oversized
// attributes rather than truncating an identity or turning invalid data into zero.
fn valid(value: &AnyValue, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    match &value.value {
        Some(any_value::Value::StringValue(s)) => text(s),
        Some(any_value::Value::ArrayValue(a)) => {
            a.values.len() <= MAX_GROUPS && a.values.iter().all(|v| valid(v, depth + 1))
        }
        Some(any_value::Value::KvlistValue(a)) => {
            a.values.len() <= 16
                && a.values
                    .iter()
                    .all(|v| v.value.as_ref().is_some_and(|v| valid(v, depth + 1)))
        }
        Some(any_value::Value::BytesValue(_)) => false,
        Some(_) => true,
        None => false,
    }
}
pub(super) fn bounded(attributes: Vec<KeyValue>) -> Vec<KeyValue> {
    attributes
        .into_iter()
        .filter(|kv| kv.value.as_ref().is_some_and(|v| valid(v, 0)))
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests;
