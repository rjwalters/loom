//! Allowlisted observed outcome metadata. No prompts, errors, account contents,
//! token-price estimates, or provider billing guesses enter this mapping.
use super::{
    any_string, any_value, kv, kv_int, kv_string, AnyValue, ArrayValue, KeyValue, KeyValueList,
};
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::SweepOutcomeRecord;
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
    for (key, value) in [
        ("loom.tokens_in", record.tokens_in),
        ("loom.tokens_out", record.tokens_out),
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
