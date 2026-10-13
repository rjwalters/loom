//! Turn an exit-0 role tick whose every tool call a guard hook denied into a
//! [`RoleTickOutcome::Failure`], so the preference walk falls through to the
//! next runtime (issue #11727, slice 1).
//!
//! `guard-loom-workflow.sh` fails CLOSED for the built-in read-only roles when
//! `loom-daemon role-tool-policy check` cannot answer (exit 2 from a daemon
//! that predates `check`, e.g. a stale session container), denying the command
//! with `BLOCKED [role-tool-policy]: … could not answer (exit N) …`. The agent
//! reports that in prose and exits 0, so the tick used to be a `Success` and
//! the `rolePreference` fallback never fired (30 h outage, 2026-10-12).
//!
//! Same shape as `sandbox_noop`: read only **this tick's own** region of the
//! role log (everything after `tick_anchor`) and match the runtime's exec
//! framing, never agent prose. A tick counts only when it shows at least one
//! exec result, **none** succeeded, and **every** `exited N` result's first
//! output lines carry the hook's denial framing. One denied command beside
//! commands that ran is the policy working as designed and does not count.
//!
//! The failure class [`CLASS`] is deterministic
//! ([`super::failure_class::is_deterministic`]): it feeds the per-workspace
//! launch-failure hold, which skips the tap for the next ticks. It has no host
//! wide hold of its own, so it is not double-armed.

use super::super::failure_sentinel::tick_region;
use super::super::read_role_log;
use crate::codex_sandbox_noop::is_exec_result;
use std::path::Path;

/// Leading text of the failure detail, greppable in the daemon log.
pub(crate) const REASON_PREFIX: &str = "guard hook denied every tool call";

/// The stable `loom.failure_class`.
pub(crate) const CLASS: &str = "hook-denied";

/// The hook's denial framing (`guard-loom-workflow.sh`, #8256).
const DENIAL: &str = "BLOCKED [role-tool-policy]";
const COULD_NOT_ANSWER: &str = "could not answer (exit ";

/// How many lines after an `exited N` result may carry the denial text.
const OUTPUT_WINDOW: usize = 3;

fn is_denial(line: &str) -> bool {
    line.contains(DENIAL) && line.contains(COULD_NOT_ANSWER)
}

/// Pure core: the failure detail when `region` shows every tool call denied.
pub(super) fn verdict_in(region: &str) -> Option<String> {
    let (mut succeeded, mut failed, mut denied) = (0usize, 0usize, 0usize);
    // Lines left in the window of the latest failed result, and whether it
    // has been counted as denied already.
    let (mut window, mut counted) = (0usize, false);
    let mut exit_code = String::new();
    for line in region.lines() {
        if is_exec_result(line, "succeeded") {
            succeeded += 1;
            window = 0;
        } else if is_exec_result(line, "exited") {
            failed += 1;
            window = OUTPUT_WINDOW;
            counted = false;
        } else if window > 0 {
            window -= 1;
            if !counted && is_denial(line) {
                counted = true;
                denied += 1;
                if exit_code.is_empty() {
                    exit_code = line
                        .split_once(COULD_NOT_ANSWER)
                        .map(|(_, rest)| rest.chars().take_while(char::is_ascii_digit).collect())
                        .unwrap_or_default();
                }
            }
        }
    }
    if failed == 0 || succeeded > 0 || denied != failed {
        return None;
    }
    Some(format!(
        "{REASON_PREFIX}: {denied} of {denied} tool calls were refused by the role-tool-policy \
         hook because `loom-daemon role-tool-policy check` could not answer (exit {exit_code}), \
         so nothing ran — reported as a failed tick, not a success; the runtime is passed over \
         by the launch-failure hold (#11727)"
    ))
}

/// Filesystem wrapper for a just-exited-0 tick. An unreadable log is no
/// opinion (`None`), never a failure.
pub(super) fn detect(log_path: &Path, tick_anchor: &str) -> Option<String> {
    if tick_anchor.is_empty() {
        return None;
    }
    let full_log = read_role_log(log_path);
    if !full_log.contains(tick_anchor) {
        return None;
    }
    verdict_in(tick_region(&full_log, tick_anchor))
}

#[cfg(test)]
mod tests;
