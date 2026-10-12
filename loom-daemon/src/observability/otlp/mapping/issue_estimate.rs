//! One blind estimate fact, dated at its curation completion.
use super::session_output::LogParts;
use super::{kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::IssueEstimate(r) = record else { return None };
    if !r.loom.is_valid() { return None }
    let mut attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv_int("loom.issue", i64::from(r.issue)),
        kv_string("loom.fact_id", super::fact_id::derive("issue.estimate",
            &[&r.repo, &r.issue.to_string(), &r.completion_id])),
    ];
    if let Some(model) = &r.model { attributes.push(kv_string("loom.model", model)) }
    if let Some(runtime) = &r.runtime { attributes.push(kv_string("loom.runtime", runtime)) }
    Some(LogParts {
        event_name: "issue.estimate", severity: SeverityNumber::Info,
        source_at: nanos(r.estimated_at), observed_at: nanos(r.estimated_at), attributes,
        body: serde_json::to_string(r).ok()?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::telemetry::kinds::issue_estimate::{Estimate, IssueEstimateRecord};

    #[test]
    fn completion_fact_exports_all_fields_at_original_time() {
        let r = IssueEstimateRecord {
            repo: "o/r".to_string(), issue: 42,
            estimate: Estimate { difficulty: 4, lines: 1500, files: 8, doctor_risk: 0.63 },
            model: Some("actual-curator".to_string()), runtime: Some("codex".to_string()),
            estimated_at: Utc::now(), completion_id: uuid::Uuid::new_v4().to_string(),
            loom: crate::telemetry::provenance::Provenance::current(),
        };
        let record = TelemetryRecord::IssueEstimate(r.clone());
        let parts = log_parts(&record).unwrap();
        assert_eq!(parts.event_name, "issue.estimate");
        assert_eq!(parts.source_at, nanos(r.estimated_at));
        assert_eq!(parts.observed_at, nanos(r.estimated_at));
        let body: IssueEstimateRecord = serde_json::from_str(&parts.body).unwrap();
        assert_eq!(body, r);
        let first = parts.attributes.iter().find(|kv| kv.key == "loom.fact_id").unwrap();
        let retry = log_parts(&record).unwrap();
        assert_eq!(retry.attributes.iter().find(|kv| kv.key == "loom.fact_id").unwrap(), first);
        let mut next = r;
        next.completion_id = uuid::Uuid::new_v4().to_string();
        let next = log_parts(&TelemetryRecord::IssueEstimate(next)).unwrap();
        assert_ne!(next.attributes.iter().find(|kv| kv.key == "loom.fact_id").unwrap(), first);
    }
}
