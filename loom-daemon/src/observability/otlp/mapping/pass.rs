//! OTLP mapping for `pass.summary` and `pass.verdict` (#10752): one log record
//! per pass and one per artifact verdict, each stamped when it was decided.
//!
//! The body is the record's JSON (the per-verdict and per-reason maps, the
//! blockers, the provenance). `log_record_for` stamps `loom.kind` (what the
//! queries filter on) on every log kind centrally (#10899). The scalars ride as
//! `loom.pass.*` attributes plus the shared `loom.repo` / `loom.role`, so
//! "passes per repo", "released
//! per pass" and "why is #n still held" need no `JSONExtract`; the two count
//! maps are also flattened into `name=count,…` strings.

use std::collections::BTreeMap;

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::kinds::pass::{PassOutcome, PassSummaryRecord, PassVerdictRecord};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `a=1,b=2`, in key order.
fn flatten(counts: &BTreeMap<String, u64>) -> String {
    counts
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// A refused pass, or one with a failed write, is a warning: the mechanism is
/// not doing its job. Everything else is info.
fn summary_severity(r: &PassSummaryRecord) -> SeverityNumber {
    let failed = r.verdicts.get("failed").copied().unwrap_or(0) > 0;
    if r.outcome == PassOutcome::Refused || failed {
        SeverityNumber::Warn
    } else {
        SeverityNumber::Info
    }
}

fn summary(r: &PassSummaryRecord) -> (&'static str, SeverityNumber, u64, Vec<KeyValue>, String) {
    let attributes = vec![
        kv_string("loom.repo", r.repo.clone()),
        kv_string("loom.pass.id", r.pass_id.clone()),
        kv_string("loom.pass.mechanism", r.mechanism.clone()),
        kv_string("loom.pass.mode", r.mode.as_str()),
        kv_string("loom.pass.outcome", r.outcome.as_str()),
        kv_int("loom.pass.examined", int(r.examined)),
        kv_string("loom.pass.verdicts", flatten(&r.verdicts)),
        kv_string("loom.pass.skip_reasons", flatten(&r.skipped)),
        kv_int("loom.pass.skipped", int(r.skipped.values().sum())),
        kv_bool("loom.pass.write_cap_hit", r.write_cap_hit),
        kv_int("loom.pass.duration_ms", int(r.duration_ms)),
        kv_int("loom.pass.github_calls", int(r.github.calls)),
        kv_int("loom.pass.github_writes", int(r.github.writes)),
        kv_int("loom.pass.github_not_modified", int(r.github.not_modified)),
        kv_int("loom.pass.verdicts_emitted", int(r.verdicts_emitted)),
        kv_int("loom.pass.verdicts_unchanged", int(r.verdicts_unchanged)),
        kv_string("loom.pass.version", r.loom.version.clone()),
        kv_string("loom.pass.revision", r.loom.revision.clone()),
    ];
    let body = serde_json::to_string(r).unwrap_or_default();
    ("pass.summary", summary_severity(r), nanos(r.ended_at), attributes, body)
}

fn verdict(r: &PassVerdictRecord) -> (&'static str, SeverityNumber, u64, Vec<KeyValue>, String) {
    let mut attributes = vec![
        kv_string("loom.repo", r.repo.clone()),
        kv_string("loom.pass.id", r.pass_id.clone()),
        kv_string("loom.pass.mechanism", r.mechanism.clone()),
        kv_string("loom.pass.mode", r.mode.as_str()),
        kv_int("loom.pass.number", int(r.number)),
        kv_string("loom.pass.artifact", r.artifact.clone()),
        kv_string("loom.pass.verdict", r.verdict.clone()),
        kv_bool("loom.pass.applied", r.applied),
    ];
    if let Some(role) = &r.role {
        attributes.push(kv_string("loom.role", role.clone()));
    }
    if let Some(reason) = &r.reason {
        attributes.push(kv_string("loom.pass.reason", reason.clone()));
    }
    if !r.blockers.is_empty() {
        let blockers = r
            .blockers
            .iter()
            .map(|b| format!("{}={}", b.reference, b.state))
            .collect::<Vec<_>>()
            .join(",");
        attributes.push(kv_string("loom.pass.blockers", blockers));
    }
    for (key, labels) in [
        ("loom.pass.labels_added", &r.labels_added),
        ("loom.pass.labels_removed", &r.labels_removed),
    ] {
        if !labels.is_empty() {
            attributes.push(kv_string(key, labels.join(",")));
        }
    }
    let severity = if r.verdict == "failed" {
        SeverityNumber::Warn
    } else {
        SeverityNumber::Info
    };
    let body = serde_json::to_string(r).unwrap_or_default();
    ("pass.verdict", severity, nanos(r.at), attributes, body)
}

/// `(event_name, severity, record time, attributes, body)` for a
/// `pass.summary` or `pass.verdict` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    match record {
        TelemetryRecord::PassSummary(r) => Some(summary(r)),
        TelemetryRecord::PassVerdict(r) => Some(verdict(r)),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pass_tests.rs"]
mod tests;
