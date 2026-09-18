//! The `indeterminate-busy` roll-up verdict and the evidence it requires
//! (#6191, corroboration tightened by #8163).
//!
//! Split into its own file because `health.rs` sits at its
//! `.loom/docs/file-size-policy.md` ratchet: new assessment logic goes in a
//! sibling module and the parent keeps only the dispatch line.
//!
//! # Why the load reading was added (#8163)
//!
//! `Verdict::IndeterminateBusy` means "the probe budget ran out against a
//! daemon we can independently see is alive — try again shortly", and it is
//! reported at its own exit code (`3`) so a watch loop can distinguish it
//! from an alert. Until #8163 the only evidence required was *that the
//! round-trip timed out*, which made "busy" a euphemism for "slow": on a host
//! with several dozen registered workspaces, `build_daemon_status` (an
//! `O(roots)` walk) routinely exceeded the client's fixed `10s` escalated
//! budget while the host sat idle, so `health` reported a host-load story
//! that was simply not true on every call.
//!
//! #8163 fixes the *cause* client-side — the escalated retry is now sized
//! from the registered root count ([`crate::status_budget`]) — and fixes the
//! *claim* here: a timeout is only reported as "busy" when a host-load
//! reading corroborates it. A timeout that survives a root-scaled budget on
//! an idle host is not "busy"; it is unexplained, and the honest verdict for
//! unexplained is `Unknown`.
//!
//! **Absent evidence never manufactures a verdict.** An unreadable load
//! average (an unsupported platform, a transient read failure, or a fixture
//! that never set it) cannot *refute* busy-ness either, so it leaves the
//! pre-#8163 behaviour exactly in place — the same fail-open convention
//! [`crate::cpu_headroom`] uses everywhere else.

use super::{alive_with_fresh_heartbeat, ipc_error_is_probe_timeout, HealthInputs};

/// Whether an observed host load corroborates the "the daemon was too busy to
/// answer" story (Issue #8163 AC2).
///
/// Uses [`crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD`] — this repo's
/// existing definition of "the host is full enough to change behaviour",
/// shared with the main-health gate — rather than inventing a second
/// saturation threshold that could drift from it.
///
/// `None` (or a non-finite reading) returns `true`: see the module doc — no
/// reading means no refutation, not a refutation.
#[must_use]
pub(super) fn load_corroborates_busy(load_per_core: Option<f64>) -> bool {
    match load_per_core {
        None => true,
        Some(lpc) if !lpc.is_finite() => true,
        Some(lpc) => lpc >= crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD,
    }
}

/// Whether this report's non-green state is entirely attributable to the
/// collector's own IPC probe budget having been exhausted against a daemon
/// local, no-IPC evidence already corroborates as running *and* that the host
/// was actually loaded enough to explain it — the roll-up counterpart of
/// [`alive_with_fresh_heartbeat`]. All of the following must hold:
///
/// - [`HealthInputs::status`] is `None` (the IPC round-trip never produced a
///   report), and the recorded [`HealthInputs::ipc_error`] classifies as a
///   *timeout* ([`ipc_error_is_probe_timeout`]) rather than a harder failure.
/// - [`alive_with_fresh_heartbeat`] corroborates the process as alive with a
///   fresh heartbeat.
/// - [`load_corroborates_busy`] over [`HealthInputs::load_per_core`] does not
///   *refute* the busy story (#8163).
///
/// Deliberately narrow: this says nothing about *why* any individual section
/// is non-green, only whether the specific "busy" story is consistent with
/// the evidence. [`super::assess`] additionally requires no section to be
/// `Verdict::Degraded` before consulting this at all — a genuine degradation
/// (a stale pid file, a hard IPC failure, a real dispatch fault) always takes
/// precedence, so this can never mask one.
#[must_use]
pub(super) fn probe_budget_busy(inputs: &HealthInputs) -> bool {
    inputs.status.is_none()
        && inputs
            .ipc_error
            .as_deref()
            .is_some_and(ipc_error_is_probe_timeout)
        && alive_with_fresh_heartbeat(inputs.install_state.as_ref())
        && load_corroborates_busy(inputs.load_per_core)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD;
    use crate::daemon_install_state::{HeartbeatFreshness, InstallState};
    use crate::health::tests::{healthy_inputs, install_report};
    use crate::health::{assess, Verdict, EXIT_DEGRADED, EXIT_INDETERMINATE_BUSY};

    /// The #6191 fixture: unreachable daemon, a lone timeout, a fresh
    /// heartbeat — and (pre-#8163 default) no load reading at all.
    fn timed_out_inputs() -> crate::health::HealthInputs {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("round-trip timed out after 2s".to_string());
        inputs.pgrep_pids = vec![];
        inputs
    }

    // ===============================================================
    // #8163 AC2 — load corroboration
    // ===============================================================

    /// The #8163 regression itself: an **idle** host whose probe timed out
    /// must not be told the daemon was busy. The verdict drops to the honest
    /// ordinary `Unknown`, because a timeout that survives a root-scaled
    /// budget on an unloaded host is unexplained, not load-bound.
    #[test]
    fn a_probe_timeout_on_an_idle_host_is_not_reported_as_busy() {
        let mut inputs = timed_out_inputs();
        inputs.load_per_core = Some(0.05);
        let report = assess(&inputs);
        assert_eq!(
            report.overall,
            Verdict::Unknown,
            "an idle host's load average refutes the 'busy' story #8163 was filed against: {}",
            report.render_human()
        );
        assert_eq!(report.exit_code(), EXIT_DEGRADED);
    }

    /// The positive case the busy verdict actually exists for: the same
    /// timeout on a genuinely saturated host still reports `IndeterminateBusy`
    /// at exit `3`, so a watch loop keeps its "try again shortly" signal.
    #[test]
    fn a_probe_timeout_on_a_loaded_host_is_still_reported_as_busy() {
        let mut inputs = timed_out_inputs();
        inputs.load_per_core = Some(DEFAULT_GATE_LOAD_THRESHOLD + 1.5);
        let report = assess(&inputs);
        assert_eq!(report.overall, Verdict::IndeterminateBusy, "{}", report.render_human());
        assert_eq!(report.exit_code(), EXIT_INDETERMINATE_BUSY);
    }

    /// Fail-open: an unreadable load average (unsupported platform,
    /// transient read failure) cannot refute busy-ness, so the pre-#8163
    /// behaviour is preserved exactly.
    #[test]
    fn an_unreadable_load_average_preserves_the_pre_8163_busy_verdict() {
        let inputs = timed_out_inputs();
        assert_eq!(inputs.load_per_core, None, "fixture default is 'no reading'");
        assert_eq!(assess(&inputs).overall, Verdict::IndeterminateBusy);
    }

    /// Pure classification pin for the threshold itself — including the
    /// non-finite reading a corrupted `/proc/loadavg` could produce.
    #[test]
    fn load_corroboration_is_pinned_to_the_shared_saturation_threshold() {
        assert!(load_corroborates_busy(None));
        assert!(load_corroborates_busy(Some(f64::NAN)));
        assert!(load_corroborates_busy(Some(f64::INFINITY)));
        assert!(load_corroborates_busy(Some(DEFAULT_GATE_LOAD_THRESHOLD)));
        assert!(load_corroborates_busy(Some(DEFAULT_GATE_LOAD_THRESHOLD + 0.01)));
        assert!(!load_corroborates_busy(Some(DEFAULT_GATE_LOAD_THRESHOLD - 0.01)));
        assert!(!load_corroborates_busy(Some(0.0)));
    }

    // ===============================================================
    // #6191 negative cases (moved here with the logic they pin)
    // ===============================================================

    /// A probe-budget timeout against a daemon whose heartbeat is STALE (not
    /// fresh) gets no benefit of the doubt. `overall` stays the ordinary
    /// `Unknown`/exit `1` — a stale heartbeat is itself grounds for
    /// suspicion, and must not silently downgrade to "just busy" (#6191).
    #[test]
    fn a_probe_timeout_with_a_stale_heartbeat_is_not_reported_as_busy() {
        let mut inputs = timed_out_inputs();
        let mut install = install_report(InstallState::AliveButUnresponsive);
        install.heartbeat_freshness = Some(HeartbeatFreshness::Stale);
        inputs.install_state = Some(install);

        let report = assess(&inputs);
        assert_eq!(report.overall, Verdict::Unknown, "{}", report.render_human());
        assert_eq!(report.exit_code(), EXIT_DEGRADED);
    }

    /// A HARD IPC failure (not a timeout) against an alive+fresh-heartbeat
    /// daemon must still resolve to `Degraded`/exit 1 — [`probe_budget_busy`]
    /// requires the timeout classification specifically, so this must never
    /// slip into the busy verdict just because the heartbeat looks fine.
    #[test]
    fn a_hard_ipc_failure_with_a_fresh_heartbeat_still_reports_degraded() {
        let mut inputs = timed_out_inputs();
        inputs.ipc_error =
            Some("connect failed: No such file or directory (os error 2)".to_string());
        // healthy_inputs()'s default install_state is already
        // AliveButUnresponsive with a fresh heartbeat.
        let report = assess(&inputs);
        assert_eq!(report.overall, Verdict::Degraded, "{}", report.render_human());
        assert_eq!(report.exit_code(), EXIT_DEGRADED);
    }
}
