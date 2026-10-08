//! Why a `no-phase-signal` sweep ended (Issue #10642).
//!
//! [`super::disposition`] synthesizes `failure_class =
//! unclassified:no-phase-signal` for a failure that carried no classifier
//! label, no phase history, and lasted too long to call a spawn death. On its
//! own that class says nothing: on `2AMLogic/2am` 278 sweeps in one day ended
//! that way, and nothing on the record said how far any of them got.
//!
//! [`NoPhaseCause`] is what the daemon *does* know at that terminal transition,
//! stated in three bounded fields:
//!
//! - `exit` — the orchestrator's exit code, or `none_observed` when the
//!   process was reaped without one (a signal death, or a sweep adopted after
//!   a daemon restart with no child handle).
//! - `last_step` — the last checkpoint phase this run reached, folded to the
//!   lifecycle vocabulary; failing that, whether the per-sweep log shows the
//!   harness started (`cli_started`) or not (`pre_cli`); `none` when the log
//!   could not be read.
//! - `reason` — one short label from a closed set, combining the two.
//!
//! Every value is drawn from a closed vocabulary (or is an integer), so the
//! fields can be grouped on without a cardinality blow-up. The struct is
//! attached only to records whose class is `unclassified:no-phase-signal`;
//! every other record is unchanged.

use serde::{Deserialize, Serialize};

/// `exit` when the reaper observed no exit status.
pub const EXIT_NONE_OBSERVED: &str = "none_observed";

/// `last_step` when nothing about the run's progress could be read.
pub const STEP_NONE: &str = "none";
/// `last_step` when the per-sweep log shows the harness started.
pub const STEP_CLI_STARTED: &str = "cli_started";
/// `last_step` when the per-sweep log shows the run died before the harness
/// started.
pub const STEP_PRE_CLI: &str = "pre_cli";

/// The lifecycle phases a `last_step` may name; anything else folds to
/// `other`, mirroring the disposition module's own fold.
const KNOWN_PHASES: [&str; 5] = ["curator", "builder", "judge", "doctor", "merge"];

/// The cause attached to a `no-phase-signal` `sweep.outcome` record (Issue
/// #10642). See the module doc for each field's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoPhaseCause {
    /// The orchestrator's exit code as a decimal string, or
    /// [`EXIT_NONE_OBSERVED`].
    pub exit: String,
    /// The last step the run was seen to reach.
    pub last_step: String,
    /// A short label from a closed set.
    pub reason: String,
}

impl NoPhaseCause {
    /// Derive the cause from what the reaper holds at the terminal transition.
    ///
    /// - `exit_code`: the observed exit status, `None` when there was none.
    /// - `cli_started`: whether this dispatch's log shows the harness started;
    ///   `None` when the log could not be read.
    /// - `last_phase`: the last checkpoint phase this run reached, if any.
    #[must_use]
    pub fn derive(
        exit_code: Option<i32>,
        cli_started: Option<bool>,
        last_phase: Option<&str>,
    ) -> Self {
        let exit = exit_code.map_or_else(|| EXIT_NONE_OBSERVED.to_string(), |c| c.to_string());
        let last_step = match (last_phase, cli_started) {
            (Some(phase), _) => {
                let phase = phase.strip_suffix("-done").unwrap_or(phase);
                KNOWN_PHASES
                    .iter()
                    .find(|known| **known == phase)
                    .copied()
                    .unwrap_or("other")
            }
            (None, Some(true)) => STEP_CLI_STARTED,
            (None, Some(false)) => STEP_PRE_CLI,
            (None, None) => STEP_NONE,
        }
        .to_string();
        let reason = match (cli_started, exit_code) {
            (Some(false), _) => "died_before_cli_start",
            (None, _) => "log_unreadable",
            (Some(true), None) => "killed_or_exit_unobserved",
            (Some(true), Some(0)) => "clean_exit_without_checkpoint",
            (Some(true), Some(_)) => "nonzero_exit_without_checkpoint",
        }
        .to_string();
        Self {
            exit,
            last_step,
            reason,
        }
    }

    /// One line for logs and forge comments, e.g. `exit=0, last_step=cli_started,
    /// reason=clean_exit_without_checkpoint`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("exit={}, last_step={}, reason={}", self.exit, self.last_step, self.reason)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_death_after_the_cli_started() {
        let cause = NoPhaseCause::derive(None, Some(true), None);
        assert_eq!(cause.exit, "none_observed");
        assert_eq!(cause.last_step, "cli_started");
        assert_eq!(cause.reason, "killed_or_exit_unobserved");
    }

    #[test]
    fn a_clean_exit_that_never_wrote_a_checkpoint() {
        let cause = NoPhaseCause::derive(Some(0), Some(true), None);
        assert_eq!(cause.exit, "0");
        assert_eq!(cause.reason, "clean_exit_without_checkpoint");
        assert_eq!(
            cause.summary(),
            "exit=0, last_step=cli_started, reason=clean_exit_without_checkpoint"
        );
    }

    #[test]
    fn a_death_before_the_cli_started() {
        let cause = NoPhaseCause::derive(Some(0), Some(false), None);
        assert_eq!(cause.last_step, "pre_cli");
        assert_eq!(cause.reason, "died_before_cli_start");
    }

    #[test]
    fn an_unreadable_log() {
        let cause = NoPhaseCause::derive(None, None, None);
        assert_eq!(cause.last_step, "none");
        assert_eq!(cause.reason, "log_unreadable");
    }

    #[test]
    fn a_checkpoint_phase_wins_and_is_folded() {
        assert_eq!(
            NoPhaseCause::derive(Some(0), Some(true), Some("curator-done")).last_step,
            "curator"
        );
        assert_eq!(
            NoPhaseCause::derive(Some(0), Some(true), Some("something-new")).last_step,
            "other",
            "an unknown marker must not widen the vocabulary"
        );
    }

    #[test]
    fn serializes_as_three_string_fields() {
        let v = serde_json::to_value(NoPhaseCause::derive(Some(137), Some(true), None)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "exit": "137",
                "last_step": "cli_started",
                "reason": "nonzero_exit_without_checkpoint"
            })
        );
    }
}
