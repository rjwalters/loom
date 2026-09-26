//! OTLP mapping for the CI record family (Issue #8824): `ci.run` / `ci.job`
//! become log records, `ci.duration` becomes one data point of the
//! `loom.ci.{run,job}.duration_ms` delta histograms. The attribute and label
//! vocabulary is owned by [`crate::telemetry::ci`] — this module only renders
//! it, so the gateway allowlist contract has a single source.
//!
//! `ci.job.log` (#8825) is a log record too, and the one kind whose **body**
//! is not its event name: it carries the chunk's raw log text, unscrubbed,
//! because the gateway is the redaction boundary. That mapping is rendered in
//! the parent module (`body_override` in `log_record_for`) and contract-tested
//! here.

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
/// `ci.job` / `ci.job.log` record; `None` for every other kind. The event
/// time is the run/job's own `completed_at`, not the envelope's emission
/// instant — a backfilled record must land at the moment CI finished.
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
        // Issue #8825. A log chunk carries no conclusion of its own (its job's
        // `ci.job` record does), so it is always Info severity — severity must
        // never be inferred from the log text, which is unscrubbed at this
        // point in the pipeline.
        TelemetryRecord::CiJobLog(r) => {
            ("ci.job.log", &r.repo, r.visibility, None, r.completed_at, r.log_attributes())
        }
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

    use opentelemetry_proto::tonic::common::v1::any_value;
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

    /// A `ci.job.log` chunk (#8825) maps to a **log** record whose body is the
    /// chunk's raw text — *not* its event name, the way every other kind's
    /// body is — and to no metric at all. The secret-shaped line survives the
    /// daemon on purpose: redaction happens at the gateway, and a scrubber
    /// here would make it ambiguous which of the two is authoritative.
    #[test]
    fn ci_job_log_chunks_become_log_records_whose_body_is_the_raw_chunk_text() {
        // Two lines, chunked at a size that fits one of them, so the
        // multi-chunk reconstruction path is what is asserted.
        let text = "09:00:11 token: ghp_FIXTUREAAAAAAAAAAAA\n09:00:12 done\n";
        let target = crate::ci_telemetry::logs::LogTarget {
            repo: "org/alpha".into(),
            visibility: crate::telemetry::RepoVisibility::Private,
            run_id: 7,
            job_id: 71,
            attempt: 1,
            workflow: "CI".into(),
            job: "test".into(),
            completed_at: chrono::DateTime::parse_from_rfc3339("2026-09-20T09:00:55Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let chunked = crate::ci_telemetry::logs::chunk(text, 1024, 48);
        let envelopes = crate::ci_telemetry::logs::log_envelopes(&target, &chunked, "host-a");
        assert!(envelopes.len() > 1, "the fixture must exercise more than one chunk");
        assert!(envelopes.iter().all(|env| signal_for(env) == Signal::Logs));
        assert!(
            build_metrics_request(&envelopes).is_none(),
            "a log chunk must not become a metric of any kind"
        );

        let request = build_logs_request(&envelopes).unwrap();
        let records = request.resource_logs[0].scope_logs[0].log_records.clone();
        assert_eq!(records.len(), envelopes.len());
        let completed = u64::try_from(target.completed_at.timestamp_nanos_opt().unwrap()).unwrap();
        let mut body = String::new();
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.event_name, "ci.job.log");
            assert_eq!(record.time_unix_nano, completed, "chunks land at job completion");
            assert!(!record.trace_id.is_empty(), "chunks correlate to their job span");
            let Some(any_value::Value::StringValue(chunk)) =
                record.body.as_ref().and_then(|body| body.value.clone())
            else {
                panic!("chunk {index} has no string body");
            };
            assert_eq!(chunk, chunked.chunks[index], "the body IS the chunk text");
            body.push_str(&chunk);
            for kv in &record.attributes {
                assert!(
                    CI_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                        || kv.key == "loom.repo"
                        || kv.key == "loom.repo.visibility",
                    "unexpected attribute {}",
                    kv.key
                );
                // No attribute may carry log text: the gateway rewrites
                // bodies only, so an attribute would ride past the scrubber.
                if let Some(any_value::Value::StringValue(value)) =
                    kv.value.as_ref().and_then(|v| v.value.clone())
                {
                    assert!(
                        !value.contains("ghp_"),
                        "attribute {} carries log text: {value}",
                        kv.key
                    );
                }
            }
        }
        assert_eq!(body, text, "ordering by chunk_index reproduces the log");
        assert!(body.contains("ghp_FIXTUREAAAAAAAAAAAA"), "the daemon does not scrub");
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
