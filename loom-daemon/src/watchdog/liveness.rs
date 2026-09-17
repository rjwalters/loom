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

use crate::daemon_install_state::{self, EnvOverrides, HeartbeatFreshness, InstallState};

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
#[must_use]
pub fn probe(loom_dir: &Path, marker: &Path) -> Snapshot {
    let env = EnvOverrides::from_env();
    let report = daemon_install_state::classify(loom_dir, marker, &env);
    Snapshot {
        state: report.state,
        started_at: report.started_at,
        pid: report.pid,
        detail: report.liveness_detail.unwrap_or_default(),
        heartbeat: report.heartbeat_freshness,
        heartbeat_age_secs: report.heartbeat_age_secs,
        heartbeat_stale_threshold_secs: report.heartbeat_stale_threshold_secs,
        heartbeat_file: super::marker::get_nonempty(marker, "heartbeat_file"),
        process_age_secs: report.process_age_secs,
    }
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
