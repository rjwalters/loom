//! The `daemon.exit` record's producer (#11839): the cause of a deliberate
//! exit, and the one place that turns it into a [`DaemonExitRecord`].
//!
//! Every deliberate exit goes through [`super::shutdown::exit`], which takes an
//! [`ExitCause`] and enqueues the record before the exporter's final drain.
//! The funnel has no handle on the workspace pool, so the daemon registers an
//! in-flight lister once at startup ([`register_in_flight_lister`]); an exit
//! before that (a fleet-state stop at boot) reports no sweeps, not an error.

use std::sync::OnceLock;
use std::time::Instant;

use chrono::Utc;

use super::queue::QueueSink;
use super::run_state_request::ExitLink;
use crate::ipc::drain_state::DrainOrigin;
use crate::telemetry::kinds::daemon_exit::{DaemonExitRecord, InFlightFate, RequestedBy};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// Why the daemon is exiting, as far as the exiting code knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCause {
    /// SIGTERM / SIGINT: the signal stream carries no sender, so the
    /// requester is `unknown`.
    Signal(&'static str),
    /// The IPC `RestartDaemon` verb (reached only when a supervisor exists).
    IpcRestart,
    /// The IPC `Shutdown` verb.
    IpcShutdown,
    /// A completed or force-timed-out drain. `then_exit` stays down; otherwise
    /// the supervisor relaunches.
    Drain {
        then_exit: bool,
        origin: DrainOrigin,
    },
    /// The fleet run-state said `stopped` at boot.
    FleetState,
    /// The opt-in idle exit.
    IdleExit,
}

impl ExitCause {
    /// The signal name, when a signal ended the daemon.
    #[must_use]
    pub fn signal(self) -> Option<&'static str> {
        match self {
            Self::Signal(name) => Some(name),
            _ => None,
        }
    }

    /// Who asked, as far as the daemon can know.
    #[must_use]
    pub fn requested_by(self) -> RequestedBy {
        match self {
            Self::Signal(_) => RequestedBy::Unknown,
            Self::IpcRestart => RequestedBy::IpcRestart,
            Self::IpcShutdown => RequestedBy::IpcShutdown,
            Self::Drain {
                origin: DrainOrigin::PauseRoll,
                ..
            } => RequestedBy::AutoUpdate,
            Self::Drain {
                origin: DrainOrigin::Operator,
                ..
            } => RequestedBy::IpcDrain,
            Self::FleetState => RequestedBy::FleetState,
            Self::IdleExit => RequestedBy::IdleExit,
        }
    }

    /// Whether a supervisor is expected to relaunch the daemon. Only the
    /// restart verb and a relaunch drain exit supervised (both refuse to run
    /// without a supervisor); everything else is meant to stay down.
    #[must_use]
    pub fn relaunch_expected(self) -> bool {
        match self {
            Self::IpcRestart => true,
            Self::Drain { then_exit, .. } => !then_exit,
            Self::Signal(_) | Self::IpcShutdown | Self::FleetState | Self::IdleExit => false,
        }
    }
}

type InFlightLister = Box<dyn Fn() -> Vec<String> + Send + Sync>;

static IN_FLIGHT: OnceLock<InFlightLister> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Record the daemon's start for `uptime_s`. Later calls are no-ops.
pub fn mark_started() {
    let _ = STARTED.set(Instant::now());
}

/// Register the function listing non-terminal sweep ids across every managed
/// root. Later calls are no-ops.
pub fn register_in_flight_lister(lister: impl Fn() -> Vec<String> + Send + Sync + 'static) {
    let _ = IN_FLIGHT.set(Box::new(lister));
}

/// Build the record for an exit with `code` for `cause`.
#[must_use]
pub fn build_record(
    cause: ExitCause,
    code: i32,
    mut in_flight: Vec<String>,
    supervisor: Option<&str>,
    uptime_s: Option<u64>,
) -> DaemonExitRecord {
    in_flight.sort();
    DaemonExitRecord {
        signal: cause.signal().map(str::to_string),
        exit_code: code,
        requested_by: cause.requested_by(),
        relaunch_expected: cause.relaunch_expected(),
        supervisor: supervisor.unwrap_or("none").to_string(),
        in_flight_sweeps: in_flight,
        in_flight_fate: InFlightFate::for_supervisor(supervisor),
        uptime_s,
        request_id: None,
        request_verb: None,
        caused_by_request_id: None,
        observed_at: Utc::now(),
        loom: Provenance::current(),
    }
}

/// Quote the request behind the exit (#12546), when the daemon's own state
/// explains it.
#[must_use]
pub fn with_link(mut record: DaemonExitRecord, link: ExitLink) -> DaemonExitRecord {
    record.request_id = link.request_id;
    record.caused_by_request_id = link.caused_by_request_id;
    record.request_verb = link.request_verb.map(|v| v.as_str().to_string());
    record
}

fn current_record(cause: ExitCause, code: i32) -> DaemonExitRecord {
    let in_flight = IN_FLIGHT.get().map(|lister| lister()).unwrap_or_default();
    let record = build_record(
        cause,
        code,
        in_flight,
        crate::ipc::detect_supervisor().as_deref(),
        STARTED.get().map(|at| at.elapsed().as_secs()),
    );
    with_link(record, super::run_state_request::link_for(cause, Instant::now()))
}

/// Enqueue the `daemon.exit` record on `sink`.
pub fn emit_to(sink: &dyn QueueSink, host_id: &str, record: DaemonExitRecord) {
    sink.offer(TelemetryEnvelope::new(host_id.to_string(), TelemetryRecord::DaemonExit(record)));
}

/// Enqueue the `daemon.exit` record on the global ops sink (a no-op when OTLP
/// export is off). Called by [`super::shutdown::exit`] before its final drain.
pub fn emit(cause: ExitCause, code: i32) {
    super::ops::emit_record(TelemetryRecord::DaemonExit(current_record(cause, code)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(cause: ExitCause, code: i32) -> DaemonExitRecord {
        build_record(cause, code, vec!["b".into(), "a".into()], Some("systemd"), Some(5))
    }

    #[test]
    fn an_external_sigterm_is_unknown_and_stays_down() {
        let r = record(ExitCause::Signal("SIGTERM"), 143);
        assert_eq!(r.signal.as_deref(), Some("SIGTERM"));
        assert_eq!(r.requested_by, RequestedBy::Unknown);
        assert!(!r.relaunch_expected);
        assert_eq!(r.exit_code, 143);
    }

    #[test]
    fn each_cause_maps_to_its_requester_and_relaunch() {
        let operator = |then_exit| ExitCause::Drain {
            then_exit,
            origin: DrainOrigin::Operator,
        };
        let roll = |then_exit| ExitCause::Drain {
            then_exit,
            origin: DrainOrigin::PauseRoll,
        };
        let cases = [
            (ExitCause::IpcRestart, RequestedBy::IpcRestart, true),
            (ExitCause::IpcShutdown, RequestedBy::IpcShutdown, false),
            (operator(false), RequestedBy::IpcDrain, true),
            (operator(true), RequestedBy::IpcDrain, false),
            (roll(false), RequestedBy::AutoUpdate, true),
            (roll(true), RequestedBy::AutoUpdate, false),
            (ExitCause::FleetState, RequestedBy::FleetState, false),
            (ExitCause::IdleExit, RequestedBy::IdleExit, false),
        ];
        for (cause, by, relaunch) in cases {
            assert_eq!(cause.requested_by(), by, "{cause:?}");
            assert_eq!(cause.relaunch_expected(), relaunch, "{cause:?}");
            assert_eq!(cause.signal(), None, "{cause:?}");
        }
    }

    #[test]
    fn in_flight_ids_are_sorted_and_fate_follows_the_supervisor() {
        let systemd = record(ExitCause::IpcRestart, 0);
        assert_eq!(systemd.in_flight_sweeps, ["a", "b"]);
        assert_eq!(systemd.in_flight_fate, InFlightFate::DieWithCgroup);
        assert_eq!(systemd.supervisor, "systemd");
        let bare = build_record(ExitCause::IdleExit, 0, vec![], None, None);
        assert_eq!(bare.in_flight_fate, InFlightFate::Survive);
        assert_eq!(bare.supervisor, "none");
        assert!(bare.uptime_s.is_none() && bare.in_flight_sweeps.is_empty());
    }

    #[test]
    fn emit_reaches_the_ops_sink_with_no_lister_registered() {
        let (_, captured) = crate::observability::ops::capture::capture(|| {
            emit(ExitCause::FleetState, 79);
        });
        let [TelemetryRecord::DaemonExit(r)] = captured.records.as_slice() else {
            panic!("exactly one daemon.exit, got {:?}", captured.records);
        };
        assert_eq!(r.requested_by, RequestedBy::FleetState);
        assert_eq!(r.exit_code, 79);
        assert!(r.loom.is_valid());
    }
}
