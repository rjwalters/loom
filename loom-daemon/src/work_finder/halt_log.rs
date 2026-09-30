//! Cause-attributed wording for the multi-workspace "dispatch halted"
//! transition log line (Issue #9591).
//!
//! The tick's `halted` slice ORs several independent holds together: a
//! verified-red `main`, an in-flight build gate, the pre-flight advisory /
//! token-pool hold, a daemon-global auto-update drain, and the host breaker.
//! The transition line used to read "main-health gate halted dispatch for N
//! of N repo(s)" whichever hold tripped. As a result, every drain-and-restart
//! roll told the operator that all mains were red. This module tallies the
//! causes and words the line after the ones that are actually active. When
//! it can attribute nothing, it says "halted" neutrally.
//!
//! It lives in its own file for the same file-size-ratchet reason as
//! `tick_report.rs`: `work_finder.rs` may not grow. It reads only the inputs
//! that already exist at the log site, so it composes with #9017's typed
//! `HaltCause` rather than competing with it.

use std::path::PathBuf;

use crate::main_health_gate::WorkspaceHealthStates;

/// Per-cause counts behind one tick's `halted` slice. The per-root counts are
/// not exclusive (a red root may also be pool-held); `drain` / `breaker` are
/// daemon-global.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct HaltTally {
    /// Roots held for any reason.
    pub held: usize,
    /// All roots this tick.
    pub total: usize,
    /// Roots whose `main` is verified-red (the main-health gate proper).
    pub main_red: usize,
    /// Roots held because a build-gate run is in flight.
    pub gate_pending: usize,
    /// Roots held by the pre-flight advisory or token-pool hold.
    pub preflight: usize,
    /// An auto-update drain is armed.
    pub drain: bool,
    /// The host-distress breaker is suppressing dispatch.
    pub breaker: bool,
}

/// Tally the per-cause counts from the same inputs the tick's `halted` fold
/// reads. `flags` is `(suppress_dispatch_during_gate, draining,
/// breaker_suppressed)`, bundled so that the call site in the size-ratcheted
/// `work_finder.rs` stays short.
#[must_use]
pub fn tally(
    halted: &[bool],
    health_states: &WorkspaceHealthStates,
    roots: &[PathBuf],
    preflight_held: &[bool],
    flags: (bool, bool, bool),
) -> HaltTally {
    let (suppress_dispatch_during_gate, drain, breaker) = flags;
    HaltTally {
        held: halted.iter().filter(|&&h| h).count(),
        total: halted.len(),
        main_red: roots.iter().filter(|r| health_states.is_halted(r)).count(),
        gate_pending: roots
            .iter()
            .filter(|r| suppress_dispatch_during_gate && health_states.is_gate_in_flight(r))
            .count(),
        preflight: preflight_held.iter().filter(|&&h| h).count(),
        drain,
        breaker,
    }
}

impl HaltTally {
    /// The WARN line for the transition into a halt. It only mentions the
    /// main-health gate when some root's `main` is actually red.
    #[must_use]
    pub fn warn_line(&self) -> String {
        let mut causes = Vec::new();
        if self.main_red > 0 {
            causes.push(format!("main-health gate: {} repo(s) with a red main", self.main_red));
        }
        if self.gate_pending > 0 {
            causes.push(format!("build gate in flight: {} repo(s)", self.gate_pending));
        }
        if self.preflight > 0 {
            causes.push(format!("pre-flight advisory / token pool: {} repo(s)", self.preflight));
        }
        if self.drain {
            causes.push("auto-update drain".to_string());
        }
        if self.breaker {
            causes.push("host breaker".to_string());
        }
        let causes = if causes.is_empty() {
            "cause unknown".to_string()
        } else {
            causes.join("; ")
        };
        format!(
            "work_finder: dispatch halted for {} of {} repo(s) ({causes}) — their ready \
             issues are held until the hold clears",
            self.held, self.total
        )
    }
}

#[cfg(test)]
#[path = "halt_log_tests.rs"]
mod tests;
