//! Billing / spending-limit "job was not started" detection (#10113).

use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use crate::ci_telemetry::api::{ApiError, ApiResponse, GithubApi};
use crate::ci_telemetry::billing::{
    classify_job, is_billing_message, AlertSink, BillingRegistry, BlockInfo, NotStartedReason,
    NOT_STARTED_REASON_ATTR,
};
use crate::ci_telemetry::records::{
    job_envelopes_with_reason, JobCreationBaseline, JobJson, RepoJson, RunJson,
};
use crate::telemetry::trace::SpanName;
use crate::telemetry::TelemetryRecord;

const BILLING: &str = "The job was not started because recent account payments have failed or \
your spending limit needs to be increased. Please check the 'Billing & plans' section in your settings";

struct Annotations(String);

impl GithubApi for Annotations {
    fn get(&self, _path: &str, _etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        Ok(ApiResponse {
            status: 200,
            body: self.0.clone(),
            ..ApiResponse::default()
        })
    }
    fn get_document(&self, _path: &str) -> Result<ApiResponse, ApiError> {
        unreachable!()
    }
}

fn annotation_body(message: &str) -> String {
    serde_json::json!([{ "annotation_level": "failure", "message": message, "title": "" }])
        .to_string()
}

fn repo() -> RepoJson {
    RepoJson {
        name: "2am".into(),
        full_name: "2AMLogic/2am".into(),
        private: true,
        archived: false,
    }
}

fn run() -> RunJson {
    serde_json::from_value(serde_json::json!({
        "id": 77, "name": "CI", "head_sha": "abc", "event": "push",
        "status": "completed", "conclusion": "failure", "head_branch": "main",
        "created_at": "2026-10-03T10:46:00Z", "updated_at": "2026-10-03T10:46:02Z",
    }))
    .unwrap()
}

fn job(steps: serde_json::Value, runner: &str, conclusion: &str) -> JobJson {
    serde_json::from_value(serde_json::json!({
        "id": 555, "name": "Language Policy", "status": "completed",
        "conclusion": conclusion, "run_attempt": 1, "runner_name": runner,
        "created_at": "2026-10-03T10:46:00Z", "started_at": "2026-10-03T10:46:01Z",
        "completed_at": "2026-10-03T10:46:01Z", "steps": steps,
    }))
    .unwrap()
}

fn not_started() -> JobJson {
    job(serde_json::json!([]), "", "failure")
}

#[test]
fn only_the_billing_message_matches() {
    assert!(is_billing_message(BILLING));
    assert!(!is_billing_message("The job was not started because the runner group is empty"));
    assert!(!is_billing_message("Process completed with exit code 1."));
}

#[test]
fn a_billing_annotated_job_is_classified_and_tagged() {
    let mut requests = 0;
    let reason = classify_job(
        &Annotations(annotation_body(BILLING)),
        "2AMLogic/2am",
        &not_started(),
        &mut requests,
    )
    .unwrap();
    assert_eq!(reason, Some(NotStartedReason::Billing));
    assert_eq!(requests, 1);

    let envelopes = job_envelopes_with_reason(
        &repo(),
        &run(),
        &not_started(),
        JobCreationBaseline::default(),
        "h",
        reason,
    );
    let span = envelopes
        .into_iter()
        .find_map(|e| match e.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiJob => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(span.attributes[NOT_STARTED_REASON_ATTR], "billing");
}

#[test]
fn false_positives_stay_plain_failures_and_cost_no_request() {
    let api = Annotations(annotation_body(BILLING));
    let mut requests = 0;
    // A real failure has steps and a runner: never even probed.
    let ran = job(
        serde_json::json!([{"name": "x", "number": 1, "status": "completed"}]),
        "GitHub Actions 1",
        "failure",
    );
    assert_eq!(classify_job(&api, "o/r", &ran, &mut requests).unwrap(), None);
    // Success, and cancelled, never are.
    assert_eq!(
        classify_job(&api, "o/r", &job(serde_json::json!([]), "", "success"), &mut requests)
            .unwrap(),
        None
    );
    assert_eq!(requests, 0);
    // Not-started shape but some other annotation: plain failure.
    let other = Annotations(annotation_body("The job was not started because of an outage"));
    assert_eq!(classify_job(&other, "o/r", &not_started(), &mut requests).unwrap(), None);
    // No attribute on an ordinary job span.
    let envelopes =
        job_envelopes_with_reason(&repo(), &run(), &ran, JobCreationBaseline::default(), "h", None);
    for e in envelopes {
        if let TelemetryRecord::Span(s) = e.record {
            assert!(!s.attributes.contains_key(NOT_STARTED_REASON_ATTR));
        }
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<BlockInfo>>);

impl AlertSink for Recorder {
    fn alert(&self, info: &BlockInfo) -> bool {
        self.0.lock().unwrap().push(info.clone());
        true
    }
}

fn t0() -> DateTime<Utc> {
    "2026-10-03T10:50:00Z".parse().unwrap()
}

#[test]
fn the_alert_fires_once_per_outage_and_rearms_after_it_clears() {
    let registry = BillingRegistry::new();
    let sink = Recorder::default();

    assert!(registry.observe(t0(), "2AMLogic/2am", 77, 555, &sink));
    // Many more blocked jobs, other repos of the same org: no new alert.
    for i in 0..5 {
        let now = t0() + Duration::minutes(i);
        assert!(!registry.observe(now, "2AMLogic/loom-ui", 80 + i as u64, 600 + i as u64, &sink));
    }
    {
        let alerts = sink.0.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].owner, "2amlogic");
        assert_eq!(alerts[0].run_url(), "https://github.com/2AMLogic/2am/actions/runs/77");
        assert!(alerts[0].alert_body().contains("Billing & plans"));
    }
    assert!(registry.owner_blocked("2AMLogic", t0() + Duration::minutes(10)));
    assert!(registry.any_blocked(t0() + Duration::minutes(10)));

    // Condition clears (no blocked job for longer than the TTL): not blocked...
    let later = t0() + Duration::hours(5);
    assert!(!registry.owner_blocked("2AMLogic", later));
    assert!(!registry.any_blocked(later));
    // ...and a NEW outage alerts again.
    assert!(registry.observe(later, "2AMLogic/2am", 99, 700, &sink));
    assert_eq!(sink.0.lock().unwrap().len(), 2);
}

#[test]
fn a_failed_delivery_retries_on_the_next_observation() {
    struct Flaky(Mutex<u32>);
    impl AlertSink for Flaky {
        fn alert(&self, _: &BlockInfo) -> bool {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            *n > 1
        }
    }
    let registry = BillingRegistry::new();
    let sink = Flaky(Mutex::new(0));
    assert!(!registry.observe(t0(), "o/r", 1, 1, &sink));
    assert!(registry.observe(t0(), "o/r", 2, 2, &sink));
    assert!(!registry.observe(t0(), "o/r", 3, 3, &sink));
}
