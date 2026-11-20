//! OTLP mapping for `champion.run` (#13029): one INFO log record per Champion
//! merge / hold-reevaluate step end, stamped at the end. The body is the
//! record's JSON; the scalars ride as `loom.champion.*` attributes.

use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::session_output::LogParts;
use super::{kv_bool, kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::ChampionRun(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv_int("loom.pr_number", i64::from(r.pr)),
        kv_string("loom.champion.outcome", &r.outcome),
        kv_string("loom.champion.verdict_reason", &r.verdict_reason),
        kv_string("loom.champion.sweep_id", &r.sweep_id),
        kv_bool("loom.champion.digest_recorded", r.digest_recorded),
        kv_string("loom.champion.version", &r.loom.version),
        kv_string("loom.champion.revision", &r.loom.revision),
        kv_bool("loom.champion.provenance_complete", r.loom.complete),
    ];
    for (key, value) in [
        ("loom.champion.hold_class", &r.hold_class),
        ("loom.champion.auth_before", &r.auth_before),
        ("loom.champion.auth_after", &r.auth_after),
    ] {
        if let Some(v) = value {
            attributes.push(kv_string(key, v));
        }
    }
    Some(LogParts {
        event_name: "champion.run",
        severity: SeverityNumber::Info,
        source_at: nanos(r.event_at),
        observed_at: nanos(r.event_at),
        attributes,
        body: serde_json::to_string(r).ok()?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::champion_run::{
        ChampionRunRecord, CHAMPION_RUN_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> ChampionRunRecord {
        ChampionRunRecord {
            repo: "o/r".into(),
            pr: 42,
            sweep_id: "step-champion_merge-42-1".into(),
            outcome: "held".into(),
            hold_class: Some("merge-risk-hold".into()),
            verdict_reason: "held".into(),
            auth_before: Some("aaaaaaaaaaaaaaaa".into()),
            auth_after: Some("bbbbbbbbbbbbbbbb".into()),
            digest_recorded: true,
            event_at: Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared() {
        let r = record();
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::ChampionRun(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "champion.run");
        assert_eq!(log.time_unix_nano, nanos(r.event_at));
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in CHAMPION_RUN_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                CHAMPION_RUN_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || ["loom.record_id", "loom.kind", "loom.pr_number", "loom.repo"]
                        .contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn absent_optional_fields_are_not_stamped() {
        let mut r = record();
        r.hold_class = None;
        r.auth_before = None;
        r.auth_after = None;
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::ChampionRun(r));
        let log = log_record_for(&envelope).unwrap();
        for key in ["hold_class", "auth_before", "auth_after"] {
            let key = format!("loom.champion.{key}");
            assert!(!log.attributes.iter().any(|kv| kv.key == key), "{key}");
        }
    }

    #[test]
    fn the_collector_keeps_every_champion_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in CHAMPION_RUN_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
