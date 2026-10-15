//! A step's `loom.sweep` span: closed by the step's end, not by recovery
//! (Issue #12253).
//!
//! A step is spawned through the sweep spawn path, so `prepare_child` opens a
//! `loom.sweep` root for it under the step's sweep id. An issue sweep's root
//! is closed by its terminal outcome (`outcome_journal`'s
//! `lifecycle::finish_execution`), but a step never reaches that path: its end
//! is [`SweepRegistry::end_step`], which emitted `role_tick.outcome` and
//! nothing else. The root therefore stayed open until orphan recovery, after
//! the next daemon restart, found both the step and the old daemon gone and
//! closed it as `process_lost` / `supervisor-lost`, with a duration that ran
//! to the restart instead of the session's end.
//!
//! Two halves fix that:
//!
//! - [`sweep_close`] is the close a step's end records: the same
//!   `loom.result` its `role_tick.outcome` reports (`success` / `failure`),
//!   the step's role, and on a failure a [`StepFailure`] class from the closed
//!   vocabulary below. [`SweepRegistry::end_step`] journals it at the moment
//!   it sees the end. A step a roll stopped closes as `interrupted_by_roll`,
//!   as a role tick does.
//! - [`step_still_running`] is what orphan recovery asks before it closes a
//!   step's span: whether the step's claim lock still names this step and its
//!   leader or its process group is alive. A leader can exit while its group
//!   (a wrapper retry) keeps running the session, which the journal's owner
//!   pid alone cannot see.
//!
//! | `loom.failure_class` | Observed |
//! |---|---|
//! | `timeout-ceiling` | the reaper killed the step at its role timeout |
//! | `cancelled` | an operator or the daemon cancelled the step |
//! | `step-outcome-failed` | the session's `LOOM_STEP_OUTCOME` said `class=failed` |
//! | `exit-<code>` | the step exited non-zero |
//! | `exit-unobserved` | the step exited, its status is unknown, and it wrote no outcome line |

use std::path::Path;

use super::capture::{StepEnd, StepOutcomeClass, StepOutcomeLine};
use crate::observability::lifecycle::{self, roll_interruption, RoleFailure};
use crate::role_runner::RoleTickOutcome;
use crate::step_role::StepRole;
use crate::sweep_registry::reaper::pid_identity;
use crate::sweep_registry::{LockOwner, SweepRegistry};
use crate::telemetry::trace::TraceAttributes;

/// Why a step failed, on its `loom.sweep` close.
#[must_use]
pub(crate) fn step_failure(end: StepEnd, line: Option<&StepOutcomeLine>) -> RoleFailure {
    let failed_line = line.filter(|l| l.class == StepOutcomeClass::Failed);
    match end {
        StepEnd::TimedOut => RoleFailure::new(
            "timeout-ceiling",
            None,
            "step ran past its role timeout and was terminated",
        ),
        StepEnd::Cancelled => RoleFailure::new("cancelled", None, "step was cancelled"),
        StepEnd::Exited(code) if failed_line.is_some() => RoleFailure::new(
            "step-outcome-failed",
            code,
            "step session reported LOOM_STEP_OUTCOME class=failed",
        ),
        StepEnd::Exited(Some(code)) => RoleFailure::new(
            format!("exit-{code}"),
            Some(code),
            &format!("step exited with code {code}"),
        ),
        StepEnd::Exited(None) => RoleFailure::new(
            "exit-unobserved",
            None,
            "step exited with no observed status and no outcome line",
        ),
    }
}

/// The `loom.result` and close attributes of a step's `loom.sweep` span.
/// `outcome` is the tick outcome the step's `role_tick.outcome` reports
/// ([`super::capture::tick_outcome`]), so the two agree. Pure.
#[must_use]
pub(crate) fn sweep_close(
    end: StepEnd,
    line: Option<&StepOutcomeLine>,
    outcome: &RoleTickOutcome,
    role: StepRole,
    model: Option<&str>,
    effort: Option<&str>,
) -> (String, TraceAttributes) {
    let (result, _) = crate::role_tick_telemetry::classify(outcome);
    let result = crate::role_tick_telemetry::result_label(result);
    let mut close = lifecycle::attributes(&[("loom.role", role.role_name())]);
    for (key, value) in [("loom.configured_model", model), ("loom.effort", effort)] {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            close.insert(key.into(), value.to_owned());
        }
    }
    if matches!(outcome, RoleTickOutcome::Failure(_)) {
        close.extend(step_failure(end, line).attributes());
    } else if let StepEnd::Exited(Some(code)) = end {
        close.insert(lifecycle::EXIT_CODE.into(), code.to_string());
    }
    (result, close)
}

impl SweepRegistry {
    /// Close a finished step's `loom.sweep` span with its real outcome, at
    /// the moment its end is seen. A step a roll stopped (`pid`, the leader
    /// H4 noted) closes as `interrupted_by_roll` instead. A no-op when
    /// tracing is off, or when the span was already closed.
    pub(crate) fn close_step_span(
        &self,
        sweep_id: &str,
        pid: Option<u32>,
        result: String,
        mut close: TraceAttributes,
    ) {
        let result = match pid.and_then(roll_interruption::take_stopped) {
            Some(roll) => {
                roll.apply(&mut close);
                roll_interruption::RESULT.to_owned()
            }
            None => result,
        };
        lifecycle::finish_execution(&self.config.workspace_root, sweep_id, &result, close);
    }
}

/// Whether the step `sweep_id` in the workspace at `root` is still running:
/// its claim lock (`issue-<N>`, or `pr-<N>` for a PR step) still names it,
/// and its leader, or the process group it leads, is alive. `false` for
/// anything that is not a step's sweep id. Orphan recovery leaves a running
/// step's span to the step's own end ([`SweepRegistry::end_step`]).
#[must_use]
pub fn step_still_running(root: &Path, sweep_id: &str) -> bool {
    let Some((role, item)) = StepRole::parse_sweep_id(sweep_id) else {
        return false;
    };
    let ns = if role.acts_on_pr() { "pr" } else { "issue" };
    let owner_path = root
        .join(".loom/locks")
        .join(format!("{ns}-{item}"))
        .join("owner.json");
    let Some(owner) = std::fs::read_to_string(owner_path)
        .ok()
        .and_then(|s| serde_json::from_str::<LockOwner>(&s).ok())
    else {
        return false;
    };
    if owner.sweep_id != sweep_id {
        return false;
    }
    pid_identity::owner_pid_alive_since(owner.owner_pid, &owner.acquired_at)
        || owner.pgid.is_some_and(group_alive)
}

/// A dead leader's recorded group still has members, and is not ours or a
/// recycled number (the reaper's own checks, without its signal-path logs).
fn group_alive(pgid: u32) -> bool {
    pgid > 1
        && Some(pgid) != crate::sweep_registry::current_process_group()
        && !pid_identity::pgid_number_was_recycled(pgid)
        && crate::sweep_registry::reaper::group_has_members(pgid)
}

#[cfg(test)]
#[path = "span_tests.rs"]
mod tests;
