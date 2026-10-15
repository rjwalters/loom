//! OTLP mapping for `daemon.exit` (#11839): one log record when the daemon
//! process ends deliberately, stamped and observed at the instant the exit
//! began. An exit nobody asked for (`requested_by=unknown`) is a WARN, because
//! it is the one an operator needs to find. The body is the record's JSON; the
//! scalars ride as `loom.daemon_exit.*` attributes.

use super::session_output::LogParts;
use super::{kv_int, kv_string, nanos};
use crate::telemetry::kinds::daemon_exit::RequestedBy;
use crate::telemetry::TelemetryRecord;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::DaemonExit(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_int("loom.daemon_exit.exit_code", i64::from(r.exit_code)),
        kv_string("loom.daemon_exit.requested_by", r.requested_by.as_str()),
        kv_string(
            "loom.daemon_exit.relaunch_expected",
            if r.relaunch_expected { "true" } else { "false" },
        ),
        kv_string("loom.daemon_exit.supervisor", &r.supervisor),
        kv_int(
            "loom.daemon_exit.in_flight_count",
            i64::try_from(r.in_flight_sweeps.len()).unwrap_or(i64::MAX),
        ),
        kv_string("loom.daemon_exit.in_flight_sweeps", r.in_flight_sweeps.join(",")),
        kv_string("loom.daemon_exit.in_flight_fate", r.in_flight_fate.as_str()),
    ];
    if let Some(signal) = &r.signal {
        attributes.push(kv_string("loom.daemon_exit.signal", signal));
    }
    if let Some(uptime) = r.uptime_s {
        attributes
            .push(kv_int("loom.daemon_exit.uptime_s", i64::try_from(uptime).unwrap_or(i64::MAX)));
    }
    Some(LogParts {
        event_name: "daemon.exit",
        severity: if r.requested_by == RequestedBy::Unknown {
            SeverityNumber::Warn
        } else {
            SeverityNumber::Info
        },
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
    use crate::telemetry::kinds::daemon_exit::{
        DaemonExitRecord, InFlightFate, DAEMON_EXIT_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> DaemonExitRecord {
        DaemonExitRecord {
            signal: Some("SIGTERM".into()),
            exit_code: 143,
            requested_by: RequestedBy::Unknown,
            relaunch_expected: false,
            supervisor: "systemd".into(),
            in_flight_sweeps: vec!["sweep-1".into(), "sweep-2".into()],
            in_flight_fate: InFlightFate::DieWithCgroup,
            uptime_s: Some(3600),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 12, 8, 42, 26).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn keys(r: DaemonExitRecord) -> Vec<String> {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::DaemonExit(r));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "daemon.exit");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn carries_every_daemon_exit_attribute() {
        let keys = keys(record());
        for key in DAEMON_EXIT_LOG_ATTRIBUTE_KEYS
            .iter()
            .chain(&["loom.kind", "loom.record_id"])
        {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
    }

    #[test]
    fn a_non_signal_exit_omits_the_signal_attribute() {
        let mut r = record();
        r.signal = None;
        r.uptime_s = None;
        let keys = keys(r);
        assert!(!keys.iter().any(|k| k == "loom.daemon_exit.signal"));
        assert!(!keys.iter().any(|k| k == "loom.daemon_exit.uptime_s"));
    }

    #[test]
    fn only_an_unattributed_exit_is_a_warning() {
        let unknown = log_parts(&TelemetryRecord::DaemonExit(record())).unwrap();
        assert_eq!(unknown.severity, SeverityNumber::Warn);
        let mut r = record();
        r.requested_by = RequestedBy::IpcRestart;
        let asked = log_parts(&TelemetryRecord::DaemonExit(r)).unwrap();
        assert_eq!(asked.severity, SeverityNumber::Info);
    }

    #[test]
    fn invalid_provenance_is_not_exported() {
        let mut r = record();
        r.loom.version = String::new();
        assert!(log_parts(&TelemetryRecord::DaemonExit(r)).is_none());
    }
}
