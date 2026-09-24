//! OTLP mapping for the CI record family (Issue #8824): `ci.run` / `ci.job`
//! become log records, `ci.duration` becomes one data point of the
//! `loom.ci.{run,job}.duration_ms` delta histograms. The attribute and label
//! vocabulary is owned by [`crate::telemetry::ci`] — this module only renders
//! it, so the gateway allowlist contract has a single source.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;
use opentelemetry_proto::tonic::metrics::v1::HistogramDataPoint;

use super::{kv, kv_string, nanos, visibility_str};
use crate::telemetry::ci::{CiAttr, CiDurationRecord};
use crate::telemetry::TelemetryRecord;

/// Explicit histogram bucket bounds, in milliseconds: 1s … 6h. CI jobs span
/// seconds (lint) to hours (release builds), so the bounds are roughly
/// logarithmic across that range.
pub(super) const DURATION_BOUNDS_MS: &[f64] = &[
    1_000.0,
    5_000.0,
    10_000.0,
    30_000.0,
    60_000.0,
    120_000.0,
    300_000.0,
    600_000.0,
    1_200_000.0,
    1_800_000.0,
    3_600_000.0,
    7_200_000.0,
    21_600_000.0,
];

fn attr_value(value: CiAttr) -> AnyValue {
    AnyValue {
        value: Some(match value {
            CiAttr::Str(s) => any_value::Value::StringValue(s),
            CiAttr::Int(i) => any_value::Value::IntValue(i),
            CiAttr::Bool(b) => any_value::Value::BoolValue(b),
        }),
    }
}

/// A failed/timed-out run or job is an error; everything else is info.
fn severity_for(conclusion: Option<&str>) -> SeverityNumber {
    match conclusion {
        Some("failure" | "timed_out" | "startup_failure") => SeverityNumber::Error,
        _ => SeverityNumber::Info,
    }
}

/// `(event_name, severity, event time, attributes)` for a `ci.run` /
/// `ci.job` record; `None` for every other kind. The event time is the
/// run/job's own `completed_at`, not the envelope's emission instant — a
/// backfilled record must land at the moment CI finished.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>)> {
    let (name, repo, visibility, conclusion, completed_at, ci_attributes) = match record {
        TelemetryRecord::CiRun(r) => (
            "ci.run",
            &r.repo,
            r.visibility,
            r.conclusion.as_deref(),
            r.completed_at,
            r.log_attributes(),
        ),
        TelemetryRecord::CiJob(r) => (
            "ci.job",
            &r.repo,
            r.visibility,
            r.conclusion.as_deref(),
            r.completed_at,
            r.log_attributes(),
        ),
        _ => return None,
    };
    let mut attributes = vec![
        kv_string("loom.repo", repo.clone()),
        kv_string("loom.repo.visibility", visibility_str(visibility)),
    ];
    attributes.extend(
        ci_attributes
            .into_iter()
            .map(|(key, value)| kv(key, attr_value(value))),
    );
    Some((name, severity_for(conclusion), nanos(completed_at), attributes))
}

/// One histogram data point for a `ci.duration` record: count 1, sum = the
/// duration, the matching bucket incremented, labels from
/// [`CiDurationRecord::metric_labels`] only.
pub(super) fn histogram_point(record: &CiDurationRecord) -> HistogramDataPoint {
    let value = record.duration_ms as f64;
    let mut bucket_counts = vec![0_u64; DURATION_BOUNDS_MS.len() + 1];
    let bucket = DURATION_BOUNDS_MS
        .iter()
        .position(|bound| value <= *bound)
        .unwrap_or(DURATION_BOUNDS_MS.len());
    bucket_counts[bucket] = 1;
    HistogramDataPoint {
        attributes: record
            .metric_labels()
            .into_iter()
            .map(|(key, value)| kv_string(key, value))
            .collect(),
        start_time_unix_nano: nanos(record.started_at),
        time_unix_nano: nanos(record.completed_at),
        count: 1,
        sum: Some(value),
        bucket_counts,
        explicit_bounds: DURATION_BOUNDS_MS.to_vec(),
        min: Some(value),
        max: Some(value),
        ..Default::default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeSet;

    use opentelemetry_proto::tonic::metrics::v1::metric::Data;

    use super::super::{build_logs_request, build_metrics_request};
    use super::DURATION_BOUNDS_MS;
    use crate::ci_telemetry::records::{job_envelopes, run_envelopes, JobJson, RepoJson, RunJson};
    use crate::observability::otlp::{signal_for, transport::Signal};
    use crate::telemetry::ci::{CI_LOG_ATTRIBUTE_KEYS, CI_METRIC_LABEL_KEYS};
    use crate::telemetry::TelemetryEnvelope;

    fn envelopes() -> Vec<TelemetryEnvelope> {
        let repo = RepoJson {
            name: "alpha".into(),
            full_name: "org/alpha".into(),
            private: true,
            archived: false,
        };
        let run: RunJson = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "CI", "head_branch": "main", "head_sha": "abc", "event": "push",
            "status": "completed", "conclusion": "failure", "run_attempt": 1,
            "created_at": "2026-09-20T09:00:00Z", "run_started_at": "2026-09-20T09:00:10Z",
            "updated_at": "2026-09-20T09:02:10Z", "triggering_actor": {"login": "octocat"}
        }))
        .unwrap();
        let job: JobJson = serde_json::from_value(serde_json::json!({
            "id": 71, "name": "test", "status": "completed", "conclusion": "timed_out",
            "started_at": "2026-09-20T09:00:10Z", "completed_at": "2026-09-20T09:00:55Z",
            "labels": ["ubuntu-latest"], "run_attempt": 1
        }))
        .unwrap();
        let mut out = job_envelopes(&repo, &run, &job, "host-a");
        out.extend(run_envelopes(&repo, &run, "host-a"));
        out
    }

    #[test]
    fn ci_records_route_to_logs_metrics_and_traces() {
        let signals: Vec<Signal> = envelopes().iter().map(signal_for).collect();
        assert_eq!(
            signals,
            vec![
                Signal::Logs,
                Signal::Metrics,
                Signal::Traces,
                Signal::Logs,
                Signal::Metrics,
                Signal::Traces
            ]
        );
    }

    #[test]
    fn ci_log_records_carry_exactly_the_declared_attributes_at_completion_time() {
        let request = build_logs_request(&envelopes()).unwrap();
        let records: Vec<_> = request.resource_logs[0].scope_logs[0].log_records.clone();
        assert_eq!(records.len(), 2);
        let mut ci_keys = BTreeSet::new();
        for record in &records {
            for kv in &record.attributes {
                assert!(
                    kv.key.starts_with("loom.ci.")
                        || kv.key == "loom.repo"
                        || kv.key == "loom.repo.visibility",
                    "unexpected attribute {}",
                    kv.key
                );
                if kv.key.starts_with("loom.ci.") {
                    ci_keys.insert(kv.key.clone());
                }
            }
            assert!(!record.trace_id.is_empty(), "CI logs correlate to their span");
        }
        let declared: BTreeSet<String> = CI_LOG_ATTRIBUTE_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .collect();
        assert!(ci_keys.is_subset(&declared));
        let job = records.iter().find(|r| r.event_name == "ci.job").unwrap();
        let completed = chrono::DateTime::parse_from_rfc3339("2026-09-20T09:00:55Z").unwrap();
        assert_eq!(
            job.time_unix_nano,
            u64::try_from(completed.timestamp_nanos_opt().unwrap()).unwrap()
        );
    }

    #[test]
    fn ci_durations_become_delta_histograms_with_allowlisted_labels() {
        let request = build_metrics_request(&envelopes()).unwrap();
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        let names: BTreeSet<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, BTreeSet::from(["loom.ci.job.duration_ms", "loom.ci.run.duration_ms"]));
        for metric in metrics {
            let Some(Data::Histogram(histogram)) = &metric.data else {
                panic!("{} must be a histogram", metric.name);
            };
            let point = &histogram.data_points[0];
            assert_eq!(point.count, 1);
            assert_eq!(point.bucket_counts.iter().sum::<u64>(), 1);
            assert_eq!(point.explicit_bounds, DURATION_BOUNDS_MS.to_vec());
            for kv in &point.attributes {
                assert!(
                    CI_METRIC_LABEL_KEYS.contains(&kv.key.as_str()),
                    "label {} not allowlisted",
                    kv.key
                );
            }
            let expected = if metric.name == "loom.ci.run.duration_ms" {
                120_000.0
            } else {
                45_000.0
            };
            assert_eq!(point.sum, Some(expected));
        }
    }
}
