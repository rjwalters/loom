//! A Codex tick refused because its account's session container was not
//! running (#10455).
//!
//! `loom-daemon session-exec host` refuses such a dispatch with exit 78 and
//! announces `SESSION_DOWN` (`session_exec::refusal`); the terminal-record
//! parser applies that to the adapter script's generic record. Without a
//! distinct outcome that failure reads as a bare `RECOVERABLE`/78 config
//! error, indistinguishable from a real fault in the (repo, role). This module turns the tick's own terminal
//! record into a failure reason with a stable [`REASON_PREFIX`], which
//! `observability::lifecycle::admission_attributes` projects to a distinct
//! `loom.admission.reason = "session-down"` on the `loom.role_attempt` span so
//! the role-failure watch can group by cause. The account records no hold:
//! see `TerminalClassification::SessionDown`.

use crate::tokens_pool::health::TerminalClassification;

use crate::role_tick_telemetry::SESSION_DOWN_REASON_PREFIX as REASON_PREFIX;

/// The failure reason for the tick whose header carries `tick_anchor`, when
/// its own terminal record says the session container was down.
pub(super) fn reason_in(contents: &str, tick_anchor: &str) -> Option<String> {
    if tick_anchor.is_empty() {
        return None;
    }
    let result = crate::sweep_registry::parse_terminal_result_after(contents, tick_anchor)?;
    (result.category == TerminalClassification::SessionDown).then(|| {
        format!(
            "{REASON_PREFIX}: the Codex session container for account {} was not running \
             (#10455)",
            result.account
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::role_tick_telemetry::is_session_down_reason as is_session_down;

    const ANCHOR: &str = "=== tick 1 ===";

    #[test]
    fn a_session_down_record_gets_the_distinct_reason() {
        let log = format!(
            "{ANCHOR}\n# LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 \
             category=SESSION_DOWN exit_code=78 model=none\n"
        );
        let reason = reason_in(&log, ANCHOR).unwrap();
        assert!(is_session_down(&reason));
        assert!(reason.contains("agent-1"));
    }

    #[test]
    fn a_refusal_announced_by_session_exec_relabels_the_scripts_generic_record() {
        // What the tick log holds since the mapping moved out of the shell:
        // `session-exec host` announces the cause, the script reports 78.
        let log = format!(
            "{ANCHOR}\n# LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN\nsession-exec: Session \
             container 'loom-codex-session-agent-1' is not running.\n# LOOM_TERMINAL_RESULT v=2 \
             provider=codex account=agent-1 category=RECOVERABLE exit_code=78 model=none\n"
        );
        let reason = reason_in(&log, ANCHOR).unwrap();
        assert!(is_session_down(&reason) && reason.contains("agent-1"));
    }

    #[test]
    fn a_recoverable_record_does_not() {
        let log = format!(
            "{ANCHOR}\n# LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 \
             category=RECOVERABLE exit_code=78 model=none\n"
        );
        assert!(reason_in(&log, ANCHOR).is_none());
        assert!(reason_in("", ANCHOR).is_none());
    }
}
