//! OTLP mapping for `roster.change` (#12322): one INFO log record per roster-pass
//! change (or `skip:not-cloned`), stamped when the pass made it. The body is the
//! record's JSON; the scalars ride as `loom.roster.*` attributes, with
//! `loom.repo` / `loom.repo.visibility` when the repo is known.

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
    let TelemetryRecord::RosterChange(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("loom.roster.action", r.action.as_str()),
        kv_string("loom.roster.name", r.name.clone()),
        kv_string("loom.roster.host", r.host.clone()),
        kv_string("loom.roster.version", r.loom.version.clone()),
        kv_string("loom.roster.revision", r.loom.revision.clone()),
        kv_string("loom.roster.tree_state", r.loom.tree_state.clone()),
        kv_bool("loom.roster.provenance_complete", r.loom.complete),
    ];
    if let Some(w) = r.from_weight {
        attributes.push(kv_int("loom.roster.from_weight", i64::from(w)));
    }
    if let Some(w) = r.to_weight {
        attributes.push(kv_int("loom.roster.to_weight", i64::from(w)));
    }
    if let Some(m) = r.maintain_only {
        attributes.push(kv_bool("loom.roster.maintain_only", m));
    }
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    attributes
        .push(kv_string("loom.repo.visibility", super::metadata::visibility_str(r.visibility)));
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("roster.change", SeverityNumber::Info, nanos(r.at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::roster_change::{
        RosterAction, RosterChangeRecord, ROSTER_CHANGE_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope};

    fn record() -> RosterChangeRecord {
        RosterChangeRecord {
            repo: Some("example/repo".into()),
            visibility: RepoVisibility::Private,
            name: "repo".into(),
            action: RosterAction::SetWeight,
            from_weight: Some(1),
            to_weight: Some(3),
            maintain_only: Some(false),
            host: "host-1".into(),
            at: Utc.with_ymd_and_hms(2026, 10, 16, 6, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared_and_stamped_at_the_pass_time() {
        let r = record();
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::RosterChange(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "roster.change");
        assert_eq!(log.time_unix_nano, nanos(r.at));
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in ROSTER_CHANGE_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                ROSTER_CHANGE_LOG_ATTRIBUTE_KEYS.contains(&key)
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
    fn the_collector_keeps_every_roster_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in ROSTER_CHANGE_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
