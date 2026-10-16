//! A step's end: its outcome line, and the role tick's capture (#11906).
//!
//! #11877 made a role tick journal the listings its agent read (the pick
//! journal) and export its `role_tick.outcome`. Both fired only inside a role
//! tick, and a step is not one: it runs detached in the sweep registry. So a
//! step carries a journal of its own from the spawn (`pick_journal::
//! attach_step`), and its end emits both through the role tick's own emit
//! (`role_tick_telemetry::emit_for_step`, which drains the journal into the
//! role's `pick.decision`), on a detached thread: the transcript scan behind
//! `role_tick.outcome` must not hold the registry lock. The record is marked
//! `source = "step"` with the step's sweep id, so it never counts as a
//! role-runner tick.
//!
//! The session's own word on its step is its last
//! `LOOM_STEP_OUTCOME role=… target=… class=… reason=…` line
//! (`targeted-role-invocation.md`, #11868), read from the step's log after
//! its own dispatch header. The class rides the record's detail and the end
//! log line; the outcome ledger (#11900) consumes it from
//! [`read_outcome_line`]. The line is advisory: the ledger decides with the
//! forge post-state, which this module does not read: the end queues the
//! step ([`super::ended`]) and the work finder's step tick reads the
//! post-state with the lock released (`step_dispatch::step_end`, #11909),
//! passing `None` when that read fails, never a default (#11962), so a failed
//! probe classifies as `failed`, not as the session's word.

use super::*;
use crate::observability::lifecycle::RoleFailure;
use crate::role_runner::RoleTickOutcome;

/// The four outcome classes of a targeted step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcomeClass {
    Verdict,
    Noop,
    Refused,
    Failed,
}

impl StepOutcomeClass {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "verdict" => Some(Self::Verdict),
            "noop" => Some(Self::Noop),
            "refused" => Some(Self::Refused),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verdict => "verdict",
            Self::Noop => "noop",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// One `LOOM_STEP_OUTCOME` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcomeLine {
    pub role: String,
    pub target: u32,
    pub class: StepOutcomeClass,
    pub reason: String,
}

const OUTCOME_PREFIX: &str = "LOOM_STEP_OUTCOME ";

/// Parse one outcome line. The prompt's own template
/// (`class=<verdict|noop|refused|failed>`) is not one. Pure.
#[must_use]
pub fn parse_outcome_line(line: &str) -> Option<StepOutcomeLine> {
    let rest = &line[line.find(OUTCOME_PREFIX)? + OUTCOME_PREFIX.len()..];
    let field = |key: &str| {
        rest.split_whitespace()
            .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim_matches(|c: char| c == '`' || c == '"'))
    };
    Some(StepOutcomeLine {
        role: field("role")?.to_string(),
        target: field("target")?.parse().ok()?,
        class: StepOutcomeClass::parse(field("class")?)?,
        reason: field("reason").unwrap_or("").to_string(),
    })
}

/// The last outcome line in `log` written after `sweep_id`'s own dispatch
/// header (an earlier run's line in the same per-item log is not this
/// step's). `None` when the session wrote none.
#[must_use]
pub fn read_outcome_line(log: &Path, sweep_id: &str) -> Option<StepOutcomeLine> {
    let text = String::from_utf8_lossy(&std::fs::read(log).ok()?).into_owned();
    let anchor = format!("sweep_id={sweep_id} ");
    let start = text.rfind(&anchor)?;
    text[start..].lines().rev().find_map(parse_outcome_line)
}

/// How a step's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEnd {
    /// It exited (the reaper saw it die), with its status when known.
    Exited(Option<i32>),
    /// Cancelled (by an operator or a roll).
    Cancelled,
    /// Killed by the reaper at its role's timeout (B3).
    TimedOut,
}

/// Whether a clean exit of a `role` step with `class` is a non-success: a
/// Builder step that exits 0 without opening a PR (`noop`) or without
/// writing any outcome line (#12500). Other roles' exit 0 is a completion.
fn clean_exit_failure(role: StepRole, class: Option<StepOutcomeClass>) -> Option<CleanExit> {
    if role != StepRole::Builder {
        return None;
    }
    match class {
        Some(StepOutcomeClass::Noop) => Some(CleanExit::NoPr),
        None => Some(CleanExit::NoOutcomeLine),
        Some(_) => None,
    }
}

#[derive(Clone, Copy)]
enum CleanExit {
    NoPr,
    NoOutcomeLine,
}

/// The role tick outcome a step's end maps to. The session's `failed`
/// class, a non-zero exit, a timeout and a cancel are failures, and so is a
/// Builder step that ends without a PR (`noop`, or no outcome line at all,
/// #12500); anything else completed. Pure.
#[must_use]
pub fn tick_outcome(
    role: StepRole,
    end: StepEnd,
    line: Option<&StepOutcomeLine>,
) -> RoleTickOutcome {
    let class = line.map(|l| l.class);
    let reason = line.map_or("", |l| l.reason.as_str());
    let clean = clean_exit_failure(role, class);
    match (end, class) {
        (StepEnd::TimedOut, _) => RoleTickOutcome::Failure("step timed out".to_string()),
        (StepEnd::Cancelled, _) => RoleTickOutcome::Failure("step cancelled".to_string()),
        (_, Some(StepOutcomeClass::Failed)) => {
            RoleTickOutcome::Failure(format!("step outcome failed: {reason}"))
        }
        (StepEnd::Exited(Some(code)), _) if code != 0 => {
            RoleTickOutcome::Failure(format!("step exited {code}"))
        }
        (StepEnd::Exited(None), None) => {
            RoleTickOutcome::Failure("step exited, status unknown".to_string())
        }
        _ => match clean {
            Some(CleanExit::NoPr) => RoleTickOutcome::Failure(format!("no_pr: {reason}")),
            Some(CleanExit::NoOutcomeLine) => {
                RoleTickOutcome::Failure("step exited without an outcome line".to_string())
            }
            None => RoleTickOutcome::Success,
        },
    }
}

/// The closed-vocabulary failure behind a failed [`tick_outcome`] (#12480):
/// `Some` exactly when `tick_outcome` is a `Failure`, one class per branch.
/// The exit code rides `exit_code`, never the class. Pure.
#[must_use]
pub fn step_failure(
    role: StepRole,
    end: StepEnd,
    line: Option<&StepOutcomeLine>,
) -> Option<RoleFailure> {
    use crate::observability::lifecycle::step;
    let class = line.map(|l| l.class);
    match (end, class) {
        (StepEnd::TimedOut, _) => Some(step::TIMED_OUT.failure(None)),
        (StepEnd::Cancelled, _) => Some(step::CANCELLED.failure(None)),
        (_, Some(StepOutcomeClass::Failed)) => Some(step::OUTCOME_FAILED.failure(None)),
        (StepEnd::Exited(Some(code)), _) if code != 0 => {
            Some(step::EXIT_NONZERO.failure(Some(code)))
        }
        (StepEnd::Exited(None), None) => Some(step::STATUS_UNKNOWN.failure(None)),
        (StepEnd::Exited(code), _) => match clean_exit_failure(role, class) {
            Some(CleanExit::NoPr) => Some(step::NO_PR.failure(code)),
            Some(CleanExit::NoOutcomeLine) => Some(step::NO_OUTCOME_LINE.failure(code)),
            None => None,
        },
    }
}

/// Everything the end-of-step emit needs, read out of the registry.
#[derive(Debug, Clone)]
pub(crate) struct StepCapture {
    pub(crate) root: PathBuf,
    pub(crate) role: StepRole,
    pub(crate) sweep_id: SweepId,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) outcome: RoleTickOutcome,
    pub(crate) failure: Option<RoleFailure>,
    pub(crate) model: String,
    pub(crate) effort: String,
}

/// Emit the role tick's capture for one finished step: its journal drained
/// into the role's `pick.decision`, and its `role_tick.outcome`. Blocking.
pub(crate) fn emit_step_capture(capture: &StepCapture) {
    crate::observability::pick_journal::adopt(
        &capture.root,
        crate::observability::pick_journal::step_path(&capture.sweep_id),
    );
    crate::role_tick_telemetry::emit_for_step(
        &capture.root,
        capture.role.role_name(),
        capture.started_at,
        &capture.outcome,
        Some(crate::role_tick_telemetry::ResolvedLaunch::new(&capture.model, &capture.effort)),
        capture.failure.clone(),
        &capture.sweep_id,
    );
}

impl SweepRegistry {
    /// A step ended (`release_dead_step`, or found dead at startup with no
    /// entry): reap the leases it still listed, then emit its capture off the
    /// lock. Returns its outcome line.
    pub(crate) fn end_step(
        &self,
        sweep_id: &str,
        item: u32,
        role: StepRole,
        started_at: DateTime<Utc>,
        end: StepEnd,
    ) -> Option<StepOutcomeLine> {
        let end = if super::timeout::take_timed_out(sweep_id) {
            StepEnd::TimedOut
        } else {
            end
        };
        let info = self.entries.get(sweep_id);
        let log_path =
            info.map_or_else(|| self.compute_step_log_path(item, role), |i| i.log_path.clone());
        let line = read_outcome_line(&log_path, sweep_id);
        log::info!(
            "sweep_registry: {role} step {sweep_id} ended ({end:?}), outcome {} (#11906)",
            line.as_ref().map_or("none".to_string(), |l| format!(
                "{} {}",
                l.class.as_str(),
                l.reason
            ))
        );
        // #12450: a granted step's assignment ends with the step, its outcome
        // class the release's outcome (no line reads as `failed`). A no-op
        // for a step the captain never granted.
        crate::fleet_dispatch::worker::release(
            sweep_id,
            line.as_ref().map_or("failed", |l| l.class.as_str()),
        );
        if !self.config.skip_label_flip {
            let reason = match end {
                StepEnd::TimedOut => crate::pr_lease_reap::REASON_TIMEOUT,
                StepEnd::Cancelled => "cancelled",
                StepEnd::Exited(_) => "exited",
            };
            self.reap_step_leases(sweep_id, reason);
        }
        if !self.config.skip_label_flip {
            super::ended::push(
                &self.config.workspace_root,
                super::ended::EndedStep {
                    sweep_id: sweep_id.to_string(),
                    item,
                    role,
                    log_path: log_path.clone(),
                    ran_for: (Utc::now() - started_at).to_std().unwrap_or_default(),
                    timed_out: end == StepEnd::TimedOut,
                },
            );
        }
        let capture = StepCapture {
            root: self.config.workspace_root.clone(),
            role,
            sweep_id: sweep_id.to_string(),
            started_at,
            outcome: tick_outcome(role, end, line.as_ref()),
            failure: step_failure(role, end, line.as_ref()),
            model: info.and_then(|i| i.model.clone()).unwrap_or_default(),
            effort: info.and_then(|i| i.effort.clone()).unwrap_or_default(),
        };
        // #12253: the step's `loom.sweep` span ends here, with the outcome
        // its `role_tick.outcome` reports, not at the next restart's orphan
        // recovery. Local journal writes only, so it runs under the lock.
        let (result, close) = super::span::sweep_close(
            end,
            line.as_ref(),
            &capture.outcome,
            role,
            info.and_then(|i| i.model.as_deref()),
            info.and_then(|i| i.effort.as_deref()),
        );
        self.close_step_span(sweep_id, info.map(|i| i.pid), result, close);
        // Hermetic fixtures (no forge) keep their tests free of the detached
        // transcript scan; the capture itself is tested directly.
        if !self.config.skip_label_flip {
            // #12027: the step's verdict record reads its log at the drain.
            let staged = crate::observability::step_verdict::StagedStep {
                sweep_id: sweep_id.to_string(),
                step: role,
                item,
                log: log_path,
            };
            let spawned = std::thread::Builder::new()
                .name("step-capture".to_string())
                .spawn(move || {
                    crate::observability::step_verdict::stage_step(staged);
                    emit_step_capture(&capture);
                });
            if let Err(e) = spawned {
                log::warn!("sweep_registry: {sweep_id}: no capture thread ({e})");
            }
        }
        line
    }
}
