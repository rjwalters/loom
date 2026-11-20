//! OTLP mapping for `pass.verdict` (#10752): one log record per artifact
//! verdict, stamped when it was decided (`pass.summary` retired, #12094).
//!
//! The body is the record's JSON (the per-verdict and per-reason maps, the
//! blockers, the provenance). `log_record_for` stamps `loom.kind` (what the
//! queries filter on) on every log kind centrally (#10899). The scalars ride as
//! `loom.pass.*` attributes plus the shared `loom.repo` / `loom.role`, so
//! "passes per repo", "released
//! per pass" and "why is #n still held" need no `JSONExtract`; the two count
//! maps are also flattened into `name=count,…` strings.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_bool, kv_int, kv_string, nanos};
use crate::telemetry::kinds::pass::{PassVerdictRecord, StepFacts};
use crate::telemetry::TelemetryRecord;

fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A role step's facts (#12027), each only when present.
fn step_attributes(s: &StepFacts, attributes: &mut Vec<KeyValue>) {
    let first_refused_at = s
        .first_refused_at
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    for (key, value) in [
        ("loom.sweep_id", s.sweep_id.as_ref()),
        ("loom.pass.head_sha", s.head_sha.as_ref()),
        ("loom.pass.step", s.step.as_ref()),
        ("loom.pass.class", s.class.as_ref()),
        ("loom.pass.reason_set", s.reason_set.as_ref()),
        ("loom.pass.ledger_class", s.ledger_class.as_ref()),
        ("loom.pass.ledger_reason", s.ledger_reason.as_ref()),
        ("loom.pass.fact_id", s.fact_id.as_ref()),
        ("loom.pass.first_refused_at", first_refused_at.as_ref()),
        ("loom.pass.criterion", s.criterion_key.as_ref()),
        ("loom.pass.proposal_label", s.proposal_label.as_ref()),
        ("loom.pass.body_hash", s.body_hash.as_ref()),
    ] {
        if let Some(value) = value {
            attributes.push(kv_string(key, value.clone()));
        }
    }
    if let Some(loom) = &s.loom {
        attributes.push(kv_string("loom.pass.version", loom.version.clone()));
        attributes.push(kv_string("loom.pass.revision", loom.revision.clone()));
    }
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
    if let Some(reason) = &r.unevaluated_reason {
        attributes.push(kv_string("loom.pass.unevaluated_reason", reason.clone()));
    }
    if let Some(b) = &r.budget {
        attributes.push(kv_string("loom.pass.budget.bucket", b.bucket.clone()));
        attributes.push(kv_int("loom.pass.budget.remaining", int(b.remaining)));
        attributes.push(kv_int("loom.pass.budget.floor", int(b.floor)));
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
    step_attributes(&r.step, &mut attributes);
    // A step's detail is the session's own reason token (#12715), bounded and
    // token-only; any other record's detail stays body-only.
    if let (Some(_), Some(detail)) = (&r.step.step, &r.detail) {
        attributes.push(kv_string("loom.pass.reason_detail", detail.clone()));
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
/// `pass.verdict` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    match record {
        TelemetryRecord::PassVerdict(r) => Some(verdict(r)),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pass_tests.rs"]
mod tests;
