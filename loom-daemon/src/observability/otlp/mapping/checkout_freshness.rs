//! OTLP mapping for `checkout.freshness` (#12509): one log record per managed
//! checkout per fast-forward pass, stamped at `captured_at`. The
//! `loom.prompt.*` attributes are the ones a daemon role tick exports; the
//! collector's log `keep_keys` must list each (contract-tested). The body is
//! the record's JSON.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos, prompt_tree};
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)` for a
/// `checkout.freshness` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::CheckoutFreshness(r) = record else {
        return None;
    };
    let mut attributes = Vec::new();
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    if let Some(reason) = &r.pass_skipped_reason {
        attributes.push(kv_string("loom.pass_skipped_reason", reason.clone()));
    }
    if let Some(age) = r.remote_tip_age_s {
        attributes
            .push(kv_int("loom.prompt.remote_tip_age_s", i64::try_from(age).unwrap_or(i64::MAX)));
    }
    attributes.extend(prompt_tree::attributes(r.prompt.as_ref()));
    let body = serde_json::to_string(r).unwrap_or_default();
    Some((
        "checkout.freshness",
        SeverityNumber::Info,
        nanos(r.captured_at),
        attributes,
        body,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::role_tick_telemetry::prompt_tree::PromptTree;
    use crate::telemetry::kinds::checkout_freshness::{
        CheckoutFreshnessRecord, CHECKOUT_FRESHNESS_LOG_ATTRIBUTE_KEYS,
    };

    fn record(skipped: Option<&str>) -> CheckoutFreshnessRecord {
        CheckoutFreshnessRecord {
            captured_at: chrono::Utc::now(),
            repo: Some("o/r".to_string()),
            pass_skipped_reason: skipped.map(str::to_string),
            remote_tip_age_s: Some(120),
            prompt: Some(PromptTree {
                commit: "a".repeat(40),
                loom_version: Some("0.19.1100".to_string()),
                commits_behind: Some(1),
                on_branch: Some(true),
                remote_tip: Some("b".repeat(40)),
                commits_behind_remote: Some(3),
                origin_ref_stale: Some(true),
                remote_repo: Some("o/r".to_string()),
                remote_foreign: Some(false),
            }),
        }
    }

    fn has(attrs: &[KeyValue], key: &str) -> bool {
        attrs.iter().any(|kv| kv.key == key)
    }

    #[test]
    fn every_attribute_is_declared_and_the_collector_keeps_it() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let (name, _, _, attributes, body) =
            log_parts(&TelemetryRecord::CheckoutFreshness(record(Some("low-disk")))).unwrap();
        assert_eq!(name, "checkout.freshness");
        for key in [
            "loom.repo",
            "loom.pass_skipped_reason",
            "loom.prompt.commits_behind_remote",
            "loom.prompt.origin_ref_stale",
        ] {
            assert!(has(&attributes, key), "{key}");
        }
        assert!(body.contains("low-disk"), "{body}");
        for kv in &attributes {
            assert!(
                CHECKOUT_FRESHNESS_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()),
                "undeclared attribute key {}",
                kv.key
            );
        }
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.pr.state"))
            .expect("the transform/privacy log keep_keys line");
        for key in CHECKOUT_FRESHNESS_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    #[test]
    fn a_skipped_pass_with_nothing_measured_carries_only_the_reason() {
        let mut r = record(Some("budget"));
        r.prompt = None;
        let (_, _, _, attributes, _) = log_parts(&TelemetryRecord::CheckoutFreshness(r)).unwrap();
        assert!(has(&attributes, "loom.pass_skipped_reason"));
        assert!(!has(&attributes, "loom.prompt.commit"));
        assert!(!has(&attributes, "loom.prompt.commits_behind_remote"));
    }
}
