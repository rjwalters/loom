//! The `daemon.run_state_request` record's producer (#12546): who asked the
//! daemon to pause, drain, stop or restart, and the link a later
//! `daemon.exit` follows back to that request.
//!
//! [`observe`] runs at each run-state IPC verb site in `handle_client`. It
//! resolves the peer (names only, never argv or environment), emits one record
//! and returns the request id. [`arm`] then marks the request as the one that
//! will end the daemon (the verb was accepted), and [`abort`] clears it. The
//! exit funnel reads it back through [`link_for`].
//!
//! Capture never fails a request: every lookup is best effort and a missing
//! one leaves its field absent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;

use super::daemon_exit::ExitCause;
use crate::telemetry::kinds::run_state_request::{RunStateRequestRecord, RunStateVerb};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::TelemetryRecord;

/// How long after an armed stop/drain request a signal exit is attributed to
/// it. Past this a SIGTERM is unexplained again.
pub const OPERATOR_STOP_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Longest parent chain recorded (guards a cyclic or very deep `/proc` tree).
const MAX_CHAIN: usize = 8;

/// The IPC peer, as far as the socket exposes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Peer {
    pub pid: Option<i32>,
    pub uid: Option<u32>,
}

impl Peer {
    /// From tokio's peer credentials (`None` when the platform has none).
    #[must_use]
    pub fn from_cred(cred: Option<tokio::net::unix::UCred>) -> Self {
        cred.map_or_else(Self::default, |c| Self {
            pid: c.pid(),
            uid: Some(c.uid()),
        })
    }
}

/// What a `daemon.exit` quotes about the request behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitLink {
    /// The request that directly caused the exit.
    pub request_id: Option<String>,
    /// A request that explains an otherwise unattributed signal exit.
    pub caused_by_request_id: Option<String>,
    /// The verb of whichever request is quoted.
    pub request_verb: Option<RunStateVerb>,
}

struct Armed {
    id: String,
    verb: RunStateVerb,
    at: Instant,
}

static ARMED: Mutex<Option<Armed>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(0);

fn armed() -> std::sync::MutexGuard<'static, Option<Armed>> {
    ARMED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The process name of `pid` (`/proc/<pid>/comm`); absent off Linux.
fn comm(pid: i32) -> Option<String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name = raw.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The parent pid out of a `/proc/<pid>/stat` line. The name field may hold
/// spaces and parens, so fields are counted from the last `)`.
fn parse_ppid(stat: &str) -> Option<i32> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn ppid(pid: i32) -> Option<i32> {
    parse_ppid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Names of the processes above `pid`, nearest first.
fn chain(pid: i32) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = vec![pid];
    let mut cur = pid;
    while names.len() < MAX_CHAIN {
        let Some(parent) = ppid(cur).filter(|p| *p > 0 && !seen.contains(p)) else {
            break;
        };
        let Some(name) = comm(parent) else { break };
        names.push(name);
        seen.push(parent);
        cur = parent;
    }
    names
}

/// Whether `pid` is the daemon itself, its parent, `init`, or a supervisor:
/// the auto-update / supervisor path rather than an operator.
fn is_internal(pid: i32, name: Option<&str>) -> bool {
    let own = i32::try_from(std::process::id()).unwrap_or(-1);
    pid == own || pid == 1 || ppid(own) == Some(pid) || matches!(name, Some("systemd" | "launchd"))
}

fn next_id() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("rsr-{}-{}-{seq}", Utc::now().timestamp_millis(), std::process::id())
}

/// Build the record for a request received now from `peer`.
#[must_use]
pub fn build_record(
    verb: RunStateVerb,
    flags: Vec<String>,
    peer: Peer,
    host_id: &str,
) -> RunStateRequestRecord {
    let caller_comm = peer.pid.and_then(comm);
    RunStateRequestRecord {
        request_id: next_id(),
        verb,
        flags,
        caller_pid: peer.pid,
        caller_uid: peer.uid,
        caller_chain: peer.pid.map(chain).unwrap_or_default(),
        caller_internal: peer
            .pid
            .is_some_and(|p| is_internal(p, caller_comm.as_deref())),
        caller_comm,
        host_id: host_id.to_string(),
        observed_at: Utc::now(),
        loom: Provenance::current(),
    }
}

/// Emit the record for a request received now and return its id.
pub fn observe(verb: RunStateVerb, flags: Vec<String>, peer: Peer) -> String {
    let record = build_record(verb, flags, peer, &crate::host_identity::host_identity());
    let id = record.request_id.clone();
    super::ops::emit_record(TelemetryRecord::RunStateRequest(record));
    id
}

/// Mark request `id` as the one that is ending the daemon (the verb was
/// accepted). Only stop-capable verbs arm.
pub fn arm(id: &str, verb: RunStateVerb) {
    if verb.can_stop() {
        *armed() = Some(Armed {
            id: id.to_string(),
            verb,
            at: Instant::now(),
        });
    }
}

/// A drain was aborted: no pending request will end the daemon.
pub fn abort() {
    *armed() = None;
}

/// The request behind an exit for `cause` at `now`, if the daemon's own
/// state explains it.
#[must_use]
pub fn link_for(cause: ExitCause, now: Instant) -> ExitLink {
    let guard = armed();
    let Some(request) = guard.as_ref() else {
        return ExitLink::default();
    };
    let verb = Some(request.verb);
    match cause {
        ExitCause::Signal(_)
            if now.saturating_duration_since(request.at) <= OPERATOR_STOP_WINDOW =>
        {
            ExitLink {
                request_id: None,
                caused_by_request_id: Some(request.id.clone()),
                request_verb: verb,
            }
        }
        ExitCause::IpcRestart | ExitCause::IpcShutdown | ExitCause::Drain { .. } => ExitLink {
            request_id: Some(request.id.clone()),
            caused_by_request_id: None,
            request_verb: verb,
        },
        _ => ExitLink::default(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ipc::drain_state::DrainOrigin;

    #[test]
    fn ppid_is_counted_from_the_last_paren() {
        assert_eq!(parse_ppid("12 (a b) c) S 77 12 12"), Some(77));
        assert_eq!(parse_ppid("garbage"), None);
    }

    #[test]
    fn an_unresolvable_peer_leaves_the_fields_absent() {
        let r = build_record(RunStateVerb::Drain, vec![], Peer::default(), "h");
        assert!(r.caller_pid.is_none() && r.caller_comm.is_none() && r.caller_chain.is_empty());
        assert!(!r.caller_internal);
        // A pid that cannot exist resolves to nothing and still builds.
        let gone = Peer {
            pid: Some(i32::MAX),
            uid: None,
        };
        assert!(build_record(RunStateVerb::Restart, vec![], gone, "h")
            .caller_comm
            .is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_callers_name_is_resolved_without_argv() {
        let me = i32::try_from(std::process::id()).unwrap();
        let peer = Peer {
            pid: Some(me),
            uid: Some(0),
        };
        let r = build_record(RunStateVerb::Drain, vec![], peer, "h");
        assert_eq!(r.caller_comm, comm(me));
        assert!(r.caller_comm.is_some());
        assert!(r.caller_internal, "the daemon's own pid is internal");
    }

    #[test]
    #[serial_test::serial]
    fn an_accepted_stop_is_linked_by_the_exit_and_an_abort_unlinks_it() {
        let drain = ExitCause::Drain {
            then_exit: true,
            origin: DrainOrigin::Operator,
        };
        let now = Instant::now();
        abort();
        assert_eq!(link_for(drain, now), ExitLink::default());

        arm("rsr-1", RunStateVerb::Drain);
        let link = link_for(drain, now);
        assert_eq!(link.request_id.as_deref(), Some("rsr-1"));
        assert_eq!(link.request_verb, Some(RunStateVerb::Drain));
        assert!(link.caused_by_request_id.is_none());

        // A SIGTERM inside the window is explained; outside it is not.
        let sig = link_for(ExitCause::Signal("SIGTERM"), now);
        assert_eq!(sig.caused_by_request_id.as_deref(), Some("rsr-1"));
        assert!(sig.request_id.is_none());
        let late = now + OPERATOR_STOP_WINDOW + Duration::from_secs(1);
        assert_eq!(link_for(ExitCause::Signal("SIGTERM"), late), ExitLink::default());

        // Causes that are not a request never borrow one.
        assert_eq!(link_for(ExitCause::IdleExit, now), ExitLink::default());

        abort();
        assert_eq!(link_for(drain, now), ExitLink::default());
    }

    /// An IPC drain from a (socket-pair) client yields a record naming the
    /// client's pid and process name, and the exit that follows quotes its id.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[serial_test::serial]
    async fn a_drain_records_the_client_and_the_exit_references_it() {
        let (daemon_end, _client_end) = tokio::net::UnixStream::pair().unwrap();
        let peer = Peer::from_cred(daemon_end.peer_cred().ok());
        let me = i32::try_from(std::process::id()).unwrap();
        assert_eq!(peer.pid, Some(me));

        abort();
        let (id, captured) = crate::observability::ops::capture::capture(|| {
            let id = observe(RunStateVerb::Drain, vec!["then_exit=true".into()], peer);
            arm(&id, RunStateVerb::Drain);
            super::super::daemon_exit::emit(
                ExitCause::Drain {
                    then_exit: true,
                    origin: DrainOrigin::Operator,
                },
                143,
            );
            id
        });
        abort();

        let [TelemetryRecord::RunStateRequest(req), TelemetryRecord::DaemonExit(exit)] =
            captured.records.as_slice()
        else {
            panic!("a run_state_request then a daemon.exit, got {:?}", captured.records);
        };
        assert_eq!(req.request_id, id);
        assert_eq!(req.verb, RunStateVerb::Drain);
        assert_eq!(req.caller_pid, Some(me));
        assert_eq!(req.caller_comm, comm(me));
        assert!(req.caller_comm.is_some());
        assert_eq!(exit.request_id.as_deref(), Some(id.as_str()));
        assert_eq!(exit.request_verb.as_deref(), Some("drain"));
        assert!(exit.caused_by_request_id.is_none());
    }

    /// The systemd SIGTERM that follows an operator stop names the request
    /// but stays `unknown`; with no pending request it is unexplained.
    #[test]
    #[serial_test::serial]
    fn a_sigterm_after_a_stop_names_the_request() {
        abort();
        let (_, captured) = crate::observability::ops::capture::capture(|| {
            let id = observe(RunStateVerb::Shutdown, vec![], Peer::default());
            arm(&id, RunStateVerb::Shutdown);
            super::super::daemon_exit::emit(ExitCause::Signal("SIGTERM"), 143);
            abort();
            super::super::daemon_exit::emit(ExitCause::Signal("SIGTERM"), 143);
        });
        let [_, TelemetryRecord::DaemonExit(linked), TelemetryRecord::DaemonExit(bare)] =
            captured.records.as_slice()
        else {
            panic!("unexpected records {:?}", captured.records);
        };
        assert!(linked.caused_by_request_id.is_some() && linked.request_id.is_none());
        assert_eq!(linked.request_verb.as_deref(), Some("shutdown"));
        assert!(bare.caused_by_request_id.is_none() && bare.request_verb.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn abort_drain_never_arms() {
        abort();
        arm("rsr-2", RunStateVerb::AbortDrain);
        assert_eq!(link_for(ExitCause::Signal("SIGTERM"), Instant::now()), ExitLink::default());
    }
}
