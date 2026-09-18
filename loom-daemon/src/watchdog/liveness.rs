//! Liveness and heartbeat freshness — delegated to [`crate::daemon_install_state`].
//!
//! # Why there is almost nothing here
//!
//! The shell watchdog and `daemon_install_state` were two implementations of
//! one classification, kept in agreement by comment. That is not a
//! characterisation: `daemon_install_state`'s own docs say its states mirror
//! "the precedence this mirrors from `loom-daemon-watchdog.sh`", its
//! `liveness_detail` field says it "mirrors the watchdog's own `liveness_detail`
//! strings", and its `EnvOverrides` says it mirrors "`loom-daemon-watchdog.sh`'s
//! 'env wins over marker' rule exactly". The detail strings really are
//! byte-identical on both sides — `launchd job {service} alive (pid {pid})`,
//! `pid file {} present but pid not alive`, `no live pid file at <none>`.
//!
//! Porting the shell by translating it would have produced a THIRD copy. This
//! module instead calls the existing one, so the agreement stops depending on
//! two authors noticing each other's comments.
//!
//! # The divergence that unification resolves (#8086)
//!
//! The two copies had already drifted, in a spot no test on either side
//! covered. The shell tested `[[ "$LOOM_DAEMON_LAUNCHD" =~ ^(0|false|no)$ ]]`,
//! which bash evaluates **case-sensitively** (the script never sets
//! `nocasematch`). `daemon_install_state::parse_launchd_override` lowercases
//! first, so it is case-INsensitive. With `LOOM_DAEMON_LAUNCHD=FALSE` the
//! watchdog therefore probed **launchd** while `loom-daemon status` probed the
//! **pid file** — the two ends disagreeing about which signal is authoritative,
//! which is the exact failure `resolve_pid_file`'s comment describes for paths.
//!
//! Every test on both sides uses `0` or `1`, so nothing pinned it. Unifying
//! adopts the case-INsensitive reading: it is the shipped behaviour of
//! `loom-daemon status`, and it is likelier to match what an operator who typed
//! `FALSE` meant. Recorded here because a divergence nobody wrote down is a bug
//! waiting to be re-litigated (#8011).

use std::path::Path;

use crate::daemon_install_state::{self, HeartbeatFreshness, InstallState};

/// One tick's view of intent-vs-reality.
pub struct Snapshot {
    pub state: InstallState,
    pub started_at: Option<String>,
    pub pid: Option<u32>,
    pub detail: String,
    pub heartbeat: Option<HeartbeatFreshness>,
    pub heartbeat_age_secs: Option<u64>,
    pub heartbeat_stale_threshold_secs: Option<u64>,
    pub heartbeat_file: Option<String>,
    pub process_age_secs: Option<u64>,
    /// The supervisor knows the job but it has no live pid — "LOADED but NOT
    /// running". The only state the bounded auto-remediation gate may act on.
    pub job_loaded: bool,
    /// The supervisor's name for the job (`gui/501/com.x`, or a systemd unit),
    /// when one applies. `None` on the pid-file path, where there is no
    /// supervisor to ask.
    pub supervisor_service: Option<String>,
    /// The supervisor's report of how the job last exited. `None` when it could
    /// not be read — treated as unclean by [`super::remediation::gate`], never
    /// as clean.
    pub last_exit_status: Option<i64>,
    /// Which signal answered the liveness question. `#5118` turns on this:
    /// the pid file is the WEAKEST source, and a negative answer from it alone
    /// must never declare an outage.
    pub source: Source,
    /// What the pid file said, when it was the source.
    pub pidfile_evidence: PidfileEvidence,
}

/// Which out-of-band signal answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Launchd,
    Systemd,
    /// The weakest signal, and the only one #5118 refuses to draw an outage
    /// from on its own.
    PidFile,
}

/// What the pid file showed. Distinct from "not alive", because **absent** and
/// **dead** call for different conclusions: a dead pid is evidence the process
/// went away, while an absent file is no evidence at all — and #5118 is the
/// incident where the second was mistaken for the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfileEvidence {
    Alive,
    Dead,
    Absent,
}

impl Snapshot {
    /// `true` when a daemon process is running, whatever its IPC state.
    ///
    /// `classify` distinguishes `AliveStarting` from `AliveButUnresponsive`
    /// because it is documented as answering "once a live IPC round-trip has
    /// failed". The watchdog asks a coarser question first — is there a process
    /// at all — and runs its own bounded probe afterwards, so both count.
    #[must_use]
    pub fn alive(&self) -> bool {
        matches!(self.state, InstallState::AliveStarting | InstallState::AliveButUnresponsive)
    }

    #[must_use]
    pub fn marker_present(&self) -> bool {
        !matches!(self.state, InstallState::NotExpected)
    }
}

/// Classify this tick.
///
/// # Why this calls the primitives rather than [`daemon_install_state::classify`]
///
/// `classify` is documented as answering "the states `status` can distinguish
/// **once a live IPC round-trip has failed**", and it bakes in that framing:
/// heartbeat freshness is computed only for `AliveButUnresponsive`, because a
/// process inside the startup-grace window is `AliveStarting` and `status` has
/// nothing to say about its heartbeat yet.
///
/// The watchdog asks a different question in a different order. It wants
/// heartbeat freshness for **any** live daemon, independently of process age,
/// and it runs its own bounded probe afterwards rather than before. Calling
/// `classify` here looked right and silently produced "no heartbeat file" for a
/// live daemon with a stale heartbeat — a section-3 DIVERGENCE reported as a
/// section-5 OK. An end-to-end run caught it; no unit test would have, because
/// each half was individually correct.
///
/// So this composes the same primitives `classify` does — `check_liveness`,
/// `check_heartbeat`, `process_age_secs`, `resolve_stale_threshold` — in the
/// watchdog's own order. Still one implementation of each; a different
/// sequence over them.
#[must_use]
pub fn probe(
    loom_dir: &Path,
    marker: &Path,
    supervisor: &super::supervisor::Supervisor,
) -> Snapshot {
    if !marker.exists() {
        return Snapshot {
            state: InstallState::NotExpected,
            started_at: None,
            pid: None,
            detail: String::new(),
            heartbeat: None,
            heartbeat_age_secs: None,
            heartbeat_stale_threshold_secs: None,
            heartbeat_file: None,
            process_age_secs: None,
            job_loaded: false,
            supervisor_service: None,
            last_exit_status: None,
            source: Source::PidFile,
            pidfile_evidence: PidfileEvidence::Absent,
        };
    }

    let m = |k: &str| super::marker::get_nonempty(marker, k);
    let pid_file = super::marker::resolve_pid_file(
        super::env::var("LOOM_PID_FILE"),
        m("pid_file"),
        super::env::var("LOOM_MACHINE_CHECKOUT"),
        super::env::var("LOOM_WORKSPACE"),
        m("repo_root"),
        Some(loom_dir.to_path_buf()),
    );

    let liveness = daemon_install_state::check_liveness_with_systemd(
        supervisor.use_launchd,
        &supervisor.label,
        pid_file.as_deref(),
        super::env::var("LOOM_LAUNCHD_DOMAIN").as_deref(),
        supervisor
            .use_systemd
            .then_some(supervisor.systemd_unit.as_str()),
    );

    let process_age = liveness
        .pid
        .and_then(daemon_install_state::process_age_secs);

    // The supervisor's own name for the job, used both in the report and for
    // the last-exit lookup. Resolved ONCE: #4536 is this repo's receipt for
    // deriving a launchd domain twice and having the two disagree.
    let service = if supervisor.use_launchd {
        Some(format!(
            "{}/{}",
            daemon_install_state::launchd_domain(super::env::var("LOOM_LAUNCHD_DOMAIN").as_deref()),
            supervisor.label
        ))
    } else if supervisor.use_systemd {
        Some(supervisor.systemd_unit.clone())
    } else {
        None
    };

    let heartbeat_file = m("heartbeat_file")
        .map_or_else(|| loom_dir.join("daemon.heartbeat"), std::path::PathBuf::from);
    let interval = m("heartbeat_interval_secs")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    let threshold = daemon_install_state::resolve_stale_threshold(
        interval,
        super::env::var("LOOM_DAEMON_HEARTBEAT_STALE_SECS").and_then(|v| v.parse().ok()),
    );

    // Heartbeat freshness is only meaningful about a process that exists.
    let (freshness, age) = if liveness.alive {
        let (f, a) = daemon_install_state::check_heartbeat(&heartbeat_file, threshold, process_age);
        (Some(f), a)
    } else {
        (None, None)
    };

    Snapshot {
        state: if liveness.alive {
            InstallState::AliveButUnresponsive
        } else {
            InstallState::ExpectedButDead
        },
        started_at: m("started_at"),
        pid: liveness.pid,
        detail: liveness.detail,
        heartbeat: freshness,
        heartbeat_age_secs: age,
        heartbeat_stale_threshold_secs: Some(threshold),
        heartbeat_file: Some(heartbeat_file.display().to_string()),
        process_age_secs: process_age,
        job_loaded: liveness.job_loaded,
        source: if supervisor.use_launchd {
            Source::Launchd
        } else if supervisor.use_systemd {
            Source::Systemd
        } else {
            Source::PidFile
        },
        pidfile_evidence: match pid_file.as_deref() {
            Some(p) => {
                let o = crate::daemon_pidfile::observe(p);
                if o.recorded_pid_alive {
                    PidfileEvidence::Alive
                } else if o.present {
                    PidfileEvidence::Dead
                } else {
                    PidfileEvidence::Absent
                }
            }
            None => PidfileEvidence::Absent,
        },
        supervisor_service: service.clone(),
        // Only asked when the job is loaded and there is a service to ask
        // about. A supervisor that has no job has no last exit to report, and
        // inventing one would hand `gate` the value that licenses a restart.
        last_exit_status: if liveness.job_loaded {
            service.as_deref().and_then(supervisor_last_exit)
        } else {
            None
        },
    }
}

/// Ask the supervisor how the job last exited.
///
/// launchd only for now: the systemd equivalent reads `ExecMainCode` and
/// `ExecMainStatus`, which is a different shape and lands with that branch.
fn supervisor_last_exit(service: &str) -> Option<i64> {
    // A systemd unit name has no domain prefix; launchd services always do.
    if !service.contains('/') {
        return None;
    }
    let mut cmd = std::process::Command::new("launchctl");
    cmd.args(["print", service]);
    let out = crate::sweep_registry::output_with_timeout(cmd, std::time::Duration::from_secs(5))
        .ok()
        .flatten()?;
    super::remediation::parse_launchd_last_exit(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(state: InstallState) -> Snapshot {
        Snapshot {
            state,
            started_at: None,
            pid: None,
            detail: String::new(),
            heartbeat: None,
            heartbeat_age_secs: None,
            heartbeat_stale_threshold_secs: None,
            heartbeat_file: None,
            process_age_secs: None,
            job_loaded: false,
            supervisor_service: None,
            last_exit_status: None,
            source: Source::PidFile,
            pidfile_evidence: PidfileEvidence::Absent,
        }
    }

    #[test]
    fn both_alive_states_count_as_alive_for_the_watchdogs_coarser_question() {
        assert!(snap(InstallState::AliveStarting).alive());
        assert!(snap(InstallState::AliveButUnresponsive).alive());
        assert!(!snap(InstallState::ExpectedButDead).alive());
        assert!(!snap(InstallState::NotExpected).alive());
    }

    #[test]
    fn only_not_expected_means_the_marker_is_absent() {
        assert!(!snap(InstallState::NotExpected).marker_present());
        assert!(snap(InstallState::ExpectedButDead).marker_present());
        assert!(snap(InstallState::AliveStarting).marker_present());
        assert!(snap(InstallState::AliveButUnresponsive).marker_present());
    }
}
