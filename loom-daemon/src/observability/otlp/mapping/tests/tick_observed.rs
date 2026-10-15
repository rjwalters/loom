#![allow(clippy::unwrap_used)]
use super::*;

/// #12029: a Judge tick's found-work signal, newest verdict and every verdict
/// reach the wire; an unobserved fact is an absent attribute.
#[test]
fn a_judge_ticks_found_work_and_verdicts_are_mapped() {
    use crate::telemetry::{RoleTickObserved, RoleTickVerdict};
    let mut idle = role_tick_outcome_envelope(crate::telemetry::RoleTickResult::Success, None);
    let mut worked = idle.clone();
    let bare = idle.clone();
    if let TelemetryRecord::RoleTickOutcome(r) = &mut idle.record {
        r.observed = RoleTickObserved {
            found_work: Some(false),
            noop_reason: Some("no_claim_no_verdict".into()),
            ..RoleTickObserved::default()
        };
    }
    if let TelemetryRecord::RoleTickOutcome(r) = &mut worked.record {
        r.observed = RoleTickObserved {
            found_work: Some(true),
            pr_number: Some(7),
            head_sha: Some("abcdef1".into()),
            verdict: Some("changes_requested".into()),
            verdicts: Some(vec![RoleTickVerdict {
                pr_number: 7,
                head_sha: "abcdef1".into(),
                verdict: "changes_requested".into(),
            }]),
            ..RoleTickObserved::default()
        };
    }
    let request = build_logs_request(&[idle, worked, bare]).unwrap();
    let records = &request.resource_logs[0].scope_logs[0].log_records;
    let attr = |record: &LogRecord, key: &str| {
        record
            .attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    };
    let s = |v: &str| Some(any_value::Value::StringValue(v.to_string()));
    assert_eq!(attr(&records[0], "loom.found_work"), Some(any_value::Value::BoolValue(false)));
    assert_eq!(attr(&records[0], "loom.noop_reason"), s("no_claim_no_verdict"));
    assert_eq!(attr(&records[0], "loom.judge_verdict"), None);
    assert_eq!(attr(&records[1], "loom.found_work"), Some(any_value::Value::BoolValue(true)));
    assert_eq!(attr(&records[1], "loom.pr_number"), Some(any_value::Value::IntValue(7)));
    assert_eq!(attr(&records[1], "loom.head_sha"), s("abcdef1"));
    assert_eq!(attr(&records[1], "loom.judge_verdict"), s("changes_requested"));
    assert!(attr(&records[1], "loom.tick_verdicts").is_some());
    for key in [
        "loom.found_work",
        "loom.noop_reason",
        "loom.pr_number",
        "loom.tick_verdicts",
    ] {
        assert_eq!(attr(&records[2], key), None, "{key} absent when unobserved");
    }
}
