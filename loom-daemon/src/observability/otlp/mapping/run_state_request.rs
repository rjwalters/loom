//! OTLP mapping for `daemon.run_state_request` (#12546): one log record when
//! the daemon receives a run-state IPC request, stamped and observed at
//! receipt. The body is the record's JSON; the scalars ride as
//! `loom.run_state.*` attributes.

use super::session_output::LogParts;
use super::{kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::RunStateRequest(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.run_state.request_id", &r.request_id),
        kv_string("loom.run_state.verb", r.verb.as_str()),
        kv_string("loom.run_state.flags", r.flags.join(",")),
        kv_string(
            "loom.run_state.caller_internal",
            if r.caller_internal { "true" } else { "false" },
        ),
        kv_string("loom.run_state.caller_chain", r.caller_chain.join(">")),
        kv_string("loom.run_state.host_id", &r.host_id),
    ];
    if let Some(pid) = r.caller_pid {
        attributes.push(kv_int("loom.run_state.caller_pid", i64::from(pid)));
    }
    if let Some(uid) = r.caller_uid {
        attributes.push(kv_int("loom.run_state.caller_uid", i64::from(uid)));
    }
    if let Some(comm) = &r.caller_comm {
        attributes.push(kv_string("loom.run_state.caller_comm", comm));
    }
    Some(LogParts {
        event_name: "daemon.run_state_request",
        severity: SeverityNumber::Info,
        source_at: nanos(r.observed_at),
        observed_at: nanos(r.observed_at),
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
    use crate::telemetry::kinds::run_state_request::{
        RunStateRequestRecord, RunStateVerb, RUN_STATE_REQUEST_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> RunStateRequestRecord {
        RunStateRequestRecord {
            request_id: "rsr-1".into(),
            verb: RunStateVerb::Drain,
            flags: vec!["then_exit=true".into()],
            caller_pid: Some(4242),
            caller_uid: Some(1000),
            caller_comm: Some("loom-daemon".into()),
            caller_chain: vec!["bash".into(), "sshd".into()],
            caller_internal: false,
            host_id: "host-1".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 12, 8, 42, 26).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn keys(r: RunStateRequestRecord) -> Vec<String> {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::RunStateRequest(r));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "daemon.run_state_request");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn carries_every_run_state_attribute() {
        let keys = keys(record());
        for key in RUN_STATE_REQUEST_LOG_ATTRIBUTE_KEYS
            .iter()
            .chain(&["loom.kind", "loom.record_id"])
        {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
    }

    #[test]
    fn unresolved_caller_fields_are_omitted() {
        let mut r = record();
        r.caller_pid = None;
        r.caller_uid = None;
        r.caller_comm = None;
        let keys = keys(r);
        for gone in ["caller_pid", "caller_uid", "caller_comm"] {
            assert!(!keys.iter().any(|k| k == &format!("loom.run_state.{gone}")));
        }
    }

    #[test]
    fn invalid_provenance_is_not_exported() {
        let mut r = record();
        r.loom.version = String::new();
        assert!(log_parts(&TelemetryRecord::RunStateRequest(r)).is_none());
    }
}
