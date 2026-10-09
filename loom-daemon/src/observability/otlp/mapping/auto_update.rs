//! OTLP mapping for `auto_update.tick` (#10414): one log record per
//! self-update decision, stamped at the tick's start.
//!
//! The body is the record's JSON. The scalars ride as `loom.auto_update.*`
//! attributes, so a per-host "decision over time" panel needs no
//! `JSONExtract`. The deciding build rides as `loom.auto_update.version` /
//! `_revision` / `_tree_state` / `_provenance_complete`.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::kinds::auto_update_tick::{AutoUpdateTickRecord, TickDecisionKind};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

/// The severity a decision warrants: a panic is an error, and so is an
/// unsatisfiable fleet floor (#10712), whatever the tick decided. A
/// stale-repo resolution and a non-success roll outcome are warnings,
/// because each means the host is not converging. Everything else is info.
fn severity(r: &AutoUpdateTickRecord) -> SeverityNumber {
    if r.floor_stall.is_some() {
        return SeverityNumber::Error;
    }
    match r.decision {
        TickDecisionKind::Panic => SeverityNumber::Error,
        TickDecisionKind::StaleRepo => SeverityNumber::Warn,
        TickDecisionKind::Fetch | TickDecisionKind::Rebuild
            if r.outcome.as_deref() != Some("success") =>
        {
            SeverityNumber::Warn
        }
        _ => SeverityNumber::Info,
    }
}

/// `(event_name, severity, record time, attributes, body)` for an
/// `auto_update.tick` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::AutoUpdateTick(r) = record else {
        return None;
    };
    let to_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let mut attributes = vec![
        kv_string("loom.auto_update.tick_id", r.tick_id.clone()),
        kv_string("loom.auto_update.decision", r.decision.as_str()),
        kv_string("loom.auto_update.reason", r.reason.clone()),
        kv_bool("loom.auto_update.roll_armed", r.roll_armed),
        kv_bool("loom.auto_update.drain_armed", r.drain.armed),
        kv_bool("loom.auto_update.drain_pending", r.drain.pending),
        kv_int("loom.auto_update.drain_refusals", i64::from(r.drain.refusals)),
        kv_int("loom.auto_update.consecutive_failures", i64::from(r.consecutive_failures)),
        kv_int("loom.auto_update.duration_ms", to_i64(r.duration_ms)),
        kv_string("loom.auto_update.version", r.loom.version.clone()),
        kv_string("loom.auto_update.revision", r.loom.revision.clone()),
        kv_string("loom.auto_update.tree_state", r.loom.tree_state.clone()),
        kv_bool("loom.auto_update.provenance_complete", r.loom.complete),
    ];
    for (key, value) in [
        ("loom.auto_update.outcome", r.outcome.clone()),
        ("loom.auto_update.installed_version", r.installed_version.clone()),
        ("loom.auto_update.target_version", r.target_version.clone()),
        ("loom.auto_update.target_published_at", r.target_published_at.clone()),
        ("loom.auto_update.drain_target", r.drain.target.clone()),
    ] {
        if let Some(value) = value {
            attributes.push(kv_string(key, value));
        }
    }
    for (key, value) in [
        ("loom.auto_update.commits_behind", r.commits_behind.map(i64::from)),
        ("loom.auto_update.hours_behind", r.hours_behind.map(i64::from)),
        ("loom.auto_update.in_flight", r.in_flight.map(to_i64)),
    ] {
        if let Some(value) = value {
            attributes.push(kv_int(key, value));
        }
    }
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("auto_update.tick", severity(r), nanos(r.started_at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "auto_update_tests.rs"]
mod tests;
