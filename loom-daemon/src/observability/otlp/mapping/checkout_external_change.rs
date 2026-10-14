//! OTLP mapping for `checkout.external_change` (#11566): one WARN log record per
//! reflog entry Loom did not write, stamped at the entry's own time (when git
//! moved the checkout), not when the daemon noticed. The body is the record's
//! JSON (it carries the discarded subjects); the scalars ride as
//! `loom.checkout.*` attributes, with `loom.repo` / `loom.repo.visibility` when
//! the repo is known.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

/// `(event_name, severity, record time, attributes, body)`; `None` for every
/// other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::CheckoutExternalChange(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("loom.checkout.change_id", r.change_id.clone()),
        kv_string("loom.checkout.kind", r.move_kind.clone()),
        kv_string("loom.checkout.ref", r.ref_name.clone()),
        kv_string("loom.checkout.from_sha", r.from_sha.clone()),
        kv_string("loom.checkout.to_sha", r.to_sha.clone()),
        kv_bool("loom.checkout.hold_active", r.hold_active),
        kv_string("loom.checkout.version", r.loom.version.clone()),
        kv_string("loom.checkout.revision", r.loom.revision.clone()),
        kv_string("loom.checkout.tree_state", r.loom.tree_state.clone()),
        kv_bool("loom.checkout.provenance_complete", r.loom.complete),
    ];
    if let Some(d) = &r.discarded_commits {
        attributes.push(kv_int(
            "loom.checkout.discarded_count",
            i64::try_from(d.count).unwrap_or(i64::MAX),
        ));
    }
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    attributes
        .push(kv_string("loom.repo.visibility", super::metadata::visibility_str(r.visibility)));
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("checkout.external_change", SeverityNumber::Warn, nanos(r.at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::checkout_external_change::{
        CheckoutExternalChangeRecord, DiscardedCommits, CHECKOUT_EXTERNAL_CHANGE_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope};

    fn record() -> CheckoutExternalChangeRecord {
        CheckoutExternalChangeRecord {
            change_id: "0123456789abcdef".into(),
            repo: Some("example/repo".into()),
            visibility: RepoVisibility::Private,
            root: "/work/repo".into(),
            host: "host-1".into(),
            move_kind: "reset".into(),
            ref_name: "HEAD".into(),
            reflog_line: "reset: moving to origin/main".into(),
            from_sha: "a".repeat(40),
            to_sha: "b".repeat(40),
            at: Utc.with_ymd_and_hms(2026, 10, 12, 0, 44, 39).unwrap(),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 12, 0, 50, 0).unwrap(),
            discarded_commits: Some(DiscardedCommits {
                count: 2,
                subjects: vec!["wip".into()],
            }),
            hold_active: true,
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared_and_stamped_at_the_reflog_time() {
        let r = record();
        let envelope =
            TelemetryEnvelope::new("host-1", TelemetryRecord::CheckoutExternalChange(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "checkout.external_change");
        assert_eq!(log.time_unix_nano, nanos(r.at));
        assert_eq!(log.severity_number, SeverityNumber::Warn as i32);
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in CHECKOUT_EXTERNAL_CHANGE_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                CHECKOUT_EXTERNAL_CHANGE_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || [
                        "loom.record_id",
                        "loom.kind",
                        "loom.repo",
                        "loom.repo.visibility"
                    ]
                    .contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn the_collector_keeps_every_checkout_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in CHECKOUT_EXTERNAL_CHANGE_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
